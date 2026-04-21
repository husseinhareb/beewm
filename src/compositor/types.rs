use smithay::desktop::Window;
use smithay::input::keyboard::xkb;
use smithay::input::pointer::CursorIcon;
use smithay::utils::{Logical, Point, Size};

use crate::config::Action;

/// A keybinding pre-resolved at startup so the hot-path avoids string
/// allocations and repeated `xkb::keysym_from_name` lookups.
#[derive(Debug, Clone)]
pub struct ResolvedKeybind {
    pub logo: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub keysym: xkb::Keysym,
    pub action: Action,
}

/// State for an in-progress floating window move (Super + LMB drag).
#[derive(Debug, Clone)]
pub struct MoveGrab {
    /// The window being moved.
    pub window: Window,
    /// Pointer position when the grab started.
    pub start_pointer: Point<f64, Logical>,
    /// Window position when the grab started.
    pub start_window_pos: Point<i32, Logical>,
}

/// State for an in-progress tiled-window swap grab (Super + LMB drag).
#[derive(Debug, Clone)]
pub struct TiledSwapGrab {
    /// The tiled window being dragged.
    pub window: Window,
    /// Workspace that owns the dragged tiled window.
    pub workspace_idx: usize,
}

/// State for an in-progress tiled-window resize (Super + RMB drag).
#[derive(Debug, Clone)]
pub struct TiledResizeGrab {
    /// The tiled window being resized.
    pub window: Window,
    /// Workspace that owns the resized tiled window.
    pub workspace_idx: usize,
    /// Which edges of the window are following the pointer.
    pub edges: ResizeEdges,
    /// Pointer position used to compute the next resize delta.
    pub last_pointer: Point<f64, Logical>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHorizontalEdge {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeVerticalEdge {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdges {
    pub horizontal: ResizeHorizontalEdge,
    pub vertical: ResizeVerticalEdge,
}

impl ResizeEdges {
    pub fn cursor_icon(self) -> CursorIcon {
        match (self.vertical, self.horizontal) {
            (ResizeVerticalEdge::Top, ResizeHorizontalEdge::Left) => CursorIcon::NwResize,
            (ResizeVerticalEdge::Top, ResizeHorizontalEdge::Right) => CursorIcon::NeResize,
            (ResizeVerticalEdge::Bottom, ResizeHorizontalEdge::Left) => CursorIcon::SwResize,
            (ResizeVerticalEdge::Bottom, ResizeHorizontalEdge::Right) => CursorIcon::SeResize,
        }
    }
}

/// State for an in-progress floating window resize (Super + RMB drag).
#[derive(Debug, Clone)]
pub struct ResizeGrab {
    /// The window being resized.
    pub window: Window,
    /// Pointer position when the resize started.
    pub start_pointer: Point<f64, Logical>,
    /// Window position when the resize started.
    pub start_window_pos: Point<i32, Logical>,
    /// Window size when the resize started.
    pub start_window_size: Size<i32, Logical>,
    /// Which edges of the window are following the pointer.
    pub edges: ResizeEdges,
    /// Latest requested window position during the interactive resize.
    pub current_window_pos: Point<i32, Logical>,
    /// Latest requested window size during the interactive resize.
    pub current_window_size: Size<i32, Logical>,
}

#[derive(Debug, Clone, Copy)]
pub struct FloatingWindowData {
    pub position: Point<i32, Logical>,
    pub size: Size<i32, Logical>,
}

impl FloatingWindowData {
    pub fn new(position: Point<i32, Logical>, size: Size<i32, Logical>) -> Self {
        Self {
            position,
            size: Size::from((size.w.max(1), size.h.max(1))),
        }
    }
}

/// Active pointer grab — only one can be active at a time.
#[derive(Debug, Clone)]
pub enum ActiveGrab {
    /// Floating window move (Super + LMB drag).
    Move(MoveGrab),
    /// Tiled window swap (Super + LMB drag on tiled window).
    TiledSwap(TiledSwapGrab),
    /// Floating window resize (Super + RMB drag).
    Resize(ResizeGrab),
    /// Tiled window resize (Super + RMB drag on tiled window).
    TiledResize(TiledResizeGrab),
}
