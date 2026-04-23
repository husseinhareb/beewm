use smithay::delegate_xwayland_shell;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowType, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XwmHandler};

use crate::compositor::state::{Beewm, root_surface};

use super::PendingX11Kind;

impl Beewm {
    fn map_x11_window(&mut self, surface: X11Surface, kind: PendingX11Kind) {
        let window = Window::new_x11_window(surface.clone());

        match kind {
            PendingX11Kind::OverrideRedirect => {
                tracing::debug!(
                    title = %surface.title(),
                    class = %surface.class(),
                    instance = %surface.instance(),
                    geometry = ?surface.geometry(),
                    "mapping override-redirect X11 window",
                );
                self.track_window(&window);
                self.space.map_element(window, surface.geometry().loc, true);
                self.needs_render = true;
            }
            PendingX11Kind::Managed => {
                let workspace_idx = self.active_workspace;
                let should_float = should_map_x11_floating(&surface);
                let split_target = self.focused_tiled_window_root(workspace_idx);

                tracing::debug!(
                    title = %surface.title(),
                    class = %surface.class(),
                    instance = %surface.instance(),
                    geometry = ?surface.geometry(),
                    should_float,
                    window_type = ?surface.window_type(),
                    transient_for = ?surface.is_transient_for(),
                    "finalizing managed X11 window mapping",
                );

                self.workspaces[workspace_idx].add_window(window.clone());
                self.publish_workspace_state();
                self.track_window(&window);

                if should_float {
                    self.map_as_floating_centered(&window);
                } else {
                    self.insert_tiled_window(workspace_idx, &window, split_target.as_ref());
                    self.relayout();
                }

                if let Some(wl_surface) = window.wl_surface().map(|surface| surface.into_owned()) {
                    self.set_keyboard_focus(Some(wl_surface));
                }

                self.space.raise_element(&window, true);

                let geometry = self
                    .space
                    .element_geometry(&window)
                    .filter(|geometry| geometry.size.w > 0 && geometry.size.h > 0)
                    .or_else(|| {
                        Self::window_root_surface(&window).and_then(|root| {
                            self.floating_windows.get(&root).map(|floating| {
                                Rectangle::new(
                                    Point::from((floating.position.x, floating.position.y)),
                                    floating.size,
                                )
                            })
                        })
                    });

                if let Some(geometry) = geometry {
                    if let Err(error) = surface.configure(geometry) {
                        tracing::warn!("Failed to configure mapped X11 window: {}", error);
                    }
                } else {
                    tracing::debug!(
                        title = %surface.title(),
                        class = %surface.class(),
                        instance = %surface.instance(),
                        "managed X11 window has no non-zero target geometry yet",
                    );
                }

                self.needs_render = true;
            }
        }
    }

    fn remove_x11_window(&mut self, surface: &X11Surface) {
        let _ = self.take_pending_x11_window(surface);

        let Some((workspace_idx, window_idx)) =
            self.workspaces
                .iter()
                .enumerate()
                .find_map(|(workspace_idx, workspace)| {
                    workspace
                        .windows
                        .iter()
                        .position(|window| {
                            window
                                .x11_surface()
                                .map(|candidate| candidate == surface)
                                .unwrap_or(false)
                        })
                        .map(|window_idx| (workspace_idx, window_idx))
                })
        else {
            let maybe_override_redirect = self
                .space
                .elements()
                .find(|window| {
                    window
                        .x11_surface()
                        .map(|candidate| candidate == surface)
                        .unwrap_or(false)
                })
                .cloned();

            if let Some(window) = maybe_override_redirect {
                if let Some(wl_surface) = window.wl_surface().map(|surface| surface.into_owned()) {
                    self.untrack_window_for_surface(&wl_surface);
                }
                self.space.unmap_elem(&window);
                self.needs_render = true;
            }

            if !surface.is_override_redirect() {
                let _ = surface.set_mapped(false);
            }
            return;
        };

        let window = self.workspaces[workspace_idx]
            .remove_window(window_idx)
            .expect("x11 window index should be valid");
        let tracked_surface = window.wl_surface().map(|surface| surface.into_owned());
        let should_restore_focus = if workspace_idx == self.active_workspace {
            match self
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
            {
                Some(current_focus) => tracked_surface
                    .as_ref()
                    .map(|surface| root_surface(surface) == root_surface(&current_focus))
                    .unwrap_or(false),
                None => true,
            }
        } else {
            false
        };

        if let Some(surface) = tracked_surface.as_ref() {
            self.untrack_window_for_surface(surface);
            self.floating_windows.remove(surface);
            self.remove_tiled_window(workspace_idx, surface);
        }

        let was_fullscreen = self
            .fullscreen_window
            .as_ref()
            .and_then(|window| window.x11_surface())
            .map(|candidate| candidate == surface)
            .unwrap_or(false);
        if was_fullscreen {
            self.fullscreen_window = None;
            for sibling in &self.workspaces[workspace_idx].windows {
                if self.space.element_geometry(sibling).is_none() {
                    self.space.map_element(sibling.clone(), (0, 0), false);
                }
            }
        }

        self.space.unmap_elem(&window);
        self.publish_workspace_state();

        if workspace_idx == self.active_workspace {
            if should_restore_focus {
                let focus = self.workspaces[self.active_workspace]
                    .focused_idx
                    .and_then(|focus_idx| {
                        self.workspaces[self.active_workspace]
                            .windows
                            .get(focus_idx)
                    })
                    .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
                self.set_keyboard_focus(focus);
            }
            self.relayout();
        } else {
            self.needs_render = true;
        }

        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }
}

fn should_map_x11_floating(surface: &X11Surface) -> bool {
    matches!(
        surface.window_type(),
        Some(
            WmWindowType::Dialog
                | WmWindowType::DropdownMenu
                | WmWindowType::Menu
                | WmWindowType::Notification
                | WmWindowType::PopupMenu
                | WmWindowType::Splash
                | WmWindowType::Toolbar
                | WmWindowType::Tooltip
                | WmWindowType::Utility
        )
    ) || surface.is_popup()
        || surface.is_transient_for().is_some()
        || surface
            .min_size()
            .zip(surface.max_size())
            .map(|(min_size, max_size)| min_size == max_size)
            .unwrap_or(false)
}

impl XWaylandShellHandler for Beewm {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, surface: X11Surface) {
        tracing::debug!(
            title = %surface.title(),
            class = %surface.class(),
            instance = %surface.instance(),
            geometry = ?surface.geometry(),
            "associated X11 window with wl_surface",
        );
        if let Some(pending) = self.take_pending_x11_window(&surface) {
            self.map_x11_window(surface, pending.kind);
        }
    }
}

impl XwmHandler for Beewm {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("XWayland WM should exist")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(
            title = %window.title(),
            class = %window.class(),
            instance = %window.instance(),
            geometry = ?window.geometry(),
            window_type = ?window.window_type(),
            transient_for = ?window.is_transient_for(),
            "new managed X11 window",
        );
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(
            title = %window.title(),
            class = %window.class(),
            instance = %window.instance(),
            geometry = ?window.geometry(),
            "new override-redirect X11 window",
        );
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(
            title = %window.title(),
            class = %window.class(),
            instance = %window.instance(),
            geometry = ?window.geometry(),
            has_wl_surface = window.wl_surface().is_some(),
            window_type = ?window.window_type(),
            transient_for = ?window.is_transient_for(),
            "map request for managed X11 window",
        );

        // Some XWayland clients only produce/associate their wl_surface after
        // the X11 map has been granted. Map the X11 side immediately, then
        // integrate the window into the compositor scene graph once the
        // wl_surface association arrives.
        if let Err(error) = window.set_mapped(true) {
            tracing::warn!("Failed to map X11 window: {}", error);
            return;
        }

        if window.wl_surface().is_some() {
            self.map_x11_window(window, PendingX11Kind::Managed);
        } else {
            self.queue_x11_window(window, PendingX11Kind::Managed);
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(
            title = %window.title(),
            class = %window.class(),
            instance = %window.instance(),
            geometry = ?window.geometry(),
            has_wl_surface = window.wl_surface().is_some(),
            "mapped override-redirect X11 window notification",
        );
        if window.wl_surface().is_some() {
            self.map_x11_window(window, PendingX11Kind::OverrideRedirect);
        } else {
            self.queue_x11_window(window, PendingX11Kind::OverrideRedirect);
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remove_x11_window(&window);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geometry = self
            .space
            .elements()
            .find(|candidate| {
                candidate
                    .x11_surface()
                    .map(|surface| surface == &window)
                    .unwrap_or(false)
            })
            .and_then(|window| self.space.element_geometry(window))
            .unwrap_or_else(|| window.geometry());

        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        if let Some(w) = w {
            geometry.size.w = w as i32;
        }
        if let Some(h) = h {
            geometry.size.h = h as i32;
        }

        if let Err(error) = window.configure(geometry) {
            tracing::warn!("Failed to configure X11 window request: {}", error);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let Some(mapped) = self
            .space
            .elements()
            .find(|candidate| {
                candidate
                    .x11_surface()
                    .map(|surface| surface == &window)
                    .unwrap_or(false)
            })
            .cloned()
        else {
            return;
        };

        let is_workspace_window = self.workspaces.iter().any(|workspace| {
            workspace
                .windows
                .iter()
                .any(|candidate| *candidate == mapped)
        });

        if !is_workspace_window {
            self.space.map_element(mapped.clone(), geometry.loc, false);
            self.needs_render = true;
            return;
        }

        if let Some(root) = Self::window_root_surface(&mapped) {
            if self.is_root_floating(&root) {
                self.floating_windows.insert(
                    root,
                    crate::compositor::types::FloatingWindowData::new(geometry.loc, geometry.size),
                );
                self.space.map_element(mapped, geometry.loc, false);
                self.needs_render = true;
            }
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn disconnected(&mut self, _xwm: XwmId) {
        self.xwm = None;
        self.xdisplay = None;
    }
}

delegate_xwayland_shell!(Beewm);
