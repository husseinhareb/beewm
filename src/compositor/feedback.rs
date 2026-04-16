use std::time::Duration;

use smithay::backend::renderer::element::{
    default_primary_scanout_output_compare, RenderElementStates,
};
use smithay::desktop::{
    layer_map_for_output,
    utils::{
        surface_presentation_feedback_flags_from_states, surface_primary_scanout_output,
        update_surface_primary_scanout_output, OutputPresentationFeedback,
    },
};
use smithay::output::Output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use super::state::Beewm;

pub fn output_frame_interval(output: &Output) -> Duration {
    output
        .current_mode()
        .map(|mode| Duration::from_micros(1_000_000_000 / mode.refresh as u64))
        .unwrap_or(Duration::from_millis(16))
}

pub fn update_primary_scanout_output(
    state: &Beewm,
    output: &Output,
    render_states: &RenderElementStates,
) {
    state.space.elements().for_each(|window| {
        window.with_surfaces(|surface, surface_data| {
            let _ = update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                render_states,
                default_primary_scanout_output_compare,
            );
        });
    });

    let layer_map = layer_map_for_output(output);
    for layer in layer_map
        .layers_on(WlrLayer::Background)
        .chain(layer_map.layers_on(WlrLayer::Bottom))
        .chain(layer_map.layers_on(WlrLayer::Top))
        .chain(layer_map.layers_on(WlrLayer::Overlay))
    {
        layer.with_surfaces(|surface, surface_data| {
            let _ = update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                render_states,
                default_primary_scanout_output_compare,
            );
        });
    }
}

pub fn send_frame_callbacks(
    state: &Beewm,
    output: &Output,
    time: impl Into<Duration>,
    throttle: Option<Duration>,
) {
    let time = time.into();

    state.space.elements().for_each(|window| {
        window.send_frame(output, time, throttle, surface_primary_scanout_output);
    });

    let layer_map = layer_map_for_output(output);
    for layer in layer_map.layers() {
        layer.send_frame(output, time, throttle, surface_primary_scanout_output);
    }
}

pub fn collect_presentation_feedback(
    state: &Beewm,
    output: &Output,
    render_states: &RenderElementStates,
) -> OutputPresentationFeedback {
    let mut output_feedback = OutputPresentationFeedback::new(output);

    state.space.elements().for_each(|window| {
        window.take_presentation_feedback(
            &mut output_feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, render_states),
        );
    });

    let layer_map = layer_map_for_output(output);
    for layer in layer_map.layers() {
        layer.take_presentation_feedback(
            &mut output_feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, render_states),
        );
    }

    output_feedback
}
