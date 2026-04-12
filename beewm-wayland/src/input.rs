use beewm_core::config::Action;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Session;
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState};
use smithay::input::keyboard::xkb;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::state::Beewm;

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
                if raw >= 0x1008FE01 && raw <= 0x1008FE0C {
                    let vt = (raw - 0x1008FE01 + 1) as i32;
                    if let Some(ref mut session) = state.session {
                        if let Some(session) = session.downcast_mut::<LibSeatSession>() {
                            let _ = session.change_vt(vt);
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
    let keysym = keysym_handle.modified_sym();

    for bind in &state.config.keybinds {
        let mut want_super = false;
        let mut want_shift = false;
        let mut want_ctrl = false;
        let mut want_alt = false;

        for m in &bind.modifiers {
            match m.to_lowercase().as_str() {
                "super" | "mod4" | "logo" => want_super = true,
                "shift" => want_shift = true,
                "ctrl" | "control" => want_ctrl = true,
                "alt" | "mod1" => want_alt = true,
                _ => {}
            }
        }

        if modifiers.logo != want_super
            || modifiers.shift != want_shift
            || modifiers.ctrl != want_ctrl
            || modifiers.alt != want_alt
        {
            continue;
        }

        // Match by xkb keysym name
        let bind_keysym = xkb::keysym_from_name(&bind.key, xkb::KEYSYM_NO_FLAGS);
        if bind_keysym == keysym {
            return Some(bind.action.clone());
        }

        // Case-insensitive fallback
        let bind_keysym_ci = xkb::keysym_from_name(&bind.key, xkb::KEYSYM_CASE_INSENSITIVE);
        if bind_keysym_ci == keysym {
            return Some(bind.action.clone());
        }
    }

    None
}

fn execute_action(state: &mut Beewm, action: Action) {
    match action {
        Action::Spawn(cmd) => {
            tracing::info!("Spawning: {}", cmd);
            if let Err(e) = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .spawn()
            {
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
            if let Some(window) = focused_window(state) {
                if let Some(toplevel) = window.toplevel() {
                    toplevel.send_close();
                }
            }
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

fn focused_window(state: &Beewm) -> Option<&smithay::desktop::Window> {
    let ws = &state.workspaces[state.active_workspace];
    let idx = ws.focused_idx?;
    state.workspace_windows[state.active_workspace].get(idx)
}

fn focus_current_window(state: &mut Beewm) {
    let ws = &state.workspaces[state.active_workspace];
    let idx = match ws.focused_idx {
        Some(i) => i,
        None => return,
    };

    let window = match state.workspace_windows[state.active_workspace].get(idx).cloned() {
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
    state.cursor_serial = state.cursor_serial.wrapping_add(1);

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();

    let under: Option<(WlSurface, smithay::utils::Point<f64, smithay::utils::Logical>)> = state
        .space
        .element_under(new_pos)
        .and_then(|(window, loc)| {
            let surface = window.toplevel()?.wl_surface().clone();
            Some((surface, loc.to_f64()))
        });

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

    // Focus follows mouse
    if state.config.focus_follows_mouse {
        if let Some((surface, _)) = under {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.set_focus(state, Some(surface), serial);
        }
    }
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
    state.cursor_serial = state.cursor_serial.wrapping_add(1);

    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();

    let under: Option<(WlSurface, smithay::utils::Point<f64, smithay::utils::Logical>)> = state
        .space
        .element_under(pos)
        .and_then(|(window, loc)| {
            let surface = window.toplevel()?.wl_surface().clone();
            Some((surface, loc.to_f64()))
        });

    pointer.motion(
        state,
        under.clone(),
        &MotionEvent {
            location: pos,
            serial,
            time: Event::time_msec(&event),
        },
    );

    // Focus follows mouse
    if state.config.focus_follows_mouse {
        if let Some((surface, _)) = under {
            let keyboard = state.seat.get_keyboard().unwrap();
            keyboard.set_focus(state, Some(surface), serial);
        }
    }
}

fn handle_pointer_button<I: InputBackend>(state: &mut Beewm, event: I::PointerButtonEvent) {
    let serial = SERIAL_COUNTER.next_serial();
    let pointer = state.seat.get_pointer().unwrap();

    pointer.button(
        state,
        &ButtonEvent {
            button: event.button_code(),
            state: event.state(),
            serial,
            time: Event::time_msec(&event),
        },
    );
}

fn handle_pointer_axis<I: InputBackend>(state: &mut Beewm, event: I::PointerAxisEvent) {
    let pointer = state.seat.get_pointer().unwrap();

    let source = event.source();
    let horizontal_amount = event
        .amount(Axis::Horizontal)
        .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 3.0 / 120.0);
    let vertical_amount = event
        .amount(Axis::Vertical)
        .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 3.0 / 120.0);

    let mut frame = AxisFrame::new(Event::time_msec(&event)).source(source);

    if horizontal_amount != 0.0 {
        frame = frame.value(Axis::Horizontal, horizontal_amount);
        if let Some(discrete) = event.amount_v120(Axis::Horizontal) {
            frame = frame.v120(Axis::Horizontal, discrete as i32);
        }
    }
    if vertical_amount != 0.0 {
        frame = frame.value(Axis::Vertical, vertical_amount);
        if let Some(discrete) = event.amount_v120(Axis::Vertical) {
            frame = frame.v120(Axis::Vertical, discrete as i32);
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}
