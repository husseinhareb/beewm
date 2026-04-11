use std::collections::HashMap;

use crate::action::DisplayAction;
use crate::config::Config;
use crate::layout::master_stack::MasterStack;
use crate::layout::Layout;
use crate::model::screen::Screen;
use crate::model::window::{Geometry, Window};
use crate::model::workspace::Workspace;
use crate::WindowHandle;

/// The central WM state, generic over the window handle type.
pub struct State<H: WindowHandle> {
    pub workspaces: Vec<Workspace<H>>,
    pub windows: HashMap<H, Window<H>>,
    pub screens: Vec<Screen>,
    pub active_workspace: usize,
    pub actions: Vec<DisplayAction<H>>,
    pub config: Config,
    pub layout: Box<dyn Layout<H>>,
}

impl<H: WindowHandle> State<H> {
    pub fn new(config: Config) -> Self {
        let num_ws = config.num_workspaces;
        let layout = Box::new(MasterStack {
            master_ratio: config.master_ratio,
        });
        Self {
            workspaces: (0..num_ws).map(Workspace::new).collect(),
            windows: HashMap::new(),
            screens: Vec::new(),
            active_workspace: 0,
            actions: Vec::new(),
            config,
            layout,
        }
    }

    /// Get the currently active workspace.
    pub fn active_workspace(&self) -> &Workspace<H> {
        &self.workspaces[self.active_workspace]
    }

    /// Get the currently active workspace mutably.
    pub fn active_workspace_mut(&mut self) -> &mut Workspace<H> {
        &mut self.workspaces[self.active_workspace]
    }

    /// Add a window to the current workspace and trigger a re-layout.
    pub fn manage_window(&mut self, handle: H, geometry: Geometry, floating: bool) {
        let ws = self.active_workspace;
        let mut window = Window::new(handle, geometry, ws);
        window.floating = floating;
        self.windows.insert(handle, window);
        self.workspaces[ws].add_window(handle);

        self.actions.push(DisplayAction::MapWindow { handle });
        self.apply_layout();
        self.focus_window(handle);
    }

    /// Remove a window from management.
    pub fn unmanage_window(&mut self, handle: H) {
        if let Some(window) = self.windows.remove(&handle) {
            let ws = window.workspace;
            self.workspaces[ws].remove_window(handle);
            self.apply_layout();
        }
    }

    /// Focus a specific window.
    pub fn focus_window(&mut self, handle: H) {
        if let Some(window) = self.windows.get(&handle) {
            let ws = window.workspace;
            self.workspaces[ws].focused = Some(handle);

            self.actions.push(DisplayAction::SetFocus { handle });
            self.actions.push(DisplayAction::RaiseWindow { handle });
            self.actions.push(DisplayAction::SetBorderColor {
                handle,
                color: self.config.border_color_focused,
            });

            // Unfocus other windows in the workspace
            let others: Vec<H> = self.workspaces[ws]
                .windows
                .iter()
                .filter(|&&w| w != handle)
                .copied()
                .collect();
            for other in others {
                self.actions.push(DisplayAction::SetBorderColor {
                    handle: other,
                    color: self.config.border_color_unfocused,
                });
            }
        }
    }

    /// Focus the next window in the active workspace.
    pub fn focus_next(&mut self) {
        self.workspaces[self.active_workspace].focus_next();
        if let Some(handle) = self.workspaces[self.active_workspace].focused {
            self.focus_window(handle);
        }
    }

    /// Focus the previous window in the active workspace.
    pub fn focus_prev(&mut self) {
        self.workspaces[self.active_workspace].focus_prev();
        if let Some(handle) = self.workspaces[self.active_workspace].focused {
            self.focus_window(handle);
        }
    }

    /// Switch to a different workspace.
    pub fn switch_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() || idx == self.active_workspace {
            return;
        }

        // Hide windows on current workspace
        for &handle in &self.workspaces[self.active_workspace].windows {
            self.actions.push(DisplayAction::UnmapWindow { handle });
        }

        self.active_workspace = idx;

        // Show windows on new workspace
        for &handle in &self.workspaces[idx].windows {
            self.actions.push(DisplayAction::MapWindow { handle });
        }

        self.apply_layout();

        if let Some(handle) = self.workspaces[idx].focused {
            self.focus_window(handle);
        }
    }

    /// Move the focused window to a different workspace.
    pub fn move_to_workspace(&mut self, target_ws: usize) {
        if target_ws >= self.workspaces.len() || target_ws == self.active_workspace {
            return;
        }

        let current_ws = self.active_workspace;
        if let Some(handle) = self.workspaces[current_ws].focused {
            // Remove from current workspace
            self.workspaces[current_ws].remove_window(handle);

            // Update the window's workspace
            if let Some(window) = self.windows.get_mut(&handle) {
                window.workspace = target_ws;
            }

            // Add to target workspace
            self.workspaces[target_ws].add_window(handle);

            // Hide the window (it's now on a different workspace)
            self.actions.push(DisplayAction::UnmapWindow { handle });

            self.apply_layout();
        }
    }

    /// Apply the current layout to the active workspace.
    pub fn apply_layout(&mut self) {
        let screen_geom = match self.screens.first() {
            Some(s) => s.geometry,
            None => return,
        };

        let ws = &self.workspaces[self.active_workspace];
        let gap = self.config.gap;
        let bw = self.config.border_width;

        // Adjust screen geometry for gaps
        let usable = Geometry::new(
            screen_geom.x + gap as i32,
            screen_geom.y + gap as i32,
            screen_geom.width.saturating_sub(gap * 2),
            screen_geom.height.saturating_sub(gap * 2),
        );

        // Only tile non-floating windows
        let tiled: Vec<H> = ws
            .windows
            .iter()
            .filter(|h| {
                self.windows
                    .get(h)
                    .map(|w| !w.floating)
                    .unwrap_or(false)
            })
            .copied()
            .collect();

        let geometries = self.layout.apply(&usable, &tiled);

        for (handle, mut geom) in tiled.into_iter().zip(geometries) {
            // Apply inner gaps between windows
            geom.x += gap as i32;
            geom.y += gap as i32;
            geom.width = geom.width.saturating_sub(gap * 2);
            geom.height = geom.height.saturating_sub(gap * 2);

            // Account for border width in geometry
            geom.width = geom.width.saturating_sub(bw * 2);
            geom.height = geom.height.saturating_sub(bw * 2);

            if let Some(window) = self.windows.get_mut(&handle) {
                window.geometry = geom;
            }

            self.actions.push(DisplayAction::ConfigureWindow {
                handle,
                geometry: geom,
                border_width: bw,
            });
        }
    }
}
