use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer as WlrLayer, LayerSurfaceCachedState,
};

use crate::compositor::layering::{
    layers_hit_tested_after_windows, layers_hit_tested_before_windows,
};
use crate::compositor::state::Beewm;
use crate::compositor::types::ActiveGrab;

use super::grab::{
    finish_resize_grab, finish_tiled_swap_grab, handle_active_grab, try_start_move_grab,
    try_start_resize_grab, try_start_tiled_swap_grab,
};
use super::{BTN_LEFT, BTN_RIGHT, layer_surface_has_keyboard_focus};

pub(in crate::compositor) fn surface_under(
    state: &Beewm,
    pos: Point<f64, Logical>,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    let output = state
        .space
        .output_under(pos)
        .next()
        .cloned()
        .or_else(|| state.space.outputs().next().cloned())?;
    let fullscreen_active = state.fullscreen_window.is_some();

    let layer_hit = |layer: WlrLayer| -> Option<(WlSurface, Point<f64, Logical>)> {
        let layer_map = layer_map_for_output(&output);
        let layer_surface = layer_map.layer_under(layer, pos)?.clone();
        let layer_geometry = layer_map.layer_geometry(&layer_surface)?;
        let local = pos - layer_geometry.loc.to_f64();
        let (surface, surface_loc) = layer_surface.surface_under(local, WindowSurfaceType::ALL)?;
        Some((surface, layer_geometry.loc.to_f64() + surface_loc.to_f64()))
    };

    for &layer in layers_hit_tested_before_windows(fullscreen_active) {
        if let Some(hit) = layer_hit(layer) {
            return Some(hit);
        }
    }

    if let Some(hit) = state.space.element_under(pos).and_then(|(window, loc)| {
        let local = pos - loc.to_f64();
        window
            .surface_under(local, WindowSurfaceType::ALL)
            .map(|(surface, surface_loc)| (surface, loc.to_f64() + surface_loc.to_f64()))
    }) {
        return Some(hit);
    }

    for &layer in layers_hit_tested_after_windows(fullscreen_active) {
        if let Some(hit) = layer_hit(layer) {
            return Some(hit);
        }
    }

    None
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

fn keyboard_focus_target_under_pointer(state: &Beewm, surface: &WlSurface) -> Option<WlSurface> {
    if let Some(window) = state.mapped_window_for_surface(surface) {
        return window
            .toplevel()
            .map(|toplevel| toplevel.wl_surface().clone());
    }

    surface_accepts_keyboard_focus(state, surface).then(|| surface.clone())
}

pub(super) fn handle_pointer_motion<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerMotionEvent,
) {
    let output = match state.space.outputs().next() {
        Some(o) => o.clone(),
        None => return,
    };

    let output_geo = state.space.output_geometry(&output).unwrap();
    let delta = event.delta();

    let mut new_pos = state.pointer_location + delta;
    new_pos.x = new_pos.x.clamp(0.0, output_geo.size.w as f64 - 1.0);
    new_pos.y = new_pos.y.clamp(0.0, output_geo.size.h as f64 - 1.0);
    state.pointer_location = new_pos;
    state.needs_render = true;

    if handle_active_grab(state, new_pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();
    let pointer_is_grabbed = pointer.is_grabbed();

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
    pointer.frame(state);

    pointer.relative_motion(
        state,
        under.clone(),
        &RelativeMotionEvent {
            delta,
            delta_unaccel: event.delta_unaccel(),
            utime: Event::time(&event),
        },
    );

    if state.config.focus_follows_mouse
        && !layer_surface_has_keyboard_focus(state)
        && !pointer_is_grabbed
    {
        if let Some((surface, _)) = under {
            let Some(target) = keyboard_focus_target_under_pointer(state, &surface) else {
                state.refresh_compositor_cursor();
                return;
            };
            let keyboard = state.seat.get_keyboard().unwrap();
            let already_focused = keyboard
                .current_focus()
                .as_ref()
                .map(|f| *f == target)
                .unwrap_or(false);
            if !already_focused {
                keyboard.set_focus(state, Some(target), serial);
            }
        }
    }

    state.refresh_compositor_cursor();
}

pub(super) fn handle_pointer_motion_absolute<I: InputBackend>(
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

    if handle_active_grab(state, pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();
    let pointer_is_grabbed = pointer.is_grabbed();

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
    pointer.frame(state);

    if state.config.focus_follows_mouse
        && !layer_surface_has_keyboard_focus(state)
        && !pointer_is_grabbed
    {
        if let Some((surface, _)) = under {
            let Some(target) = keyboard_focus_target_under_pointer(state, &surface) else {
                state.refresh_compositor_cursor();
                return;
            };
            let keyboard = state.seat.get_keyboard().unwrap();
            let already_focused = keyboard
                .current_focus()
                .as_ref()
                .map(|f| *f == target)
                .unwrap_or(false);
            if !already_focused {
                keyboard.set_focus(state, Some(target), serial);
            }
        }
    }

    state.refresh_compositor_cursor();
}

pub(super) fn handle_pointer_button<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerButtonEvent,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let btn_state = event.state();

    if button == BTN_LEFT && btn_state == ButtonState::Pressed {
        if try_start_move_grab(state) {
            return;
        }
        if try_start_tiled_swap_grab(state) {
            return;
        }
    }

    if button == BTN_LEFT && btn_state == ButtonState::Released {
        if let Some(ActiveGrab::Move(_)) = state.active_grab.take() {
            state.refresh_compositor_cursor();
            return;
        }
        if finish_tiled_swap_grab(state) {
            return;
        }
    }

    if button == BTN_RIGHT && btn_state == ButtonState::Pressed {
        if try_start_resize_grab(state) {
            return;
        }
    }

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
    pointer.frame(state);
}

pub(super) fn handle_pointer_axis<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerAxisEvent,
) {
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
