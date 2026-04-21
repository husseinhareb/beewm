use std::os::fd::AsFd;
use std::time::Duration;

use crate::config::Config;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::GlesError;
use smithay::backend::renderer::glow::{GlowFrame, GlowRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::winit::{self, WinitEvent};
use smithay::input::pointer::CursorImageStatus;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::channel::Event as ChannelEvent;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::presentation::Refresh;
use smithay::wayland::socket::ListeningSocketSource;

use crate::compositor::commands::ChildEnvironment;
use crate::compositor::feedback::{
    collect_presentation_feedback, output_frame_interval, send_frame_callbacks,
    update_primary_scanout_output,
};
use crate::compositor::ipc;
use crate::compositor::layering::{layers_rendered_above_windows, layers_rendered_below_windows};
use crate::compositor::render::{layer_render_elements, window_render_elements};
use crate::compositor::state::{Beewm, ClientState};
use crate::xwayland::{delegate_backend_xwayland, start_xwayland};

struct WinitData {
    state: Beewm,
    display: Display<Beewm>,
    presentation_sequence: u64,
}

delegate_backend_xwayland!(WinitData, state);

enum WinitRenderElement {
    Surface(Box<WaylandSurfaceRenderElement<GlowRenderer>>),
    Border(SolidColorRenderElement),
}

impl From<WaylandSurfaceRenderElement<GlowRenderer>> for WinitRenderElement {
    fn from(value: WaylandSurfaceRenderElement<GlowRenderer>) -> Self {
        Self::Surface(Box::new(value))
    }
}

impl From<SolidColorRenderElement> for WinitRenderElement {
    fn from(value: SolidColorRenderElement) -> Self {
        Self::Border(value)
    }
}

impl Element for WinitRenderElement {
    fn id(&self) -> &Id {
        match self {
            Self::Surface(element) => element.id(),
            Self::Border(element) => element.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            Self::Surface(element) => element.current_commit(),
            Self::Border(element) => element.current_commit(),
        }
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        match self {
            Self::Surface(element) => element.location(scale),
            Self::Border(element) => element.location(scale),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            Self::Surface(element) => element.src(),
            Self::Border(element) => element.src(),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            Self::Surface(element) => element.transform(),
            Self::Border(element) => element.transform(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            Self::Surface(element) => element.geometry(scale),
            Self::Border(element) => element.geometry(scale),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            Self::Surface(element) => element.damage_since(scale, commit),
            Self::Border(element) => element.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            Self::Surface(element) => element.opaque_regions(scale),
            Self::Border(element) => element.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            Self::Surface(element) => element.alpha(),
            Self::Border(element) => element.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            Self::Surface(element) => element.kind(),
            Self::Border(element) => element.kind(),
        }
    }
}

impl RenderElement<GlowRenderer> for WinitRenderElement {
    fn draw(
        &self,
        frame: &mut GlowFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        match self {
            Self::Surface(element) => RenderElement::<GlowRenderer>::draw(
                element.as_ref(),
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
            Self::Border(element) => RenderElement::<GlowRenderer>::draw(
                element,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
            ),
        }
    }

    fn underlying_storage(&self, renderer: &mut GlowRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            Self::Surface(element) => element.as_ref().underlying_storage(renderer),
            Self::Border(element) => element.underlying_storage(renderer),
        }
    }
}

/// Run the compositor using the winit backend (nested inside an existing session).
pub fn run_winit(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<WinitData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);
    let display_fd = display.as_fd().try_clone_to_owned()?;
    let (_ipc_server, ipc_channel) = ipc::start()?;

    let mut data = WinitData {
        state,
        display,
        presentation_sequence: 0,
    };

    start_xwayland(event_loop.handle(), &display_handle, &mut data.state);

    // Set up the Wayland listening socket
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    tracing::info!("Wayland socket: {:?}", socket_name);

    event_loop
        .handle()
        .insert_source(listening_socket, |client_stream, _, data| {
            if let Err(e) = data
                .state
                .display_handle
                .insert_client(client_stream, std::sync::Arc::new(ClientState::default()))
            {
                tracing::error!("Failed to insert client: {}", e);
            }
        })?;

    // Wake when clients send requests; the Display itself stays in `WinitData`
    // so compositor-initiated configures and frame callbacks can be flushed on
    // every loop iteration as well.
    event_loop.handle().insert_source(
        Generic::new(
            display_fd,
            Interest::READ,
            smithay::reexports::calloop::Mode::Level,
        ),
        |_, _, data| {
            data.display.dispatch_clients(&mut data.state)?;
            data.display.flush_clients()?;
            Ok(PostAction::Continue)
        },
    )?;

    event_loop
        .handle()
        .insert_source(ipc_channel, |event, _, data| match event {
            ChannelEvent::Msg(command) => ipc::apply_command(&mut data.state, command),
            ChannelEvent::Closed => {
                tracing::warn!("Workspace IPC channel closed");
            }
        })?;

    // Initialize winit backend
    let (mut winit_backend, winit_evt) = winit::init::<GlowRenderer>()?;

    // Create the output
    let mode = Mode {
        size: winit_backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "beewm".into(),
            model: "winit".into(),
        },
    );

    output.create_global::<Beewm>(&display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    data.state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);

    // Insert winit event source
    event_loop
        .handle()
        .insert_source(winit_evt, move |event, _, data| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                if let Some(output) = data.state.space.outputs().next().cloned() {
                    output.change_current_state(Some(mode), None, None, None);
                }
                data.state.relayout();
            }
            WinitEvent::Input(event) => {
                crate::compositor::input::handle_input(&mut data.state, event);
            }
            WinitEvent::Focus(_) | WinitEvent::Redraw => {}
            WinitEvent::CloseRequested => {
                data.state.running = false;
            }
        })?;

    // Keep compositor-specific env on child processes instead of mutating the
    // global process environment, which is unsafe in Rust 2024.
    let mut child_env = ChildEnvironment::wayland(socket_name);
    child_env.set_sanitize_display(true);
    data.state.child_env = child_env;

    data.state.mark_output_ready();

    tracing::info!("Starting winit event loop");
    let mut applied_cursor_status_serial = u64::MAX;

    while data.state.running {
        if applied_cursor_status_serial != data.state.cursor_status_serial {
            apply_cursor(&winit_backend, &data.state.cursor_status);
            applied_cursor_status_serial = data.state.cursor_status_serial;
        }

        // Only render when something visual has changed.
        if data.state.needs_render {
            let output = data.state.space.outputs().next().cloned();
            if let Some(ref output) = output {
                let border_elements = data.state.border_elements();
                let mut submitted = false;

                let render_result = match winit_backend.bind() {
                    Ok((renderer, mut framebuffer)) => {
                        let fullscreen_active = data.state.fullscreen_window.is_some();
                        let window_elements =
                            window_render_elements(renderer, &data.state.space, output, 1.0);
                        let layers_above = layer_render_elements(
                            renderer,
                            output,
                            layers_rendered_above_windows(fullscreen_active),
                            1.0,
                        );
                        let layers_below = layer_render_elements(
                            renderer,
                            output,
                            layers_rendered_below_windows(fullscreen_active),
                            1.0,
                        );

                        let mut elements: Vec<WinitRenderElement> = Vec::new();
                        elements.extend(layers_above.into_iter().map(WinitRenderElement::from));
                        elements.extend(border_elements.into_iter().map(WinitRenderElement::from));
                        elements.extend(window_elements.into_iter().map(WinitRenderElement::from));
                        elements.extend(layers_below.into_iter().map(WinitRenderElement::from));

                        Some(damage_tracker.render_output(
                            renderer,
                            &mut framebuffer,
                            0,
                            &elements,
                            [0.1, 0.1, 0.1, 1.0],
                        ))
                    }
                    Err(err) => {
                        tracing::error!("winit bind failed: {:?}", err);
                        None
                    }
                };

                match render_result {
                    Some(Ok(render_result)) => {
                        update_primary_scanout_output(&data.state, output, &render_result.states);

                        if let Some(damage) = render_result.damage {
                            if let Err(err) = winit_backend.submit(Some(damage)) {
                                tracing::error!("winit submit failed: {:?}", err);
                            } else {
                                submitted = true;
                                let elapsed = data.state.start_time.elapsed();
                                send_frame_callbacks(
                                    &data.state,
                                    output,
                                    elapsed,
                                    Some(Duration::ZERO),
                                );

                                let mut presentation_feedback = collect_presentation_feedback(
                                    &data.state,
                                    output,
                                    &render_result.states,
                                );
                                data.presentation_sequence =
                                    data.presentation_sequence.wrapping_add(1);
                                presentation_feedback.presented(
                                    data.state.presentation_clock.now(),
                                    Refresh::fixed(output_frame_interval(output)),
                                    data.presentation_sequence,
                                    smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                                );
                            }
                        } else {
                            send_frame_callbacks(
                                &data.state,
                                output,
                                data.state.start_time.elapsed(),
                                Some(Duration::ZERO),
                            );
                            submitted = true;
                        }
                    }
                    Some(Err(err)) => {
                        tracing::error!("winit render_output failed: {:?}", err);
                    }
                    None => {}
                }

                if submitted {
                    data.state.needs_render = false;
                }
            }
        }

        // Dispatch event loop
        let timeout = Duration::from_millis(16);
        event_loop.dispatch(Some(timeout), &mut data)?;

        // Process pending surface state (sends wl_surface.enter/leave)
        // BEFORE flushing so clients receive enter events in the same
        // batch as configures and frame callbacks.
        data.state.space.refresh();
        data.display.flush_clients()?;
    }

    Ok(())
}

fn apply_cursor(
    backend: &smithay::backend::winit::WinitGraphicsBackend<GlowRenderer>,
    status: &CursorImageStatus,
) {
    match status {
        CursorImageStatus::Hidden => backend.window().set_cursor_visible(false),
        CursorImageStatus::Named(icon) => {
            backend.window().set_cursor_visible(true);
            backend.window().set_cursor(*icon);
        }
        CursorImageStatus::Surface(_) => {
            // wl_pointer cursor surfaces are not yet software-rendered in the nested backend,
            // so fall back to a standard arrow instead of leaving the cursor blank.
            backend.window().set_cursor_visible(true);
            backend
                .window()
                .set_cursor(smithay::input::pointer::CursorIcon::Default);
        }
    }
}

//
