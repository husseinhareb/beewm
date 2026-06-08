use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::with_states;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};
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
    try_start_resize_grab, try_start_tiled_resize_grab, try_start_tiled_swap_grab,
};
use super::{BTN_LEFT, BTN_RIGHT, layer_surface_has_keyboard_focus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftButtonReleaseAction {
    FinishMove,
    FinishTiledSwap,
    ForwardToClient,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeftButtonGrabKind {
    Move,
    TiledSwap,
    Other,
}

fn left_button_release_action(grab_kind: Option<LeftButtonGrabKind>) -> LeftButtonReleaseAction {
    match grab_kind {
        Some(LeftButtonGrabKind::Move) => LeftButtonReleaseAction::FinishMove,
        Some(LeftButtonGrabKind::TiledSwap) => LeftButtonReleaseAction::FinishTiledSwap,
        Some(LeftButtonGrabKind::Other) | None => LeftButtonReleaseAction::ForwardToClient,
    }
}

pub(in crate::compositor) fn surface_under(
    state: &Beewm,
    pos: Point<f64, Logical>,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    let output = state.output_under_point(pos)?;

    // While locked, the pointer may only ever reach the lock surface — never a
    // window or layer-shell surface underneath. The lock surface is anchored at
    // the output origin and covers it fully.
    if state.locked {
        let lock = state.lock_surfaces.get(&output)?;
        if !lock.alive() {
            return None;
        }
        let output_loc = state.space.output_geometry(&output)?.loc.to_f64();
        return Some((lock.wl_surface().clone(), output_loc));
    }

    let fullscreen_active = state.screen_owned_by_window();

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

fn keyboard_focus_target_under_pointer(
    state: &Beewm,
    surface: &WlSurface,
) -> Option<crate::compositor::focus_target::KeyboardFocusTarget> {
    if let Some(window) = state.mapped_window_for_surface(surface) {
        // Override-redirect X11 windows (menus, tooltips, dropdowns) must never
        // receive keyboard focus. They are self-managed and not expected to be
        // focused by the WM. Focusing them triggers X11Surface::leave() on the
        // previously focused window, which calls set_input_focus(NONE) and
        // generates a FocusOut event — causing apps like Steam to dismiss their
        // popup menus as the user moves the cursor toward them.
        if window
            .x11_surface()
            .map(|x11| x11.is_override_redirect())
            .unwrap_or(false)
        {
            return None;
        }
        return crate::compositor::focus_target::KeyboardFocusTarget::from_window(&window);
    }

    surface_accepts_keyboard_focus(state, surface).then(|| surface.clone().into())
}

pub(super) fn handle_pointer_motion<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerMotionEvent,
) {
    state.notify_activity();
    let output = match state.focused_output() {
        Some(o) => o,
        None => return,
    };

    let output_geo = state.space.output_geometry(&output).unwrap();
    let delta = event.delta();

    let mut new_pos = state.pointer_location + delta;
    new_pos.x = new_pos.x.clamp(0.0, output_geo.size.w as f64 - 1.0);
    new_pos.y = new_pos.y.clamp(0.0, output_geo.size.h as f64 - 1.0);

    // If the surface currently under the cursor has an active pointer lock, keep the
    // cursor fixed and only deliver relative motion to the game.
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let under_cursor = surface_under(state, state.pointer_location);
    let is_locked = under_cursor
        .as_ref()
        .map(|(surface, _)| {
            with_pointer_constraint(surface, &pointer, |constraint| {
                constraint
                    .map(|c| c.is_active() && matches!(*c, PointerConstraint::Locked(_)))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    if is_locked {
        let serial = SERIAL_COUNTER.next_serial();
        let Some(pointer) = state.seat.get_pointer() else {
            return;
        };
        // Smithay's PointerInternal::relative_motion ignores the focus parameter
        // and delivers only to its internal self.focus, which is only set by
        // pointer.motion() calls. Call motion() first at the current (fixed) cursor
        // position so smithay's internal focus is kept correct, then deliver the
        // relative delta. Without this, relative_motion events are silently dropped
        // when smithay's internal focus is None or stale.
        pointer.motion(
            state,
            under_cursor.clone(),
            &MotionEvent {
                location: state.pointer_location,
                serial,
                time: Event::time_msec(&event),
            },
        );
        pointer.relative_motion(
            state,
            under_cursor,
            &RelativeMotionEvent {
                delta,
                delta_unaccel: event.delta_unaccel(),
                utime: Event::time(&event),
            },
        );
        pointer.frame(state);
        return;
    }

    // Only schedule a render when the cursor crossed an integer-pixel
    // boundary. High-DPI mice send sub-pixel events at >1 kHz; rendering on
    // every event burned CPU rebuilding the element list for plane updates
    // that the DRM driver coalesced anyway.
    if new_pos.x.floor() != state.pointer_location.x.floor()
        || new_pos.y.floor() != state.pointer_location.y.floor()
    {
        state.needs_render = true;
    }
    state.pointer_location = new_pos;

    if handle_active_grab(state, new_pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
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
        && let Some((surface, _)) = under
    {
        let Some(target) = keyboard_focus_target_under_pointer(state, &surface) else {
            state.refresh_compositor_cursor();
            return;
        };
        let Some(keyboard) = state.seat.get_keyboard() else {
            return;
        };
        let already_focused = keyboard
            .current_focus()
            .as_ref()
            .map(|f| *f == target)
            .unwrap_or(false);
        if !already_focused {
            keyboard.set_focus(state, Some(target), serial);
        }
    }

    state.refresh_compositor_cursor();
}

pub(super) fn handle_pointer_motion_absolute<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerMotionAbsoluteEvent,
) {
    state.notify_activity();
    let output = match state.focused_output() {
        Some(o) => o,
        None => return,
    };

    let output_geo = state.space.output_geometry(&output).unwrap();
    let pos = event.position_transformed(output_geo.size);

    if pos.x.floor() != state.pointer_location.x.floor()
        || pos.y.floor() != state.pointer_location.y.floor()
    {
        state.needs_render = true;
    }
    state.pointer_location = pos;

    if handle_active_grab(state, pos) {
        return;
    }

    let serial = SERIAL_COUNTER.next_serial();
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
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
        && let Some((surface, _)) = under
    {
        let Some(target) = keyboard_focus_target_under_pointer(state, &surface) else {
            state.refresh_compositor_cursor();
            return;
        };
        let Some(keyboard) = state.seat.get_keyboard() else {
            return;
        };
        let already_focused = keyboard
            .current_focus()
            .as_ref()
            .map(|f| *f == target)
            .unwrap_or(false);
        if !already_focused {
            keyboard.set_focus(state, Some(target), serial);
        }
    }

    state.refresh_compositor_cursor();
}

pub(super) fn handle_pointer_button<I: InputBackend>(
    state: &mut Beewm,
    event: I::PointerButtonEvent,
) {
    state.notify_activity();

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
        let grab_kind = match state.active_grab.as_ref() {
            Some(ActiveGrab::Move(_)) => Some(LeftButtonGrabKind::Move),
            Some(ActiveGrab::TiledSwap(_)) => Some(LeftButtonGrabKind::TiledSwap),
            Some(_) => Some(LeftButtonGrabKind::Other),
            None => None,
        };

        match left_button_release_action(grab_kind) {
            LeftButtonReleaseAction::FinishMove => {
                state.active_grab = None;
                state.refresh_compositor_cursor();
                return;
            }
            LeftButtonReleaseAction::FinishTiledSwap => {
                if finish_tiled_swap_grab(state) {
                    return;
                }
            }
            LeftButtonReleaseAction::ForwardToClient => {}
        }
    }

    if button == BTN_RIGHT && btn_state == ButtonState::Pressed {
        if try_start_resize_grab(state) {
            return;
        }
        if try_start_tiled_resize_grab(state) {
            return;
        }
    }

    if button == BTN_RIGHT && btn_state == ButtonState::Released && finish_resize_grab(state) {
        return;
    }

    // Click-to-focus: on any button press with no compositor-level grab active,
    // focus whatever is under the pointer. This also dismisses popup grabs
    // (via set_keyboard_focus) when the user clicks outside a popup.
    if btn_state == ButtonState::Pressed && state.active_grab.is_none() {
        let pos = state.pointer_location;
        if let Some((surface, _)) = surface_under(state, pos)
            && let Some(target) = keyboard_focus_target_under_pointer(state, &surface)
        {
            let x11_target = match &target {
                crate::compositor::focus_target::KeyboardFocusTarget::X11(x11) => Some(x11.clone()),
                crate::compositor::focus_target::KeyboardFocusTarget::Wayland(_) => None,
            };
            if let Some(x11) = x11_target {
                // Keep XWayland's stacking in sync even when focus was
                // already on this window. Steam can keep the focus border
                // while an old sibling remains above it in the X server.
                state.raise_x11_window(&x11);
            }

            let already_focused = state
                .seat
                .get_keyboard()
                .and_then(|kb| kb.current_focus())
                .map(|f| f == target)
                .unwrap_or(false);
            if !already_focused {
                state.set_keyboard_focus_target(Some(target));
            }
        }
    }

    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
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

pub(super) fn handle_pointer_axis<I: InputBackend>(state: &mut Beewm, event: I::PointerAxisEvent) {
    state.notify_activity();
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };

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
        frame = frame.value(Axis::Horizontal, discrete * 3.0 / 120.0);
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
        frame = frame.value(Axis::Vertical, discrete * 3.0 / 120.0);
        frame = frame.v120(Axis::Vertical, discrete as i32);
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

#[cfg(test)]
mod tests {
    use super::{LeftButtonGrabKind, LeftButtonReleaseAction, left_button_release_action};

    #[test]
    fn left_release_routes_tiled_swap_grabs_to_swap_completion() {
        assert_eq!(
            left_button_release_action(Some(LeftButtonGrabKind::TiledSwap)),
            LeftButtonReleaseAction::FinishTiledSwap
        );
    }

    #[test]
    fn left_release_only_clears_move_grabs_directly() {
        assert_eq!(
            left_button_release_action(Some(LeftButtonGrabKind::Move)),
            LeftButtonReleaseAction::FinishMove
        );
        assert_eq!(
            left_button_release_action(Some(LeftButtonGrabKind::Other)),
            LeftButtonReleaseAction::ForwardToClient
        );
        assert_eq!(
            left_button_release_action(None),
            LeftButtonReleaseAction::ForwardToClient
        );
    }
}
