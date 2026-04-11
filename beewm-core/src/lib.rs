pub mod action;
pub mod config;
pub mod event;
pub mod handler;
pub mod layout;
pub mod manager;
pub mod model;
pub mod state;

use std::fmt::Debug;
use std::hash::Hash;

use action::DisplayAction;
use event::DisplayEvent;

/// A window handle is a backend-specific identifier for a window.
pub trait WindowHandle: Copy + Clone + Debug + Eq + Hash + Send + 'static {}

/// The abstraction boundary between the core WM logic and a display server backend.
pub trait DisplayServer {
    type Handle: WindowHandle;

    /// Poll for the next event from the display server.
    fn next_event(&mut self) -> Result<DisplayEvent<Self::Handle>, Box<dyn std::error::Error>>;

    /// Execute an action on the display server.
    fn execute(
        &mut self,
        action: DisplayAction<Self::Handle>,
    ) -> Result<(), Box<dyn std::error::Error>>;

    /// Flush pending requests to the display server.
    fn flush(&self) -> Result<(), Box<dyn std::error::Error>>;

    /// Return a file descriptor that becomes readable when events are available.
    /// Used for async polling with tokio.
    fn as_fd(&self) -> std::os::unix::io::RawFd;
}
