use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus;

use super::{Beewm, root_surface};

impl Beewm {
    pub(crate) fn window_root_surface(window: &Window) -> Option<WlSurface> {
        window.wl_surface().map(|surface| root_surface(&surface))
    }

    pub(crate) fn is_root_floating(&self, root: &WlSurface) -> bool {
        self.floating_windows.contains_key(root)
    }

    /// The window fullscreened on the *active* workspace, if any.
    pub(crate) fn active_fullscreen(&self) -> Option<&Window> {
        self.workspaces[self.active_workspace()].fullscreen.as_ref()
    }

    pub(crate) fn is_root_fullscreen(&self, root: &WlSurface) -> bool {
        // Check every workspace, not just the active one: a window can be the
        // fullscreen of a hidden workspace and must still be treated as
        // fullscreen (e.g. when relaying out or reclassifying that workspace).
        self.workspaces.iter().any(|workspace| {
            workspace
                .fullscreen
                .as_ref()
                .and_then(Self::window_root_surface)
                .map(|fullscreen_root| fullscreen_root == *root)
                .unwrap_or(false)
        })
    }

    pub(crate) fn focused_tiled_window_root(&self, workspace_idx: usize) -> Option<WlSurface> {
        let keyboard_focus = (workspace_idx == self.active_workspace())
            .then(|| {
                self.seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.current_focus())
                    .and_then(|target| target.wl_surface().map(|s| s.into_owned()))
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

    /// Smallest tile worth creating, in logical pixels. Below this a window has
    /// no room for content once its border and gap are taken out.
    const MIN_TILE: (u32, u32) = (160, 120);

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

        let split_target = self.usable_split_target(workspace_idx, split_target);
        self.layout_manager
            .insert(workspace_idx, split_target.as_ref(), root);
    }

    /// Pick the tile a new window should split.
    ///
    /// Every insert halves its target, so repeatedly opening windows onto the
    /// newest one shrinks it geometrically: the nth window gets 2⁻ⁿ of the
    /// screen, and by around the twelfth it is a handful of pixels. When the
    /// window the user is actually on can no longer be halved into two usable
    /// tiles, the new window goes to the largest tile on the workspace instead.
    /// That bounds the shrinking — the biggest tile is always at least the
    /// average — at the cost of the new window not landing next to the focused
    /// one, which only happens once the focused one is too small to share.
    ///
    /// Returns the caller's choice untouched whenever it still has room, which
    /// is the overwhelmingly common case.
    fn usable_split_target(
        &self,
        workspace_idx: usize,
        requested: Option<&WlSurface>,
    ) -> Option<WlSurface> {
        let usable = self.tiling_usable_geometry()?;
        let tiled_roots = self.tiled_window_roots_in_workspace(workspace_idx);
        let geometries = self
            .layout_manager
            .geometries(workspace_idx, &usable, &tiled_roots);

        let splittable = |geo: &crate::model::window::Geometry| {
            // Halving happens on alternating axes, so a tile is only safe to
            // split if *either* half would still be usable. Checking both axes
            // keeps a tall-but-narrow tile from being ruled out.
            (geo.width / 2 >= Self::MIN_TILE.0 && geo.height >= Self::MIN_TILE.1)
                || (geo.height / 2 >= Self::MIN_TILE.1 && geo.width >= Self::MIN_TILE.0)
        };

        if let Some(requested) = requested
            && geometries.get(requested).map(splittable).unwrap_or(false)
        {
            return Some(requested.clone());
        }

        geometries
            .iter()
            .max_by_key(|(_, geo)| geo.width as u64 * geo.height as u64)
            .map(|(root, _)| root.clone())
            .or_else(|| requested.cloned())
    }

    pub(crate) fn remove_tiled_window(&mut self, workspace_idx: usize, surface: &WlSurface) {
        self.layout_manager
            .remove(workspace_idx, &root_surface(surface));
    }

    /// The window fullscreened on the workspace `output` is currently showing.
    ///
    /// Per-output on purpose: every fullscreen decision below (layer
    /// suppression, borders, scanout, pointer hit-testing) applies to one
    /// output's frame, so asking about the *focused* output's workspace would
    /// let a game on one monitor blank the panels on all the others.
    pub(crate) fn fullscreen_on_output(&self, output: &Output) -> Option<&Window> {
        let ws_idx = self
            .outputs
            .iter()
            .find(|ctx| &ctx.output == output)
            .map(|ctx| ctx.active_workspace)?;
        self.workspaces[ws_idx].fullscreen.as_ref()
    }

    pub(crate) fn rectangle_covers_output(
        &self,
        output: &Output,
        geo: Rectangle<i32, Logical>,
    ) -> bool {
        let Some(output_geo) = self.space.output_geometry(output) else {
            return false;
        };

        geo.loc.x <= output_geo.loc.x
            && geo.loc.y <= output_geo.loc.y
            && geo.loc.x + geo.size.w >= output_geo.loc.x + output_geo.size.w
            && geo.loc.y + geo.size.h >= output_geo.loc.y + output_geo.size.h
    }

    pub(crate) fn x11_window_covers_output(&self, output: &Output, window: &Window) -> bool {
        window.x11_surface().is_some()
            && self
                .space
                .element_geometry(window)
                .map(|geo| self.rectangle_covers_output(output, geo))
                .unwrap_or(false)
    }

    pub fn screen_owned_by_x11_window(&self, output: &Output) -> bool {
        self.fullscreen_on_output(output)
            .and_then(|window| window.x11_surface())
            .is_some()
            || self
                .space
                .elements()
                .any(|window| self.x11_window_covers_output(output, window))
    }

    /// True when something is occupying the whole of `output` and we should
    /// treat that screen as fullscreen-owned for layer suppression / scanout
    /// purposes. Covers the two paths that block layers:
    /// 1. An app fullscreened via xdg-shell or `_NET_WM_STATE_FULLSCREEN`
    ///    (the usual `fullscreen_window` field).
    /// 2. An X11 window that has sized itself to cover the output. Some games
    ///    do this without keeping `_NET_WM_STATE_FULLSCREEN` set, so using
    ///    only `fullscreen_window` lets borders/layers reappear and prevents
    ///    direct scanout.
    pub fn screen_owned_by_window(&self, output: &Output) -> bool {
        if self.fullscreen_on_output(output).is_some() {
            return true;
        }

        self.screen_owned_by_x11_window(output)
    }

    /// Re-raise every floating window of the active workspace so that they
    /// sit above all tiled windows in the space's z-stack.
    ///
    /// Re-raising in `workspaces[ws].windows` insertion order preserves the
    /// relative stacking of multiple floating windows: the most recently
    /// inserted one ends up on top because it is raised last.
    pub(crate) fn raise_floating_windows(&mut self) {
        let ws_idx = self.active_workspace();
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
        // Sticky windows (browser Picture-in-Picture) must stay above the
        // floating stack too. They live in their home workspace's window list,
        // so the loop above never sees them once you switch away.
        self.raise_sticky_windows();
    }
}
