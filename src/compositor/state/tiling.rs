use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;

use super::{Beewm, root_surface};

impl Beewm {
    pub(crate) fn window_root_surface(window: &Window) -> Option<WlSurface> {
        window
            .wl_surface()
            .map(|surface| root_surface(&surface.into_owned()))
    }

    pub(crate) fn is_root_floating(&self, root: &WlSurface) -> bool {
        self.floating_windows.contains_key(root)
    }

    pub(crate) fn is_root_fullscreen(&self, root: &WlSurface) -> bool {
        self.fullscreen_window
            .as_ref()
            .and_then(Self::window_root_surface)
            .map(|fullscreen_root| fullscreen_root == *root)
            .unwrap_or(false)
    }

    pub(crate) fn focused_tiled_window_root(&self, workspace_idx: usize) -> Option<WlSurface> {
        let keyboard_focus = (workspace_idx == self.active_workspace)
            .then(|| {
                self.seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus())
            })
            .flatten()
            .and_then(|surface| {
                self.window_index_for_surface(workspace_idx, &surface)
                    .and_then(|idx| self.workspaces[workspace_idx].windows.get(idx))
                    .and_then(Self::window_root_surface)
            });

        keyboard_focus
            .or_else(|| {
                self.workspaces[workspace_idx]
                    .focused_idx
                    .and_then(|idx| self.workspaces[workspace_idx].windows.get(idx))
                    .and_then(Self::window_root_surface)
            })
            .filter(|root| !self.is_root_floating(root) && !self.is_root_fullscreen(root))
    }

    pub(crate) fn tiled_windows_in_workspace(&self, workspace_idx: usize) -> Vec<Window> {
        self.workspaces[workspace_idx]
            .windows
            .iter()
            .filter(|window| {
                let root = Self::window_root_surface(window);
                let is_fullscreen = root
                    .as_ref()
                    .map(|root| self.is_root_fullscreen(root))
                    .unwrap_or(false);
                let is_floating = root
                    .as_ref()
                    .map(|root| self.is_root_floating(root))
                    .unwrap_or(false);
                !is_fullscreen && !is_floating
            })
            .cloned()
            .collect()
    }

    pub(crate) fn tiled_window_roots_in_workspace(&self, workspace_idx: usize) -> Vec<WlSurface> {
        self.tiled_windows_in_workspace(workspace_idx)
            .iter()
            .filter_map(Self::window_root_surface)
            .collect()
    }

    pub(crate) fn insert_tiled_window(
        &mut self,
        workspace_idx: usize,
        window: &Window,
        split_target: Option<&WlSurface>,
    ) {
        let Some(root) = Self::window_root_surface(window) else {
            return;
        };

        if self.is_root_floating(&root) || self.is_root_fullscreen(&root) {
            return;
        }

        self.layout_manager
            .insert(workspace_idx, split_target, root);
    }

    pub(crate) fn remove_tiled_window(&mut self, workspace_idx: usize, surface: &WlSurface) {
        self.layout_manager
            .remove(workspace_idx, &root_surface(surface));
    }

    /// Re-raise every floating window of the active workspace so that they
    /// sit above all tiled windows in the space's z-stack.
    ///
    /// Re-raising in `workspaces[ws].windows` insertion order preserves the
    /// relative stacking of multiple floating windows: the most recently
    /// inserted one ends up on top because it is raised last.
    pub(crate) fn raise_floating_windows(&mut self) {
        let ws_idx = self.active_workspace;
        let floating: Vec<Window> = self.workspaces[ws_idx]
            .windows
            .iter()
            .filter(|window| {
                Self::window_root_surface(window)
                    .map(|root| self.is_root_floating(&root))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for window in floating {
            // `activate = false` so the floating windows' xdg activated state is
            // not toggled — only their z-position is corrected.
            self.space.raise_element(&window, false);
        }
    }
}
