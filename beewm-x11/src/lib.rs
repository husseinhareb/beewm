pub mod actions;
pub mod atoms;
pub mod connection;
pub mod event_translate;

use std::os::unix::io::RawFd;

use beewm_core::action::DisplayAction;
use beewm_core::event::DisplayEvent;
use beewm_core::{DisplayServer, WindowHandle};
use connection::X11Connection;
use x11rb::protocol::xproto::Window;

/// An X11 window handle — just a wrapper around the X11 window ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct X11Handle(pub Window);

impl WindowHandle for X11Handle {}

/// The X11 display server backend.
pub struct X11Backend {
    pub conn: X11Connection,
}

impl X11Backend {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let conn = X11Connection::new()?;
        Ok(Self { conn })
    }
}

impl DisplayServer for X11Backend {
    type Handle = X11Handle;

    fn next_event(&mut self) -> Result<DisplayEvent<X11Handle>, Box<dyn std::error::Error>> {
        let event = self.conn.poll_for_event()?;
        match event {
            Some(ev) => event_translate::translate(&self.conn, ev),
            None => Err("no events available".into()),
        }
    }

    fn execute(
        &mut self,
        action: DisplayAction<X11Handle>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        actions::execute(&self.conn, action)
    }

    fn flush(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.flush()?;
        Ok(())
    }

    fn as_fd(&self) -> RawFd {
        self.conn.fd()
    }
}
