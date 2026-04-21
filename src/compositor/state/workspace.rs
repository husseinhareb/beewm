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

        // If a window is fullscreened, restore it before leaving the workspace.
        self.restore_fullscreen();

        // Unmap all windows from the current workspace
        for window in &self.workspaces[self.active_workspace].windows {
            self.space.unmap_elem(window);
        }

        self.active_workspace = idx;
        self.publish_workspace_state();

        self.needs_render = true;
        self.relayout();

        // Focus the active window on the new workspace
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| {
                self.workspaces[self.active_workspace]
                    .windows
                    .get(focus_idx)
            })
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
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
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            self.set_keyboard_focus(None);
        }
    }
}
