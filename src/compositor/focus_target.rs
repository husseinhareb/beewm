//! Unified keyboard focus target for Wayland and XWayland clients.
//!
//! Smithay's `Seat` is generic over `SeatHandler::KeyboardFocus`. If we set
//! that type to a plain `WlSurface`, then focusing an X11 window's wl_surface
//! delivers `wl_keyboard.enter` to XWayland but never invokes the X11-side
//! `set_input_focus`/`WM_TAKE_FOCUS` dance that `X11Surface`'s own
//! `KeyboardTarget` impl performs. The X11 client then has no idea which of
//! its windows the keys belong to and drops them, which manifests as
//! "keyboard does nothing in this game" / "Steam buttons don't respond".
//!
//! This enum is the canonical fix: focus stays type-safe but smithay can
//! dispatch the right protocol-specific enter/leave path depending on which
//! variant we picked.

use std::borrow::Cow;

use smithay::desktop::{PopupKind, Window};
use smithay::input::Seat;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
};
use smithay::input::touch::{
    DownEvent, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchTarget,
    UpEvent,
};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use super::state::Beewm;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// Pick the right variant for a `Window`: an X11Surface when one is
    /// attached (so XWayland's protocol-specific focus path runs), otherwise
    /// the underlying wl_surface (xdg-shell toplevels).
    pub fn from_window(window: &Window) -> Option<Self> {
        if let Some(x11) = window.x11_surface().cloned() {
            return Some(Self::X11(x11));
        }
        window
            .wl_surface()
            .map(|s| Self::Wayland(s.into_owned()))
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl From<X11Surface> for KeyboardFocusTarget {
    fn from(surface: X11Surface) -> Self {
        Self::X11(surface)
    }
}

/// Required by smithay's `PopupKeyboardGrab` (it converts the active keyboard
/// focus to a pointer focus when granting a popup grab). For both variants
/// the underlying wl_surface always exists by the time something is focused
/// — Wayland holds it inherently, and X11 windows are only focused after
/// XWayland associates a wl_surface with them.
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        target
            .wl_surface()
            .map(|s| s.into_owned())
            .expect("focused target had no associated wl_surface")
    }
}

/// Required by smithay's `PopupManager::grab_popup`: when a popup grabs the
/// keyboard, the manager constructs a `KeyboardFocus` from the popup's
/// `PopupKind`. Popups are always Wayland surfaces, so we wrap accordingly.
impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

impl IsAlive for KeyboardFocusTarget {
    #[inline]
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    #[inline]
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(Cow::Borrowed(s)),
            // X11Surface has an inherent `wl_surface() -> Option<WlSurface>`
            // that shadows the WaylandFocus trait method — disambiguate.
            Self::X11(s) => <X11Surface as WaylandFocus>::wl_surface(s),
        }
    }

    fn same_client_as(&self, object_id: &ObjectId) -> bool {
        match self {
            Self::Wayland(s) => s.id().same_client_as(object_id),
            Self::X11(s) => s.same_client_as(object_id),
        }
    }
}

impl KeyboardTarget<Beewm> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Beewm>, data: &mut Beewm, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        key: KeysymHandle<'_>,
        state: smithay::backend::input::KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
        }
    }
}

// PointerTarget is implemented on plain WlSurface in the rest of the code
// (set_pointer_focus etc.); pointer focus is still WlSurface-based for now.
// The two impls below let our SeatHandler declare both KeyboardFocus and
// PointerFocus / TouchFocus as KeyboardFocusTarget so we can converge them
// in a future change without re-typing every callsite.
impl PointerTarget<Beewm> for KeyboardFocusTarget {
    fn enter(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &MotionEvent) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::enter(s.as_ref(), seat, data, event);
        }
    }

    fn motion(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &MotionEvent) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::motion(&*s, seat, data, event);
        }
    }

    fn relative_motion(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &RelativeMotionEvent) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::relative_motion(&*s, seat, data, event);
        }
    }

    fn button(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &ButtonEvent) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::button(&*s, seat, data, event);
        }
    }

    fn axis(&self, seat: &Seat<Beewm>, data: &mut Beewm, frame: AxisFrame) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::axis(&*s, seat, data, frame);
        }
    }

    fn frame(&self, seat: &Seat<Beewm>, data: &mut Beewm) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::frame(&*s, seat, data);
        }
    }

    fn leave(&self, seat: &Seat<Beewm>, data: &mut Beewm, serial: Serial, time: u32) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::leave(&*s, seat, data, serial, time);
        }
    }

    fn gesture_swipe_begin(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GestureSwipeBeginEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_swipe_begin(&*s, seat, data, event);
        }
    }

    fn gesture_swipe_update(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GestureSwipeUpdateEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_swipe_update(&*s, seat, data, event);
        }
    }

    fn gesture_swipe_end(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GestureSwipeEndEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_swipe_end(&*s, seat, data, event);
        }
    }

    fn gesture_pinch_begin(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GesturePinchBeginEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_pinch_begin(&*s, seat, data, event);
        }
    }

    fn gesture_pinch_update(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GesturePinchUpdateEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_pinch_update(&*s, seat, data, event);
        }
    }

    fn gesture_pinch_end(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GesturePinchEndEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_pinch_end(&*s, seat, data, event);
        }
    }

    fn gesture_hold_begin(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GestureHoldBeginEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_hold_begin(&*s, seat, data, event);
        }
    }

    fn gesture_hold_end(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &GestureHoldEndEvent,
    ) {
        if let Some(s) = self.wl_surface() {
            PointerTarget::gesture_hold_end(&*s, seat, data, event);
        }
    }
}

impl TouchTarget<Beewm> for KeyboardFocusTarget {
    fn down(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &DownEvent, serial: Serial) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::down(&*s, seat, data, event, serial);
        }
    }

    fn up(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &UpEvent, serial: Serial) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::up(&*s, seat, data, event, serial);
        }
    }

    fn motion(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &TouchMotionEvent,
        serial: Serial,
    ) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::motion(&*s, seat, data, event, serial);
        }
    }

    fn frame(&self, seat: &Seat<Beewm>, data: &mut Beewm, serial: Serial) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::frame(&*s, seat, data, serial);
        }
    }

    fn cancel(&self, seat: &Seat<Beewm>, data: &mut Beewm, serial: Serial) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::cancel(&*s, seat, data, serial);
        }
    }

    fn shape(&self, seat: &Seat<Beewm>, data: &mut Beewm, event: &ShapeEvent, serial: Serial) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::shape(&*s, seat, data, event, serial);
        }
    }

    fn orientation(
        &self,
        seat: &Seat<Beewm>,
        data: &mut Beewm,
        event: &OrientationEvent,
        serial: Serial,
    ) {
        if let Some(s) = self.wl_surface() {
            TouchTarget::orientation(&*s, seat, data, event, serial);
        }
    }
}
