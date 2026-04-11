pub mod master_stack;

use crate::model::window::Geometry;

/// A layout algorithm that arranges windows within a screen area.
pub trait Layout: std::fmt::Debug {
    /// Given screen geometry and the number of windows, return the geometry for each window.
    fn apply(&self, screen: &Geometry, window_count: usize) -> Vec<Geometry>;

    /// Human-readable name of the layout.
    fn name(&self) -> &str;
}
