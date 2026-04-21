mod grab;
mod keyboard;
mod pointer;

use smithay::backend::input::{InputBackend, InputEvent};

use super::state::Beewm;

pub use grab::{resize_edges_for_pointer, resized_window_geometry_from_start};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const MIN_FLOATING_WINDOW_SIZE: i32 = 1;

/// Returns `true` when keyboard focus is currently on a layer-shell surface
/// (e.g. wofi).  In that case focus-follows-mouse must NOT steal focus away.
fn layer_surface_has_keyboard_focus(state: &Beewm) -> bool {
    let Some(focused) = state.seat.get_keyboard().and_then(|kb| kb.current_focus()) else {
        return false;
    };
    if state.mapped_window_for_surface(&focused).is_some() {
        return false;
    }
    state
        .space
        .layer_for_surface(&focused, smithay::desktop::WindowSurfaceType::ALL)
        .is_some()
}

/// Process an input event from any backend.
pub fn handle_input<I: InputBackend>(state: &mut Beewm, event: InputEvent<I>) {
    match event {
        InputEvent::Keyboard { event } => keyboard::handle_keyboard::<I>(state, event),
        InputEvent::PointerMotion { event } => pointer::handle_pointer_motion::<I>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            pointer::handle_pointer_motion_absolute::<I>(state, event)
        }
        InputEvent::PointerButton { event } => pointer::handle_pointer_button::<I>(state, event),
        InputEvent::PointerAxis { event } => pointer::handle_pointer_axis::<I>(state, event),
        _ => {}
    }
}