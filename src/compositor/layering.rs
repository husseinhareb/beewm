use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

const LAYERS_ABOVE_WINDOWS: [WlrLayer; 2] = [WlrLayer::Overlay, WlrLayer::Top];
const LAYERS_BELOW_WINDOWS: [WlrLayer; 2] = [WlrLayer::Bottom, WlrLayer::Background];
const FULLSCREEN_LAYERS_BELOW_WINDOWS: [WlrLayer; 4] = [
    WlrLayer::Overlay,
    WlrLayer::Top,
    WlrLayer::Bottom,
    WlrLayer::Background,
];

pub(crate) fn layers_rendered_above_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_ABOVE_WINDOWS
    }
}

pub(crate) fn layers_rendered_below_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &FULLSCREEN_LAYERS_BELOW_WINDOWS
    } else {
        &LAYERS_BELOW_WINDOWS
    }
}

pub(crate) fn layers_hit_tested_before_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_ABOVE_WINDOWS
    }
}

pub(crate) fn layers_hit_tested_after_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_BELOW_WINDOWS
    }
}

#[cfg(test)]
mod tests {
    use super::{
        layers_hit_tested_after_windows, layers_hit_tested_before_windows,
        layers_rendered_above_windows, layers_rendered_below_windows,
    };
    use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

    #[test]
    fn normal_layer_order_keeps_top_surfaces_above_windows() {
        assert_eq!(
            layers_rendered_above_windows(false),
            &[WlrLayer::Overlay, WlrLayer::Top]
        );
        assert_eq!(
            layers_rendered_below_windows(false),
            &[WlrLayer::Bottom, WlrLayer::Background]
        );
        assert_eq!(
            layers_hit_tested_before_windows(false),
            &[WlrLayer::Overlay, WlrLayer::Top]
        );
        assert_eq!(
            layers_hit_tested_after_windows(false),
            &[WlrLayer::Bottom, WlrLayer::Background]
        );
    }

    #[test]
    fn fullscreen_moves_all_layer_surfaces_behind_windows() {
        assert!(layers_rendered_above_windows(true).is_empty());
        assert_eq!(
            layers_rendered_below_windows(true),
            &[
                WlrLayer::Overlay,
                WlrLayer::Top,
                WlrLayer::Bottom,
                WlrLayer::Background,
            ]
        );
        assert!(layers_hit_tested_before_windows(true).is_empty());
        assert!(layers_hit_tested_after_windows(true).is_empty());
    }
}
