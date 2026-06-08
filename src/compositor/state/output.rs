use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use super::{Beewm, FloatingWindowData};

/// Per-output compositor-side state. Backend/render state (the `DrmCompositor`,
/// renderer, vblank bookkeeping) is keyed separately in the backend — this only
/// holds the logical placement the rest of the compositor reasons about.
pub struct OutputCtx {
    pub output: Output,
    /// Top-left of this output in the global `Space` coordinate space. Mirrors
    /// the position passed to `Space::map_output`.
    pub position: Point<i32, Logical>,
    /// Index of the workspace currently *visible* on this output. The
    /// authoritative per-output active workspace; the old global
    /// `Beewm::active_workspace` is now derived from the focused output's value.
    pub active_workspace: usize,
}

/// Choose the workspace a newly-added output should display: the
/// lowest-numbered workspace not already shown on another output, else
/// workspace 0. Pure so it can be unit-tested without a live compositor.
pub fn pick_initial_workspace(shown_on_other_outputs: &[usize], num_workspaces: usize) -> usize {
    (0..num_workspaces)
        .find(|ws| !shown_on_other_outputs.contains(ws))
        .unwrap_or(0)
}

/// Minimum pixels of a floating window kept inside the usable area per axis so
/// it can always be grabbed again after an output change. Mirrors the
/// interactive-drag clamp margin in `input::grab`.
const ONSCREEN_MARGIN: i32 = 48;

/// The result of removing the output at `removed_idx` from a `Vec`-indexed
/// output registry: the surviving outputs shift down by one, so every stored
/// output index (each workspace's home, the focused output) must be remapped.
/// Pure so the fiddly reindex is unit-tested without a live compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRemovalPlan {
    /// New index of the output that adopts the removed output's workspaces, or
    /// `None` when no outputs remain (zero-output interval).
    pub survivor: Option<usize>,
    /// New `Workspace.output` value for each workspace, in workspace order.
    pub workspace_outputs: Vec<usize>,
    /// New `focused_output` index.
    pub focused_output: usize,
}

/// Compute the reindex after removing `removed_idx` from a registry of
/// `output_count` outputs. Workspaces homed on the removed output migrate to the
/// survivor (new index 0); outputs after the removed one shift down by one.
pub fn plan_output_removal(
    removed_idx: usize,
    output_count: usize,
    workspace_outputs: &[usize],
    focused_output: usize,
) -> OutputRemovalPlan {
    let survivor = (output_count.saturating_sub(1) > 0).then_some(0usize);
    let remap = |old: usize| -> usize {
        if old == removed_idx {
            survivor.unwrap_or(0)
        } else if old > removed_idx {
            old - 1
        } else {
            old
        }
    };
    OutputRemovalPlan {
        survivor,
        workspace_outputs: workspace_outputs.iter().map(|&o| remap(o)).collect(),
        focused_output: remap(focused_output),
    }
}

/// Clamp a window's top-left so at least `margin` px stays inside `usable`.
fn clamp_into_usable(
    pos: Point<i32, Logical>,
    size: Size<i32, Logical>,
    usable: Rectangle<i32, Logical>,
    margin: i32,
) -> Point<i32, Logical> {
    let clamp = |value: i32, lo: i32, hi: i32| if lo > hi { hi } else { value.clamp(lo, hi) };
    let mx = margin.min(size.w.max(1));
    let my = margin.min(size.h.max(1));
    Point::from((
        clamp(
            pos.x,
            usable.loc.x + mx - size.w,
            usable.loc.x + usable.size.w - mx,
        ),
        clamp(
            pos.y,
            usable.loc.y + my - size.h,
            usable.loc.y + usable.size.h - my,
        ),
    ))
}

impl Beewm {
    /// Register a newly-available output: map it into the `Space` at `position`
    /// and record an [`OutputCtx`]. Idempotent — re-registering an output that is
    /// already known just updates its position. Single entry point so the
    /// `Space` and the output registry can never disagree.
    pub fn add_output(&mut self, output: Output, position: Point<i32, Logical>) {
        self.space.map_output(&output, position);
        if let Some(ctx) = self.outputs.iter_mut().find(|ctx| ctx.output == output) {
            ctx.position = position;
            return;
        }
        let new_idx = self.outputs.len();
        // Show the lowest workspace not already visible elsewhere, and home it
        // on this output so the per-output workspace model is consistent.
        let shown: Vec<usize> = self
            .outputs
            .iter()
            .map(|ctx| ctx.active_workspace)
            .collect();
        let active_workspace = pick_initial_workspace(&shown, self.workspaces.len());
        if let Some(workspace) = self.workspaces.get_mut(active_workspace) {
            workspace.output = new_idx;
        }
        self.outputs.push(OutputCtx {
            output,
            position,
            active_workspace,
        });
        if self.focused_output >= self.outputs.len() {
            self.focused_output = 0;
        }
    }

    /// The active (visible) workspace of the focused output. Returns 0 when no
    /// outputs exist yet (during construction, before the backend adds one) so
    /// the early `publish_*` calls in `Beewm::new` stay valid.
    pub fn active_workspace(&self) -> usize {
        self.outputs
            .get(self.focused_output)
            .map(|ctx| ctx.active_workspace)
            .unwrap_or(0)
    }

    /// Make `idx` the visible workspace on the focused output and home it there.
    /// The single writer of a focused output's `active_workspace`.
    pub(crate) fn set_active_workspace(&mut self, idx: usize) {
        let focused = self.focused_output;
        if let Some(ctx) = self.outputs.get_mut(focused) {
            ctx.active_workspace = idx;
        }
        if let Some(workspace) = self.workspaces.get_mut(idx) {
            workspace.output = focused;
        }
    }

    /// The output that currently owns keyboard focus and receives newly-mapped
    /// windows / `switch_workspace`. With a single output this is simply that
    /// output, so every caller below collapses to today's behavior.
    pub(crate) fn focused_output(&self) -> Option<Output> {
        self.outputs
            .get(self.focused_output)
            .or_else(|| self.outputs.first())
            .map(|ctx| ctx.output.clone())
    }

    /// The output a window is displayed on, resolved from its current `Space`
    /// geometry (the output under the window's center). Falls back to the
    /// focused output when the window has no geometry yet (e.g. just mapped).
    pub(crate) fn output_for_window(&self, window: &Window) -> Option<Output> {
        self.space
            .element_geometry(window)
            .and_then(|geo| {
                let center = (geo.loc + Point::from((geo.size.w / 2, geo.size.h / 2))).to_f64();
                self.space.output_under(center).next().cloned()
            })
            .or_else(|| self.focused_output())
    }

    /// The output a surface is displayed on. Resolves through the mapped window
    /// when there is one (so subsurfaces/popups inherit their toplevel's
    /// output); otherwise falls back to the focused output.
    pub(crate) fn output_for_surface(&self, surface: &WlSurface) -> Option<Output> {
        if let Some(window) = self.mapped_window_for_surface(surface)
            && let Some(output) = self.output_for_window(&window)
        {
            return Some(output);
        }
        self.focused_output()
    }

    /// The output under a point in global `Space` coordinates, else the focused
    /// output (e.g. when the point lands in a gap between mismatched outputs).
    pub(crate) fn output_under_point(&self, point: Point<f64, Logical>) -> Option<Output> {
        self.space
            .output_under(point)
            .next()
            .cloned()
            .or_else(|| self.focused_output())
    }

    /// Remove an output (disconnect / GPU gone): unmap it, migrate its
    /// workspaces to a surviving output, repack remaining outputs, pull any
    /// floating windows back on-screen, refocus, and relayout. With no outputs
    /// left, state is kept in memory (windows survive) and re-shown on reconnect.
    pub fn remove_output(&mut self, output: &Output) {
        let Some(removed_idx) = self.outputs.iter().position(|ctx| &ctx.output == output) else {
            return;
        };

        // Unmap the windows of the workspace that was visible on this output so
        // they don't linger in the Space at now-invalid coordinates.
        let visible_ws = self.outputs[removed_idx].active_workspace;
        for window in self.workspaces[visible_ws].windows.clone() {
            self.space.unmap_elem(&window);
        }

        self.space.unmap_output(output);
        self.lock_surfaces.remove(output);
        self.outputs.remove(removed_idx);

        // Reindex every stored output index (workspace homes + focused output).
        // `output_count` is the pre-removal count = current registry length + 1.
        let workspace_outputs: Vec<usize> = self.workspaces.iter().map(|ws| ws.output).collect();
        let plan = plan_output_removal(
            removed_idx,
            self.outputs.len() + 1,
            &workspace_outputs,
            self.focused_output,
        );
        for (ws, new_output) in self
            .workspaces
            .iter_mut()
            .zip(plan.workspace_outputs.iter())
        {
            ws.output = *new_output;
        }
        self.focused_output = plan
            .focused_output
            .min(self.outputs.len().saturating_sub(1));

        if self.outputs.is_empty() {
            // Zero-output interval: nothing to render; everything is preserved.
            tracing::warn!("All outputs removed; compositor running headless until one returns");
            self.needs_render = false;
            return;
        }

        self.recompute_output_positions();
        self.reclamp_floating_windows();
        self.relayout_all();

        // Refocus the now-focused output's active workspace.
        let ws = self.active_workspace();
        let focus = self.workspaces[ws]
            .focused_idx
            .and_then(|idx| self.workspaces[ws].windows.get(idx))
            .and_then(|window| window.wl_surface().map(|s| s.into_owned()));
        self.set_keyboard_focus(focus);
        self.needs_render = true;
    }

    /// Repack all outputs left-to-right in the global Space coordinate space and
    /// keep each `OutputCtx.position` in sync. Called after add/remove/mode-change.
    pub(crate) fn recompute_output_positions(&mut self) {
        let mut x = 0;
        for idx in 0..self.outputs.len() {
            let output = self.outputs[idx].output.clone();
            let width = self
                .space
                .output_geometry(&output)
                .map(|geo| geo.size.w)
                .unwrap_or(0);
            let position = Point::from((x, 0));
            self.outputs[idx].position = position;
            self.space.map_output(&output, position);
            x += width.max(0);
        }
    }

    /// Pull every floating window back so a grabbable strip stays inside its
    /// output's usable area. Run after an output's geometry changes or an output
    /// is removed, so a float can never become stranded off every screen
    /// (review bug B4).
    pub(crate) fn reclamp_floating_windows(&mut self) {
        let roots: Vec<WlSurface> = self.floating_windows.keys().cloned().collect();
        for root in roots {
            let Some(data) = self.floating_windows.get(&root).copied() else {
                continue;
            };
            // Resolve the output the float belongs to; if it is stranded off all
            // outputs, `output_for_window`/`focused_output` pulls it to the
            // focused output.
            let output = match self.mapped_window_for_surface(&root) {
                Some(window) => self.output_for_window(&window),
                None => self.focused_output(),
            };
            let Some(output) = output else { continue };
            let Some(usable) = self.floating_usable_rect_for(&output) else {
                continue;
            };
            let clamped = clamp_into_usable(data.position, data.size, usable, ONSCREEN_MARGIN);
            if clamped != data.position {
                self.floating_windows
                    .insert(root.clone(), FloatingWindowData::new(clamped, data.size));
                if let Some(window) = self.mapped_window_for_surface(&root)
                    && self.space.element_geometry(&window).is_some()
                {
                    self.space.map_element(window, clamped, false);
                }
            }
        }
        self.needs_render = true;
    }

    /// React to an output's mode/scale changing: repack positions, pull floats
    /// back on-screen, and relayout every output.
    pub fn handle_output_geometry_changed(&mut self) {
        self.recompute_output_positions();
        self.reclamp_floating_windows();
        self.relayout_all();
        self.needs_render = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputRemovalPlan, pick_initial_workspace, plan_output_removal};

    #[test]
    fn first_output_takes_workspace_zero() {
        assert_eq!(pick_initial_workspace(&[], 10), 0);
    }

    #[test]
    fn second_output_takes_lowest_free_workspace() {
        // Output 0 shows workspace 0, so a second output picks workspace 1.
        assert_eq!(pick_initial_workspace(&[0], 10), 1);
        // Non-contiguous occupancy still picks the lowest free index.
        assert_eq!(pick_initial_workspace(&[0, 2], 10), 1);
    }

    #[test]
    fn falls_back_to_zero_when_all_workspaces_are_shown() {
        assert_eq!(pick_initial_workspace(&[0, 1], 2), 0);
    }

    #[test]
    fn removing_last_output_leaves_no_survivor() {
        let plan = plan_output_removal(0, 1, &[0, 0, 0], 0);
        assert_eq!(
            plan,
            OutputRemovalPlan {
                survivor: None,
                // No outputs remain; homes collapse to index 0 (re-homed on reconnect).
                workspace_outputs: vec![0, 0, 0],
                focused_output: 0,
            }
        );
    }

    #[test]
    fn removing_output_migrates_its_workspaces_to_the_survivor() {
        // Two outputs (0, 1); workspaces 0,1 home on output 0, workspace 2 on
        // output 1. Removing output 0: survivor becomes new index 0, output 1
        // shifts down to 0, and output-0 workspaces migrate to 0.
        let plan = plan_output_removal(0, 2, &[0, 0, 1], 1);
        assert_eq!(plan.survivor, Some(0));
        assert_eq!(plan.workspace_outputs, vec![0, 0, 0]);
        assert_eq!(plan.focused_output, 0); // was 1, shifts down to 0
    }

    #[test]
    fn removing_a_later_output_shifts_higher_indices_down() {
        // Three outputs; remove index 1. Index 2 shifts to 1; index 1's
        // workspaces migrate to survivor 0; focused output 2 -> 1.
        let plan = plan_output_removal(1, 3, &[0, 1, 2, 2], 2);
        assert_eq!(plan.survivor, Some(0));
        assert_eq!(plan.workspace_outputs, vec![0, 0, 1, 1]);
        assert_eq!(plan.focused_output, 1);
    }
}
