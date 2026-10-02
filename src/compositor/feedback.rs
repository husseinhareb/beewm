use std::time::Duration;

use smithay::backend::renderer::element::{
    RenderElementStates, default_primary_scanout_output_compare,
};
use smithay::desktop::{
    layer_map_for_output,
    utils::{
        OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
        surface_primary_scanout_output, update_surface_primary_scanout_output,
    },
};
use smithay::output::Output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use super::state::Beewm;

/// The nominal time between two frames on `output`, from its current mode's
/// refresh rate (in mHz). Virtual/headless outputs and displays with a broken
/// EDID can report `refresh <= 0`; falling back to ~60Hz keeps that from
/// dividing by zero and taking the compositor down.
pub fn output_frame_interval(output: &Output) -> Duration {
    output
        .current_mode()
        .filter(|mode| mode.refresh > 0)
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

    // The space only holds the active workspace, so every other workspace's
    // clients are getting no frame callbacks and have stopped drawing. That is
    // exactly what should happen normally — but the overview puts those windows
    // on screen as live thumbnails, and without a callback a video or animation
    // sits frozen on whatever frame it last committed.
    //
    // They have no primary scanout output either (nothing has been scanning
    // them out), so the callback is addressed to the overview's own output
    // rather than looked up per surface. This lasts only while the grid is
    // held open.
    if let Some(overview) = state.overview.as_ref()
        && overview.output == *output
    {
        for item in &overview.items {
            if state.space.elements().any(|window| *window == item.window) {
                continue;
            }
            item.window
                .send_frame(output, time, throttle, |_, _| Some(output.clone()));
        }
    }

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
