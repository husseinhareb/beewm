use smithay::wayland::seat::WaylandFocus;

use super::Beewm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatToggleTransition {
    SinkToTiling,
    KeepFloating,
    MakeFloating,
}

pub fn float_toggle_transition(is_fullscreen: bool, is_floating: bool) -> FloatToggleTransition {
    if is_fullscreen {
        if is_floating {
            FloatToggleTransition::KeepFloating
        } else {
            FloatToggleTransition::MakeFloating
        }
    } else if is_floating {
        FloatToggleTransition::SinkToTiling
    } else {
        FloatToggleTransition::MakeFloating
    }
}

impl Beewm {
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

        // Leave the current workspace's fullscreen state intact — it is
        // re-presented when we return. Just unmap everything currently shown.
        for window in &self.workspaces[self.active_workspace].windows {
            self.space.unmap_elem(window);
        }

        self.active_workspace = idx;
        self.publish_workspace_state();

        self.needs_render = true;
        self.relayout();

        // If the workspace we entered had a fullscreen window, re-present it
        // over the freshly relaid-out tiles.
        if let Some(window) = self.workspaces[self.active_workspace].fullscreen.clone() {
            self.show_fullscreen_window(&window);
        }

        // Focus the workspace's fullscreen window if it has one, otherwise its
        // last-focused tiled window.
        let focus = self.workspaces[self.active_workspace]
            .fullscreen
            .clone()
            .or_else(|| {
                self.workspaces[self.active_workspace]
                    .focused_idx
                    .and_then(|focus_idx| {
                        self.workspaces[self.active_workspace]
                            .windows
                            .get(focus_idx)
                            .cloned()
                    })
            })
            .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            self.set_keyboard_focus(None);
        }
    }

    /// Move the focused window to another workspace.
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace {
            return;
        }

        // Exit fullscreen before moving the focused window. The focused window
        // is the active workspace's fullscreen window when one is set, so moving
        // it must clear that slot first — otherwise the source workspace keeps a
        // fullscreen entry pointing at a window that no longer lives there,
        // leaving layer suppression active and siblings unmapped.
        self.restore_fullscreen();

        let focus_idx = match self.active_workspace_focused_index() {
            Some(i) => i,
            None => return,
        };

        let current = self.active_workspace;
        if focus_idx >= self.workspaces[current].windows.len() {
            return;
        }

        // Remove window from current workspace
        let window = self.workspaces[current].remove_window(focus_idx).unwrap();

        // Unmap from space (it's being moved away from the visible workspace)
        self.space.unmap_elem(&window);

        // Add to target workspace
        let split_target = self.focused_tiled_window_root(target);
        let window_root = Self::window_root_surface(&window);
        let is_floating = window_root
            .as_ref()
            .map(|root| self.is_root_floating(root))
            .unwrap_or(false);
        if let Some(root) = window_root.as_ref() {
            self.remove_tiled_window(current, root);
        }
        self.workspaces[target].add_window(window);
        if !is_floating {
            let inserted = self.workspaces[target]
                .windows
                .last()
                .cloned()
                .expect("just pushed a window");
            self.insert_tiled_window(target, &inserted, split_target.as_ref());
        }
        self.publish_workspace_state();

        tracing::info!(
            "Moved window from workspace {} to {}",
            current + 1,
            target + 1
        );

        self.relayout();

        // Focus next window on current workspace if any
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| {
                self.workspaces[self.active_workspace]
                    .windows
                    .get(focus_idx)
            })
            .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            self.set_keyboard_focus(None);
        }
    }
}
