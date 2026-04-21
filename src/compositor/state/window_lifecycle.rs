use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Point, Size};

use crate::model::window::Geometry;

use super::{Beewm, FloatingWindowData, root_surface};

impl Beewm {
    pub(crate) fn tiling_usable_geometry(&self) -> Option<Geometry> {
        let output = self.space.outputs().next()?.clone();
        let output_geo = self.space.output_geometry(&output)?;
        let gap = self.config.gap as i32;

        let non_exclusive = {
            let lm = smithay::desktop::layer_map_for_output(&output);
            lm.non_exclusive_zone()
        };
        let tile_origin = output_geo.loc + non_exclusive.loc;
        let tile_size = non_exclusive.size;

        Some(Geometry::new(
            tile_origin.x + gap,
            tile_origin.y + gap,
            (tile_size.w - gap * 2).max(0) as u32,
            (tile_size.h - gap * 2).max(0) as u32,
        ))
    }

    pub(crate) fn configured_tiled_size(
        &self,
        geo: Geometry,
    ) -> Size<i32, smithay::utils::Logical> {
        let gap = self.config.gap as i32;
        let bw = self.config.border_width as i32;
        let w = (geo.width as i32 - gap * 2 - bw * 2).max(1);
        let h = (geo.height as i32 - gap * 2 - bw * 2).max(1);
        Size::from((w, h))
    }

    pub(crate) fn initial_toplevel_size(
        &self,
        surface: &WlSurface,
    ) -> Option<Size<i32, smithay::utils::Logical>> {
        let usable = self.tiling_usable_geometry()?;
        let ws_idx = self.active_workspace;
        let root = root_surface(surface);

        let geo = {
            let split_target = self.focused_tiled_window_root(ws_idx);
            self.layout_manager
                .preview_insert(ws_idx, split_target.as_ref(), root.clone(), &usable)
                .or_else(|| {
                    let tile_count = self.workspaces[ws_idx]
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
                        .count()
                        + 1;
                    self.layout_manager
                        .positional_layout()?
                        .apply(&usable, tile_count)
                        .into_iter()
                        .nth(tile_count - 1)
                })
        }?;

        Some(self.configured_tiled_size(geo))
    }

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

        let is_fullscreen = self.is_root_fullscreen(&root);
        let is_floating = self.is_root_floating(&root);

        if is_fullscreen {
            self.exit_fullscreen_internal(false);
        }

        match super::workspace::float_toggle_transition(is_fullscreen, is_floating) {
            super::workspace::FloatToggleTransition::SinkToTiling => {
                self.floating_windows.remove(&root);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|s| {
                        s.states.unset(xdg_toplevel::State::Resizing);
                    });
                    toplevel.send_configure();
                }
                let split_target = self.focused_tiled_window_root(self.active_workspace);
                self.insert_tiled_window(self.active_workspace, &window, split_target.as_ref());
                self.relayout();
            }
            super::workspace::FloatToggleTransition::KeepFloating => {
                self.relayout();
                self.space.raise_element(&window, true);
                self.needs_render = true;
            }
            super::workspace::FloatToggleTransition::MakeFloating => {
                self.float_window(window);
            }
        }
    }

    /// Swap two tiled windows within a workspace.
    pub fn swap_tiled_windows(
        &mut self,
        workspace_idx: usize,
        first_surface: &WlSurface,
        second_surface: &WlSurface,
    ) -> bool {
        if workspace_idx >= self.workspaces.len() {
            return false;
        }

        let first_root = root_surface(first_surface);
        let second_root = root_surface(second_surface);
        if first_root == second_root
            || self.is_root_floating(&first_root)
            || self.is_root_floating(&second_root)
            || self.is_root_fullscreen(&first_root)
            || self.is_root_fullscreen(&second_root)
        {
            return false;
        }

        if !self
            .layout_manager
            .swap(workspace_idx, &first_root, &second_root)
        {
            return false;
        }

        let Some(first_idx) = self.window_index_for_surface(workspace_idx, &first_root) else {
            return false;
        };
        let Some(second_idx) = self.window_index_for_surface(workspace_idx, &second_root) else {
            return false;
        };

        self.workspaces[workspace_idx].swap_windows(first_idx, second_idx);

        if workspace_idx == self.active_workspace {
            self.relayout();
        } else {
            self.needs_render = true;
        }

        true
    }

    /// Float a newly-mapped window centered on the screen using its own
    /// natural size.
    pub fn map_as_floating_centered(&mut self, window: &Window) {
        let root = match window.toplevel().map(|t| root_surface(t.wl_surface())) {
            Some(r) => r,
            None => return,
        };
        let output = match self.space.outputs().next().cloned() {
            Some(o) => o,
            None => return,
        };
        let output_geo = self.space.output_geometry(&output).unwrap();
        let win_size = window.geometry().size;
        let win_w = if win_size.w > 0 {
            win_size.w
        } else {
            output_geo.size.w / 2
        };
        let win_h = if win_size.h > 0 {
            win_size.h
        } else {
            output_geo.size.h / 2
        };
        let pos = Point::from((
            output_geo.loc.x + (output_geo.size.w - win_w) / 2,
            output_geo.loc.y + (output_geo.size.h - win_h) / 2,
        ));
        self.floating_windows.insert(
            root,
            FloatingWindowData::new(pos, Size::from((win_w, win_h))),
        );
        self.space.map_element(window.clone(), pos, true);
    }

    fn float_window(&mut self, window: Window) {
        let root = match window.toplevel().map(|t| root_surface(t.wl_surface())) {
            Some(r) => r,
            None => return,
        };
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
                s.states.unset(xdg_toplevel::State::Fullscreen);
                s.size = Some(Size::from((float_w, float_h)));
            });
            toplevel.send_configure();
        }
        self.remove_tiled_window(self.active_workspace, &root);
        self.space.map_element(window.clone(), pos, true);
        self.floating_windows.insert(
            root,
            FloatingWindowData::new(pos, Size::from((float_w, float_h))),
        );
        self.relayout();
        self.needs_render = true;
    }

    /// Re-place all floating windows on the active workspace back into the
    /// space at their stored positions.
    fn remap_floating_windows(&mut self) {
        let ws_idx = self.active_workspace;
        for window in self.workspaces[ws_idx].windows.clone() {
            let root = match window.toplevel().map(|t| t.wl_surface().clone()) {
                Some(surface) => root_surface(&surface),
                None => continue,
            };
            if let Some(floating) = self.floating_windows.get(&root).copied() {
                self.space.map_element(window, floating.position, false);
            }
        }
    }

    /// Toggle fullscreen for the currently focused window.
    pub fn toggle_fullscreen(&mut self) {
        if self.fullscreen_window.is_some() {
            self.exit_fullscreen_internal(true);
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
            let ws_idx = self.active_workspace;
            for sibling in &self.workspaces[ws_idx].windows {
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
    pub fn restore_fullscreen(&mut self) {
        self.exit_fullscreen_internal(true);
    }

    fn exit_fullscreen_internal(&mut self, relayout: bool) -> Option<Window> {
        let fs_window = self.fullscreen_window.take()?;
        let restore_floating = Self::window_root_surface(&fs_window)
            .and_then(|root| self.floating_windows.get(&root).copied());

        if let Some(toplevel) = fs_window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = restore_floating.map(|floating| floating.size);
            });
            toplevel.send_configure();
        }

        if let Some(floating) = restore_floating {
            self.space
                .map_element(fs_window.clone(), floating.position, true);
        }

        let ws_idx = self.active_workspace;
        for window in self.workspaces[ws_idx].windows.clone() {
            if self.space.element_geometry(&window).is_none() {
                self.space.map_element(window, (0, 0), false);
            }
        }

        if relayout {
            self.relayout();
        } else {
            self.needs_render = true;
        }

        Some(fs_window)
    }

    /// Re-tile all windows in the space using the current layout.
    pub fn relayout(&mut self) {
        let Some(usable) = self.tiling_usable_geometry() else {
            return;
        };
        let gap = self.config.gap as i32;

        let windows = &self.workspaces[self.active_workspace].windows;
        if windows.is_empty() {
            return;
        }

        let tiled_windows: Vec<Window> = windows
            .iter()
            .filter(|w| {
                let root = Self::window_root_surface(w);
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
            .collect();
        let tile_count = tiled_windows.len();
        if tile_count == 0 {
            self.remap_floating_windows();
            return;
        }

        let keyed_geos = self
            .layout_manager
            .geometries(self.active_workspace, &usable, tile_count);
        let positional_geos = self
            .layout_manager
            .positional_layout()
            .map(|layout| layout.apply(&usable, tile_count));

        for (index, window) in tiled_windows.iter().enumerate() {
            let geo = Self::window_root_surface(window)
                .and_then(|root| keyed_geos.get(&root).copied())
                .or_else(|| {
                    positional_geos
                        .as_ref()
                        .and_then(|geos| geos.get(index).copied())
                });
            let Some(geo) = geo else {
                continue;
            };
            let x = geo.x + gap;
            let y = geo.y + gap;
            let size = self.configured_tiled_size(geo);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(size);
                });
                toplevel.send_pending_configure();
            }

            let location = Point::from((x, y));
            if self.space.element_location(window) != Some(location) {
                self.space.map_element(window.clone(), location, false);
            }
        }
        self.needs_render = true;
        self.remap_floating_windows();
    }
}
