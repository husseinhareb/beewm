use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus;

use crate::compositor::focus_target::KeyboardFocusTarget;
use crate::config::FocusDirection;

use super::{Beewm, root_surface};

impl Beewm {
    pub fn invalidate_borders(&mut self) {
        self.border_commit_serial = self.border_commit_serial.wrapping_add(1);
        self.needs_render = true;
    }

    pub fn window_index_for_surface(
        &self,
        workspace_idx: usize,
        surface: &WlSurface,
    ) -> Option<usize> {
        let surface_root = root_surface(surface);
        self.workspaces[workspace_idx]
            .windows
            .iter()
            .position(|window| {
                window
                    .wl_surface()
                    .as_ref()
                    .map(|window_surface| **window_surface == surface_root)
                    .unwrap_or(false)
            })
    }

    pub fn mapped_window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.window_lookup.get(&root_surface(surface)).cloned()
    }

    pub fn track_window(&mut self, window: &Window) {
        if let Some(surface) = window.wl_surface().as_ref() {
            self.window_lookup
                .insert((**surface).clone(), window.clone());
        }
    }

    pub fn untrack_window_for_surface(&mut self, surface: &WlSurface) {
        self.window_lookup.remove(&root_surface(surface));
    }

    pub fn active_workspace_focused_index(&self) -> Option<usize> {
        if let Some(target) = self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            if let Some(surface) = target.wl_surface() {
                if let Some(idx) = self.window_index_for_surface(self.active_workspace, &surface) {
                    return Some(idx);
                }
            }
        }
        self.workspaces[self.active_workspace].focused_idx
    }

    pub fn active_workspace_focused_window(&self) -> Option<&Window> {
        let idx = self.active_workspace_focused_index()?;
        self.workspaces[self.active_workspace].windows.get(idx)
    }

    pub fn focus_current_window(&mut self) {
        let Some(idx) = self.workspaces[self.active_workspace].focused_idx else {
            return;
        };
        self.focus_active_workspace_window(idx);
    }

    /// Move keyboard focus to the next/previous tiled window in *on-screen
    /// layout order* (the order `LayoutManager::geometries` produces), wrapping
    /// around. This is what `FocusNext`/`FocusPrev` bind to.
    ///
    /// Cycling over the layout order — rather than the workspace's
    /// window-insertion order — means `mod+Tab` walks neighbours as they appear
    /// on screen instead of jumping around by creation time. When the active
    /// workspace has no tiled windows (everything is floating), fall back to the
    /// insertion-order cycle so floating-only workspaces still cycle.
    pub fn focus_in_cycle(&mut self, forward: bool) {
        let ws_idx = self.active_workspace;
        let ordered = self.layout_manager.ordered_roots(ws_idx);
        if ordered.is_empty() {
            let workspace = &mut self.workspaces[ws_idx];
            if forward {
                workspace.focus_next();
            } else {
                workspace.focus_prev();
            }
            self.focus_current_window();
            return;
        }

        let current_pos = self
            .focused_tiled_window_root(ws_idx)
            .and_then(|root| ordered.iter().position(|candidate| *candidate == root));

        let next_pos = match current_pos {
            Some(pos) if forward => (pos + 1) % ordered.len(),
            Some(pos) => (pos + ordered.len() - 1) % ordered.len(),
            None => 0,
        };

        let target_root = ordered[next_pos].clone();
        if let Some(idx) = self.window_index_for_surface(ws_idx, &target_root) {
            self.focus_active_workspace_window(idx);
        }
    }

    pub fn focus_window_in_direction(&mut self, direction: FocusDirection) {
        let Some(current_idx) = self.active_workspace_focused_index() else {
            return;
        };

        let Some(current_window) = self.workspaces[self.active_workspace]
            .windows
            .get(current_idx)
            .cloned()
        else {
            return;
        };

        let Some(current_geometry) = self.space.element_geometry(&current_window) else {
            return;
        };

        let target_idx = {
            let workspace = &self.workspaces[self.active_workspace];
            best_directional_focus_candidate(
                current_geometry,
                workspace
                    .windows
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != current_idx)
                    .filter_map(|(idx, window)| {
                        self.space.element_geometry(window).map(|geo| (idx, geo))
                    }),
                direction,
            )
        };

        if let Some(target_idx) = target_idx {
            self.focus_active_workspace_window(target_idx);
        }
    }

    /// The single authoritative writer of `Workspace::focused_idx`. The seat's
    /// keyboard focus is the source of truth for "which window is focused";
    /// `focused_idx` is only a cache of that (and the value restored when a
    /// workspace is re-entered). Smithay calls this from `focus_changed` after
    /// it has already updated the seat focus, so `active_workspace_focused_index`
    /// — which reads the seat — agrees with what we cache here. No other code
    /// path should assign `focused_idx` directly.
    pub fn note_keyboard_focus_change(&mut self, focused: Option<&WlSurface>) {
        let new_idx = focused
            .and_then(|s| self.window_index_for_surface(self.active_workspace, s));

        let old_idx = self.workspaces[self.active_workspace].focused_idx;

        if new_idx != old_idx {
            if let Some(idx) = old_idx {
                if let Some(window) = self.workspaces[self.active_workspace].windows.get(idx) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|s| {
                            s.states.unset(xdg_toplevel::State::Activated);
                        });
                        toplevel.send_pending_configure();
                    }
                    if let Some(x11) = window.x11_surface() {
                        let _ = x11.set_activated(false);
                    }
                }
            }
            if let Some(idx) = new_idx {
                if let Some(window) = self.workspaces[self.active_workspace].windows.get(idx) {
                    if let Some(toplevel) = window.toplevel() {
                        toplevel.with_pending_state(|s| {
                            s.states.set(xdg_toplevel::State::Activated);
                        });
                        toplevel.send_pending_configure();
                    }
                    if let Some(x11) = window.x11_surface() {
                        let _ = x11.set_activated(true);
                    }
                }
            }
        }

        if let Some(idx) = new_idx {
            self.workspaces[self.active_workspace].focused_idx = Some(idx);
            // The cache must agree with the seat keyboard focus we just synced
            // from. `active_workspace_focused_index` prefers the seat, so this
            // catches any drift between the two representations of focus.
            debug_assert_eq!(
                self.active_workspace_focused_index(),
                Some(idx),
                "focused_idx cache diverged from the seat keyboard focus",
            );
        }

        self.invalidate_borders();
        self.request_focus_publish();
    }

    /// Convenience entrypoint for callers that have only a wl_surface in
    /// hand (xdg-shell paths). Looks the surface up against the live window
    /// map so an X11 client surface gets routed through `X11Surface`'s
    /// KeyboardTarget rather than the bare wl_surface.
    pub fn set_keyboard_focus(&mut self, focused: Option<WlSurface>) {
        let target = focused.map(|surface| self.focus_target_for_surface(&surface));
        self.set_keyboard_focus_target(target);
    }

    /// Resolve a wl_surface to the right keyboard-focus variant. Falls back
    /// to `Wayland` when the surface isn't a tracked window (e.g. layer
    /// surfaces — those don't have X11 input focus to worry about).
    pub(crate) fn focus_target_for_surface(&self, surface: &WlSurface) -> KeyboardFocusTarget {
        match self.mapped_window_for_surface(surface) {
            Some(window) => {
                KeyboardFocusTarget::from_window(&window).unwrap_or_else(|| surface.clone().into())
            }
            None => surface.clone().into(),
        }
    }

    pub fn set_keyboard_focus_target(&mut self, focused: Option<KeyboardFocusTarget>) {
        let serial = SERIAL_COUNTER.next_serial();

        // Dismiss any active popup grab before re-focusing. We unset the keyboard
        // grab first so that PopupPointerGrab::unset() finds keyboard.is_grabbed()
        // = false and skips the intermediate keyboard.set_focus(root) call, which
        // would otherwise emit a spurious focus_changed event before we set the
        // real focus below.
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if keyboard.is_grabbed() {
            keyboard.unset_grab(self);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            if pointer.is_grabbed() {
                let time_ms = self.start_time.elapsed().as_millis() as u32;
                pointer.unset_grab(self, serial, time_ms);
            }
        }

        let is_none = focused.is_none();
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        keyboard.set_focus(self, focused, serial);

        // Smithay does not invoke SeatHandler::focus_changed when the focus is unset.
        if is_none {
            if let Some(prev) = self.prev_keyboard_focus.take() {
                if self.deactivate_pointer_constraint_for(&prev) {
                    self.set_cursor_status(smithay::input::pointer::CursorImageStatus::default_named());
                }
            }
            self.note_keyboard_focus_change(None);
        }
    }

    fn focus_active_workspace_window(&mut self, idx: usize) {
        let Some(window) = self.workspaces[self.active_workspace]
            .windows
            .get(idx)
            .cloned()
        else {
            return;
        };

        // Don't set `focused_idx` here: the seat's keyboard focus is the single
        // source of truth. Setting the focus below routes through
        // `note_keyboard_focus_change`, which updates the `focused_idx` cache.
        if let Some(target) = KeyboardFocusTarget::from_window(&window) {
            self.set_keyboard_focus_target(Some(target));
        }

        self.space.raise_element(&window, true);
        // Keep floating dialogs above their parent: raising a tiled window with
        // directional focus must not occlude a transient/modal dialog. Skip when
        // the focused window is itself floating so it stays on top.
        let raised_floating = Self::window_root_surface(&window)
            .map(|root| self.is_root_floating(&root))
            .unwrap_or(false);
        if !raised_floating {
            self.raise_floating_windows();
        }
        // Sync the X11 z-order. Without this, XWayland still thinks the
        // previously-focused X11 window is on top and routes pointer events
        // accordingly — observable as "Steam has the focus border but clicks
        // do nothing" after returning from a game.
        if let Some(x11) = window.x11_surface() {
            self.raise_x11_window(x11);
        }
        self.needs_render = true;
    }

    /// Tell XWayland to raise this X11 window in the X server's stacking
    /// order. Keep separate from `space.raise_element` because that one only
    /// touches the wl_surface scene graph — X11 pointer event routing is
    /// driven by the X server's own z-order.
    pub(crate) fn raise_x11_window(&mut self, surface: &smithay::xwayland::X11Surface) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(error) = xwm.raise_window(surface) {
                tracing::warn!(
                    target: "beewm::xwayland",
                    "Failed to raise X11 window in X server stacking order: {}",
                    error,
                );
            }
        }
    }
}

fn best_directional_focus_candidate(
    current: Rectangle<i32, Logical>,
    candidates: impl IntoIterator<Item = (usize, Rectangle<i32, Logical>)>,
    direction: FocusDirection,
) -> Option<usize> {
    candidates
        .into_iter()
        .filter_map(|(idx, candidate)| {
            directional_candidate_score(current, candidate, direction).map(|score| (idx, score))
        })
        .min_by_key(|(_, score)| *score)
        .map(|(idx, _)| idx)
}

fn directional_candidate_score(
    current: Rectangle<i32, Logical>,
    candidate: Rectangle<i32, Logical>,
    direction: FocusDirection,
) -> Option<(bool, i32, i32, i64, i64)> {
    let current_center = rect_center_doubled(current);
    let candidate_center = rect_center_doubled(candidate);

    let is_in_direction = match direction {
        FocusDirection::Left => candidate_center.0 < current_center.0,
        FocusDirection::Right => candidate_center.0 > current_center.0,
        FocusDirection::Up => candidate_center.1 < current_center.1,
        FocusDirection::Down => candidate_center.1 > current_center.1,
    };

    if !is_in_direction {
        return None;
    }

    let (primary_gap, secondary_gap, primary_delta, secondary_delta) = match direction {
        FocusDirection::Left => (
            (current.loc.x - rect_right(candidate)).max(0),
            interval_gap(
                current.loc.y,
                rect_bottom(current),
                candidate.loc.y,
                rect_bottom(candidate),
            ),
            current_center.0 - candidate_center.0,
            (current_center.1 - candidate_center.1).abs(),
        ),
        FocusDirection::Right => (
            (candidate.loc.x - rect_right(current)).max(0),
            interval_gap(
                current.loc.y,
                rect_bottom(current),
                candidate.loc.y,
                rect_bottom(candidate),
            ),
            candidate_center.0 - current_center.0,
            (current_center.1 - candidate_center.1).abs(),
        ),
        FocusDirection::Up => (
            (current.loc.y - rect_bottom(candidate)).max(0),
            interval_gap(
                current.loc.x,
                rect_right(current),
                candidate.loc.x,
                rect_right(candidate),
            ),
            current_center.1 - candidate_center.1,
            (current_center.0 - candidate_center.0).abs(),
        ),
        FocusDirection::Down => (
            (candidate.loc.y - rect_bottom(current)).max(0),
            interval_gap(
                current.loc.x,
                rect_right(current),
                candidate.loc.x,
                rect_right(candidate),
            ),
            candidate_center.1 - current_center.1,
            (current_center.0 - candidate_center.0).abs(),
        ),
    };

    Some((
        secondary_gap != 0,
        primary_gap,
        secondary_gap,
        primary_delta,
        secondary_delta,
    ))
}

fn rect_center_doubled(rect: Rectangle<i32, Logical>) -> (i64, i64) {
    (
        (rect.loc.x as i64 * 2) + rect.size.w as i64,
        (rect.loc.y as i64 * 2) + rect.size.h as i64,
    )
}

fn rect_right(rect: Rectangle<i32, Logical>) -> i32 {
    rect.loc.x + rect.size.w
}

fn rect_bottom(rect: Rectangle<i32, Logical>) -> i32 {
    rect.loc.y + rect.size.h
}

fn interval_gap(first_start: i32, first_end: i32, second_start: i32, second_end: i32) -> i32 {
    if first_end <= second_start {
        second_start - first_end
    } else if second_end <= first_start {
        first_start - second_end
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Rectangle, Size};

    use super::{best_directional_focus_candidate, interval_gap};
    use crate::config::FocusDirection;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, smithay::utils::Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn directional_focus_prefers_overlapping_neighbor() {
        let current = rect(0, 0, 100, 100);
        let candidates = vec![(1, rect(120, 160, 100, 100)), (2, rect(120, 0, 100, 100))];

        let result = best_directional_focus_candidate(current, candidates, FocusDirection::Right);

        assert_eq!(result, Some(2));
    }

    #[test]
    fn directional_focus_picks_nearest_window_in_direction() {
        let current = rect(0, 0, 100, 100);
        let candidates = vec![
            (1, rect(240, 0, 100, 100)),
            (2, rect(120, 10, 100, 100)),
            (3, rect(-120, 0, 100, 100)),
        ];

        let result = best_directional_focus_candidate(current, candidates, FocusDirection::Right);

        assert_eq!(result, Some(2));
    }

    #[test]
    fn directional_focus_ignores_windows_not_in_requested_direction() {
        let current = rect(100, 100, 100, 100);
        let candidates = vec![(1, rect(100, 260, 100, 100)), (2, rect(260, 100, 100, 100))];

        let result = best_directional_focus_candidate(current, candidates, FocusDirection::Up);

        assert_eq!(result, None);
    }

    #[test]
    fn interval_gap_is_zero_for_overlapping_ranges() {
        assert_eq!(interval_gap(0, 100, 50, 150), 0);
        assert_eq!(interval_gap(50, 150, 0, 100), 0);
        assert_eq!(interval_gap(0, 100, 100, 200), 0);
    }
}
