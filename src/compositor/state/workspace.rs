use smithay::wayland::seat::WaylandFocus;

use crate::config::FocusDirection;

use std::collections::HashSet;

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use crate::compositor::animation::OutgoingWindow;

use super::Beewm;
use super::focus::best_directional_focus_candidate;
use crate::model::workspace::Workspace;

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
        let plan = plan_workspace_switch(idx, self.workspaces.len(), self.focused_output, &active);
        if plan != WorkspaceSwitch::NoOp {
            // A drag can't survive the window leaving the screen: the grab would
            // keep re-mapping it into whatever workspace is now visible.
            self.cancel_active_grab();
        }
        match plan {
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

        // Capture the outgoing windows for the slide animation before they
        // leave the Space (their positions are gone after `unmap_elem`).
        let slide_output = self.focused_output().filter(|output| {
            self.animations.wants_workspace_slide()
                && !(self.animations.disable_for_fullscreen()
                    && (self.screen_owned_by_window(output)
                        || self.workspaces[idx].fullscreen.is_some()))
        });
        let outgoing = match &slide_output {
            Some(_) => self.outgoing_slide_windows(current),
            None => Vec::new(),
        };

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

        // A *tiled* sticky window must also leave the old workspace's tiling
        // tree and join the new one's — otherwise it follows you across
        // workspaces while keeping the geometry of the workspace it was mapped
        // on, ignoring the layout of the one it is actually shown on.
        self.migrate_sticky_tiled_windows(current, idx);

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

        if let Some(output) = slide_output {
            self.start_workspace_slide(&output, current, idx, outgoing);
        }
    }

    /// The non-sticky windows of workspace `ws` that are currently in the
    /// Space, bottom to top, with the position they are drawn at.
    fn outgoing_slide_windows(&self, ws: usize) -> Vec<OutgoingWindow> {
        self.space
            .elements()
            .filter(|window| self.workspaces[ws].windows.contains(window))
            .filter(|window| !self.is_window_sticky(window))
            .filter_map(|window| {
                Some(OutgoingWindow {
                    window: window.clone(),
                    location: self.space.element_location(window)?,
                    geometry: self.space.element_geometry(window)?,
                })
            })
            .collect()
    }

    /// Slide `output` from workspace `from` to the now-visible workspace `to`.
    /// The new workspace's windows are already laid out at their real
    /// positions; the slide only offsets how they are drawn.
    fn start_workspace_slide(
        &mut self,
        output: &Output,
        from: usize,
        to: usize,
        outgoing: Vec<OutgoingWindow>,
    ) {
        let Some(width) = self.space.output_geometry(output).map(|geo| geo.size.w) else {
            return;
        };
        let incoming: HashSet<WlSurface> = self.workspaces[to]
            .windows
            .iter()
            .filter(|window| !self.is_window_sticky(window))
            .filter_map(Self::window_root_surface)
            .collect();
        if incoming.is_empty() && outgoing.is_empty() {
            return;
        }
        self.animations.start_workspace_slide(
            output,
            to > from,
            width,
            incoming,
            outgoing,
            std::time::Instant::now(),
        );
        self.needs_render = true;
    }

    pub(crate) fn is_window_sticky(&self, window: &Window) -> bool {
        Self::window_root_surface(window)
            .map(|root| self.sticky_windows.contains(&root))
            .unwrap_or(false)
    }

    /// Re-home every tiled sticky window from workspace `from` to workspace
    /// `to`, so it participates in the target workspace's tiling: alone on an
    /// empty workspace it takes the whole screen, next to one window it splits
    /// it, and so on. Floating and fullscreen sticky windows keep their own
    /// geometry and are left alone.
    fn migrate_sticky_tiled_windows(&mut self, from: usize, to: usize) {
        let moving: Vec<usize> = self.workspaces[from]
            .windows
            .iter()
            .enumerate()
            .filter(|(_, window)| {
                Self::window_root_surface(window)
                    .map(|root| {
                        self.sticky_windows.contains(&root)
                            && !self.is_root_floating(&root)
                            && !self.is_root_fullscreen(&root)
                    })
                    .unwrap_or(false)
            })
            .map(|(idx, _)| idx)
            .collect();

        // Back to front so the remaining indices stay valid as we remove.
        for idx in moving.into_iter().rev() {
            let split_target = self.focused_tiled_window_root(to);
            let Some(window) = self.workspaces[from].remove_window(idx) else {
                continue;
            };
            if let Some(root) = Self::window_root_surface(&window) {
                self.remove_tiled_window(from, &root);
            }
            // Preserve the target workspace's own focus: `add_window` focuses
            // what it pushes, which would hand the sticky window the keyboard
            // on every single workspace switch.
            let focused = self.workspaces[to].focused_idx;
            self.workspaces[to].add_window(window.clone());
            if focused.is_some() {
                self.workspaces[to].focused_idx = focused;
            }
            self.insert_tiled_window(to, &window, split_target.as_ref());
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

    /// Change the number of workspaces live (config reload of `workspaces`).
    ///
    /// Growing appends empty workspaces. Shrinking first moves any output
    /// showing a dropped workspace onto a free surviving one, then moves every
    /// window from the dropped workspaces onto the last surviving workspace.
    /// Refuses to go below the number of outputs, since each output must show
    /// its own workspace. Returns whether the count was applied.
    pub(crate) fn set_workspace_count(&mut self, count: usize, split_ratio: f64) -> bool {
        let old = self.workspaces.len();
        if count == old {
            return true;
        }
        if count == 0 || count < self.outputs.len() {
            tracing::warn!(
                "workspaces {} is fewer than the {} connected outputs; keeping {}",
                count,
                self.outputs.len(),
                old,
            );
            return false;
        }
        self.cancel_active_grab();

        if count < old {
            // Re-point outputs that show a dropped workspace. Reuses the normal
            // switch path so sticky windows, fullscreen and mapping all behave
            // exactly like a user-initiated workspace switch.
            let original_focus = self.focused_output;
            for out in 0..self.outputs.len() {
                if self.outputs[out].active_workspace < count {
                    continue;
                }
                let shown: Vec<usize> = self
                    .outputs
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != out)
                    .map(|(_, ctx)| ctx.active_workspace)
                    .collect();
                let replacement = super::output::pick_initial_workspace(&shown, count);
                self.focused_output = out;
                self.show_workspace_on_focused_output(replacement);
            }
            self.focused_output = original_focus;

            // Every dropped workspace is now hidden. Fold its windows into the
            // last surviving workspace.
            let target = count - 1;
            let map_into_view = self.output_showing_workspace(target).is_some()
                && self.workspaces[target].fullscreen.is_none();
            for ws in count..old {
                self.exit_fullscreen_on_workspace(ws, false);
                let windows = std::mem::take(&mut self.workspaces[ws].windows);
                for window in windows {
                    let root = Self::window_root_surface(&window);
                    if let Some(root) = root.as_ref() {
                        self.remove_tiled_window(ws, root);
                    }
                    // Keep the target's own focus rather than handing it to
                    // whatever window was folded in last.
                    let focused = self.workspaces[target].focused_idx;
                    let split_target = self.focused_tiled_window_root(target);
                    self.workspaces[target].add_window(window.clone());
                    if focused.is_some() {
                        self.workspaces[target].focused_idx = focused;
                    }
                    self.insert_tiled_window(target, &window, split_target.as_ref());
                    if map_into_view && self.space.element_geometry(&window).is_none() {
                        let position = root
                            .and_then(|root| self.floating_windows.get(&root).map(|f| f.position))
                            .unwrap_or_else(|| (0, 0).into());
                        self.space.map_element(window, position, false);
                    }
                }
            }
            self.workspaces.truncate(count);
            tracing::info!(
                "Workspaces reduced {} -> {}; windows moved to workspace {}",
                old,
                count,
                count,
            );
        } else {
            self.workspaces.resize_with(count, Workspace::new);
            tracing::info!("Workspaces increased {} -> {}", old, count);
        }

        self.layout_manager.set_workspace_count(count, split_ratio);
        self.publish_workspace_state();
        self.relayout();
        self.raise_sticky_windows();

        // Showing replacement workspaces may have moved keyboard focus to
        // another output; return it to the focused output's window.
        let ws = self.active_workspace();
        let focus = self.workspaces[ws]
            .fullscreen
            .clone()
            .or_else(|| {
                self.workspaces[ws]
                    .focused_idx
                    .and_then(|idx| self.workspaces[ws].windows.get(idx).cloned())
            })
            .and_then(|window| window.wl_surface().map(|surface| surface.into_owned()));
        self.set_keyboard_focus(focus);
        true
    }

    /// Move the focused window to another workspace (which may be homed on a
    /// different output).
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace() {
            return;
        }

        // Same as `switch_workspace`: the window is about to leave the visible
        // workspace, so any grab on it has to end here rather than keep
        // dragging it across the workspace boundary.
        self.cancel_active_grab();

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
