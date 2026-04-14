use std::os::fd::AsFd;
use std::time::Duration;

use beewm_core::config::Config;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::glow::GlowRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::utils::Transform;
use smithay::wayland::socket::ListeningSocketSource;

use crate::state::{Beewm, CalloopData, ClientState};

struct WinitData {
    calloop: CalloopData,
    display: Display<Beewm>,
}

/// Run the compositor using the winit backend (nested inside an existing session).
pub fn run_winit(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<WinitData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);
    let display_fd = display.as_fd().try_clone_to_owned()?;

    let mut data = WinitData {
        calloop: CalloopData {
            state,
            display_handle: display_handle.clone(),
        },
        display,
    };

    // Set up the Wayland listening socket
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    tracing::info!("Wayland socket: {:?}", socket_name);

    event_loop.handle().insert_source(
        listening_socket,
        |client_stream, _, data| {
            if let Err(e) = data.calloop.display_handle.insert_client(
                client_stream,
                std::sync::Arc::new(ClientState::default()),
            ) {
                tracing::error!("Failed to insert client: {}", e);
            }
        },
    )?;

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
            data.display.dispatch_clients(&mut data.calloop.state)?;
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
    data.calloop.state.space.map_output(&output, (0, 0));

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
                if let Some(output) = data.calloop.state.space.outputs().next().cloned() {
                    output.change_current_state(Some(mode), None, None, None);
                }
                data.calloop.state.relayout();
            }
            WinitEvent::Input(event) => {
                crate::input::handle_input(&mut data.calloop.state, event);
            }
            WinitEvent::Focus(_) | WinitEvent::Redraw => {}
            WinitEvent::CloseRequested => {
                data.calloop.state.running = false;
            }
        })?;

    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Declare this as a Wayland session — GTK, Qt, and Electron all check
    // XDG_SESSION_TYPE and auto-select their Wayland backends from it.
    // Do NOT set GDK_BACKEND or QT_QPA_PLATFORM directly: those override
    // auto-detection and crash apps when optional protocols are missing.
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    // Electron/Chromium (VS Code, etc.).
    std::env::set_var("ELECTRON_OZONE_PLATFORM_HINT", "auto");
    std::env::set_var("NIXOS_OZONE_WL", "1");

    tracing::info!("Starting winit event loop");

    while data.calloop.state.running {
        // Only render when something visual has changed.
        if data.calloop.state.needs_render {
            let output = data.calloop.state.space.outputs().next().cloned();
            if let Some(ref output) = output {
                let size = winit_backend.window_size();
                let damage = smithay::utils::Rectangle::from_size(size);

                let border_elements = data.calloop.state.border_elements();
                let mut submitted = false;

                let render_result = match winit_backend.bind() {
                    Ok((renderer, mut framebuffer)) => Some(
                        smithay::desktop::space::render_output::<
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
                            [&data.calloop.state.space],
                            &border_elements,
                            &mut damage_tracker,
                            [0.1, 0.1, 0.1, 1.0],
                        ),
                    ),
                    Err(err) => {
                        tracing::error!("winit bind failed: {:?}", err);
                        None
                    }
                };

                match render_result {
                    Some(Ok(_)) => {
                        if let Err(err) = winit_backend.submit(Some(&[damage])) {
                            tracing::error!("winit submit failed: {:?}", err);
                        } else {
                            submitted = true;
                        }
                    }
                    Some(Err(err)) => {
                        tracing::error!("winit render_output failed: {:?}", err);
                    }
                    None => {}
                }

                if submitted {
                    // Tell clients to draw their next frame
                    let elapsed = data.calloop.state.start_time.elapsed();
                    data.calloop.state.space.elements().for_each(|window| {
                        window.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                            Some(output.clone())
                        });
                    });

                    // Send frame to layer surfaces too
                    let layer_map = smithay::desktop::layer_map_for_output(output);
                    for layer in layer_map.layers() {
                        layer.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                            Some(output.clone())
                        });
                    }
                    data.calloop.state.needs_render = false;
                }
            }
        }

        // Dispatch event loop
        let timeout = Duration::from_millis(16);
        event_loop.dispatch(Some(timeout), &mut data)?;
        data.display.flush_clients()?;

        // Process pending surface state
        data.calloop.state.space.refresh();
    }

    Ok(())
}
