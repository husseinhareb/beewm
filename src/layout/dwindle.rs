use crate::layout::Layout;
use crate::model::window::Geometry;

/// Dwindle layout: each new window consumes a slice of the remaining area,
/// alternating horizontal and vertical splits.
#[derive(Debug, Clone)]
pub struct Dwindle {
    /// Ratio of the remaining region assigned to the newly inserted window.
    pub split_ratio: f64,
}

impl Default for Dwindle {
    fn default() -> Self {
        Self { split_ratio: 0.50 }
    }
}

impl Layout for Dwindle {
    fn apply(&self, screen: &Geometry, window_count: usize) -> Vec<Geometry> {
        if window_count == 0 {
            return Vec::new();
        }

        let split_ratio = if self.split_ratio.is_finite() {
            self.split_ratio.clamp(0.0, 1.0)
        } else {
            Self::default().split_ratio
        };

        let mut remaining = *screen;
        let mut geometries = Vec::with_capacity(window_count);

        for index in 0..window_count {
            if index == window_count - 1 {
                geometries.push(remaining);
                break;
            }

            if index % 2 == 0 {
                let primary_width = (remaining.width as f64 * split_ratio) as u32;
                let secondary_width = remaining.width.saturating_sub(primary_width);

                geometries.push(Geometry::new(
                    remaining.x,
                    remaining.y,
                    primary_width,
                    remaining.height,
                ));

                remaining = Geometry::new(
                    remaining.x + primary_width as i32,
                    remaining.y,
                    secondary_width,
                    remaining.height,
                );
            } else {
                let primary_height = (remaining.height as f64 * split_ratio) as u32;
                let secondary_height = remaining.height.saturating_sub(primary_height);

                geometries.push(Geometry::new(
                    remaining.x,
                    remaining.y,
                    remaining.width,
                    primary_height,
                ));

                remaining = Geometry::new(
                    remaining.x,
                    remaining.y + primary_height as i32,
                    remaining.width,
                    secondary_height,
                );
            }
        }

        geometries
    }
}
