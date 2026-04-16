use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::desktop::{Window, layer_map_for_output};
use smithay::utils::{Coordinate, Logical, Rectangle};
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use super::Beewm;

fn border_rectangles<Kind>(x: i32, y: i32, w: i32, h: i32, bw: i32) -> [Rectangle<i32, Kind>; 4]
where
    i32: Coordinate,
{
    [
        Rectangle::new((x - bw, y - bw).into(), (w + bw * 2, bw).into()),
        Rectangle::new((x - bw, y + h).into(), (w + bw * 2, bw).into()),
        Rectangle::new((x - bw, y).into(), (bw, h).into()),
        Rectangle::new((x + w, y).into(), (bw, h).into()),
    ]
}

fn window_border_overlaps_layer(
    window_geo: Rectangle<i32, Logical>,
    layer_geo: Rectangle<i32, Logical>,
    bw: i32,
) -> bool {
    if bw <= 0 {
        return false;
    }

    border_rectangles::<Logical>(
        window_geo.loc.x,
        window_geo.loc.y,
        window_geo.size.w,
        window_geo.size.h,
        bw,
    )
    .into_iter()
    .any(|border_geo| border_geo.overlaps(layer_geo))
}

impl Beewm {
    /// Returns `true` when a top/overlay layer actually overlaps a border.
    /// Persistent panels usually reserve an exclusive zone and should not hide
    /// borders globally just because they are mapped.
    pub fn has_layer_surface_overlapping_borders(&self, bw: i32) -> bool {
        let Some(output) = self.space.outputs().next() else {
            return false;
        };

        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                self.fullscreen_window
                    .as_ref()
                    .map(|fs| fs != *w)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        if windows.is_empty() {
            return false;
        }

        let layer_map = layer_map_for_output(output);
        for layer in [WlrLayer::Overlay, WlrLayer::Top] {
            for layer_surface in layer_map.layers_on(layer) {
                let Some(layer_geo) = layer_map.layer_geometry(layer_surface) else {
                    continue;
                };
                if windows.iter().any(|window| {
                    self.space
                        .element_geometry(window)
                        .map(|window_geo| window_border_overlaps_layer(window_geo, layer_geo, bw))
                        .unwrap_or(false)
                }) {
                    return true;
                }
            }
        }
        false
    }

    /// Build border render elements for all visible windows.
    pub fn border_elements(&mut self) -> Vec<SolidColorRenderElement> {
        let bw = self.config.border_width as i32;
        if bw == 0 || self.has_layer_surface_overlapping_borders(bw) {
            return Vec::new();
        }

        let focused_surface = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let focused_color = self.border_color_focused;
        let unfocused_color = self.border_color_unfocused;

        let mut elements = Vec::new();
        // Exclude the fullscreen window — it has no borders.
        let windows: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                self.fullscreen_window
                    .as_ref()
                    .map(|fs| fs != *w)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        let window_count = windows.len();

        // Ensure we have enough pre-allocated IDs (4 per window: top, bottom, left, right).
        let needed = window_count * 4;
        while self.border_ids.len() < needed {
            self.border_ids.push(Id::new());
        }

        for (win_idx, window) in windows.iter().enumerate() {
            let geo = match self.space.element_geometry(window) {
                Some(g) => g,
                None => continue,
            };

            let is_focused = window
                .toplevel()
                .and_then(|tl| focused_surface.as_ref().map(|fs| *fs == *tl.wl_surface()))
                .unwrap_or(false);

            let color = if is_focused {
                focused_color
            } else {
                unfocused_color
            };

            let commit = CommitCounter::from(self.border_commit_serial as usize);
            let [top_geo, bottom_geo, left_geo, right_geo] =
                border_rectangles(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h, bw);

            // Use stable pre-allocated IDs so the DRM compositor's damage
            // tracker can recognise unchanged elements across frames.
            let base = win_idx * 4;
            let border_top_id = self.border_ids[base].clone();
            let border_bottom_id = self.border_ids[base + 1].clone();
            let border_left_id = self.border_ids[base + 2].clone();
            let border_right_id = self.border_ids[base + 3].clone();

            // Top border
            elements.push(SolidColorRenderElement::new(
                border_top_id,
                top_geo,
                commit,
                color,
                Kind::Unspecified,
            ));
            // Bottom border
            elements.push(SolidColorRenderElement::new(
                border_bottom_id,
                bottom_geo,
                commit,
                color,
                Kind::Unspecified,
            ));
            // Left border
            elements.push(SolidColorRenderElement::new(
                border_left_id,
                left_geo,
                commit,
                color,
                Kind::Unspecified,
            ));
            // Right border
            elements.push(SolidColorRenderElement::new(
                border_right_id,
                right_geo,
                commit,
                color,
                Kind::Unspecified,
            ));
        }

        elements
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Logical, Rectangle};

    use super::window_border_overlaps_layer;

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (width, height).into())
    }

    #[test]
    fn reserved_top_bar_does_not_hide_borders() {
        let window = rect(4, 34, 400, 300);
        let top_bar = rect(0, 0, 1920, 30);

        assert!(!window_border_overlaps_layer(window, top_bar, 2));
    }

    #[test]
    fn fullscreen_overlay_hides_borders() {
        let window = rect(100, 100, 400, 300);
        let overlay = rect(0, 0, 1920, 1080);

        assert!(window_border_overlaps_layer(window, overlay, 2));
    }

    #[test]
    fn centered_popup_does_not_hide_borders() {
        let window = rect(100, 100, 400, 300);
        let popup = rect(180, 160, 120, 80);

        assert!(!window_border_overlaps_layer(window, popup, 2));
    }

    #[test]
    fn popup_crossing_border_hides_borders() {
        let window = rect(100, 100, 400, 300);
        let popup = rect(98, 120, 24, 80);

        assert!(window_border_overlaps_layer(window, popup, 2));
    }
}
