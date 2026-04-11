use crate::layout::Layout;
use crate::model::window::Geometry;
use crate::WindowHandle;

/// Classic master-stack layout: one master window on the left, remaining windows stacked
/// on the right.
#[derive(Debug, Clone)]
pub struct MasterStack {
    /// Ratio of screen width allocated to the master window (0.0 - 1.0).
    pub master_ratio: f64,
}

impl Default for MasterStack {
    fn default() -> Self {
        Self { master_ratio: 0.55 }
    }
}

impl<H: WindowHandle> Layout<H> for MasterStack {
    fn apply(&self, screen: &Geometry, windows: &[H]) -> Vec<Geometry> {
        if windows.is_empty() {
            return Vec::new();
        }

        if windows.len() == 1 {
            return vec![*screen];
        }

        let master_width = (screen.width as f64 * self.master_ratio) as u32;
        let stack_width = screen.width - master_width;
        let stack_count = windows.len() - 1;
        let stack_height = screen.height / stack_count as u32;

        let mut geometries = Vec::with_capacity(windows.len());

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

    fn name(&self) -> &str {
        "master-stack"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct TestHandle(u32);
    impl WindowHandle for TestHandle {}

    #[test]
    fn empty_windows() {
        let layout = MasterStack::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, &[] as &[TestHandle]);
        assert!(result.is_empty());
    }

    #[test]
    fn single_window_fills_screen() {
        let layout = MasterStack::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, &[TestHandle(1)]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], screen);
    }

    #[test]
    fn two_windows_split() {
        let layout = MasterStack::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, &[TestHandle(1), TestHandle(2)]);
        assert_eq!(result.len(), 2);
        // Master takes ~55%
        assert_eq!(result[0].width, 1056);
        assert_eq!(result[0].height, 1080);
        // Stack takes the rest
        assert_eq!(result[1].x, 1056);
        assert_eq!(result[1].width, 864);
        assert_eq!(result[1].height, 1080);
    }

    #[test]
    fn three_windows_stacked() {
        let layout = MasterStack::default();
        let screen = Geometry::new(0, 0, 1920, 1080);
        let result = layout.apply(&screen, &[TestHandle(1), TestHandle(2), TestHandle(3)]);
        assert_eq!(result.len(), 3);
        // Two stack windows each get half height
        assert_eq!(result[1].height, 540);
        assert_eq!(result[2].height, 540);
        assert_eq!(result[2].y, 540);
    }
}
