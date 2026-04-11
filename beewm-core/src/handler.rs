use crate::action::DisplayAction;
use crate::config::Action;
use crate::event::DisplayEvent;
use crate::state::State;
use crate::WindowHandle;

/// Process a display event and update state accordingly.
/// Returns true if the event loop should continue, false to quit.
pub fn handle_event<H: WindowHandle>(
    state: &mut State<H>,
    event: DisplayEvent<H>,
) -> bool {
    match event {
        DisplayEvent::WindowCreated {
            handle,
            geometry,
            floating,
        } => {
            tracing::debug!(?handle, "Window created");
            state.manage_window(handle, geometry, floating);
        }

        DisplayEvent::WindowDestroyed { handle } => {
            tracing::debug!(?handle, "Window destroyed");
            state.unmanage_window(handle);
        }

        DisplayEvent::MapRequest { handle } => {
            tracing::debug!(?handle, "Map request");
            if !state.windows.contains_key(&handle) {
                // We don't know about this window yet — it should come in as WindowCreated.
                // For now, just map it.
                state.actions.push(DisplayAction::MapWindow { handle });
            }
        }

        DisplayEvent::UnmapNotify { handle } => {
            tracing::debug!(?handle, "Unmap notify");
            state.unmanage_window(handle);
        }

        DisplayEvent::ConfigureRequest { handle, geometry } => {
            tracing::debug!(?handle, "Configure request");
            // For managed tiled windows, ignore the request and re-apply layout.
            // For floating/unmanaged windows, honor the request.
            if let Some(window) = state.windows.get(&handle) {
                if window.floating {
                    state.actions.push(DisplayAction::ConfigureWindow {
                        handle,
                        geometry,
                        border_width: state.config.border_width,
                    });
                }
                // Tiled windows: layout will handle positioning, do nothing extra.
            } else {
                // Unmanaged window — pass through.
                state.actions.push(DisplayAction::ConfigureWindow {
                    handle,
                    geometry,
                    border_width: 0,
                });
            }
        }

        DisplayEvent::KeyPress {
            modifiers,
            keycode,
        } => {
            tracing::debug!(modifiers, keycode, "Key press");
            return handle_keypress(state, modifiers, keycode);
        }

        DisplayEvent::FocusIn { handle } => {
            if state.windows.contains_key(&handle) {
                state.focus_window(handle);
            }
        }

        DisplayEvent::EnterNotify { handle } => {
            if state.config.focus_follows_mouse && state.windows.contains_key(&handle) {
                state.focus_window(handle);
            }
        }

        DisplayEvent::ScreenChange => {
            tracing::info!("Screen configuration changed");
            // TODO: re-query screens and re-layout
        }
    }

    true
}

fn handle_keypress<H: WindowHandle>(
    state: &mut State<H>,
    modifiers: u16,
    keycode: u32,
) -> bool {
    // Find matching keybind
    let action = state
        .config
        .keybinds
        .iter()
        .find(|kb| {
            kb.key == keycode.to_string()
                && modifier_match(&kb.modifiers, modifiers)
        })
        .map(|kb| kb.action.clone());

    if let Some(action) = action {
        match action {
            Action::FocusNext => state.focus_next(),
            Action::FocusPrev => state.focus_prev(),
            Action::CloseWindow => {
                let ws = state.active_workspace;
                if let Some(handle) = state.workspaces[ws].focused {
                    state.actions.push(DisplayAction::DestroyWindow { handle });
                }
            }
            Action::SwitchWorkspace(idx) => state.switch_workspace(idx),
            Action::MoveToWorkspace(idx) => state.move_to_workspace(idx),
            Action::Spawn(cmd) => {
                if let Err(e) = spawn_process(&cmd) {
                    tracing::error!("Failed to spawn {}: {}", cmd, e);
                }
            }
            Action::Quit => return false,
        }
    }

    true
}

fn modifier_match(config_mods: &[String], actual: u16) -> bool {
    let mut expected: u16 = 0;
    for m in config_mods {
        match m.to_lowercase().as_str() {
            "mod4" | "super" => expected |= 1 << 6, // Mod4Mask
            "shift" => expected |= 1 << 0,          // ShiftMask
            "control" | "ctrl" => expected |= 1 << 2, // ControlMask
            "mod1" | "alt" => expected |= 1 << 3,   // Mod1Mask
            _ => {}
        }
    }
    // Mask out lock indicators (NumLock, CapsLock, ScrollLock)
    let clean = actual & !(1 << 1 | 1 << 4 | 1 << 5);
    clean == expected
}

fn spawn_process(cmd: &str) -> Result<(), Box<dyn std::error::Error>> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .spawn()?;
    Ok(())
}
