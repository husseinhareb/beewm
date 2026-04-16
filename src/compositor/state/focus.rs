use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;

use super::{Beewm, root_surface};

impl Beewm {
    pub fn window_index_for_surface(
        &self,
        workspace_idx: usize,
        surface: &WlSurface,
    ) -> Option<usize> {
        let surface_root = root_surface(surface);
        self.workspace_windows[workspace_idx]
            .iter()
            .position(|window| {
                window
                    .wl_surface()
                    .as_ref()
                    .map(|window_surface| **window_surface == surface_root)
                    .unwrap_or(false)
            })
    }

    pub fn mapped_window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.window_lookup.get(&root_surface(surface)).cloned()
    }

    pub fn track_window(&mut self, window: &Window) {
        if let Some(surface) = window.wl_surface().as_ref() {
            self.window_lookup
                .insert((**surface).clone(), window.clone());
        }
    }

    pub fn untrack_window_for_surface(&mut self, surface: &WlSurface) {
        self.window_lookup.remove(&root_surface(surface));
    }

    pub fn active_workspace_focused_index(&self) -> Option<usize> {
        match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(surface) => self.window_index_for_surface(self.active_workspace, &surface),
            None => self.workspaces[self.active_workspace].focused_idx,
        }
    }

    pub fn active_workspace_focused_window(&self) -> Option<&Window> {
        let idx = self.active_workspace_focused_index()?;
        self.workspace_windows[self.active_workspace].get(idx)
    }

    pub fn note_keyboard_focus_change(&mut self, focused: Option<&WlSurface>) {
        if let Some(surface) = focused {
            if let Some(idx) = self.window_index_for_surface(self.active_workspace, surface) {
                self.workspaces[self.active_workspace].focused_idx = Some(idx);
            }
        }

        self.border_commit_serial = self.border_commit_serial.wrapping_add(1);
        self.needs_render = true;
    }

    pub fn set_keyboard_focus(&mut self, focused: Option<WlSurface>) {
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, focused.clone(), serial);

        // Smithay does not invoke SeatHandler::focus_changed when the focus is unset.
        if focused.is_none() {
            self.note_keyboard_focus_change(None);
        }
    }
}
