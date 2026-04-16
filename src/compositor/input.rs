use crate::config::Action;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::session::Session;
use smithay::desktop::{layer_map_for_output, WindowSurfaceType};
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer as WlrLayer, LayerSurfaceCachedState,
};

use super::commands::spawn_shell_command;
use super::state::{Beewm, MoveGrab};

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
            if let Err(e) = spawn_shell_command(&cmd, state.sanitize_display_for_children) {
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

    // If we are in a floating-window move grab, move the window and skip
    // normal pointer dispatch so the client doesn't see spurious leave events.
    if let Some(ref grab) = state.move_grab.clone() {
        let dx = (new_pos.x - grab.start_pointer.x) as i32;
        let dy = (new_pos.y - grab.start_pointer.y) as i32;
        let new_win_pos = smithay::utils::Point::from((
            grab.start_window_pos.x + dx,
            grab.start_window_pos.y + dy,
        ));
        state
            .space
            .map_element(grab.window.clone(), new_win_pos, true);
        // Update the stored floating position so relayout doesn't revert it.
        if let Some(toplevel) = grab.window.toplevel() {
            let root = {
                let mut r = toplevel.wl_surface().clone();
                while let Some(p) = smithay::wayland::compositor::get_parent(&r) {
                    r = p;
                }
                r
            };
            state.floating_windows.insert(root, new_win_pos);
        }
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

    // BTN_LEFT = 0x110 = 272
    const BTN_LEFT: u32 = 0x110;

    // Super + LMB press on a floating window → start move grab.
    if button == BTN_LEFT && btn_state == ButtonState::Pressed {
        let keyboard = state.seat.get_keyboard().unwrap();
        let modifiers = keyboard.modifier_state();
        if modifiers.logo {
            if let Some((surface, _)) = surface_under(state, state.pointer_location) {
                if let Some(window) = state.mapped_window_for_surface(&surface) {
                    let root = {
                        let mut r = surface.clone();
                        while let Some(p) = smithay::wayland::compositor::get_parent(&r) {
                            r = p;
                        }
                        r
                    };
                    if state.floating_windows.contains_key(&root) {
                        let win_pos = state
                            .space
                            .element_geometry(&window)
                            .map(|g| g.loc)
                            .unwrap_or_default();
                        state.move_grab = Some(MoveGrab {
                            window: window.clone(),
                            start_pointer: state.pointer_location,
                            start_window_pos: win_pos,
                        });
                        // Don't forward this button press to the client.
                        return;
                    }
                }
            }
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
