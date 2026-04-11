use crate::model::window::Geometry;
use crate::WindowHandle;

/// Actions the core WM requests the display server to perform.
#[derive(Debug, Clone)]
pub enum DisplayAction<H: WindowHandle> {
    /// Configure a window's geometry.
    ConfigureWindow {
        handle: H,
        geometry: Geometry,
        border_width: u32,
    },

    /// Map (show) a window.
    MapWindow {
        handle: H,
    },

    /// Unmap (hide) a window.
    UnmapWindow {
        handle: H,
    },

    /// Set input focus to a window.
    SetFocus {
        handle: H,
    },

    /// Raise a window to the top of the stack.
    RaiseWindow {
        handle: H,
    },

    /// Destroy / close a window.
    DestroyWindow {
        handle: H,
    },

    /// Grab a key combination so we receive KeyPress events for it.
    GrabKey {
        modifiers: u16,
        keycode: u32,
    },

    /// Set the border color of a window.
    SetBorderColor {
        handle: H,
        color: u32,
    },
}
