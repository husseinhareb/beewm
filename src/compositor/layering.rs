use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

const LAYERS_ABOVE_WINDOWS: [WlrLayer; 2] = [WlrLayer::Overlay, WlrLayer::Top];
const LAYERS_BELOW_WINDOWS: [WlrLayer; 2] = [WlrLayer::Bottom, WlrLayer::Background];
const FULLSCREEN_LAYERS_BELOW_WINDOWS: [WlrLayer; 4] = [
    WlrLayer::Overlay,
    WlrLayer::Top,
    WlrLayer::Bottom,
    WlrLayer::Background,
];

pub fn layers_rendered_above_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_ABOVE_WINDOWS
    }
}

pub fn layers_rendered_below_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &FULLSCREEN_LAYERS_BELOW_WINDOWS
    } else {
        &LAYERS_BELOW_WINDOWS
    }
}

pub fn layers_hit_tested_before_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_ABOVE_WINDOWS
    }
}

pub fn layers_hit_tested_after_windows(fullscreen_active: bool) -> &'static [WlrLayer] {
    if fullscreen_active {
        &[]
    } else {
        &LAYERS_BELOW_WINDOWS
    }
}
