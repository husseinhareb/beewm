use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Point, Size};

use crate::model::window::Geometry;

use super::{root_surface, Beewm};

impl Beewm {
    /// Toggle the floating state of the currently focused window.
    pub fn toggle_float(&mut self) {
        let window = match self.active_workspace_focused_window().cloned() {
            Some(w) => w,
            None => return,
        };
        let surface = match window.toplevel().map(|t| t.wl_surface().clone()) {
            Some(s) => s,
            None => return,
        };
        let root = root_surface(&surface);

        if self.floating_windows.remove(&root).is_some() {
            // Was floating — sink back into tiling.
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| {
                    s.states.unset(xdg_toplevel::State::Resizing);
                });
                toplevel.send_configure();
            }
            self.relayout();
        } else {
            // Tile → float: resize to half the screen and center.
            let output = match self.space.outputs().next().cloned() {
                Some(o) => o,
                None => return,
            };
            let output_geo = self.space.output_geometry(&output).unwrap();
            let float_w = output_geo.size.w / 2;
            let float_h = output_geo.size.h / 2;
            let pos = Point::from((
                output_geo.loc.x + (output_geo.size.w - float_w) / 2,
                output_geo.loc.y + (output_geo.size.h - float_h) / 2,
            ));
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| {
                    s.size = Some(Size::from((float_w, float_h)));
                });
                toplevel.send_configure();
            }
            self.space.map_element(window.clone(), pos, true);
            self.floating_windows.insert(root, pos);
            // Relayout the remaining tiled windows.
            self.relayout();
            self.needs_render = true;
        }
    }

    /// Re-place all floating windows on the active workspace back into the
    /// space at their stored positions. Called after relayout so they sit
    /// on top of tiled windows.
    fn remap_floating_windows(&mut self) {
        let ws_idx = self.active_workspace;
        for window in self.workspace_windows[ws_idx].clone() {
            let root = match window.toplevel().map(|t| t.wl_surface().clone()) {
                Some(surface) => root_surface(&surface),
                None => continue,
            };
            if let Some(&pos) = self.floating_windows.get(&root) {
                self.space.map_element(window, pos, false);
            }
        }
    }

    /// Toggle fullscreen for the currently focused window.
    pub fn toggle_fullscreen(&mut self) {
        if let Some(fs_window) = self.fullscreen_window.take() {
            // Tell the client it is no longer fullscreen.
            if let Some(toplevel) = fs_window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.size = None;
                });
                toplevel.send_configure();
            }
            // Remap all sibling windows that were hidden behind the fullscreen one.
            let ws_idx = self.active_workspace;
            for window in self.workspace_windows[ws_idx].clone() {
                if self.space.element_geometry(&window).is_none() {
                    self.space.map_element(window, (0, 0), false);
                }
            }
            self.relayout();
        } else {
            let window = match self.active_workspace_focused_window().cloned() {
                Some(w) => w,
                None => return,
            };
            let output = match self.space.outputs().next().cloned() {
                Some(o) => o,
                None => return,
            };
            let output_geo = self.space.output_geometry(&output).unwrap();
            // Unmap every other window in this workspace so nothing shows
            // through a transparent fullscreen client.
            let ws_idx = self.active_workspace;
            for sibling in &self.workspace_windows[ws_idx] {
                if *sibling != window {
                    self.space.unmap_elem(sibling);
                }
            }
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                    state.size = Some(output_geo.size);
                });
                toplevel.send_configure();
            }
            self.space.map_element(window.clone(), output_geo.loc, true);
            self.fullscreen_window = Some(window);
            self.needs_render = true;
        }
    }

    /// Exit fullscreen (if any) and restore the tiled layout.
    /// Used when switching workspaces so siblings are remapped correctly.
    pub fn restore_fullscreen(&mut self) {
        if let Some(fs_window) = self.fullscreen_window.take() {
            if let Some(toplevel) = fs_window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                    state.size = None;
                });
                toplevel.send_configure();
            }
            // Remap hidden siblings before the workspace is unmapped.
            let ws_idx = self.active_workspace;
            for window in self.workspace_windows[ws_idx].clone() {
                if self.space.element_geometry(&window).is_none() {
                    self.space.map_element(window, (0, 0), false);
                }
            }
            self.relayout();
            // relayout calls remap_floating_windows internally.
        }
    }

    /// Re-tile all windows in the space using the current layout.
    pub fn relayout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };

        let output_geo = self.space.output_geometry(&output).unwrap();
        let gap = self.config.gap as i32;
        let bw = self.config.border_width as i32;

        // Shrink the tiling area to respect layer-shell exclusive zones
        // (e.g. a top bar reserves space so windows don't go under it).
        let non_exclusive = {
            let lm = smithay::desktop::layer_map_for_output(&output);
            lm.non_exclusive_zone()
        };
        let tile_origin = output_geo.loc + non_exclusive.loc;
        let tile_size = non_exclusive.size;

        let usable = Geometry::new(
            tile_origin.x + gap,
            tile_origin.y + gap,
            (tile_size.w - gap * 2).max(0) as u32,
            (tile_size.h - gap * 2).max(0) as u32,
        );

        let windows = &self.workspace_windows[self.active_workspace];
        if windows.is_empty() {
            return;
        }

        // Skip floating and fullscreen windows — they manage their own geometry.
        let tiled_windows: Vec<Window> = windows
            .iter()
            .filter(|w| {
                let is_fullscreen = self
                    .fullscreen_window
                    .as_ref()
                    .map(|fs| *fs == **w)
                    .unwrap_or(false);
                let is_floating = w
                    .toplevel()
                    .map(|t| self.floating_windows.contains_key(&root_surface(t.wl_surface())))
                    .unwrap_or(false);
                !is_fullscreen && !is_floating
            })
            .cloned()
            .collect();
        let tile_count = tiled_windows.len();
        if tile_count == 0 {
            return;
        }

        let geos = self.layout.apply(&usable, tile_count);

        for (window, geo) in tiled_windows.iter().zip(geos.iter()) {
            let x = geo.x + gap;
            let y = geo.y + gap;
            let w = (geo.width as i32 - gap * 2 - bw * 2).max(1);
            let h = (geo.height as i32 - gap * 2 - bw * 2).max(1);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(Size::from((w, h)));
                });
                toplevel.send_pending_configure();
            }

            let location = Point::from((x, y));
            if self.space.element_location(window) != Some(location) {
                self.space.map_element(window.clone(), location, false);
            }
        }
        self.needs_render = true;
        // Floating windows sit on top of tiled ones; re-place them.
        self.remap_floating_windows();
    }

    /// Switch to a different workspace by index.
    pub fn switch_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() || idx == self.active_workspace {
            return;
        }

        tracing::info!(
            "Switching workspace {} -> {}",
            self.active_workspace + 1,
            idx + 1
        );

        // If a window is fullscreened, restore it before leaving the workspace.
        self.restore_fullscreen();

        // Unmap all windows from the current workspace
        for window in &self.workspace_windows[self.active_workspace] {
            self.space.unmap_elem(window);
        }

        self.active_workspace = idx;
        self.publish_active_workspace_state();

        self.needs_render = true;
        self.relayout();

        // Focus the active window on the new workspace
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| self.workspace_windows[self.active_workspace].get(focus_idx))
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            // No windows — clear keyboard focus
            self.set_keyboard_focus(None);
        }
    }

    /// Move the focused window to another workspace.
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace {
            return;
        }

        let focus_idx = match self.active_workspace_focused_index() {
            Some(i) => i,
            None => return,
        };

        let current = self.active_workspace;
        if focus_idx >= self.workspace_windows[current].len() {
            return;
        }

        // Remove window from current workspace
        let window = self.workspace_windows[current].remove(focus_idx);
        self.workspaces[current].remove_window(focus_idx);

        // Unmap from space (it's being moved away from the visible workspace)
        self.space.unmap_elem(&window);

        // Add to target workspace
        self.workspace_windows[target].push(window);
        self.workspaces[target].add_window();

        tracing::info!(
            "Moved window from workspace {} to {}",
            current + 1,
            target + 1
        );

        self.relayout();

        // Focus next window on current workspace if any
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| self.workspace_windows[self.active_workspace].get(focus_idx))
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            self.set_keyboard_focus(None);
        }
    }
}
