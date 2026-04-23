use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;

use smithay::backend::allocator::Format;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmEventTime};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::InputEvent;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::Event as SessionEvent;
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::channel::Event as ChannelEvent;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction, RegistrationToken};
use smithay::reexports::drm::control::{Device as ControlDevice, ModeTypeFlags, connector, crtc};
use smithay::reexports::input::Libinput;
use smithay::reexports::input::ScrollMethod;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};
use smithay::utils::{Monotonic, Time};
use smithay::wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd};
use smithay::wayland::presentation::Refresh;
use smithay::wayland::socket::ListeningSocketSource;

use crate::compositor::commands::ChildEnvironment;
use crate::compositor::feedback::{
    collect_presentation_feedback, output_frame_interval, send_frame_callbacks,
    update_primary_scanout_output,
};
use crate::compositor::ipc;
use crate::compositor::layering::{layers_rendered_above_windows, layers_rendered_below_windows};
use crate::compositor::render::{
    OutputRenderElement, layer_render_elements, window_render_elements,
};
use crate::compositor::screencopy::process_pending_screencopies;
use crate::compositor::state::{Beewm, ClientState, lookup_client_compositor_state};
use crate::xwayland::{delegate_backend_xwayland, start_xwayland};

/// Per-GPU state for the DRM backend.
struct GpuData {
    _drm_device: DrmDevice,
    _drm_notifier_token: RegistrationToken,
    _gbm_device: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    compositor: DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    output: Output,
    /// True when a vblank has fired and we may render the next frame.
    can_render: bool,
    pending_presentation_feedback: Option<smithay::desktop::utils::OutputPresentationFeedback>,
}

/// Top-level calloop data for the DRM/udev backend —
/// combines compositor state with GPU state so VBlank handlers can reach both.
struct UdevData {
    state: Beewm,
    gpu: Option<GpuData>,
    /// Owned so we can call flush_clients() anywhere in the main loop.
    display: Display<Beewm>,
}

delegate_backend_xwayland!(UdevData, state);

/// Run the compositor on real hardware from a TTY using DRM/KMS.
pub fn run_udev(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<UdevData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);
    let (_ipc_server, ipc_channel) = ipc::start()?;

    // Clone the display fd before moving display into UdevData — used to
    // wake calloop when clients send data.
    let display_fd = display
        .as_fd()
        .try_clone_to_owned()
        .expect("Failed to clone wayland display fd");

    let mut data = UdevData {
        state,
        gpu: None,
        display,
    };

    start_xwayland(event_loop.handle(), &display_handle, &mut data.state);

    let loop_handle = event_loop.handle();
    data.state
        .install_syncobj_blocker_source(Box::new(move |source, client| {
            let client = client.clone();
            if let Err(error) =
                loop_handle.insert_source(source, move |(), _, data: &mut UdevData| {
                    if let Some(client_state) = lookup_client_compositor_state(&client) {
                        let display_handle = data.state.display_handle.clone();
                        client_state.blocker_cleared(&mut data.state, &display_handle);
                    }
                    Ok(())
                })
            {
                tracing::warn!("Failed to install explicit-sync fence source: {}", error);
            }
        }));

    // --- Session ---
    let (mut session, notifier) = LibSeatSession::new()?;
    tracing::info!("Session opened on seat: {}", session.seat());

    event_loop
        .handle()
        .insert_source(notifier, |event, _, data| match event {
            SessionEvent::PauseSession => {
                tracing::info!("Session paused");
                if let Some(gpu) = data.gpu.as_mut() {
                    gpu._drm_device.pause();
                    gpu.can_render = false;
                }
                data.state.needs_render = false;
            }
            SessionEvent::ActivateSession => {
                tracing::info!("Session activated");
                if let Some(gpu) = data.gpu.as_mut() {
                    if let Err(err) = gpu._drm_device.activate(true) {
                        tracing::error!("Failed to reactivate DRM device: {}", err);
                        gpu.can_render = false;
                        return;
                    }
                    if let Err(err) = gpu.compositor.reset_state() {
                        tracing::error!(
                            "Failed to reset compositor state after reactivation: {}",
                            err
                        );
                    }
                    gpu.can_render = true;
                }
                data.state.needs_render = true;
            }
        })?;

    // --- Wayland socket ---
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    tracing::info!("Wayland socket: {:?}", socket_name);
    // Keep compositor-specific env on child processes instead of mutating the
    // global process environment, which is unsafe in Rust 2024.
    let mut child_env = ChildEnvironment::wayland(socket_name);
    child_env.set_sanitize_display(true);

    // Ensure XDG_RUNTIME_DIR is set — required by Wayland clients like kitty.
    // seatd/logind normally sets this; provide a fallback for bare TTY sessions.
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        let uid = unsafe { libc::getuid() };
        let path = format!("/run/user/{}", uid);
        if std::path::Path::new(&path).exists() {
            child_env.set("XDG_RUNTIME_DIR", &path);
            tracing::info!("Set XDG_RUNTIME_DIR to {}", path);
        }
    }
    data.state.child_env = child_env;

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

    // Register the display fd so calloop wakes up when clients send data.
    // dispatch_clients + flush_clients are called via data.display below.
    event_loop.handle().insert_source(
        Generic::new(
            display_fd,
            Interest::READ,
            smithay::reexports::calloop::Mode::Level,
        ),
        |_, _, data: &mut UdevData| {
            data.display
                .dispatch_clients(&mut data.state)
                .map_err(std::io::Error::other)?;
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

    // --- Libinput ---
    let mut libinput_context =
        Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
    libinput_context
        .udev_assign_seat(&session.seat())
        .map_err(|_| "Failed to assign libinput seat")?;

    let libinput_backend = LibinputInputBackend::new(libinput_context);
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, data| {
            // Tap-to-click is a libinput-specific feature; configure it as
            // devices appear (e.g. touchpad at startup or on hotplug).
            if let InputEvent::DeviceAdded { mut device } = event {
                let is_touchpad = device.config_tap_finger_count() > 0;
                if is_touchpad {
                    // Tap-to-click
                    let tap = data.state.config.tap_to_click;
                    let r = device.config_tap_set_enabled(tap);
                    tracing::info!(
                        "libinput: tap-to-click {} on '{}' ({:?})",
                        if tap { "enabled" } else { "disabled" },
                        device.name(),
                        r,
                    );

                    // Two-finger scroll — enable it when the device supports it
                    let supported = device.config_scroll_methods();
                    if supported.contains(&ScrollMethod::TwoFinger) {
                        let r = device.config_scroll_set_method(ScrollMethod::TwoFinger);
                        tracing::info!(
                            "libinput: two-finger scroll enabled on '{}' ({:?})",
                            device.name(),
                            r,
                        );
                    }

                    // Natural (reversed) scroll direction
                    if device.config_scroll_has_natural_scroll() {
                        let natural = data.state.config.natural_scroll;
                        let r = device.config_scroll_set_natural_scroll_enabled(natural);
                        tracing::info!(
                            "libinput: natural scroll {} on '{}' ({:?})",
                            if natural { "enabled" } else { "disabled" },
                            device.name(),
                            r,
                        );
                    }
                }
                return;
            }
            crate::compositor::input::handle_input(&mut data.state, event);
        })?;

    // --- Udev: enumerate GPUs ---
    let udev = UdevBackend::new(session.seat())?;

    for (device_id, path) in udev.device_list() {
        tracing::info!("Found DRM device: {} at {}", device_id, path.display());
        if data.gpu.is_none() {
            match init_gpu(&mut session, &event_loop, &display_handle, path) {
                Ok((gd, dmabuf_formats, syncobj_state)) => {
                    data.state.space.map_output(&gd.output, (0, 0));
                    data.gpu = Some(gd);
                    data.state.drm_syncobj_state = syncobj_state;
                    let display_handle = data.state.display_handle.clone();
                    data.state._dmabuf_global = Some(
                        data.state
                            .dmabuf_state
                            .create_global::<Beewm>(&display_handle, dmabuf_formats),
                    );
                }
                Err(e) => tracing::warn!("Failed to init GPU {}: {}", path.display(), e),
            }
        }
    }

    // Insert udev for hotplug (we don't handle hotplug in detail yet)
    event_loop
        .handle()
        .insert_source(udev, |event, _, _data| match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!("DRM device added: {} at {}", device_id, path.display());
            }
            UdevEvent::Changed { device_id } => {
                tracing::info!("DRM device changed: {}", device_id);
            }
            UdevEvent::Removed { device_id } => {
                tracing::info!("DRM device removed: {}", device_id);
            }
        })?;

    if data.gpu.is_none() {
        return Err("No usable GPU found".into());
    }

    // Store session for VT switching
    data.state.session = Some(session.clone());

    // Start autostart clients only after an output exists and XWayland has
    // produced a usable DISPLAY (or failed to do so).
    data.state.mark_output_ready();

    tracing::info!("Starting udev event loop");

    while data.state.running {
        let timeout = if data.state.active_grab.is_some() || data.state.needs_render {
            Duration::from_millis(1)
        } else {
            Duration::from_millis(20)
        };
        event_loop.dispatch(Some(timeout), &mut data)?;
        // Process pending surface state (sends wl_surface.enter/leave)
        // BEFORE flushing so clients receive enter events in the same
        // batch as configures and frame callbacks.
        data.state.space.refresh();
        // Flush outgoing Wayland events (configure, enter, frame callbacks, etc.)
        // MUST be called every loop iteration — without this, clients never
        // receive compositor-initiated events such as xdg_toplevel.configure.
        if let Err(err) = data.display.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients: {}", err);
        }
        // Only render when the previous frame has been presented (VBlank fired)
        // AND something visual has actually changed. Rendering after dispatch
        // keeps live resizes closer to the latest pointer and commit state.
        if data.gpu.as_ref().is_some_and(|g| g.can_render) && data.state.needs_render {
            render_frame(&mut data);
        }
    }

    Ok(())
}

/// Render the current state into the DRM framebuffer and queue it.
fn render_frame(data: &mut UdevData) {
    let gpu = match data.gpu.as_mut() {
        Some(g) => g,
        None => return,
    };
    gpu.can_render = false;

    let window_elements =
        window_render_elements(&mut gpu.renderer, &data.state.space, &gpu.output, 1.0);

    let border_elements = data.state.border_elements();
    let cursor_elements = data.state.cursor_elements(&mut gpu.renderer);

    // Render layer-shell surfaces (waybar, beebar, etc.) at the correct Z-order.
    // Clone output so we can borrow it for layer_map while also using gpu.renderer.
    let output = gpu.output.clone();
    let fullscreen_active = data.state.fullscreen_window.is_some();

    let layers_below = layer_render_elements(
        &mut gpu.renderer,
        &output,
        layers_rendered_below_windows(fullscreen_active),
        1.0,
    );
    let layers_above = layer_render_elements(
        &mut gpu.renderer,
        &output,
        layers_rendered_above_windows(fullscreen_active),
        1.0,
    );

    process_pending_screencopies(&mut data.state, &mut gpu.renderer, &output);

    // Build final element list front-to-back (first = topmost).
    let mut elements: Vec<OutputRenderElement> = Vec::new();
    elements.extend(cursor_elements.into_iter().map(OutputRenderElement::from));
    elements.extend(layers_above.into_iter().map(OutputRenderElement::from));
    elements.extend(border_elements.into_iter().map(OutputRenderElement::from));
    elements.extend(window_elements.into_iter().map(OutputRenderElement::from));
    elements.extend(layers_below.into_iter().map(OutputRenderElement::from));

    let gpu = data.gpu.as_mut().unwrap();

    let result = gpu.compositor.render_frame::<_, OutputRenderElement>(
        &mut gpu.renderer,
        &elements,
        [0.1, 0.1, 0.1, 1.0],
        FrameFlags::DEFAULT,
    );

    match result {
        Ok(result) => {
            let render_states = result.states.clone();
            update_primary_scanout_output(&data.state, &output, &render_states);

            if result.is_empty {
                // No damage — nothing to scan out.  Clear the render request
                // so we don't spin; the next surface commit / relayout will
                // set `needs_render = true` again.
                data.state.needs_render = false;
                gpu.can_render = true;
                gpu.pending_presentation_feedback = None;
                // No VBlank will fire, so send frame callbacks now to keep
                // clients from stalling.
                let elapsed = data.state.start_time.elapsed();
                send_frame_callbacks(
                    &data.state,
                    &output,
                    elapsed,
                    Some(output_frame_interval(&output)),
                );
            } else if let Err(e) = gpu.compositor.queue_frame(()) {
                tracing::error!("Failed to queue frame: {:?}", e);
                gpu.can_render = true;
                gpu.pending_presentation_feedback = None;
                // Frame was never queued — no VBlank coming; unblock clients.
                let elapsed = data.state.start_time.elapsed();
                send_frame_callbacks(&data.state, &output, elapsed, None);
            } else {
                gpu.pending_presentation_feedback = Some(collect_presentation_feedback(
                    &data.state,
                    &output,
                    &render_states,
                ));
            }
            // For the normal non-empty case, frame callbacks are sent from the
            // VBlank handler once the hardware confirms the frame is on screen.
        }
        Err(e) => {
            tracing::error!("Render error: {:?}", e);
            gpu.can_render = true;
            gpu.pending_presentation_feedback = None;
            // Render failed — no VBlank coming; unblock clients.
            let elapsed = data.state.start_time.elapsed();
            send_frame_callbacks(&data.state, &output, elapsed, None);
        }
    }
}

fn init_gpu(
    session: &mut LibSeatSession,
    event_loop: &EventLoop<UdevData>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    path: &Path,
) -> Result<(GpuData, Vec<Format>, Option<DrmSyncobjState>), Box<dyn std::error::Error>> {
    // Open DRM device via session
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let device_fd: DeviceFd = fd.into();
    let drm_fd = DrmDeviceFd::new(device_fd);

    let (mut drm_device, drm_notifier) = DrmDevice::new(drm_fd.clone(), false)?;

    // Find a connected connector and pick the preferred mode
    let resources = drm_fd.resource_handles()?;
    let mut selected_connector = None;
    let mut selected_mode = None;

    for conn_handle in resources.connectors() {
        if let Ok(conn_info) = drm_fd.get_connector(*conn_handle, false) {
            if conn_info.state() == connector::State::Connected && !conn_info.modes().is_empty() {
                // Pick the preferred mode, or first available
                let mode = conn_info
                    .modes()
                    .iter()
                    .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                    .copied()
                    .unwrap_or(conn_info.modes()[0]);

                selected_connector = Some(*conn_handle);
                selected_mode = Some(mode);
                tracing::info!(
                    "Using connector {:?}, mode {}x{}@{}",
                    conn_handle,
                    mode.size().0,
                    mode.size().1,
                    mode.vrefresh()
                );
                break;
            }
        }
    }

    let connector_handle = selected_connector.ok_or("No connected display found")?;
    let drm_mode = selected_mode.ok_or("No display mode available")?;

    // Find a suitable CRTC for this connector
    let crtc_handle = find_crtc_for_connector(&drm_fd, &resources, connector_handle)?;

    // Create DRM surface
    let drm_surface = drm_device.create_surface(crtc_handle, drm_mode, &[connector_handle])?;

    // Create GBM device
    let gbm_device = GbmDevice::new(drm_fd.clone())?;

    // Create EGL display + context + renderer
    let egl_display = unsafe { EGLDisplay::new(gbm_device.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer_formats = egl_display
        .dmabuf_render_formats()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let dmabuf_formats = renderer_formats.clone();

    let renderer = unsafe { GlesRenderer::new(egl_context)? };

    // Create GBM allocator + framebuffer exporter
    let gbm_allocator = GbmAllocator::new(
        gbm_device.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let gbm_exporter = GbmFramebufferExporter::new(gbm_device.clone(), None);

    // Create smithay Output
    let (phys_w, phys_h) = {
        if let Ok(info) = drm_fd.get_connector(connector_handle, false) {
            let size = info.size().unwrap_or((0, 0));
            (size.0 as i32, size.1 as i32)
        } else {
            (0, 0)
        }
    };

    let output = Output::new(
        format!("{:?}", connector_handle),
        PhysicalProperties {
            size: (phys_w, phys_h).into(),
            subpixel: Subpixel::Unknown,
            make: "beewm".into(),
            model: "drm".into(),
        },
    );

    let output_mode = OutputMode {
        size: (drm_mode.size().0 as i32, drm_mode.size().1 as i32).into(),
        refresh: (drm_mode.vrefresh() * 1000) as i32,
    };

    output.create_global::<Beewm>(display_handle);
    output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);

    // Create DRM compositor
    let cursor_size = drm_device.cursor_size();

    use smithay::backend::allocator::Fourcc;
    let color_formats = [Fourcc::Argb8888, Fourcc::Xrgb8888];

    let compositor = DrmCompositor::new(
        &output,
        drm_surface,
        None,
        gbm_allocator,
        gbm_exporter,
        color_formats,
        renderer_formats,
        cursor_size,
        Some(gbm_device.clone()),
    )?;

    let syncobj_state = if supports_syncobj_eventfd(&drm_fd) {
        Some(DrmSyncobjState::new::<Beewm>(
            display_handle,
            drm_fd.clone(),
        ))
    } else {
        tracing::info!("DRM syncobj eventfd unsupported on {}", path.display());
        None
    };

    // VBlank: frame was presented — acknowledge it and allow the next render.
    let drm_notifier_token = event_loop.handle().insert_source(
        drm_notifier,
        |event, metadata, data: &mut UdevData| match event {
            DrmEvent::VBlank(_crtc) => {
                if let Some(gpu) = data.gpu.as_mut() {
                    if let Err(e) = gpu.compositor.frame_submitted() {
                        tracing::error!("frame_submitted error: {:?}", e);
                    }
                    gpu.can_render = true;
                    let refresh = Refresh::fixed(output_frame_interval(&gpu.output));
                    let presentation_time = metadata
                        .as_ref()
                        .and_then(|meta| match meta.time {
                            DrmEventTime::Monotonic(duration) => Some(Time::<Monotonic>::from(duration)),
                            DrmEventTime::Realtime(_) => None,
                        })
                        .unwrap_or_else(|| data.state.presentation_clock.now());
                    let sequence = metadata
                        .as_ref()
                        .map(|meta| meta.sequence as u64)
                        .unwrap_or(0);
                    if let Some(mut feedback) = gpu.pending_presentation_feedback.take() {
                        feedback.presented(
                            presentation_time,
                            refresh,
                            sequence,
                            smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                        );
                    }
                }

                // Frame is now on screen — send frame callbacks so clients
                // render their next frame in sync with the display VBlank.
                let elapsed = data.state.start_time.elapsed();
                if let Some(gpu) = data.gpu.as_ref() {
                    send_frame_callbacks(
                        &data.state,
                        &gpu.output,
                        elapsed,
                        Some(output_frame_interval(&gpu.output)),
                    );
                }
            }
            DrmEvent::Error(e) => tracing::error!("DRM error: {:?}", e),
        },
    )?;

    Ok((
        GpuData {
            _drm_device: drm_device,
            _drm_notifier_token: drm_notifier_token,
            _gbm_device: gbm_device,
            renderer,
            compositor,
            output,
            can_render: true, // allow first frame immediately
            pending_presentation_feedback: None,
        },
        dmabuf_formats,
        syncobj_state,
    ))
}

/// Find a CRTC that can drive the given connector.
fn find_crtc_for_connector(
    drm: &DrmDeviceFd,
    resources: &smithay::reexports::drm::control::ResourceHandles,
    connector: connector::Handle,
) -> Result<crtc::Handle, Box<dyn std::error::Error>> {
    let conn_info = drm.get_connector(connector, false)?;

    for encoder_handle in conn_info.encoders() {
        if let Ok(encoder_info) = drm.get_encoder(*encoder_handle) {
            let possible_crtcs = encoder_info.possible_crtcs();
            let crtcs = resources.filter_crtcs(possible_crtcs);
            if let Some(&crtc_handle) = crtcs.first() {
                return Ok(crtc_handle);
            }
        }
    }

    Err("No suitable CRTC found for connector".into())
}
