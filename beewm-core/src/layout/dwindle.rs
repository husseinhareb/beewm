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
        Self { split_ratio: 0.55 }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_windows() {
        let layout = Dwindle::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, 0);
        assert!(result.is_empty());
    }

    #[test]
    fn single_window_fills_screen() {
        let layout = Dwindle::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, 1);
        assert_eq!(result, vec![screen]);
    }

    #[test]
    fn two_windows_split_horizontally_first() {
        let layout = Dwindle::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, 2);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], Geometry::new(0, 0, 1056, 1080));
        assert_eq!(result[1], Geometry::new(1056, 0, 864, 1080));
    }

    #[test]
    fn three_windows_dwindle() {
        let layout = Dwindle::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], Geometry::new(0, 0, 1056, 1080));
        assert_eq!(result[1], Geometry::new(1056, 0, 864, 594));
        assert_eq!(result[2], Geometry::new(1056, 594, 864, 486));
    }

    #[test]
    fn four_windows_alternate_axis() {
        let layout = Dwindle { split_ratio: 0.5 };
        let screen = Geometry::new(0, 0, 100, 80);
        let result = layout.apply(&screen, 4);
        assert_eq!(result, vec![
            Geometry::new(0, 0, 50, 80),
            Geometry::new(50, 0, 50, 40),
            Geometry::new(50, 40, 25, 40),
            Geometry::new(75, 40, 25, 40),
        ]);
    }
}
