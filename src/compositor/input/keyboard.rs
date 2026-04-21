use smithay::backend::input::{Event, InputBackend, KeyState, KeyboardKeyEvent};
use smithay::backend::session::Session;
use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState};
use smithay::utils::SERIAL_COUNTER;

use crate::compositor::commands::spawn_shell_command;
use crate::compositor::state::Beewm;
use crate::config::Action;

pub(super) fn handle_keyboard<I: InputBackend>(state: &mut Beewm, event: I::KeyboardKeyEvent) {
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
            state.focus_current_window();
        }
        Action::FocusPrev => {
            let ws = &mut state.workspaces[state.active_workspace];
            ws.focus_prev();
            state.focus_current_window();
        }
        Action::FocusDirection(direction) => {
            state.focus_window_in_direction(direction);
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
