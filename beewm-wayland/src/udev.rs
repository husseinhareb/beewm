use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use beewm_core::config::Config;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Format;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::Event as SessionEvent;
use smithay::backend::session::Session;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::desktop::space::space_render_elements;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::backend::input::InputEvent;
use smithay::reexports::input::ScrollMethod;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};
use smithay::wayland::socket::ListeningSocketSource;

use crate::render::OutputRenderElement;
use crate::state::{Beewm, CalloopData, ClientState};

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
    dmabuf_formats: Vec<Format>,
    /// True when a vblank has fired and we may render the next frame.
    can_render: bool,
}

/// Top-level calloop data for the DRM/udev backend —
/// combines compositor state with GPU state so VBlank handlers can reach both.
struct UdevData {
    calloop: CalloopData,
    gpu: Option<GpuData>,
    /// Owned so we can call flush_clients() anywhere in the main loop.
    display: Display<Beewm>,
}

/// Run the compositor on real hardware from a TTY using DRM/KMS.
pub fn run_udev(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<UdevData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);

    // Clone the display fd before moving display into UdevData — used to
    // wake calloop when clients send data.
    let display_fd = display.as_fd().try_clone_to_owned()
        .expect("Failed to clone wayland display fd");

    let mut data = UdevData {
        calloop: CalloopData {
            state,
            display_handle: display_handle.clone(),
        },
        gpu: None,
        display,
    };

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
                data.calloop.state.needs_render = false;
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
                        tracing::error!("Failed to reset compositor state after reactivation: {}", err);
                    }
                    gpu.can_render = true;
                }
                data.calloop.state.needs_render = true;
            }
        })?;

    // --- Wayland socket ---
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    tracing::info!("Wayland socket: {:?}", socket_name);
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    // Declare this as a Wayland session — GTK, Qt, and Electron all check
    // XDG_SESSION_TYPE and auto-select their Wayland backends from it.
    // Do NOT set GDK_BACKEND or QT_QPA_PLATFORM directly: those override
    // auto-detection and crash apps when optional protocols are missing.
    std::env::set_var("XDG_SESSION_TYPE", "wayland");
    // Electron/Chromium (VS Code, etc.).
    std::env::set_var("ELECTRON_OZONE_PLATFORM_HINT", "auto");
    std::env::set_var("NIXOS_OZONE_WL", "1");

    // Ensure XDG_RUNTIME_DIR is set — required by Wayland clients like kitty.
    // seatd/logind normally sets this; provide a fallback for bare TTY sessions.
    if std::env::var("XDG_RUNTIME_DIR").is_err() {
        let uid = unsafe { libc::getuid() };
        let path = format!("/run/user/{}", uid);
        if std::path::Path::new(&path).exists() {
            std::env::set_var("XDG_RUNTIME_DIR", &path);
            tracing::info!("Set XDG_RUNTIME_DIR to {}", path);
        }
    }

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
                .dispatch_clients(&mut data.calloop.state)
                .map_err(std::io::Error::other)?;
            data.display.flush_clients()?;
            Ok(PostAction::Continue)
        },
    )?;

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
                    let tap = data.calloop.state.config.tap_to_click;
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
                        let natural = data.calloop.state.config.natural_scroll;
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
            crate::input::handle_input(&mut data.calloop.state, event);
        })?;

    // --- Udev: enumerate GPUs ---
    let udev = UdevBackend::new(session.seat())?;

    for (device_id, path) in udev.device_list() {
        tracing::info!("Found DRM device: {} at {}", device_id, path.display());
        if data.gpu.is_none() {
            match init_gpu(
                &mut session,
                &event_loop,
                &display_handle,
                path,
            ) {
                Ok(gd) => {
                    data.calloop.state.space.map_output(&gd.output, (0, 0));
                    data.gpu = Some(gd);
                }
                Err(e) => tracing::warn!("Failed to init GPU {}: {}", path.display(), e),
            }
        }
    }

    // Insert udev for hotplug (we don't handle hotplug in detail yet)
    event_loop.handle().insert_source(udev, |event, _, _data| {
        match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!("DRM device added: {} at {}", device_id, path.display());
            }
            UdevEvent::Changed { device_id } => {
                tracing::info!("DRM device changed: {}", device_id);
            }
            UdevEvent::Removed { device_id } => {
                tracing::info!("DRM device removed: {}", device_id);
            }
        }
    })?;

    if data.gpu.is_none() {
        return Err("No usable GPU found".into());
    }

    // Store session for VT switching
    data.calloop.state.session = Some(Box::new(session.clone()));

    // Create DMABUF global with renderer formats
    let dmabuf_formats = data.gpu.as_ref().unwrap().dmabuf_formats.clone();
    data.calloop.state._dmabuf_global = Some(
        data.calloop
            .state
            .dmabuf_state
            .create_global::<Beewm>(&data.calloop.display_handle, dmabuf_formats),
    );

    tracing::info!("Starting udev event loop");

    while data.calloop.state.running {
        // Only render when the previous frame has been presented (VBlank fired)
        // AND something visual has actually changed.
        if data.gpu.as_ref().is_some_and(|g| g.can_render) && data.calloop.state.needs_render {
            render_frame(&mut data);
        }
        event_loop.dispatch(Some(Duration::from_millis(20)), &mut data)?;
        // Flush outgoing Wayland events (configure, enter, frame callbacks, etc.)
        // MUST be called every loop iteration — without this, clients never
        // receive compositor-initiated events such as xdg_toplevel.configure.
        if let Err(err) = data.display.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients: {}", err);
        }
        data.calloop.state.space.refresh();
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

    let space_elements = match space_render_elements(
        &mut gpu.renderer,
        [&data.calloop.state.space],
        &gpu.output,
        1.0,
    ) {
        Ok(elems) => elems,
        Err(e) => {
            tracing::error!("space_render_elements failed: {:?}", e);
            Vec::new()
        }
    };

    let border_elements = data.calloop.state.border_elements();
    let cursor_elements = data.calloop.state.cursor_elements();

    let mut elements: Vec<OutputRenderElement> = Vec::new();
    elements.extend(cursor_elements.into_iter().map(OutputRenderElement::from));
    elements.extend(border_elements.into_iter().map(OutputRenderElement::from));
    elements.extend(space_elements.into_iter().map(OutputRenderElement::from));

    let gpu = data.gpu.as_mut().unwrap();

    let result = gpu.compositor.render_frame::<_, OutputRenderElement>(
        &mut gpu.renderer,
        &elements,
        [0.1, 0.1, 0.1, 1.0],
        FrameFlags::DEFAULT,
    );

    match result {
        Ok(result) => {
            if result.is_empty {
                // No damage — nothing to scan out.  Clear the render request
                // so we don't spin; the next surface commit / relayout will
                // set `needs_render = true` again.
                data.calloop.state.needs_render = false;
                gpu.can_render = true;
            } else if let Err(e) = gpu.compositor.queue_frame(()) {
                tracing::error!("Failed to queue frame: {:?}", e);
                gpu.can_render = true;
            }
        }
        Err(e) => {
            tracing::error!("Render error: {:?}", e);
            gpu.can_render = true;
        }
    }

    // Always send frame callbacks so clients can submit their next buffer,
    // even when the compositor had no damage or hit a render error.
    let elapsed = data.calloop.state.start_time.elapsed();
    let output = data.gpu.as_ref().unwrap().output.clone();
    data.calloop.state.space.elements().for_each(|window| {
        window.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    });
    let layer_map = smithay::desktop::layer_map_for_output(&output);
    for layer in layer_map.layers() {
        layer.send_frame(&output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }
}

fn init_gpu(
    session: &mut LibSeatSession,
    event_loop: &EventLoop<UdevData>,
    display_handle: &smithay::reexports::wayland_server::DisplayHandle,
    path: &Path,
) -> Result<GpuData, Box<dyn std::error::Error>> {
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

    let connector_handle =
        selected_connector.ok_or("No connected display found")?;
    let drm_mode = selected_mode.ok_or("No display mode available")?;

    // Find a suitable CRTC for this connector
    let crtc_handle = find_crtc_for_connector(&drm_fd, &resources, connector_handle)?;

    // Create DRM surface
    let drm_surface =
        drm_device.create_surface(crtc_handle, drm_mode, &[connector_handle])?;

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

    // VBlank: frame was presented — acknowledge it and allow the next render.
    let drm_notifier_token = event_loop.handle().insert_source(
        drm_notifier,
        |event, _, data: &mut UdevData| match event {
            DrmEvent::VBlank(_crtc) => {
                if let Some(gpu) = data.gpu.as_mut() {
                    if let Err(e) = gpu.compositor.frame_submitted() {
                        tracing::error!("frame_submitted error: {:?}", e);
                    }
                    gpu.can_render = true;
                }
            }
            DrmEvent::Error(e) => tracing::error!("DRM error: {:?}", e),
        },
    )?;

    Ok(GpuData {
        _drm_device: drm_device,
        _drm_notifier_token: drm_notifier_token,
        _gbm_device: gbm_device,
        renderer,
        compositor,
        output,
        dmabuf_formats,
        can_render: true, // allow first frame immediately
    })
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
