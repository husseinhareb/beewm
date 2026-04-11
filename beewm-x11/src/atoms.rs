use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

/// EWMH and ICCCM atoms used by the window manager.
pub struct Atoms {
    pub wm_protocols: Atom,
    pub wm_delete_window: Atom,
    pub wm_state: Atom,
    pub wm_take_focus: Atom,
    pub net_supported: Atom,
    pub net_wm_name: Atom,
    pub net_wm_state: Atom,
    pub net_wm_state_fullscreen: Atom,
    pub net_active_window: Atom,
    pub net_wm_window_type: Atom,
    pub net_wm_window_type_dialog: Atom,
    pub net_wm_window_type_splash: Atom,
    pub net_wm_window_type_utility: Atom,
    pub net_current_desktop: Atom,
    pub net_number_of_desktops: Atom,
}

impl Atoms {
    pub fn new(conn: &RustConnection) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            wm_protocols: Self::intern(conn, "WM_PROTOCOLS")?,
            wm_delete_window: Self::intern(conn, "WM_DELETE_WINDOW")?,
            wm_state: Self::intern(conn, "WM_STATE")?,
            wm_take_focus: Self::intern(conn, "WM_TAKE_FOCUS")?,
            net_supported: Self::intern(conn, "_NET_SUPPORTED")?,
            net_wm_name: Self::intern(conn, "_NET_WM_NAME")?,
            net_wm_state: Self::intern(conn, "_NET_WM_STATE")?,
            net_wm_state_fullscreen: Self::intern(conn, "_NET_WM_STATE_FULLSCREEN")?,
            net_active_window: Self::intern(conn, "_NET_ACTIVE_WINDOW")?,
            net_wm_window_type: Self::intern(conn, "_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_dialog: Self::intern(conn, "_NET_WM_WINDOW_TYPE_DIALOG")?,
            net_wm_window_type_splash: Self::intern(conn, "_NET_WM_WINDOW_TYPE_SPLASH")?,
            net_wm_window_type_utility: Self::intern(conn, "_NET_WM_WINDOW_TYPE_UTILITY")?,
            net_current_desktop: Self::intern(conn, "_NET_CURRENT_DESKTOP")?,
            net_number_of_desktops: Self::intern(conn, "_NET_NUMBER_OF_DESKTOPS")?,
        })
    }

    fn intern(conn: &RustConnection, name: &str) -> Result<Atom, Box<dyn std::error::Error>> {
        Ok(intern_atom(conn, false, name.as_bytes())?.reply()?.atom)
    }

    /// Check if a window has a specific window type.
    pub fn is_floating_type(&self, type_atoms: &[Atom]) -> bool {
        type_atoms.iter().any(|&a| {
            a == self.net_wm_window_type_dialog
                || a == self.net_wm_window_type_splash
                || a == self.net_wm_window_type_utility
        })
    }
}
