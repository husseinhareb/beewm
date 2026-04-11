use beewm_core::event::DisplayEvent;
use beewm_core::model::window::Geometry;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;

use crate::connection::X11Connection;
use crate::X11Handle;

/// Translate an x11rb protocol event into a core DisplayEvent.
pub fn translate(
    conn: &X11Connection,
    event: Event,
) -> Result<DisplayEvent<X11Handle>, Box<dyn std::error::Error>> {
    match event {
        Event::MapRequest(ev) => {
            let handle = X11Handle(ev.window);
            let geom = get_window_geometry(conn, ev.window)?;
            let floating = is_floating(conn, ev.window);

            Ok(DisplayEvent::WindowCreated {
                handle,
                geometry: geom,
                floating,
            })
        }

        Event::UnmapNotify(ev) => Ok(DisplayEvent::UnmapNotify {
            handle: X11Handle(ev.window),
        }),

        Event::DestroyNotify(ev) => Ok(DisplayEvent::WindowDestroyed {
            handle: X11Handle(ev.window),
        }),

        Event::ConfigureRequest(ev) => Ok(DisplayEvent::ConfigureRequest {
            handle: X11Handle(ev.window),
            geometry: Geometry::new(
                ev.x as i32,
                ev.y as i32,
                ev.width as u32,
                ev.height as u32,
            ),
        }),

        Event::KeyPress(ev) => Ok(DisplayEvent::KeyPress {
            modifiers: ev.state.into(),
            keycode: ev.detail as u32,
        }),

        Event::EnterNotify(ev) => Ok(DisplayEvent::EnterNotify {
            handle: X11Handle(ev.event),
        }),

        Event::FocusIn(ev) => Ok(DisplayEvent::FocusIn {
            handle: X11Handle(ev.event),
        }),

        _ => Err(format!("Unhandled X11 event: {:?}", event).into()),
    }
}

fn get_window_geometry(
    conn: &X11Connection,
    window: Window,
) -> Result<Geometry, Box<dyn std::error::Error>> {
    let geom = x11rb::protocol::xproto::get_geometry(&conn.conn, window)?.reply()?;
    Ok(Geometry::new(
        geom.x as i32,
        geom.y as i32,
        geom.width as u32,
        geom.height as u32,
    ))
}

fn is_floating(conn: &X11Connection, window: Window) -> bool {
    let Ok(cookie) = x11rb::protocol::xproto::get_property(
        &conn.conn,
        false,
        window,
        conn.atoms.net_wm_window_type,
        AtomEnum::ATOM,
        0,
        32,
    ) else {
        return false;
    };
    let Ok(reply) = cookie.reply() else {
        return false;
    };

    if reply.type_ == u32::from(AtomEnum::ATOM) && reply.format == 32 {
        let atoms: Vec<Atom> = reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default();
        conn.atoms.is_floating_type(&atoms)
    } else {
        false
    }
}
