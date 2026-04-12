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
use smithay::backend::session::Session;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::desktop::space::space_render_elements;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, Device as ControlDevice, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};
use smithay::wayland::socket::ListeningSocketSource;

use crate::render::OutputRenderElement;
use crate::state::{Beewm, CalloopData, ClientState};

/// Per-GPU state for the DRM backend.
#[allow(dead_code)]
struct GpuData {
    drm_device: DrmDevice,
    drm_notifier_token: RegistrationToken,
    gbm_device: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    compositor: DrmCompositor<
        GbmAllocator<DrmDeviceFd>,
        GbmFramebufferExporter<DrmDeviceFd>,
        (),
        DrmDeviceFd,
    >,
    output: Output,
    dmabuf_formats: Vec<Format>,
}

/// Run the compositor on real hardware from a TTY using DRM/KMS.
pub fn run_udev(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);

    let mut data = CalloopData {
        state,
        display_handle: display_handle.clone(),
    };

    // --- Session ---
    let (mut session, notifier) = LibSeatSession::new()?;
    tracing::info!("Session opened on seat: {}", session.seat());

    event_loop
        .handle()
        .insert_source(notifier, |_, _, _| {})?;

    // --- Wayland socket ---
    let listening_socket = ListeningSocketSource::new_auto()?;
    let socket_name = listening_socket.socket_name().to_os_string();
    tracing::info!("Wayland socket: {:?}", socket_name);
    std::env::set_var("WAYLAND_DISPLAY", &socket_name);

    event_loop.handle().insert_source(
        listening_socket,
        |client_stream, _, data| {
            if let Err(e) = data.display_handle.insert_client(
                client_stream,
                std::sync::Arc::new(ClientState::default()),
            ) {
                tracing::error!("Failed to insert client: {}", e);
            }
        },
    )?;

    // Insert the Display into the event loop
    event_loop.handle().insert_source(
        Generic::new(
            display,
            Interest::READ,
            smithay::reexports::calloop::Mode::Level,
        ),
        |_, display, data| {
            unsafe {
                display
                    .get_mut()
                    .dispatch_clients(&mut data.state)
                    .unwrap();
            }
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
            crate::input::handle_input(&mut data.state, event);
        })?;

    // --- Udev: enumerate GPUs ---
    let udev = UdevBackend::new(&session.seat())?;
    let mut gpu_data: Option<GpuData> = None;

    // Open the primary GPU from initial device list
    for (device_id, path) in udev.device_list() {
        tracing::info!("Found DRM device: {} at {}", device_id, path.display());
        if gpu_data.is_none() {
            match init_gpu(
                &mut session,
                &event_loop,
                &display_handle,
                &path,
            ) {
                Ok(gd) => {
                    data.state.space.map_output(&gd.output, (0, 0));
                    gpu_data = Some(gd);
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

    let gpu = gpu_data.ok_or("No usable GPU found")?;
    // Move GPU data into a RefCell-like holder so we can use it in the loop
    // We just store it as local mutable state since we own the loop.
    let mut gpu = gpu;

    // Store session for VT switching
    data.state.session = Some(Box::new(session.clone()));

    // Create DMABUF global with renderer formats
    let dmabuf_formats = gpu.dmabuf_formats.clone();
    data.state.dmabuf_global = Some(
        data.state
            .dmabuf_state
            .create_global::<Beewm>(&data.display_handle, dmabuf_formats),
    );

    tracing::info!("Starting udev event loop");

    while data.state.running {
        // Collect space (window + layer) elements
        let space_elements = space_render_elements(
            &mut gpu.renderer,
            [&data.state.space],
            &gpu.output,
            1.0,
        )
        .unwrap_or_else(|_| Vec::new());

        // Collect custom elements
        let border_elements = data.state.border_elements();
        let cursor_elements = data.state.cursor_elements();

        // Build final element list: cursor → borders → space (front to back)
        let mut elements: Vec<OutputRenderElement> = Vec::new();
        elements.extend(cursor_elements.into_iter().map(OutputRenderElement::from));
        elements.extend(border_elements.into_iter().map(OutputRenderElement::from));
        elements.extend(space_elements.into_iter().map(OutputRenderElement::from));

        let result = gpu.compositor.render_frame::<_, OutputRenderElement>(
            &mut gpu.renderer,
            &elements,
            [0.1, 0.1, 0.1, 1.0],
            FrameFlags::empty(),
        );

        match result {
            Ok(render_result) => {
                if !render_result.is_empty {
                    if let Err(e) = gpu.compositor.queue_frame(()) {
                        tracing::error!("Failed to queue frame: {:?}", e);
                    }
                }
            }
            Err(e) => {
                tracing::error!("Render error: {:?}", e);
            }
        }

        // Tell clients to draw their next frame
        let elapsed = data.state.start_time.elapsed();
        let output = &gpu.output;
        data.state.space.elements().for_each(|window| {
            window.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        });

        let layer_map = smithay::desktop::layer_map_for_output(output);
        for layer in layer_map.layers() {
            layer.send_frame(output, elapsed, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }

        // Dispatch event loop — wait for vblank or input
        let timeout = Duration::from_millis(16);
        event_loop.dispatch(Some(timeout), &mut data)?;

        data.state.space.refresh();
    }

    Ok(())
}

fn init_gpu(
    session: &mut LibSeatSession,
    event_loop: &EventLoop<CalloopData>,
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

    // Insert DRM event source for vblank handling
    let drm_notifier_token = event_loop.handle().insert_source(
        drm_notifier,
        |event, _, _data| match event {
            DrmEvent::VBlank(_crtc) => {}
            DrmEvent::Error(e) => tracing::error!("DRM error: {:?}", e),
        },
    )?;

    Ok(GpuData {
        drm_device,
        drm_notifier_token,
        gbm_device,
        renderer,
        compositor,
        output,
        dmabuf_formats,
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
