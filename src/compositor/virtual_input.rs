//! Input-injection and output-listing globals for remote-control tools (e.g.
//! `wayvnc`).
//!
//! `beewm` already exposes `wlr-screencopy` for screen capture; this module
//! adds what a VNC session additionally needs: hand-written `zwp-virtual-keyboard-v1`
//! and `zwlr-virtual-pointer-v1` support, plus a minimal read-only
//! `zwlr-output-management-v1` (wayvnc queries it to enumerate outputs —
//! smithay ships the generated protocol bindings for all three but no
//! server-side handling, unlike screencopy).
//!
//! The virtual-keyboard manager is hand-written rather than using smithay's
//! own (which forwards keys straight to the focused client's `wl_keyboard`)
//! so that VNC-injected keys go through the same keybind-aware path as real
//! hardware keys — see `synthetic_key` — and beewm's own bindings (workspace
//! switch, spawn, etc.) work over VNC instead of only reaching whatever
//! client currently has focus.

use std::sync::Mutex;

use smithay::input::keyboard::xkb;
use smithay::output::Output;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_pointer::{
    Axis as WlAxis, ButtonState as WlButtonState,
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::{Logical, Point, Rectangle};

use super::input::{synthetic_axis, synthetic_button, synthetic_key, synthetic_motion_absolute};
use super::state::Beewm;

pub(crate) fn create_virtual_keyboard_manager_global<D>(display: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()>
        + Dispatch<ZwpVirtualKeyboardManagerV1, ()>
        + Dispatch<ZwpVirtualKeyboardV1, ()>
        + 'static,
{
    display.create_global::<D, ZwpVirtualKeyboardManagerV1, _>(1, ())
}

impl GlobalDispatch<ZwpVirtualKeyboardManagerV1, (), Beewm> for Beewm {
    fn bind(
        _state: &mut Beewm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { id, .. } => {
                data_init.init(id, ());
            }
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, (), Beewm> for Beewm {
    fn request(
        state: &mut Beewm,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            // Ignore the uploaded keymap: keys are interpreted with beewm's
            // own configured keymap, same as real hardware keys — see the
            // module doc comment for why.
            zwp_virtual_keyboard_v1::Request::Keymap { .. } => {}
            zwp_virtual_keyboard_v1::Request::Key {
                time,
                key,
                state: pressed,
            } => {
                let key_state = if pressed == 1 {
                    smithay::backend::input::KeyState::Pressed
                } else {
                    smithay::backend::input::KeyState::Released
                };
                synthetic_key(state, xkb::Keycode::new(key + 8), key_state, time);
            }
            // No-op: modifier state is tracked from the Key events themselves,
            // exactly as it is for a physical keyboard.
            zwp_virtual_keyboard_v1::Request::Modifiers { .. } => {}
            zwp_virtual_keyboard_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

const OUTPUT_MANAGER_VERSION: u32 = 3;

pub(crate) fn create_virtual_pointer_manager_global<D>(display: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<ZwlrVirtualPointerManagerV1, ()>
        + Dispatch<ZwlrVirtualPointerManagerV1, ()>
        + Dispatch<ZwlrVirtualPointerV1, VirtualPointerData>
        + 'static,
{
    display.create_global::<D, ZwlrVirtualPointerManagerV1, _>(2, ())
}

#[derive(Debug, Clone, Copy)]
enum PendingOp {
    Motion {
        dx: f64,
        dy: f64,
    },
    MotionAbsolute {
        x: u32,
        y: u32,
        x_extent: u32,
        y_extent: u32,
    },
    Button {
        button: u32,
        pressed: bool,
    },
    Axis {
        axis: WlAxis,
        value: f64,
        /// True when this came from `axis_discrete`, which per the protocol
        /// *extends* a matching `axis` event rather than adding to it.
        discrete: bool,
    },
}

/// Sum one frame's axis events into `(horizontal, vertical)` scroll deltas.
///
/// `axis_discrete` "allows the client to extend data normally sent using the
/// axis event with discrete value", so a client mirroring `wl_pointer` sends
/// both for a single scroll tick. Adding them together would scroll twice as
/// far, so a discrete value *replaces* the continuous one for that axis;
/// clients that send only one of the two are unaffected.
fn fold_axis(ops: &[PendingOp]) -> (f64, f64) {
    let (mut cont_h, mut cont_v, mut disc_h, mut disc_v) = (0.0, 0.0, 0.0, 0.0);
    for op in ops {
        let PendingOp::Axis {
            axis,
            value,
            discrete,
        } = op
        else {
            continue;
        };
        match (*axis, *discrete) {
            (WlAxis::HorizontalScroll, false) => cont_h += *value,
            (WlAxis::HorizontalScroll, true) => disc_h += *value,
            (WlAxis::VerticalScroll, false) => cont_v += *value,
            (WlAxis::VerticalScroll, true) => disc_v += *value,
            _ => {}
        }
    }
    (
        if disc_h != 0.0 { disc_h } else { cont_h },
        if disc_v != 0.0 { disc_v } else { cont_v },
    )
}

#[derive(Debug, Default)]
pub(crate) struct VirtualPointerData {
    /// The output this pointer was bound to by
    /// `create_virtual_pointer_with_output`, if any. `motion_absolute` is
    /// normalized against it rather than against whichever output happens to
    /// come first.
    output: Option<WlOutput>,
    pending: Mutex<Vec<PendingOp>>,
}

/// Map a normalized `x/x_extent`, `y/y_extent` position onto `geo`.
fn absolute_pos_in(
    geo: Rectangle<i32, Logical>,
    x: u32,
    y: u32,
    x_extent: u32,
    y_extent: u32,
) -> Point<f64, Logical> {
    let frac = |v: u32, extent: u32| {
        if extent > 0 {
            v as f64 / extent as f64
        } else {
            0.0
        }
    };
    Point::from((
        geo.loc.x as f64 + frac(x, x_extent) * geo.size.w as f64,
        geo.loc.y as f64 + frac(y, y_extent) * geo.size.h as f64,
    ))
}

fn resolve_absolute_pos(
    state: &Beewm,
    data: &VirtualPointerData,
    x: u32,
    y: u32,
    x_extent: u32,
    y_extent: u32,
) -> Point<f64, Logical> {
    let geo = data
        .output
        .as_ref()
        .and_then(Output::from_resource)
        .or_else(|| state.focused_output())
        .and_then(|output| state.space.output_geometry(&output));

    match geo {
        Some(geo) => absolute_pos_in(geo, x, y, x_extent, y_extent),
        None => state.pointer_location,
    }
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, (), Beewm> for Beewm {
    fn bind(
        _state: &mut Beewm,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointer { id, .. } => {
                data_init.init(id, VirtualPointerData::default());
            }
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointerWithOutput {
                id,
                output,
                ..
            } => {
                data_init.init(
                    id,
                    VirtualPointerData {
                        output,
                        ..Default::default()
                    },
                );
            }
            zwlr_virtual_pointer_manager_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrVirtualPointerV1, VirtualPointerData, Beewm> for Beewm {
    fn request(
        state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        data: &VirtualPointerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_virtual_pointer_v1::Request::Motion { dx, dy, .. } => {
                data.pending
                    .lock()
                    .unwrap()
                    .push(PendingOp::Motion { dx, dy });
            }
            zwlr_virtual_pointer_v1::Request::MotionAbsolute {
                x,
                y,
                x_extent,
                y_extent,
                ..
            } => {
                data.pending
                    .lock()
                    .unwrap()
                    .push(PendingOp::MotionAbsolute {
                        x,
                        y,
                        x_extent,
                        y_extent,
                    });
            }
            zwlr_virtual_pointer_v1::Request::Button { button, state, .. } => {
                let pressed = state == WEnum::Value(WlButtonState::Pressed);
                data.pending
                    .lock()
                    .unwrap()
                    .push(PendingOp::Button { button, pressed });
            }
            zwlr_virtual_pointer_v1::Request::Axis { axis, value, .. } => {
                if let WEnum::Value(axis) = axis {
                    data.pending.lock().unwrap().push(PendingOp::Axis {
                        axis,
                        value,
                        discrete: false,
                    });
                }
            }
            zwlr_virtual_pointer_v1::Request::AxisDiscrete { axis, value, .. } => {
                if let WEnum::Value(axis) = axis {
                    data.pending.lock().unwrap().push(PendingOp::Axis {
                        axis,
                        value,
                        discrete: true,
                    });
                }
            }
            zwlr_virtual_pointer_v1::Request::Frame => {
                let ops: Vec<PendingOp> = std::mem::take(&mut *data.pending.lock().unwrap());
                let time_ms = state.start_time.elapsed().as_millis() as u32;
                let (axis_h, axis_v) = fold_axis(&ops);

                for op in ops {
                    match op {
                        PendingOp::Motion { dx, dy } => {
                            let pos = state.pointer_location + Point::from((dx, dy));
                            synthetic_motion_absolute(state, pos, time_ms);
                        }
                        PendingOp::MotionAbsolute {
                            x,
                            y,
                            x_extent,
                            y_extent,
                        } => {
                            let pos = resolve_absolute_pos(state, data, x, y, x_extent, y_extent);
                            synthetic_motion_absolute(state, pos, time_ms);
                        }
                        PendingOp::Button { button, pressed } => {
                            let btn_state = if pressed {
                                smithay::backend::input::ButtonState::Pressed
                            } else {
                                smithay::backend::input::ButtonState::Released
                            };
                            synthetic_button(state, button, btn_state, time_ms);
                        }
                        // Already folded into `axis_h` / `axis_v` above.
                        PendingOp::Axis { .. } => {}
                    }
                }

                if axis_h != 0.0 || axis_v != 0.0 {
                    synthetic_axis(state, axis_h, axis_v, time_ms);
                }
            }
            zwlr_virtual_pointer_v1::Request::AxisSource { .. }
            | zwlr_virtual_pointer_v1::Request::AxisStop { .. } => {}
            zwlr_virtual_pointer_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut Beewm,
        _client: ClientId,
        _resource: &ZwlrVirtualPointerV1,
        _data: &VirtualPointerData,
    ) {
    }
}

// --- zwlr-output-management-v1 (read-only: heads are listed, but any
// configuration attempt is rejected — beewm's outputs aren't reconfigurable
// through this protocol). wayvnc queries this to enumerate outputs before it
// will start serving; without it, it waits indefinitely and never opens its
// listening socket.

/// Every head and mode resource one bound manager has been sent, so they can
/// be `finished` before a fresh list is advertised.
#[derive(Debug, Default)]
pub(crate) struct OutputManagerData {
    heads: Mutex<Vec<ZwlrOutputHeadV1>>,
    modes: Mutex<Vec<ZwlrOutputModeV1>>,
}

pub(crate) fn create_output_manager_global<D>(display: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<ZwlrOutputManagerV1, ()>
        + Dispatch<ZwlrOutputManagerV1, OutputManagerData>
        + Dispatch<ZwlrOutputHeadV1, ()>
        + Dispatch<ZwlrOutputModeV1, ()>
        + Dispatch<ZwlrOutputConfigurationV1, ()>
        + Dispatch<ZwlrOutputConfigurationHeadV1, ()>
        + 'static,
{
    display.create_global::<D, ZwlrOutputManagerV1, _>(OUTPUT_MANAGER_VERSION, ())
}

/// Retire the heads and modes a manager was previously sent. The protocol
/// requires each to be `finished` before it stops being advertised.
fn finish_heads(data: &OutputManagerData) {
    for mode in data.modes.lock().unwrap().drain(..) {
        mode.finished();
    }
    for head in data.heads.lock().unwrap().drain(..) {
        head.finished();
    }
}

/// Re-advertise every bound manager's head list. Called whenever an output is
/// added, removed, or moved: the protocol is push-based, so without this a
/// long-lived client (wayvnc holds its manager open) keeps the list it got at
/// bind time and never learns about a hotplug.
pub(crate) fn refresh_output_heads(state: &mut Beewm) {
    state.output_managers.retain(|manager| manager.is_alive());
    if state.output_managers.is_empty() {
        return;
    }

    let managers = state.output_managers.clone();
    let dh = state.display_handle.clone();
    for manager in managers {
        let Some(data) = manager.data::<OutputManagerData>() else {
            continue;
        };
        let Ok(client) = dh.get_client(manager.id()) else {
            continue;
        };
        finish_heads(data);
        advertise_heads(state, &manager, data, &client, &dh);
    }
}

fn advertise_heads(
    state: &Beewm,
    manager: &ZwlrOutputManagerV1,
    data: &OutputManagerData,
    client: &Client,
    dh: &DisplayHandle,
) {
    let version = manager.version();
    for output in state.space.outputs() {
        let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, (), Beewm>(dh, version, ())
        else {
            continue;
        };
        data.heads.lock().unwrap().push(head.clone());
        manager.head(&head);
        head.name(output.name());
        head.description(output.description());

        // A failure here must not `continue`: the head is already advertised,
        // so skipping the rest of the loop body would leave it without
        // enabled/position/transform/scale and the client would see a
        // half-described head.
        if let Some(mode) = output.current_mode()
            && let Ok(mode_resource) =
                client.create_resource::<ZwlrOutputModeV1, (), Beewm>(dh, version, ())
        {
            data.modes.lock().unwrap().push(mode_resource.clone());
            head.mode(&mode_resource);
            mode_resource.size(mode.size.w, mode.size.h);
            if mode.refresh > 0 {
                mode_resource.refresh(mode.refresh);
            }
            mode_resource.preferred();
            head.current_mode(&mode_resource);
        }

        head.enabled(1);
        let pos = state
            .space
            .output_geometry(output)
            .map(|geo| geo.loc)
            .unwrap_or_default();
        head.position(pos.x, pos.y);
        head.transform(output.current_transform().into());
        let scale = match output.current_scale() {
            smithay::output::Scale::Integer(s) => s as f64,
            smithay::output::Scale::Fractional(s) => s,
            smithay::output::Scale::Custom { fractional, .. } => fractional,
        };
        head.scale(scale);
    }

    manager.done(u32::from(smithay::utils::SERIAL_COUNTER.next_serial()));
}

impl GlobalDispatch<ZwlrOutputManagerV1, (), Beewm> for Beewm {
    fn bind(
        state: &mut Beewm,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        let manager = data_init.init(resource, OutputManagerData::default());
        if let Some(data) = manager.data::<OutputManagerData>() {
            advertise_heads(state, &manager, data, client, dh);
        }
        // Tracked so hotplug can push an updated head list — see
        // `refresh_output_heads`.
        state.output_managers.push(manager);
    }
}

impl Dispatch<ZwlrOutputManagerV1, OutputManagerData, Beewm> for Beewm {
    fn request(
        state: &mut Beewm,
        _client: &Client,
        resource: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        data: &OutputManagerData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, .. } => {
                data_init.init(id, ());
            }
            zwlr_output_manager_v1::Request::Stop => {
                // The manager goes inert and must confirm with `finished`,
                // which is a destructor — so drop the registry entry too.
                finish_heads(data);
                resource.finished();
                state.output_managers.retain(|manager| manager != resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut Beewm,
        _client: ClientId,
        resource: &ZwlrOutputManagerV1,
        _data: &OutputManagerData,
    ) {
        state.output_managers.retain(|manager| manager != resource);
    }
}

impl Dispatch<ZwlrOutputHeadV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrOutputHeadV1,
        _request: zwlr_output_head_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        // Only `release`, a destructor — nothing to clean up.
    }
}

impl Dispatch<ZwlrOutputModeV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrOutputModeV1,
        _request: zwlr_output_mode_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        // Only `release`, a destructor — nothing to clean up.
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Beewm>,
    ) {
        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, .. } => {
                data_init.init(id, ());
            }
            zwlr_output_configuration_v1::Request::DisableHead { .. } => {}
            zwlr_output_configuration_v1::Request::Apply
            | zwlr_output_configuration_v1::Request::Test => {
                // beewm's outputs aren't reconfigurable through this protocol.
                resource.failed();
            }
            zwlr_output_configuration_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, (), Beewm> for Beewm {
    fn request(
        _state: &mut Beewm,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        _request: zwlr_output_configuration_head_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Beewm>,
    ) {
        // set_mode / set_position / set_transform / set_scale: no-ops, since
        // `apply`/`test` on the parent configuration always fail.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(axis: WlAxis, value: f64, discrete: bool) -> PendingOp {
        PendingOp::Axis {
            axis,
            value,
            discrete,
        }
    }

    /// A client mirroring `wl_pointer` sends `axis` *and* `axis_discrete` for
    /// one scroll tick. Summing them scrolled twice as far as asked.
    #[test]
    fn discrete_axis_replaces_the_continuous_one_it_extends() {
        let ops = [
            axis(WlAxis::VerticalScroll, 15.0, false),
            axis(WlAxis::VerticalScroll, 15.0, true),
        ];
        assert_eq!(fold_axis(&ops), (0.0, 15.0));
    }

    #[test]
    fn axis_only_and_discrete_only_clients_both_scroll() {
        assert_eq!(
            fold_axis(&[axis(WlAxis::VerticalScroll, 10.0, false)]),
            (0.0, 10.0)
        );
        assert_eq!(
            fold_axis(&[axis(WlAxis::HorizontalScroll, -10.0, true)]),
            (-10.0, 0.0)
        );
    }

    #[test]
    fn repeated_scrolls_within_one_frame_accumulate() {
        let ops = [
            axis(WlAxis::VerticalScroll, 5.0, false),
            axis(WlAxis::VerticalScroll, 5.0, false),
        ];
        assert_eq!(fold_axis(&ops), (0.0, 10.0));
    }

    #[test]
    fn absolute_position_is_relative_to_the_target_output() {
        // A second output at x=1920: the middle of it is 1920 + 960, not 960.
        let geo = Rectangle::new((1920, 0).into(), (1920, 1080).into());
        assert_eq!(
            absolute_pos_in(geo, 960, 540, 1920, 1080),
            Point::from((2880.0, 540.0))
        );
    }

    /// `x_extent`/`y_extent` are client-supplied; a zero must not divide.
    #[test]
    fn zero_extent_lands_on_the_output_origin() {
        let geo = Rectangle::new((100, 50).into(), (800, 600).into());
        assert_eq!(
            absolute_pos_in(geo, 400, 300, 0, 0),
            Point::from((100.0, 50.0))
        );
    }
}
