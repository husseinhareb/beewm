use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use crate::compositor::state::Beewm;
use crate::compositor::types::{
    ActiveGrab, FloatingWindowData, MoveGrab, ResizeEdges, ResizeGrab, ResizeHorizontalEdge,
    ResizeVerticalEdge, TiledResizeGrab, TiledSwapGrab,
};

use super::MIN_FLOATING_WINDOW_SIZE;
use super::pointer::surface_under;

pub(super) fn handle_active_grab(state: &mut Beewm, pointer: Point<f64, Logical>) -> bool {
    match state.active_grab.clone() {
        Some(ActiveGrab::Move(grab)) => {
            apply_move_grab(state, &grab, pointer);
            true
        }
        Some(ActiveGrab::Resize(grab)) => {
            apply_resize_grab(state, &grab, pointer);
            true
        }
        Some(ActiveGrab::TiledResize(grab)) => {
            apply_tiled_resize_grab(state, &grab, pointer);
            true
        }
        Some(ActiveGrab::TiledSwap(grab)) => {
            apply_tiled_swap_grab(state, &grab, pointer);
            true
        }
        None => false,
    }
}

fn apply_move_grab(state: &mut Beewm, grab: &MoveGrab, pointer: Point<f64, Logical>) {
    apply_window_move_from_start(
        state,
        &grab.window,
        grab.start_pointer,
        grab.start_window_pos,
        pointer,
    );
}

fn apply_tiled_swap_grab(state: &mut Beewm, grab: &TiledSwapGrab, pointer: Point<f64, Logical>) {
    apply_window_move_from_start(
        state,
        &grab.window,
        grab.start_pointer,
        grab.start_window_pos,
        pointer,
    );
    update_tiled_swap_target(state);
}

fn apply_window_move_from_start(
    state: &mut Beewm,
    window: &Window,
    start_pointer: Point<f64, Logical>,
    start_window_pos: Point<i32, Logical>,
    pointer: Point<f64, Logical>,
) {
    let new_window_pos = dragged_window_location(start_window_pos, start_pointer, pointer);
    state
        .space
        .map_element(window.clone(), new_window_pos, true);
    if let Some(root) = Beewm::window_root_surface(window) {
        let size = state
            .space
            .element_geometry(window)
            .map(|geo| geo.size)
            .or_else(|| {
                state
                    .floating_windows
                    .get(&root)
                    .map(|floating| floating.size)
            });
        if let Some(size) = size {
            state
                .floating_windows
                .insert(root, FloatingWindowData::new(new_window_pos, size));
        }
    }
}

fn apply_resize_grab(state: &mut Beewm, grab: &ResizeGrab, pointer: Point<f64, Logical>) {
    let (new_window_pos, new_window_size) = resized_window_geometry(grab, pointer);
    state
        .space
        .map_element(grab.window.clone(), new_window_pos, true);

    if let Some(root) = Beewm::window_root_surface(&grab.window) {
        state.floating_windows.insert(
            root,
            FloatingWindowData::new(new_window_pos, new_window_size),
        );
    }

    if let Some(toplevel) = grab.window.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(Size::from((new_window_size.w, new_window_size.h)));
        });
        toplevel.send_pending_configure();
    }

    if let Some(ActiveGrab::Resize(active_grab)) = state.active_grab.as_mut() {
        active_grab.current_window_pos = new_window_pos;
        active_grab.current_window_size = new_window_size;
    }
}

fn apply_tiled_resize_grab(
    state: &mut Beewm,
    grab: &TiledResizeGrab,
    pointer: Point<f64, Logical>,
) {
    let dx = (pointer.x - grab.last_pointer.x) as i32;
    let dy = (pointer.y - grab.last_pointer.y) as i32;

    let Some(root) = Beewm::window_root_surface(&grab.window) else {
        if let Some(ActiveGrab::TiledResize(active_grab)) = state.active_grab.as_mut() {
            active_grab.last_pointer = pointer;
        }
        return;
    };

    let Some(usable) = state.tiling_usable_geometry() else {
        if let Some(ActiveGrab::TiledResize(active_grab)) = state.active_grab.as_mut() {
            active_grab.last_pointer = pointer;
        }
        return;
    };

    let tiled_roots = state.tiled_window_roots_in_workspace(grab.workspace_idx);
    if (dx != 0 || dy != 0)
        && state.layout_manager.resize(
            grab.workspace_idx,
            &usable,
            &tiled_roots,
            &root,
            grab.edges,
            (dx, dy),
        )
    {
        state.relayout();

        if let Some(toplevel) = grab.window.toplevel() {
            if let Some(size) = tiled_window_target_size(state, grab.workspace_idx, &root) {
                toplevel.with_pending_state(|state| {
                    state.states.set(xdg_toplevel::State::Resizing);
                    state.size = Some(size);
                });
                toplevel.send_pending_configure();
            }
        }
    }

    if let Some(ActiveGrab::TiledResize(active_grab)) = state.active_grab.as_mut() {
        active_grab.last_pointer = pointer;
    }
}

pub(super) fn try_start_move_grab(state: &mut Beewm) -> bool {
    let Some(window) = floating_window_under_pointer_with_logo(state) else {
        return false;
    };

    let Some(window_geo) = state.space.element_geometry(&window) else {
        return false;
    };

    focus_and_raise_window(state, &window);
    state.active_grab = Some(ActiveGrab::Move(MoveGrab {
        window,
        start_pointer: state.pointer_location,
        start_window_pos: window_geo.loc,
    }));
    state.refresh_compositor_cursor();
    true
}

pub(super) fn try_start_tiled_swap_grab(state: &mut Beewm) -> bool {
    let Some(window) = tiled_window_under_pointer_with_logo(state) else {
        return false;
    };
    let Some(window_geo) = state.space.element_geometry(&window) else {
        return false;
    };
    let Some(root) = Beewm::window_root_surface(&window) else {
        return false;
    };

    let layout_snapshot = state.layout_manager.clone();

    focus_and_raise_window(state, &window);
    state.floating_windows.insert(
        root.clone(),
        FloatingWindowData::new(window_geo.loc, window_geo.size),
    );
    state.remove_tiled_window(state.active_workspace, &root);
    state.tiled_swap_layout_snapshot = Some(layout_snapshot);
    state.active_grab = Some(ActiveGrab::TiledSwap(TiledSwapGrab {
        window: window.clone(),
        workspace_idx: state.active_workspace,
        start_pointer: state.pointer_location,
        start_window_pos: window_geo.loc,
        start_window_size: window_geo.size,
    }));
    state.tiled_swap_target = None;
    state.invalidate_borders();
    state.relayout();
    state.space.raise_element(&window, true);
    state.refresh_compositor_cursor();
    true
}

pub(super) fn try_start_resize_grab(state: &mut Beewm) -> bool {
    let Some(window) = floating_window_under_pointer_with_logo(state) else {
        return false;
    };

    let Some(window_geo) = state.space.element_geometry(&window) else {
        return false;
    };

    let edges = resize_edges_for_pointer(window_geo.loc, window_geo.size, state.pointer_location);
    focus_and_raise_window(state, &window);

    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(Size::from((window_geo.size.w, window_geo.size.h)));
        });
        toplevel.send_pending_configure();
    }

    state.active_grab = Some(ActiveGrab::Resize(ResizeGrab {
        window,
        start_pointer: state.pointer_location,
        start_window_pos: window_geo.loc,
        start_window_size: window_geo.size,
        edges,
        current_window_pos: window_geo.loc,
        current_window_size: window_geo.size,
    }));
    state.refresh_compositor_cursor();
    true
}

pub(super) fn try_start_tiled_resize_grab(state: &mut Beewm) -> bool {
    let Some(window) = tiled_window_under_pointer_with_logo(state) else {
        return false;
    };

    let Some(window_geo) = state.space.element_geometry(&window) else {
        return false;
    };
    let Some(root) = Beewm::window_root_surface(&window) else {
        return false;
    };

    let edges = resize_edges_for_pointer(window_geo.loc, window_geo.size, state.pointer_location);
    let initial_size = tiled_window_target_size(state, state.active_workspace, &root)
        .unwrap_or_else(|| Size::from((window_geo.size.w, window_geo.size.h)));
    focus_and_raise_window(state, &window);

    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(initial_size);
        });
        toplevel.send_pending_configure();
    }

    state.active_grab = Some(ActiveGrab::TiledResize(TiledResizeGrab {
        window,
        workspace_idx: state.active_workspace,
        edges,
        last_pointer: state.pointer_location,
    }));
    state.refresh_compositor_cursor();
    true
}

pub(super) fn finish_tiled_swap_grab(state: &mut Beewm) -> bool {
    let grab = match state.active_grab.take() {
        Some(ActiveGrab::TiledSwap(grab)) => grab,
        other => {
            state.active_grab = other;
            return false;
        }
    };

    if let Some(layout_snapshot) = state.tiled_swap_layout_snapshot.take() {
        state.layout_manager = layout_snapshot;
    }

    let Some(source_root) = Beewm::window_root_surface(&grab.window) else {
        state.tiled_swap_target = None;
        state.invalidate_borders();
        state.relayout();
        state.refresh_compositor_cursor();
        return true;
    };

    state.floating_windows.remove(&source_root);
    let target_root = state.tiled_swap_target.take();
    let dropped_in_original_slot = pointer_in_original_tiled_slot(&grab, state.pointer_location);
    let swapped = (!dropped_in_original_slot)
        .then(|| {
            target_root.as_ref().map(|target_root| {
                state.swap_tiled_windows(grab.workspace_idx, &source_root, target_root)
            })
        })
        .flatten()
        .unwrap_or(false);
    if !swapped {
        state.relayout();
    }

    state.invalidate_borders();
    state.refresh_compositor_cursor();
    true
}

fn update_tiled_swap_target(state: &mut Beewm) {
    let new_target = candidate_tiled_swap_target(state);
    if state.tiled_swap_target != new_target {
        state.tiled_swap_target = new_target;
        state.invalidate_borders();
    }
}

fn candidate_tiled_swap_target(state: &Beewm) -> Option<WlSurface> {
    let grab = match &state.active_grab {
        Some(ActiveGrab::TiledSwap(grab)) => grab,
        _ => return None,
    };
    if original_tiled_slot_contains_pointer(
        grab.start_window_pos,
        grab.start_window_size,
        state.pointer_location,
    ) {
        return None;
    }
    let source_root = Beewm::window_root_surface(&grab.window)?;
    tiled_swap_target_from_geometries(
        state.pointer_location,
        &source_root,
        state.workspaces[grab.workspace_idx]
            .windows
            .iter()
            .filter_map(|window| {
                let root = Beewm::window_root_surface(window)?;
                if state.is_root_floating(&root)
                    || state.is_root_fullscreen(&root)
                    || state
                        .window_index_for_surface(grab.workspace_idx, &root)
                        .is_none()
                {
                    return None;
                }
                state
                    .space
                    .element_geometry(window)
                    .map(|geometry| (root, geometry))
            }),
    )
}

pub(super) fn finish_resize_grab(state: &mut Beewm) -> bool {
    let grab = match state.active_grab.take() {
        Some(ActiveGrab::Resize(grab)) => grab,
        Some(ActiveGrab::TiledResize(grab)) => {
            if let Some(toplevel) = grab.window.toplevel() {
                let size = Beewm::window_root_surface(&grab.window)
                    .as_ref()
                    .and_then(|root| tiled_window_target_size(state, grab.workspace_idx, root));
                toplevel.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = size;
                });
                toplevel.send_configure();
            }
            state.refresh_compositor_cursor();
            return true;
        }
        other => {
            state.active_grab = other;
            return false;
        }
    };

    if let Some(toplevel) = grab.window.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Resizing);
            state.size = Some(Size::from((
                grab.current_window_size.w,
                grab.current_window_size.h,
            )));
        });
        toplevel.send_configure();
    }

    state
        .space
        .map_element(grab.window.clone(), grab.current_window_pos, true);
    if let Some(root) = Beewm::window_root_surface(&grab.window) {
        state.floating_windows.insert(
            root,
            FloatingWindowData::new(grab.current_window_pos, grab.current_window_size),
        );
    }
    state.refresh_compositor_cursor();
    true
}

fn tiled_window_target_size(
    state: &Beewm,
    workspace_idx: usize,
    root: &WlSurface,
) -> Option<Size<i32, Logical>> {
    let usable = state.tiling_usable_geometry()?;
    let tiled_roots = state.tiled_window_roots_in_workspace(workspace_idx);
    let geometry = state
        .layout_manager
        .geometries(workspace_idx, &usable, &tiled_roots)
        .get(root)
        .copied()?;
    Some(state.configured_tiled_size(geometry))
}

fn floating_window_under_pointer_with_logo(state: &mut Beewm) -> Option<Window> {
    let keyboard = state.seat.get_keyboard().unwrap();
    let modifiers = keyboard.modifier_state();
    if !modifiers.logo {
        return None;
    }

    let (surface, _) = surface_under(state, state.pointer_location)?;
    let window = state.mapped_window_for_surface(&surface)?;
    let root = Beewm::window_root_surface(&window)?;
    state.floating_windows.contains_key(&root).then_some(window)
}

fn tiled_window_under_pointer_with_logo(state: &mut Beewm) -> Option<Window> {
    let keyboard = state.seat.get_keyboard().unwrap();
    let modifiers = keyboard.modifier_state();
    if !modifiers.logo {
        return None;
    }

    let (surface, _) = surface_under(state, state.pointer_location)?;
    let window = state.mapped_window_for_surface(&surface)?;
    let root = Beewm::window_root_surface(&window)?;
    (!state.is_root_floating(&root) && !state.is_root_fullscreen(&root)).then_some(window)
}

fn focus_and_raise_window(state: &mut Beewm, window: &Window) {
    if let Some(surface) = window.wl_surface().map(|surface| surface.into_owned()) {
        state.set_keyboard_focus(Some(surface));
    }
    state.space.raise_element(window, true);
    state.needs_render = true;
}

pub fn resize_edges_for_pointer(
    window_pos: Point<i32, Logical>,
    window_size: Size<i32, Logical>,
    pointer: Point<f64, Logical>,
) -> ResizeEdges {
    let center_x = window_pos.x as f64 + window_size.w as f64 / 2.0;
    let center_y = window_pos.y as f64 + window_size.h as f64 / 2.0;

    ResizeEdges {
        horizontal: if pointer.x < center_x {
            ResizeHorizontalEdge::Left
        } else {
            ResizeHorizontalEdge::Right
        },
        vertical: if pointer.y < center_y {
            ResizeVerticalEdge::Top
        } else {
            ResizeVerticalEdge::Bottom
        },
    }
}

fn resized_window_geometry(
    grab: &ResizeGrab,
    pointer: Point<f64, Logical>,
) -> (Point<i32, Logical>, Size<i32, Logical>) {
    resized_window_geometry_from_start(
        grab.start_window_pos,
        grab.start_window_size,
        grab.start_pointer,
        pointer,
        grab.edges,
    )
}

pub fn resized_window_geometry_from_start(
    start_window_pos: Point<i32, Logical>,
    start_window_size: Size<i32, Logical>,
    start_pointer: Point<f64, Logical>,
    pointer: Point<f64, Logical>,
    edges: ResizeEdges,
) -> (Point<i32, Logical>, Size<i32, Logical>) {
    let dx = (pointer.x - start_pointer.x) as i32;
    let dy = (pointer.y - start_pointer.y) as i32;

    let start_left = start_window_pos.x;
    let start_top = start_window_pos.y;
    let start_right = start_left + start_window_size.w.max(MIN_FLOATING_WINDOW_SIZE);
    let start_bottom = start_top + start_window_size.h.max(MIN_FLOATING_WINDOW_SIZE);

    let (new_x, new_width) = match edges.horizontal {
        ResizeHorizontalEdge::Left => {
            let new_x = (start_left + dx).min(start_right - MIN_FLOATING_WINDOW_SIZE);
            let new_width = (start_right - new_x).max(MIN_FLOATING_WINDOW_SIZE);
            (new_x, new_width)
        }
        ResizeHorizontalEdge::Right => {
            let new_width = (start_window_size.w + dx).max(MIN_FLOATING_WINDOW_SIZE);
            (start_left, new_width)
        }
    };

    let (new_y, new_height) = match edges.vertical {
        ResizeVerticalEdge::Top => {
            let new_y = (start_top + dy).min(start_bottom - MIN_FLOATING_WINDOW_SIZE);
            let new_height = (start_bottom - new_y).max(MIN_FLOATING_WINDOW_SIZE);
            (new_y, new_height)
        }
        ResizeVerticalEdge::Bottom => {
            let new_height = (start_window_size.h + dy).max(MIN_FLOATING_WINDOW_SIZE);
            (start_top, new_height)
        }
    };

    (
        Point::from((new_x, new_y)),
        Size::from((new_width, new_height)),
    )
}

fn dragged_window_location(
    start_window_pos: Point<i32, Logical>,
    start_pointer: Point<f64, Logical>,
    pointer: Point<f64, Logical>,
) -> Point<i32, Logical> {
    let dx = (pointer.x - start_pointer.x) as i32;
    let dy = (pointer.y - start_pointer.y) as i32;
    Point::from((start_window_pos.x + dx, start_window_pos.y + dy))
}

fn tiled_swap_target_from_geometries<T: PartialEq>(
    pointer: Point<f64, Logical>,
    source_root: &T,
    candidates: impl IntoIterator<Item = (T, Rectangle<i32, Logical>)>,
) -> Option<T> {
    candidates.into_iter().find_map(|(root, geometry)| {
        (root != *source_root && window_geometry_contains_pointer(geometry, pointer))
            .then_some(root)
    })
}

fn pointer_in_original_tiled_slot(grab: &TiledSwapGrab, pointer: Point<f64, Logical>) -> bool {
    original_tiled_slot_contains_pointer(grab.start_window_pos, grab.start_window_size, pointer)
}

fn original_tiled_slot_contains_pointer(
    start_window_pos: Point<i32, Logical>,
    start_window_size: Size<i32, Logical>,
    pointer: Point<f64, Logical>,
) -> bool {
    window_geometry_contains_pointer(Rectangle::new(start_window_pos, start_window_size), pointer)
}

fn window_geometry_contains_pointer(
    geometry: Rectangle<i32, Logical>,
    pointer: Point<f64, Logical>,
) -> bool {
    pointer.x >= geometry.loc.x as f64
        && pointer.x < (geometry.loc.x + geometry.size.w) as f64
        && pointer.y >= geometry.loc.y as f64
        && pointer.y < (geometry.loc.y + geometry.size.h) as f64
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Logical, Point, Rectangle, Size};

    use super::{
        dragged_window_location, original_tiled_slot_contains_pointer,
        tiled_swap_target_from_geometries,
    };

    #[test]
    fn dragged_window_location_preserves_the_initial_pointer_offset() {
        let location = dragged_window_location(
            Point::<i32, Logical>::from((100, 200)),
            Point::<f64, Logical>::from((140.0, 260.0)),
            Point::<f64, Logical>::from((220.0, 320.0)),
        );

        assert_eq!(location, Point::from((180, 260)));
    }

    #[test]
    fn tiled_swap_target_ignores_the_dragged_root() {
        let target = tiled_swap_target_from_geometries(
            Point::<f64, Logical>::from((40.0, 40.0)),
            &1u8,
            [
                (
                    1u8,
                    Rectangle::new((0, 0).into(), Size::<i32, Logical>::from((100, 100))),
                ),
                (
                    2u8,
                    Rectangle::new((120, 0).into(), Size::<i32, Logical>::from((100, 100))),
                ),
            ],
        );

        assert_eq!(target, None);
    }

    #[test]
    fn tiled_swap_target_finds_the_window_under_the_pointer() {
        let target = tiled_swap_target_from_geometries(
            Point::<f64, Logical>::from((150.0, 40.0)),
            &1u8,
            [
                (
                    1u8,
                    Rectangle::new((0, 0).into(), Size::<i32, Logical>::from((100, 100))),
                ),
                (
                    2u8,
                    Rectangle::new((120, 0).into(), Size::<i32, Logical>::from((100, 100))),
                ),
            ],
        );

        assert_eq!(target, Some(2));
    }

    #[test]
    fn original_tiled_slot_contains_the_release_pointer() {
        assert!(original_tiled_slot_contains_pointer(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 150)),
            Point::<f64, Logical>::from((250.0, 275.0)),
        ));
        assert!(!original_tiled_slot_contains_pointer(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 150)),
            Point::<f64, Logical>::from((410.0, 275.0)),
        ));
    }
}
