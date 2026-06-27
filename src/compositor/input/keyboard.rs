use smithay::backend::input::{Event, InputBackend, KeyState, KeyboardKeyEvent};
use smithay::backend::session::Session;
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState, xkb};
use smithay::utils::SERIAL_COUNTER;

use crate::compositor::commands::spawn_shell_command;
use crate::compositor::state::Beewm;
use crate::config::Action;

pub(super) fn handle_keyboard<I: InputBackend>(state: &mut Beewm, event: I::KeyboardKeyEvent) {
    // Any key counts as activity: restart the screen-timeout countdown and wake
    // the screen if it was blanked.
    state.notify_activity();

    let serial = SERIAL_COUNTER.next_serial();
    let time = Event::time_msec(&event);
    let keycode = event.key_code();
    let key_state = event.state();

    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };

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
                    if let Some(session) = state.session.as_mut()
                        && let Err(error) = session.change_vt(vt)
                    {
                        tracing::warn!("Failed to switch to VT {}: {}", vt, error);
                    }
                    return FilterResult::Intercept(());
                }

                // While the session is locked, the compositor must act on NO
                // keybinding — every key is forwarded only to the focused lock
                // surface. This is what stops `mod+enter` (or any bind) from
                // opening a terminal behind the lock. VT switching above is
                // intentionally still allowed; the lock persists across VTs.
                if !state.locked
                    && let Some(action) = match_keybind(state, modifiers, keycode, &keysym_handle)
                {
                    execute_action(state, action);
                    return FilterResult::Intercept(());
                }
            }
            FilterResult::Forward
        },
    );

    // XKB state is now up to date for this key; let event-socket subscribers
    // (bars) know if the lock/Shift status changed. The physical LEDs are
    // handled separately via `SeatHandler::led_state_changed`.
    state.publish_keyboard_status();
}

fn match_keybind(
    state: &Beewm,
    modifiers: &ModifiersState,
    keycode: xkb::Keycode,
    keysym_handle: &KeysymHandle<'_>,
) -> Option<Action> {
    let raw = keysym_handle.raw_syms();
    let keysym = if raw.is_empty() {
        keysym_handle.modified_sym()
    } else {
        raw[0]
    };

    for bind in &state.resolved_keybinds {
        if modifiers.logo != bind.logo
            || modifiers.shift != bind.shift
            || modifiers.ctrl != bind.ctrl
            || modifiers.alt != bind.alt
        {
            continue;
        }
        // Prefer physical-position matching (keycode from US layout); fall back
        // to keysym for special keys (XF86 media keys, etc.) not in the US map.
        let matches = match bind.keycode {
            Some(expected) => keycode == expected,
            None => bind.keysym == keysym,
        };
        if matches {
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
            state.focus_in_cycle(true);
        }
        Action::FocusPrev => {
            state.focus_in_cycle(false);
        }
        Action::FocusDirection(direction) => {
            state.focus_window_in_direction(direction);
        }
        Action::FocusOutput(direction) => {
            state.focus_output_in_direction(direction);
        }
        Action::MoveWindowToOutput(direction) => {
            state.move_window_to_output(direction);
        }
        Action::CloseWindow => {
            if let Some(window) = state.active_workspace_focused_window()
                && let Some(toplevel) = window.toplevel()
            {
                toplevel.send_close();
            }
        }
        Action::ToggleFullscreen => {
            state.toggle_fullscreen();
        }
        Action::ToggleFloat => {
            state.toggle_float();
        }
        Action::ToggleSticky => {
            state.toggle_sticky();
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
