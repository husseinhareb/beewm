use crate::WindowHandle;

/// A rectangle representing position and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Geometry {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// A managed window in the WM.
#[derive(Debug, Clone)]
pub struct Window<H: WindowHandle> {
    pub handle: H,
    pub geometry: Geometry,
    pub workspace: usize,
    pub floating: bool,
    pub visible: bool,
}

impl<H: WindowHandle> Window<H> {
    pub fn new(handle: H, geometry: Geometry, workspace: usize) -> Self {
        Self {
            handle,
            geometry,
            workspace,
            floating: false,
            visible: true,
        }
    }
}
