// Wayland backend — not yet implemented.
// This is a placeholder for future Wayland support.

use beewm_core::action::DisplayAction;
use beewm_core::event::DisplayEvent;
use beewm_core::{DisplayServer, WindowHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaylandHandle(pub u64);

impl WindowHandle for WaylandHandle {}

pub struct WaylandBackend;

impl DisplayServer for WaylandBackend {
    type Handle = WaylandHandle;

    fn next_event(&mut self) -> Result<DisplayEvent<Self::Handle>, Box<dyn std::error::Error>> {
        todo!("Wayland backend not yet implemented")
    }

    fn execute(
        &mut self,
        _action: DisplayAction<Self::Handle>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Wayland backend not yet implemented")
    }

    fn flush(&self) -> Result<(), Box<dyn std::error::Error>> {
        todo!("Wayland backend not yet implemented")
    }

    fn as_fd(&self) -> std::os::unix::io::RawFd {
        todo!("Wayland backend not yet implemented")
    }
}
