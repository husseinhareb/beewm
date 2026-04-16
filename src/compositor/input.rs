use crate::config::Action;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::session::Session;
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer as WlrLayer, LayerSurfaceCachedState,
};

use super::commands::spawn_shell_command;
use super::state::{
    Beewm, MoveGrab, ResizeEdges, ResizeGrab, ResizeHorizontalEdge, ResizeVerticalEdge,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const MIN_FLOATING_WINDOW_SIZE: i32 = 1;

/// Returns `true` when keyboard focus is currently on a layer-shell surface
/// (e.g. wofi).  In that case focus-follows-mouse must NOT steal focus away.
fn layer_surface_has_keyboard_focus(state: &Beewm) -> bool {
    let Some(focused) = state.seat.get_keyboard().and_then(|kb| kb.current_focus()) else {
        return false;
    };
    // If the focused surface belongs to a mapped tiled window it is NOT a
    // layer surface.
    if state.mapped_window_for_surface(&focused).is_some() {
        return false;
    }
    // Check whether any output's layer map contains this surface.
    state
        .space
        .layer_for_surface(&focused, smithay::desktop::WindowSurfaceType::ALL)
        .is_some()
}

/// Process an input event from any backend.
pub fn handle_input<I: InputBackend>(state: &mut Beewm, event: InputEvent<I>) {
    match event {
        InputEvent::Keyboard { event } => handle_keyboard::<I>(state, event),
        InputEvent::PointerMotion { event } => handle_pointer_motion::<I>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            handle_pointer_motion_absolute::<I>(state, event)
        }
        InputEvent::PointerButton { event } => handle_pointer_button::<I>(state, event),
        InputEvent::PointerAxis { event } => handle_pointer_axis::<I>(state, event),
        _ => {}
    }
}

fn handle_keyboard<I: InputBackend>(state: &mut Beewm, event: I::KeyboardKeyEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let time = Event::time_msec(&event);
    let keycode = event.key_code();
    let key_state = event.state();

    let keyboard = state.seat.get_keyboard().unwrap();

    keyboard.input::<(), _>(
        state,
        keycode,
        key_state,
        serial,
        time,
        |state, modifiers, keysym_handle| {
            if key_state == KeyState::Pressed {
                // VT switching: XF86Switch_VT_1 through XF86Switch_VT_12
                let keysym = keysym_handle.modified_sym();
                let raw = keysym.raw();
                if (0x1008FE01..=0x1008FE0C).contains(&raw) {
                    let vt = (raw - 0x1008FE01 + 1) as i32;
                    if let Some(session) = state.session.as_mut() {
                        if let Err(error) = session.change_vt(vt) {
                            tracing::warn!("Failed to switch to VT {}: {}", vt, error);
                        }
                    }
                    return FilterResult::Intercept(());
                }

                if let Some(action) = match_keybind(state, modifiers, &keysym_handle) {
                    execute_action(state, action);
                    return FilterResult::Intercept(());
                }
            }
            FilterResult::Forward
        },
    );
}

fn match_keybind(
    state: &Beewm,
    modifiers: &ModifiersState,
    keysym_handle: &KeysymHandle<'_>,
) -> Option<Action> {
    let raw = keysym_handle.raw_syms();
    let keysym = if raw.is_empty() {
        keysym_handle.modified_sym()
    } else {
        raw[0]
    };

    for bind in &state.resolved_keybinds {
        if modifiers.logo == bind.logo
            && modifiers.shift == bind.shift
            && modifiers.ctrl == bind.ctrl
            && modifiers.alt == bind.alt
            && bind.keysym == keysym
        {
            return Some(bind.action.clone());
        }
    }

    None
}

fn execute_action(state: &mut Beewm, action: Action) {
    match action {
        Action::Spawn(cmd) => {
            tracing::info!("Spawning: {}", cmd);
            if let Err(e) = spawn_shell_command(&cmd, &state.child_env) {
                tracing::error!("Failed to spawn '{}': {}", cmd, e);
            }
        }
        Action::FocusNext => {
            let ws = &mut state.workspaces[state.active_workspace];
            ws.focus_next();
            focus_current_window(state);
        }
        Action::FocusPrev => {
            let ws = &mut state.workspaces[state.active_workspace];
            ws.focus_prev();
            focus_current_window(state);
        }
        Action::CloseWindow => {
            if let Some(window) = state.active_workspace_focused_window() {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
        }
        Action::ToggleFullscreen => {
            state.toggle_fullscreen();
        }
        Action::ToggleFloat => {
            state.toggle_float();
        }
        Action::Quit => {
            tracing::info!("Quit requested");
            state.running = false;
        }
        Action::SwitchWorkspace(idx) => {
            state.switch_workspace(idx);
        }
        Action::MoveToWorkspace(idx) => {
            state.move_to_workspace(idx);
        }
    }
}

fn focus_current_window(state: &mut Beewm) {
    let ws = &state.workspaces[state.active_workspace];
    let idx = match ws.focused_idx {
        Some(i) => i,
        None => return,
    };

    let window = match state.workspace_windows[state.active_workspace]
        .get(idx)
        .cloned()
    {
        Some(w) => w,
        None => return,
    };

    let serial = SERIAL_COUNTER.next_serial();
    let keyboard = state.seat.get_keyboard().unwrap();
    if let Some(toplevel) = window.toplevel() {
        let surface = toplevel.wl_surface().clone();
        keyboard.set_focus(state, Some(surface), serial);
    }
    state.space.raise_element(&window, true);
    state.needs_render = true;
}

fn surface_under(
    state: &Beewm,
    pos: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<(
    WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    let output = state
        .space
        .output_under(pos)
        .next()
        .cloned()
        .or_else(|| state.space.outputs().next().cloned())?;

    // Hit-test in z-order: Overlay and Top sit above windows, Bottom and
    // Background sit below.  Checking Background/Bottom before windows caused
    // full-screen background layer surfaces (e.g. wallpaper daemons) to absorb
    // every pointer event, making windows unclickable.
    let layer_hit = |layer: WlrLayer| -> Option<(
        WlSurface,
        smithay::utils::Point<f64, smithay::utils::Logical>,
    )> {
        let layer_map = layer_map_for_output(&output);
        let layer_surface = layer_map.layer_under(layer, pos)?.clone();
        let layer_geometry = layer_map.layer_geometry(&layer_surface)?;
        let local = pos - layer_geometry.loc.to_f64();
        let (surface, surface_loc) = layer_surface.surface_under(local, WindowSurfaceType::ALL)?;
        Some((surface, layer_geometry.loc.to_f64() + surface_loc.to_f64()))
    };

    // 1. Overlay (topmost)
    if let Some(hit) = layer_hit(WlrLayer::Overlay) {
        return Some(hit);
    }

    // 2. Top layer (panels, bars)
    if let Some(hit) = layer_hit(WlrLayer::Top) {
        return Some(hit);
    }

    // 3. Regular windows
    if let Some(hit) = state.space.element_under(pos).and_then(|(window, loc)| {
        let local = pos - loc.to_f64();
        window
            .surface_under(local, WindowSurfaceType::ALL)
            .map(|(surface, surface_loc)| (surface, loc.to_f64() + surface_loc.to_f64()))
    }) {
        return Some(hit);
    }

    // 4. Bottom layer
    if let Some(hit) = layer_hit(WlrLayer::Bottom) {
        return Some(hit);
    }

    // 5. Background layer (wallpapers — rarely receive pointer events)
    layer_hit(WlrLayer::Background)
}

fn surface_accepts_keyboard_focus(state: &Beewm, surface: &WlSurface) -> bool {
    if state.mapped_window_for_surface(surface).is_some() {
        return true;
    }

    let Some(layer) = state
        .space
        .layer_for_surface(surface, WindowSurfaceType::ALL)
    else {
        return false;
    };

    with_states(layer.wl_surface(), |states| {
        states
            .cached_state
            .get::<LayerSurfaceCachedState>()
            .current()
            .keyboard_interactivity
            != KeyboardInteractivity::None
    })
}

fn handle_pointer_motion<I: InputBackend>(state: &mut Beewm, event: I::PointerMotionEvent) {
    let output = match state.space.outputs().next() {
        Some(o) => o.clone(),
        None => return,
    };

    let output_geo = state.space.output_geometry(&output).unwrap();
    let delta = event.delta();

    // Update pointer location, clamping to output bounds
    let mut new_pos = state.pointer_location + delta;
    new_pos.x = new_pos.x.clamp(0.0, output_geo.size.w as f64 - 1.0);
    new_pos.y = new_pos.y.clamp(0.0, output_geo.size.h as f64 - 1.0);
    state.pointer_location = new_pos;
    state.needs_render = true;

    if handle_active_floating_grab(state, new_pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();

    let under = surface_under(state, new_pos);

    pointer.motion(
        state,
        under.clone(),
        &MotionEvent {
            location: new_pos,
            serial,
            time: Event::time_msec(&event),
        },
    );

    // Emit relative motion for clients that care (e.g. games)
    pointer.relative_motion(
        state,
        under.clone(),
        &RelativeMotionEvent {
            delta,
            delta_unaccel: event.delta_unaccel(),
            utime: Event::time(&event),
        },
    );

    // Focus follows mouse — only change focus when the surface changes.
    // Never steal focus away from a layer-shell surface (e.g. wofi).
    if state.config.focus_follows_mouse && !layer_surface_has_keyboard_focus(state) {
        if let Some((surface, _)) = under {
            let keyboard = state.seat.get_keyboard().unwrap();
            let already_focused = keyboard
                .current_focus()
                .as_ref()
                .map(|f| *f == surface)
                .unwrap_or(false);
            if !already_focused && surface_accepts_keyboard_focus(state, &surface) {
                keyboard.set_focus(state, Some(surface), serial);
            }
        }
    }

    // Update the compositor-driven cursor shape (border resize arrows, etc.).
    state.refresh_compositor_cursor();
}

fn handle_pointer_motion_absolute<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerMotionAbsoluteEvent,
) {
    let output = match state.space.outputs().next() {
        Some(o) => o.clone(),
        None => return,
    };

    let output_geo = state.space.output_geometry(&output).unwrap();
    let pos = event.position_transformed(output_geo.size);

    state.pointer_location = pos;
    state.needs_render = true;

    if handle_active_floating_grab(state, pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();

    let under = surface_under(state, pos);

    pointer.motion(
        state,
        under.clone(),
        &MotionEvent {
            location: pos,
            serial,
            time: Event::time_msec(&event),
        },
    );

    // Focus follows mouse — only change focus when the surface changes.
    // Never steal focus away from a layer-shell surface (e.g. wofi).
    if state.config.focus_follows_mouse && !layer_surface_has_keyboard_focus(state) {
        if let Some((surface, _)) = under {
            let keyboard = state.seat.get_keyboard().unwrap();
            let already_focused = keyboard
                .current_focus()
                .as_ref()
                .map(|f| *f == surface)
                .unwrap_or(false);
            if !already_focused && surface_accepts_keyboard_focus(state, &surface) {
                keyboard.set_focus(state, Some(surface), serial);
            }
        }
    }

    // Update the compositor-driven cursor shape (border resize arrows, etc.).
    state.refresh_compositor_cursor();
}

fn handle_pointer_button<I: InputBackend>(state: &mut Beewm, event: I::PointerButtonEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let btn_state = event.state();

    // Super + LMB press on a floating window → start move grab.
    if button == BTN_LEFT && btn_state == ButtonState::Pressed {
        if try_start_move_grab(state) {
            return;
        }
    }

    // LMB release → end move grab.
    if button == BTN_LEFT && btn_state == ButtonState::Released {
        if state.move_grab.take().is_some() {
            // Grab ended; restore compositor cursor (was Grabbing during drag).
            state.refresh_compositor_cursor();
            // Don't forward the release to the client.
            return;
        }
    }

    // Super + RMB press on a floating window → start resize grab.
    if button == BTN_RIGHT && btn_state == ButtonState::Pressed {
        if try_start_resize_grab(state) {
            return;
        }
    }

    // RMB release → end resize grab.
    if button == BTN_RIGHT && btn_state == ButtonState::Released {
        if finish_resize_grab(state) {
            return;
        }
    }

    let pointer = state.seat.get_pointer().unwrap();
    pointer.button(
        state,
        &ButtonEvent {
            button,
            state: btn_state,
            serial,
            time: Event::time_msec(&event),
        },
    );
}

fn handle_pointer_axis<I: InputBackend>(state: &mut Beewm, event: I::PointerAxisEvent) {
    let pointer = state.seat.get_pointer().unwrap();

    let source = event.source();
    let horizontal_amount = event.amount(Axis::Horizontal);
    let vertical_amount = event.amount(Axis::Vertical);
    let horizontal_amount_v120 = event.amount_v120(Axis::Horizontal);
    let vertical_amount_v120 = event.amount_v120(Axis::Vertical);

    let mut frame = AxisFrame::new(Event::time_msec(&event)).source(source);

    if let Some(amount) = horizontal_amount {
        if amount != 0.0 {
            frame = frame.value(Axis::Horizontal, amount);
            if let Some(discrete) = horizontal_amount_v120 {
                frame = frame.v120(Axis::Horizontal, discrete as i32);
            }
        } else if source == AxisSource::Finger {
            frame = frame.stop(Axis::Horizontal);
        }
    } else if let Some(discrete) = horizontal_amount_v120 {
        frame = frame.value(Axis::Horizontal, discrete as f64 * 3.0 / 120.0);
        frame = frame.v120(Axis::Horizontal, discrete as i32);
    }

    if let Some(amount) = vertical_amount {
        if amount != 0.0 {
            frame = frame.value(Axis::Vertical, amount);
            if let Some(discrete) = vertical_amount_v120 {
                frame = frame.v120(Axis::Vertical, discrete as i32);
            }
        } else if source == AxisSource::Finger {
            frame = frame.stop(Axis::Vertical);
        }
    } else if let Some(discrete) = vertical_amount_v120 {
        frame = frame.value(Axis::Vertical, discrete as f64 * 3.0 / 120.0);
        frame = frame.v120(Axis::Vertical, discrete as i32);
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

fn handle_active_floating_grab(state: &mut Beewm, pointer: Point<f64, Logical>) -> bool {
    if let Some(grab) = state.move_grab.clone() {
        apply_move_grab(state, &grab, pointer);
        return true;
    }

    if let Some(grab) = state.resize_grab.clone() {
        apply_resize_grab(state, &grab, pointer);
        return true;
    }

    false
}

fn apply_move_grab(state: &mut Beewm, grab: &MoveGrab, pointer: Point<f64, Logical>) {
    let dx = (pointer.x - grab.start_pointer.x) as i32;
    let dy = (pointer.y - grab.start_pointer.y) as i32;
    let new_window_pos = Point::from((grab.start_window_pos.x + dx, grab.start_window_pos.y + dy));
    state
        .space
        .map_element(grab.window.clone(), new_window_pos, true);
    if let Some(root) = Beewm::window_root_surface(&grab.window) {
        state.floating_windows.insert(root, new_window_pos);
    }
}

fn apply_resize_grab(state: &mut Beewm, grab: &ResizeGrab, pointer: Point<f64, Logical>) {
    let (new_window_pos, new_window_size) = resized_window_geometry(grab, pointer);
    state
        .space
        .map_element(grab.window.clone(), new_window_pos, true);

    if let Some(root) = Beewm::window_root_surface(&grab.window) {
        state.floating_windows.insert(root, new_window_pos);
    }

    if let Some(toplevel) = grab.window.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(Size::from((new_window_size.w, new_window_size.h)));
        });
        toplevel.send_pending_configure();
    }

    if let Some(active_grab) = state.resize_grab.as_mut() {
        active_grab.current_window_pos = new_window_pos;
        active_grab.current_window_size = new_window_size;
    }
}

fn try_start_move_grab(state: &mut Beewm) -> bool {
    let Some(window) = floating_window_under_pointer_with_logo(state) else {
        return false;
    };

    let Some(window_geo) = state.space.element_geometry(&window) else {
        return false;
    };

    focus_and_raise_window(state, &window);
    state.move_grab = Some(MoveGrab {
        window,
        start_pointer: state.pointer_location,
        start_window_pos: window_geo.loc,
    });
    state.refresh_compositor_cursor();
    true
}

fn try_start_resize_grab(state: &mut Beewm) -> bool {
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

    state.resize_grab = Some(ResizeGrab {
        window,
        start_pointer: state.pointer_location,
        start_window_pos: window_geo.loc,
        start_window_size: window_geo.size,
        edges,
        current_window_pos: window_geo.loc,
        current_window_size: window_geo.size,
    });
    state.refresh_compositor_cursor();
    true
}

fn finish_resize_grab(state: &mut Beewm) -> bool {
    let Some(grab) = state.resize_grab.take() else {
        return false;
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
        state.floating_windows.insert(root, grab.current_window_pos);
    }
    state.refresh_compositor_cursor();
    true
}

fn floating_window_under_pointer_with_logo(state: &mut Beewm) -> Option<smithay::desktop::Window> {
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

fn focus_and_raise_window(state: &mut Beewm, window: &smithay::desktop::Window) {
    if let Some(toplevel) = window.toplevel() {
        state.set_keyboard_focus(Some(toplevel.wl_surface().clone()));
    }
    state.space.raise_element(window, true);
    state.needs_render = true;
}

fn resize_edges_for_pointer(
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

fn resized_window_geometry_from_start(
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

#[cfg(test)]
mod tests {
    use super::{
        ResizeEdges, ResizeHorizontalEdge, ResizeVerticalEdge, resize_edges_for_pointer,
        resized_window_geometry_from_start,
    };
    use smithay::utils::{Logical, Point, Size};

    #[test]
    fn resize_edges_use_the_window_center_as_the_anchor_split() {
        let edges = resize_edges_for_pointer(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 200)),
            Point::<f64, Logical>::from((120.0, 220.0)),
        );
        assert_eq!(
            edges,
            ResizeEdges {
                horizontal: ResizeHorizontalEdge::Left,
                vertical: ResizeVerticalEdge::Top,
            }
        );

        let edges = resize_edges_for_pointer(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 200)),
            Point::<f64, Logical>::from((399.0, 399.0)),
        );
        assert_eq!(
            edges,
            ResizeEdges {
                horizontal: ResizeHorizontalEdge::Right,
                vertical: ResizeVerticalEdge::Bottom,
            }
        );
    }

    #[test]
    fn resizing_from_the_bottom_right_grows_width_and_height_only() {
        let (pos, size) = resized_window_geometry_from_start(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 150)),
            Point::<f64, Logical>::from((400.0, 350.0)),
            Point::<f64, Logical>::from((460.0, 390.0)),
            ResizeEdges {
                horizontal: ResizeHorizontalEdge::Right,
                vertical: ResizeVerticalEdge::Bottom,
            },
        );
        assert_eq!(pos, Point::from((100, 200)));
        assert_eq!(size, Size::from((360, 190)));
    }

    #[test]
    fn resizing_from_the_top_left_keeps_the_bottom_right_corner_fixed() {
        let (pos, size) = resized_window_geometry_from_start(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 150)),
            Point::<f64, Logical>::from((100.0, 200.0)),
            Point::<f64, Logical>::from((70.0, 170.0)),
            ResizeEdges {
                horizontal: ResizeHorizontalEdge::Left,
                vertical: ResizeVerticalEdge::Top,
            },
        );
        assert_eq!(pos, Point::from((70, 170)));
        assert_eq!(size, Size::from((330, 180)));
    }

    #[test]
    fn resizing_from_left_and_top_clamps_at_one_pixel() {
        let (pos, size) = resized_window_geometry_from_start(
            Point::<i32, Logical>::from((100, 200)),
            Size::<i32, Logical>::from((300, 150)),
            Point::<f64, Logical>::from((100.0, 200.0)),
            Point::<f64, Logical>::from((500.0, 500.0)),
            ResizeEdges {
                horizontal: ResizeHorizontalEdge::Left,
                vertical: ResizeVerticalEdge::Top,
            },
        );
        assert_eq!(pos, Point::from((399, 349)));
        assert_eq!(size, Size::from((1, 1)));
    }
}
