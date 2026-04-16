use std::os::fd::AsFd;
use std::time::Duration;

use crate::config::Config;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::glow::GlowRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::input::pointer::CursorImageStatus;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::utils::Transform;
use smithay::wayland::presentation::Refresh;
use smithay::wayland::socket::ListeningSocketSource;

use crate::compositor::commands::spawn_startup_commands;
use crate::compositor::feedback::{
    collect_presentation_feedback, output_frame_interval, send_frame_callbacks,
    update_primary_scanout_output,
};
use crate::compositor::state::{Beewm, ClientState};

struct WinitData {
    state: Beewm,
    display: Display<Beewm>,
    presentation_sequence: u64,
}

/// Run the compositor using the winit backend (nested inside an existing session).
pub fn run_winit(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<WinitData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);
    let display_fd = display.as_fd().try_clone_to_owned()?;

    let mut data = WinitData {
        state,
        display,
        presentation_sequence: 0,
    };

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

    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Declare this as a Wayland session — GTK, Qt, and Electron all check
    // XDG_SESSION_TYPE and auto-select their Wayland backends from it.
    // Do NOT set GDK_BACKEND or QT_QPA_PLATFORM directly: those override
    // auto-detection and crash apps when optional protocols are missing.
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    // Force Electron/Chromium onto native Wayland so they exercise the nested
    // compositor instead of falling back to the host X11 session.
    std::env::set_var("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
    std::env::set_var("NIXOS_OZONE_WL", "1");

    data.state.sanitize_display_for_children = true;

    spawn_startup_commands(
        &data.state.config.autostart_commands,
        data.state.sanitize_display_for_children,
    );

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
                        Some(smithay::desktop::space::render_output::<
                            _,
                            SolidColorRenderElement,
                            _,
                            _,
                        >(
                            output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&data.state.space],
                            &border_elements,
                            &mut damage_tracker,
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
        data.display.flush_clients()?;

        // Process pending surface state
        data.state.space.refresh();
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
