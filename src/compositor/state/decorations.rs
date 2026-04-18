use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::desktop::{Window, layer_map_for_output};
use smithay::utils::{Coordinate, Logical, Physical, Rectangle};
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

fn expand_by_border<Kind>(geo: Rectangle<i32, Kind>, bw: i32) -> Rectangle<i32, Kind>
where
    i32: Coordinate,
{
    if bw <= 0 {
        return geo;
    }

    Rectangle::new(
        (geo.loc.x - bw, geo.loc.y - bw).into(),
        (geo.size.w + bw * 2, geo.size.h + bw * 2).into(),
    )
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

fn visible_border_rectangles<Kind>(
    window_geo: Rectangle<i32, Kind>,
    bw: i32,
    occluders: &[Rectangle<i32, Kind>],
) -> Vec<Rectangle<i32, Kind>>
where
    i32: Coordinate,
{
    if bw <= 0 {
        return Vec::new();
    }

    border_rectangles::<Kind>(
        window_geo.loc.x,
        window_geo.loc.y,
        window_geo.size.w,
        window_geo.size.h,
        bw,
    )
    .into_iter()
    .flat_map(|border_geo| border_geo.subtract_rects(occluders.iter().copied()))
    .collect()
}

fn root_is_swap_highlighted<T: PartialEq>(
    root: &T,
    dragged_root: Option<&T>,
    target_root: Option<&T>,
) -> bool {
    dragged_root.is_some_and(|dragged| dragged == root)
        || target_root.is_some_and(|target| target == root)
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
        let dragged_root = self
            .tiled_swap_grab
            .as_ref()
            .and_then(|grab| Self::window_root_surface(&grab.window));
        let target_root = self.tiled_swap_target.clone();

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
        let mut border_fragments = Vec::new();

        for (win_idx, window) in windows.iter().enumerate() {
            let geo = match self.space.element_geometry(window) {
                Some(g) => g,
                None => continue,
            };
            let occluders: Vec<Rectangle<i32, Logical>> = windows
                .iter()
                .skip(win_idx + 1)
                .filter_map(|candidate| {
                    let root = Self::window_root_surface(candidate)?;
                    self.is_root_floating(&root)
                        .then(|| {
                            self.space
                                .element_geometry(candidate)
                                .map(|geo| expand_by_border(geo, bw))
                        })
                        .flatten()
                })
                .collect();

            let is_focused = window
                .toplevel()
                .and_then(|tl| focused_surface.as_ref().map(|fs| *fs == *tl.wl_surface()))
                .unwrap_or(false);
            let is_swap_highlighted = Self::window_root_surface(window)
                .as_ref()
                .map(|root| {
                    root_is_swap_highlighted(root, dragged_root.as_ref(), target_root.as_ref())
                })
                .unwrap_or(false);

            let color = if is_focused || is_swap_highlighted {
                focused_color
            } else {
                unfocused_color
            };
            border_fragments.extend(
                visible_border_rectangles(geo, bw, &occluders)
                    .into_iter()
                    .map(|rect| (rect, color)),
            );
        }

        while self.border_ids.len() < border_fragments.len() {
            self.border_ids.push(Id::new());
        }

        let commit = CommitCounter::from(self.border_commit_serial as usize);
        border_fragments
            .into_iter()
            .enumerate()
            .map(|(idx, (rect, color))| {
                SolidColorRenderElement::new(
                    self.border_ids[idx].clone(),
                    Rectangle::<i32, Physical>::new(
                        (rect.loc.x, rect.loc.y).into(),
                        (rect.size.w, rect.size.h).into(),
                    ),
                    commit,
                    color,
                    Kind::Unspecified,
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Logical, Rectangle};

    use super::{
        expand_by_border, root_is_swap_highlighted, visible_border_rectangles,
        window_border_overlaps_layer,
    };

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

    #[test]
    fn swap_highlight_matches_dragged_and_target_roots() {
        assert!(root_is_swap_highlighted(&1, Some(&1), Some(&2)));
        assert!(root_is_swap_highlighted(&2, Some(&1), Some(&2)));
        assert!(!root_is_swap_highlighted(&3, Some(&1), Some(&2)));
    }

    #[test]
    fn floating_window_clips_the_overlapped_border_segments() {
        let window = rect(100, 100, 400, 300);
        let floating = rect(180, 98, 120, 40);

        let visible = visible_border_rectangles(window, 2, &[floating]);

        assert!(!visible.is_empty());
        assert!(visible.iter().all(|rect| !rect.overlaps(floating)));
        assert!(visible.iter().any(|rect| rect.loc.y == 98));
    }

    #[test]
    fn non_overlapping_floating_window_keeps_all_four_borders() {
        let window = rect(100, 100, 400, 300);
        let floating = rect(180, 160, 120, 80);

        let visible = visible_border_rectangles(window, 2, &[floating]);

        assert_eq!(visible.len(), 4);
    }

    #[test]
    fn floating_window_border_also_clips_the_window_behind_it() {
        let window = rect(100, 100, 400, 300);
        let floating_client = rect(180, 100, 120, 40);
        let floating_with_border = expand_by_border(floating_client, 2);

        let visible = visible_border_rectangles(window, 2, &[floating_with_border]);

        assert!(!visible.is_empty());
        assert!(
            visible
                .iter()
                .all(|rect| !rect.overlaps(floating_with_border))
        );
        assert!(visible.iter().any(|rect| rect.loc.y == 98));
    }
}
