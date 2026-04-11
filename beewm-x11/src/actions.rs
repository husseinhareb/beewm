use beewm_core::action::DisplayAction;
use x11rb::protocol::xproto::*;

use crate::connection::X11Connection;
use crate::X11Handle;

/// Execute a display action on the X11 server.
pub fn execute(
    conn: &X11Connection,
    action: DisplayAction<X11Handle>,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        DisplayAction::ConfigureWindow {
            handle,
            geometry,
            border_width,
        } => {
            let aux = ConfigureWindowAux::new()
                .x(geometry.x)
                .y(geometry.y)
                .width(geometry.width)
                .height(geometry.height)
                .border_width(border_width);
            configure_window(&conn.conn, handle.0, &aux)?;
        }

        DisplayAction::MapWindow { handle } => {
            map_window(&conn.conn, handle.0)?;
        }

        DisplayAction::UnmapWindow { handle } => {
            unmap_window(&conn.conn, handle.0)?;
        }

        DisplayAction::SetFocus { handle } => {
            set_input_focus(
                &conn.conn,
                InputFocus::POINTER_ROOT,
                handle.0,
                x11rb::CURRENT_TIME,
            )?;
        }

        DisplayAction::RaiseWindow { handle } => {
            configure_window(
                &conn.conn,
                handle.0,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
        }

        DisplayAction::DestroyWindow { handle } => {
            // Try graceful close via WM_DELETE_WINDOW first
            let supports_delete = supports_protocol(conn, handle.0, conn.atoms.wm_delete_window);

            if supports_delete {
                let event = ClientMessageEvent::new(
                    32,
                    handle.0,
                    conn.atoms.wm_protocols,
                    [conn.atoms.wm_delete_window, x11rb::CURRENT_TIME, 0, 0, 0],
                );
                send_event(
                    &conn.conn,
                    false,
                    handle.0,
                    EventMask::NO_EVENT,
                    event,
                )?;
            } else {
                destroy_window(&conn.conn, handle.0)?;
            }
        }

        DisplayAction::GrabKey {
            modifiers,
            keycode,
        } => {
            grab_key(
                &conn.conn,
                false,
                conn.root(),
                ModMask::from(modifiers),
                keycode as u8,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?;
        }

        DisplayAction::SetBorderColor { handle, color } => {
            change_window_attributes(
                &conn.conn,
                handle.0,
                &ChangeWindowAttributesAux::new().border_pixel(color),
            )?;
        }
    }

    Ok(())
}

fn supports_protocol(conn: &X11Connection, window: Window, protocol: Atom) -> bool {
    let Ok(cookie) = get_property(
        &conn.conn,
        false,
        window,
        conn.atoms.wm_protocols,
        AtomEnum::ATOM,
        0,
        64,
    ) else {
        return false;
    };
    let Ok(reply) = cookie.reply() else {
        return false;
    };

    reply
        .value32()
        .map(|atoms| atoms.into_iter().any(|a| a == protocol))
        .unwrap_or(false)
}
