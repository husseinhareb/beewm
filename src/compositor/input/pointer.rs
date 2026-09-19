use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, PointerAxisEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::desktop::{WindowSurfaceType, layer_map_for_output};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
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

    // A `LayerMap` is arranged in *output-local* coordinates (origin at the
    // output's top-left), while `pos` is global `Space` coordinates. Hit tests
    // and the surface locations we hand back have to be translated by the
    // output's origin, or every layer surface on a secondary output is unclickable.
    let output_loc = state
        .space
        .output_geometry(&output)
        .map(|geo| geo.loc.to_f64())
        .unwrap_or_default();
    let layer_hit = |layer: WlrLayer| -> Option<(WlSurface, Point<f64, Logical>)> {
        let local_pos = pos - output_loc;
        let layer_map = layer_map_for_output(&output);
        let layer_surface = layer_map.layer_under(layer, local_pos)?.clone();
        let layer_geometry = layer_map.layer_geometry(&layer_surface)?;
        let local = local_pos - layer_geometry.loc.to_f64();
        let (surface, surface_loc) = layer_surface.surface_under(local, WindowSurfaceType::ALL)?;
        Some((
            surface,
            output_loc + layer_geometry.loc.to_f64() + surface_loc.to_f64(),
        ))
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
    let delta = event.delta();

    let mut new_pos = state.pointer_location + delta;
    // Outputs live side by side in one global coordinate space, so a position
    // that lands on *any* output is valid — that is how the cursor crosses onto
    // a second monitor. Only when it lands nowhere (the gap between mismatched
    // outputs, or past the outermost edge) do we pull it back into the output
    // it came from, whose geometry starts at that output's own origin.
    if state.space.output_under(new_pos).next().is_none() {
        let Some(output) = state.output_under_point(state.pointer_location) else {
            return;
        };
        let Some(output_geo) = state.space.output_geometry(&output) else {
            return;
        };
        new_pos = clamp_to_output(output_geo, new_pos);
    }

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

    // While the overview grid is up it owns the pointer: hovering a thumbnail
    // selects it and nothing reaches the clients behind it.
    if state.overview_pointer_moved(new_pos) {
        return;
    }

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
    let Some(output) = state.focused_output() else {
        return;
    };
    // An output that is registered but has no mode yet has no geometry; there is
    // nothing sensible to map the absolute position onto, so drop the event
    // rather than panic on the compositor thread.
    let Some(output_geo) = state.space.output_geometry(&output) else {
        return;
    };

    // `position_transformed` is relative to the output it was mapped onto;
    // `synthetic_motion_absolute` works in global Space coordinates.
    let pos = output_geo.loc.to_f64() + event.position_transformed(output_geo.size);
    synthetic_motion_absolute(state, pos, Event::time_msec(&event));
}

/// Keep `pos` inside `geo`. The far edge is excluded so the point still
/// hit-tests as being on the output rather than one pixel past it.
fn clamp_to_output(geo: Rectangle<i32, Logical>, pos: Point<f64, Logical>) -> Point<f64, Logical> {
    Point::from((
        pos.x
            .clamp(geo.loc.x as f64, (geo.loc.x + geo.size.w) as f64 - 1.0),
        pos.y
            .clamp(geo.loc.y as f64, (geo.loc.y + geo.size.h) as f64 - 1.0),
    ))
}

/// Inject an absolute pointer motion from a source with no backing
/// `InputBackend` event, e.g. a `wlr-virtual-pointer` client used by VNC or
/// other remote-control tools. `pos` is already in global compositor logical
/// coordinates.
pub(crate) fn synthetic_motion_absolute(state: &mut Beewm, pos: Point<f64, Logical>, time: u32) {
    state.notify_activity();

    // A source with no backing hardware event (wlr-virtual-pointer relative
    // motion) can walk the cursor arbitrarily far off-screen, where nothing
    // hit-tests and there is no way to bring it back. The hardware relative
    // path clamps for the same reason; do it here so every caller is covered.
    let pos = match state
        .output_under_point(pos)
        .and_then(|output| state.space.output_geometry(&output))
    {
        Some(geo) => clamp_to_output(geo, pos),
        None => pos,
    };

    if pos.x.floor() != state.pointer_location.x.floor()
        || pos.y.floor() != state.pointer_location.y.floor()
    {
        state.needs_render = true;
    }
    state.pointer_location = pos;

    if state.overview_pointer_moved(pos) {
        return;
    }

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
            time,
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
    synthetic_button(
        state,
        event.button_code(),
        event.state(),
        Event::time_msec(&event),
    );
}

/// Inject a button press/release from a source with no backing `InputBackend`
/// event, e.g. a `wlr-virtual-pointer` client used by VNC or other
/// remote-control tools.
pub(crate) fn synthetic_button(state: &mut Beewm, button: u32, btn_state: ButtonState, time: u32) {
    state.notify_activity();

    let serial = SERIAL_COUNTER.next_serial();

    // A button going down cancels a *pending* overview, so holding Super to
    // start a `mod+drag` never turns into the grid, and picks the hovered
    // thumbnail when the grid is already up. The matching release is swallowed
    // too, so no client sees a release it never saw a press for.
    match btn_state {
        ButtonState::Pressed => {
            state.cancel_overview_hold();
            if state.overview_pointer_pressed() {
                state.overview_swallowed_button = Some(button);
                return;
            }
        }
        ButtonState::Released => {
            if state.overview_swallowed_button == Some(button) {
                state.overview_swallowed_button = None;
                return;
            }
        }
    }

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
            time,
        },
    );
    pointer.frame(state);
}

pub(super) fn handle_pointer_axis<I: InputBackend>(state: &mut Beewm, event: I::PointerAxisEvent) {
    state.notify_activity();
    if state.overview.is_some() {
        return;
    }
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

/// Inject a scroll/axis event from a source with no backing `InputBackend`
/// event, e.g. a `wlr-virtual-pointer` client used by VNC or other
/// remote-control tools. `horizontal`/`vertical` are wheel-style deltas.
pub(crate) fn synthetic_axis(state: &mut Beewm, horizontal: f64, vertical: f64, time: u32) {
    state.notify_activity();
    if state.overview.is_some() {
        return;
    }
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };

    let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
    if horizontal != 0.0 {
        frame = frame.value(Axis::Horizontal, horizontal);
    }
    if vertical != 0.0 {
        frame = frame.value(Axis::Vertical, vertical);
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}

#[cfg(test)]
mod tests {
    use super::{
        LeftButtonGrabKind, LeftButtonReleaseAction, clamp_to_output, left_button_release_action,
    };
    use smithay::utils::{Point, Rectangle};

    /// A wlr-virtual-pointer client can send an unbounded relative delta. Left
    /// unclamped the cursor walks off-screen, hit-tests nothing, and there is
    /// no way to bring it back.
    #[test]
    fn out_of_range_motion_is_pulled_back_onto_the_output() {
        let geo = Rectangle::new((1920, 0).into(), (1920, 1080).into());

        assert_eq!(
            clamp_to_output(geo, Point::from((-5000.0, 9000.0))),
            Point::from((1920.0, 1079.0))
        );
        assert_eq!(
            clamp_to_output(geo, Point::from((99999.0, -1.0))),
            Point::from((3839.0, 0.0))
        );
    }

    #[test]
    fn motion_already_on_the_output_is_untouched() {
        let geo = Rectangle::new((1920, 0).into(), (1920, 1080).into());
        let pos = Point::from((2000.5, 300.25));

        assert_eq!(clamp_to_output(geo, pos), pos);
    }

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
