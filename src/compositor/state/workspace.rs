use smithay::wayland::seat::WaylandFocus;

use crate::config::FocusDirection;

use super::Beewm;
use super::focus::best_directional_focus_candidate;

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

/// What `switch_workspace` should do, decided purely from the current
/// per-output state. Keeping this a free function makes the i3-style
/// multi-output routing unit-testable without a live compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceSwitch {
    /// Target is already the focused output's visible workspace — nothing to do.
    NoOp,
    /// Target is visible on another output; move focus to that output index.
    FocusOutput(usize),
    /// Target is hidden; show it on the focused output.
    ShowHere,
}

/// Resolve a `switch_workspace(target)` request against the active workspace of
/// every output. i3 semantics: switching to a workspace already shown on another
/// output focuses that output instead of relocating the workspace.
pub fn plan_workspace_switch(
    target: usize,
    num_workspaces: usize,
    focused_output: usize,
    output_active_workspaces: &[usize],
) -> WorkspaceSwitch {
    if target >= num_workspaces {
        return WorkspaceSwitch::NoOp;
    }
    match output_active_workspaces.iter().position(|&ws| ws == target) {
        Some(out) if out == focused_output => WorkspaceSwitch::NoOp,
        Some(out) => WorkspaceSwitch::FocusOutput(out),
        None => WorkspaceSwitch::ShowHere,
    }
}

impl Beewm {
    /// Switch the focused output to `idx` (or focus the output already showing
    /// it). See [`plan_workspace_switch`] for the decision logic.
    pub fn switch_workspace(&mut self, idx: usize) {
        let active: Vec<usize> = self
            .outputs
            .iter()
            .map(|ctx| ctx.active_workspace)
            .collect();
        match plan_workspace_switch(idx, self.workspaces.len(), self.focused_output, &active) {
            WorkspaceSwitch::NoOp => {}
            WorkspaceSwitch::FocusOutput(out) => self.focus_output_index(out),
            WorkspaceSwitch::ShowHere => self.show_workspace_on_focused_output(idx),
        }
    }

    /// Hide the focused output's current workspace and present `idx` on it.
    /// This is the single-output behavior, now scoped to the focused output.
    fn show_workspace_on_focused_output(&mut self, idx: usize) {
        let current = self.active_workspace();

        tracing::info!("Switching workspace {} -> {}", current + 1, idx + 1);

        // Leave the current workspace's fullscreen state intact — it is
        // re-presented when we return. Unmap everything currently shown except
        // sticky windows, which stay mapped at their position so they follow us
        // onto the next workspace.
        for window in &self.workspaces[current].windows {
            let is_sticky = Self::window_root_surface(window)
                .map(|root| self.sticky_windows.contains(&root))
                .unwrap_or(false);
            if !is_sticky {
                self.space.unmap_elem(window);
            }
        }

        self.set_active_workspace(idx);
        self.publish_workspace_state();

        self.needs_render = true;
        self.relayout();
        // Keep sticky windows above the freshly relaid-out workspace.
        self.raise_sticky_windows();

        // If the workspace we entered had a fullscreen window, re-present it
        // over the freshly relaid-out tiles.
        if let Some(window) = self.workspaces[self.active_workspace()].fullscreen.clone() {
            self.show_fullscreen_window(&window);
        }

        // Focus the workspace's fullscreen window if it has one, otherwise its
        // last-focused tiled window.
        let focus = self.workspaces[self.active_workspace()]
            .fullscreen
            .clone()
            .or_else(|| {
                self.workspaces[self.active_workspace()]
                    .focused_idx
                    .and_then(|focus_idx| {
                        self.workspaces[self.active_workspace()]
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

    /// Move keyboard focus to another output and refocus its active workspace's
    /// last-focused window. No-op when `out` is invalid or already focused
    /// (so single-output sessions never reach the body).
    pub(crate) fn focus_output_index(&mut self, out: usize) {
        if out >= self.outputs.len() || out == self.focused_output {
            return;
        }
        self.focused_output = out;
        self.publish_workspace_state();

        let ws = self.active_workspace();
        let focus = self.workspaces[ws]
            .focused_idx
            .and_then(|idx| self.workspaces[ws].windows.get(idx))
            .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
        self.set_keyboard_focus(focus);
        self.needs_render = true;
    }

    /// Move keyboard focus to the nearest output in `direction`. No-op with a
    /// single output (no candidate exists).
    pub fn focus_output_in_direction(&mut self, direction: FocusDirection) {
        let Some(target) = self.output_in_direction(direction) else {
            return;
        };
        self.focus_output_index(target);
    }

    /// Move the focused window to the active workspace of the nearest output in
    /// `direction`, then follow it there. No-op with a single output.
    pub fn move_window_to_output(&mut self, direction: FocusDirection) {
        let Some(target_out) = self.output_in_direction(direction) else {
            return;
        };
        let target_ws = self.outputs[target_out].active_workspace;
        self.move_to_workspace(target_ws);
        self.focus_output_index(target_out);
    }

    /// Index of the nearest output to the focused one in `direction`, scored by
    /// the same geometry heuristic as directional window focus.
    fn output_in_direction(&self, direction: FocusDirection) -> Option<usize> {
        let current = self
            .outputs
            .get(self.focused_output)
            .and_then(|ctx| self.space.output_geometry(&ctx.output))?;
        let candidates: Vec<_> = self
            .outputs
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != self.focused_output)
            .filter_map(|(idx, ctx)| {
                self.space
                    .output_geometry(&ctx.output)
                    .map(|geo| (idx, geo))
            })
            .collect();
        best_directional_focus_candidate(current, candidates, direction)
    }

    /// Move the focused window to another workspace (which may be homed on a
    /// different output).
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace() {
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

        let current = self.active_workspace();
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
        let focus = self.workspaces[self.active_workspace()]
            .focused_idx
            .and_then(|focus_idx| {
                self.workspaces[self.active_workspace()]
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

#[cfg(test)]
mod tests {
    use super::{WorkspaceSwitch, plan_workspace_switch};

    #[test]
    fn switch_to_current_workspace_is_a_noop() {
        // Focused output (0) already shows workspace 2.
        assert_eq!(
            plan_workspace_switch(2, 10, 0, &[2, 5]),
            WorkspaceSwitch::NoOp
        );
    }

    #[test]
    fn switch_to_hidden_workspace_shows_it_on_the_focused_output() {
        assert_eq!(
            plan_workspace_switch(7, 10, 0, &[2, 5]),
            WorkspaceSwitch::ShowHere
        );
    }

    #[test]
    fn switch_to_workspace_visible_elsewhere_focuses_that_output() {
        // Workspace 5 is visible on output 1; switching to it from output 0
        // moves focus to output 1 rather than relocating the workspace.
        assert_eq!(
            plan_workspace_switch(5, 10, 0, &[2, 5]),
            WorkspaceSwitch::FocusOutput(1)
        );
    }

    #[test]
    fn switch_out_of_range_is_a_noop() {
        assert_eq!(
            plan_workspace_switch(12, 10, 0, &[2, 5]),
            WorkspaceSwitch::NoOp
        );
    }

    #[test]
    fn single_output_switch_matches_legacy_behavior() {
        // One output showing ws 0: switching to 0 is a no-op, to any other
        // in-range workspace shows it here — exactly the old single-output rule.
        assert_eq!(plan_workspace_switch(0, 10, 0, &[0]), WorkspaceSwitch::NoOp);
        assert_eq!(
            plan_workspace_switch(3, 10, 0, &[0]),
            WorkspaceSwitch::ShowHere
        );
    }
}
