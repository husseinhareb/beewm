use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};

use crate::compositor::state::Beewm;
use crate::compositor::types::{
    ActiveGrab, FloatingWindowData, MoveGrab, ResizeEdges, ResizeGrab, ResizeHorizontalEdge,
    ResizeVerticalEdge, TiledSwapGrab,
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
        Some(ActiveGrab::TiledSwap(_)) => {
            update_tiled_swap_target(state);
            true
        }
        None => false,
    }
}

fn apply_move_grab(state: &mut Beewm, grab: &MoveGrab, pointer: Point<f64, Logical>) {
    let dx = (pointer.x - grab.start_pointer.x) as i32;
    let dy = (pointer.y - grab.start_pointer.y) as i32;
    let new_window_pos = Point::from((grab.start_window_pos.x + dx, grab.start_window_pos.y + dy));
    state
        .space
        .map_element(grab.window.clone(), new_window_pos, true);
    if let Some(root) = Beewm::window_root_surface(&grab.window) {
        let size = state
            .space
            .element_geometry(&grab.window)
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

    focus_and_raise_window(state, &window);
    state.active_grab = Some(ActiveGrab::TiledSwap(TiledSwapGrab {
        window,
        workspace_idx: state.active_workspace,
    }));
    state.tiled_swap_target = None;
    state.invalidate_borders();
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

pub(super) fn finish_tiled_swap_grab(state: &mut Beewm) -> bool {
    let grab = match state.active_grab.take() {
        Some(ActiveGrab::TiledSwap(grab)) => grab,
        other => {
            state.active_grab = other;
            return false;
        }
    };

    let Some(source_root) = Beewm::window_root_surface(&grab.window) else {
        state.tiled_swap_target = None;
        state.invalidate_borders();
        state.refresh_compositor_cursor();
        return true;
    };

    let target_root = state.tiled_swap_target.take();
    if let Some(target_root) = target_root {
        state.swap_tiled_windows(grab.workspace_idx, &source_root, &target_root);
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
    let source_root = Beewm::window_root_surface(&grab.window)?;
    let (surface, _) = surface_under(state, state.pointer_location)?;
    let window = state.mapped_window_for_surface(&surface)?;
    let target_root = Beewm::window_root_surface(&window)?;
    if target_root == source_root
        || state.is_root_floating(&target_root)
        || state.is_root_fullscreen(&target_root)
        || state
            .window_index_for_surface(grab.workspace_idx, &target_root)
            .is_none()
    {
        None
    } else {
        Some(target_root)
    }
}

pub(super) fn finish_resize_grab(state: &mut Beewm) -> bool {
    let grab = match state.active_grab.take() {
        Some(ActiveGrab::Resize(grab)) => grab,
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
    if let Some(toplevel) = window.toplevel() {
        state.set_keyboard_focus(Some(toplevel.wl_surface().clone()));
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
