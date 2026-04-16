use crate::layout::Layout;
use crate::model::window::Geometry;

/// Classic master-stack layout: one master window on the left, remaining windows stacked
/// on the right.
#[derive(Debug, Clone)]
pub struct MasterStack {
    /// Ratio of screen width allocated to the master window (0.0 - 1.0).
    pub master_ratio: f64,
}

impl Default for MasterStack {
    fn default() -> Self {
        Self { master_ratio: 0.50 }
    }
}

impl Layout for MasterStack {
    fn apply(&self, screen: &Geometry, window_count: usize) -> Vec<Geometry> {
        if window_count == 0 {
            return Vec::new();
        }

        if window_count == 1 {
            return vec![*screen];
        }

        let master_ratio = if self.master_ratio.is_finite() {
            self.master_ratio.clamp(0.0, 1.0)
        } else {
            Self::default().master_ratio
        };
        let master_width = (screen.width as f64 * master_ratio) as u32;
        let stack_width = screen.width - master_width;
        let stack_count = window_count - 1;
        let stack_height = screen.height / stack_count as u32;

        let mut geometries = Vec::with_capacity(window_count);

        // Master window
        geometries.push(Geometry::new(
            screen.x,
            screen.y,
            master_width,
            screen.height,
        ));

        // Stack windows
        for i in 0..stack_count {
            let y = screen.y + (i as u32 * stack_height) as i32;
            let h = if i == stack_count - 1 {
                // Last stack window gets remaining height to avoid rounding gaps
                screen.height - (i as u32 * stack_height)
            } else {
                stack_height
            };
            geometries.push(Geometry::new(
                screen.x + master_width as i32,
                y,
                stack_width,
                h,
            ));
        }

        geometries
    }
}
