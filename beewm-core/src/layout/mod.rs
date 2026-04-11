pub mod master_stack;

use crate::model::window::Geometry;
use crate::WindowHandle;

/// A layout algorithm that arranges windows within a screen area.
pub trait Layout<H: WindowHandle>: std::fmt::Debug {
    /// Given screen geometry and a list of window handles, return the geometry for each window
    /// in the same order.
    fn apply(&self, screen: &Geometry, windows: &[H]) -> Vec<Geometry>;

    /// Human-readable name of the layout.
    fn name(&self) -> &str;
}
