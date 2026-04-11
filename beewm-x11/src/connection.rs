use std::os::unix::io::{AsRawFd, RawFd};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

use crate::atoms::Atoms;

/// Wrapper around the x11rb connection with setup state.
pub struct X11Connection {
    pub conn: RustConnection,
    pub screen_num: usize,
    pub atoms: Atoms,
}

impl X11Connection {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(None)?;

        let atoms = Atoms::new(&conn)?;

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // Become the window manager by requesting SubstructureRedirect on root.
        let mask = EventMask::SUBSTRUCTURE_REDIRECT
            | EventMask::SUBSTRUCTURE_NOTIFY
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::PROPERTY_CHANGE;

        let result = change_window_attributes(
            &conn,
            root,
            &ChangeWindowAttributesAux::new().event_mask(mask),
        )?
        .check();

        if result.is_err() {
            return Err("Another window manager is already running".into());
        }

        conn.flush()?;

        tracing::info!(
            "Connected to X11 display, screen {}, root window 0x{:x}",
            screen_num,
            root
        );

        Ok(Self {
            conn,
            screen_num,
            atoms,
        })
    }

    /// Get the root window.
    pub fn root(&self) -> Window {
        self.conn.setup().roots[self.screen_num].root
    }

    /// Get the root screen geometry.
    pub fn screen_geometry(&self) -> (u32, u32) {
        let screen = &self.conn.setup().roots[self.screen_num];
        (screen.width_in_pixels as u32, screen.height_in_pixels as u32)
    }

    /// Poll for an event without blocking.
    pub fn poll_for_event(
        &self,
    ) -> Result<Option<x11rb::protocol::Event>, Box<dyn std::error::Error>> {
        Ok(self.conn.poll_for_event()?)
    }

    /// Flush pending requests.
    pub fn flush(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.conn.flush()?;
        Ok(())
    }

    /// Get the raw file descriptor for async polling.
    pub fn fd(&self) -> RawFd {
        self.conn.stream().as_raw_fd()
    }
}
