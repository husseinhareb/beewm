use crate::model::window::Geometry;
use crate::WindowHandle;

/// Events emitted by the display server backend, consumed by the core WM.
#[derive(Debug, Clone)]
pub enum DisplayEvent<H: WindowHandle> {
    /// A new window was created / requests to be managed.
    WindowCreated {
        handle: H,
        geometry: Geometry,
        floating: bool,
    },

    /// A window was destroyed.
    WindowDestroyed {
        handle: H,
    },

    /// A window requests a new configuration (size/position).
    ConfigureRequest {
        handle: H,
        geometry: Geometry,
    },

    /// A window requests to be mapped (shown).
    MapRequest {
        handle: H,
    },

    /// A window was unmapped (hidden).
    UnmapNotify {
        handle: H,
    },

    /// A key was pressed while we had a grab.
    KeyPress {
        modifiers: u16,
        keycode: u32,
    },

    /// Focus entered a window.
    FocusIn {
        handle: H,
    },

    /// The pointer entered a window.
    EnterNotify {
        handle: H,
    },

    /// A screen/monitor configuration changed.
    ScreenChange,
}
