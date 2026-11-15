//! `xdg-activation-v1`: lets one client hand focus to another window.
//!
//! This is what makes "click a link in the terminal" land you in the browser,
//! "open" on a notification raise its app, and opening a file in an
//! already-running editor bring that editor forward.
//!
//! Focus stealing policy: a token is only honoured if, when it was created,
//! the requesting client was the one the user was interacting with (it held
//! keyboard or pointer focus) and it quoted a serial from a real input event on
//! our seat. Tokens also expire, so an app cannot stash one and pop up later.

use std::time::Duration;

use smithay::input::Seat;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};

use crate::compositor::focus_target::KeyboardFocusTarget;
use crate::compositor::state::Beewm;

/// How long after its creation a token may still move focus. Long enough for a
/// cold-starting browser to open a link, short enough that a stale token can't
/// yank focus away minutes later.
const TOKEN_LIFETIME: Duration = Duration::from_secs(10);

impl XdgActivationHandler for Beewm {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    /// Returning `false` drops the token, so `request_activation` never fires
    /// for it and the requesting app simply stays where it is.
    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let valid = self.token_is_trusted(&data);
        if !valid {
            tracing::debug!(
                target: "beewm::activation",
                app_id = data.app_id.as_deref().unwrap_or(""),
                "rejected activation token: requester was not the focused client",
            );
        }
        valid
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let fresh = token_data.timestamp.elapsed() < TOKEN_LIFETIME;
        // One use per token: a second request with the same token is either a
        // bug or an attempt to replay it.
        self.xdg_activation_state.remove_token(&token);
        if !fresh || self.locked {
            return;
        }
        self.activate_surface(&surface);
    }
}

impl Beewm {
    fn token_is_trusted(&self, data: &XdgActivationTokenData) -> bool {
        let Some((serial, seat)) = data.serial.as_ref() else {
            return false;
        };
        if Seat::<Beewm>::from_resource(seat).as_ref() != Some(&self.seat) {
            return false;
        }
        let Some(requester) = data.client_id.as_ref() else {
            return false;
        };

        let keyboard = self.seat.get_keyboard();
        let keyboard_client = keyboard
            .as_ref()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|focus| focus.wl_surface().and_then(|s| s.client()))
            .map(|client| client.id());
        // The serial must postdate the moment that client got keyboard focus,
        // or it is a leftover from an earlier focus period.
        let keyboard_ok = keyboard_client.as_ref() == Some(requester)
            && keyboard
                .as_ref()
                .and_then(|keyboard| keyboard.last_enter())
                .map(|enter| serial.is_no_older_than(&enter))
                .unwrap_or(false);

        // Clicking something that never takes keyboard focus, like a
        // notification popup or a bar, still counts as the user's intent.
        let pointer_ok = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.current_focus())
            .and_then(|surface| surface.client())
            .map(|client| &client.id() == requester)
            .unwrap_or(false);

        keyboard_ok || pointer_ok
    }

    /// Bring the window owning `surface` forward, switching to its workspace
    /// first when it lives on one that is not visible.
    fn activate_surface(&mut self, surface: &WlSurface) {
        let Some(window) = self.mapped_window_for_surface(surface) else {
            // Not mapped yet. A brand-new window is focused on map anyway.
            return;
        };
        let Some(root) = Self::window_root_surface(&window) else {
            return;
        };
        let Some(workspace) = (0..self.workspaces.len())
            .find(|&idx| self.window_index_for_surface(idx, &root).is_some())
        else {
            return;
        };

        // The overview would otherwise sit on top of the window we just raised.
        self.close_overview(false);

        // A sticky window is already on screen wherever we are; a floating one
        // still lists under its home workspace, so going there would be wrong.
        if self.is_window_sticky(&window) {
            if let Some(target) = KeyboardFocusTarget::from_window(&window) {
                self.set_keyboard_focus_target(Some(target));
            }
            self.space.raise_element(&window, true);
            self.needs_render = true;
            return;
        }

        if workspace != self.active_workspace() {
            self.switch_workspace(workspace);
        }
        if let Some(idx) = self.window_index_for_surface(self.active_workspace(), &root) {
            tracing::debug!(
                target: "beewm::activation",
                workspace = workspace + 1,
                "activating window on request",
            );
            self.focus_active_workspace_window(idx);
        }
    }
}
