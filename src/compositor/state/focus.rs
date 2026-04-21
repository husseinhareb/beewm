use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::seat::WaylandFocus;

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
        match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(surface) => self.window_index_for_surface(self.active_workspace, &surface),
            None => self.workspaces[self.active_workspace].focused_idx,
        }
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

    pub fn note_keyboard_focus_change(&mut self, focused: Option<&WlSurface>) {
        if let Some(surface) = focused {
            if let Some(idx) = self.window_index_for_surface(self.active_workspace, surface) {
                self.workspaces[self.active_workspace].focused_idx = Some(idx);
            }
        }

        self.invalidate_borders();
    }

    pub fn set_keyboard_focus(&mut self, focused: Option<WlSurface>) {
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, focused.clone(), serial);

        // Smithay does not invoke SeatHandler::focus_changed when the focus is unset.
        if focused.is_none() {
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

        self.workspaces[self.active_workspace].focused_idx = Some(idx);

        if let Some(surface) = window.wl_surface().map(|surface| surface.into_owned()) {
            self.set_keyboard_focus(Some(surface));
        }

        self.space.raise_element(&window, true);
        self.needs_render = true;
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
