use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;

use smithay::backend::allocator::Format;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement};
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
use smithay::reexports::input::{Libinput, ScrollMethod};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};
use smithay::utils::{Monotonic, Time};
use smithay::wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd};
use smithay::wayland::presentation::Refresh;
use smithay::wayland::socket::ListeningSocketSource;

use crate::compositor::commands::ChildEnvironment;
use crate::compositor::config_watcher;
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
    /// Rolling counters for the `beewm::frame` instrumentation. Reset whenever
    /// a summary line is emitted so the file stays digestible at high refresh
    /// rates instead of growing one log line per frame.
    frame_stats: FrameStats,
}

#[derive(Debug)]
struct FrameStats {
    window_start: Instant,
    frames: u32,
    empty_frames: u32,
    scanout_frames: u32,
    composition_frames: u32,
    overlay_frames: u32,
    cursor_plane_frames: u32,
    render_time_total: Duration,
    render_time_max: Duration,
    /// Last primary-path classification — used to log scanout↔composition
    /// transitions immediately (one line per change) on top of the periodic
    /// summary, so the cause of an FPS drop is easy to spot in the log.
    last_primary_was_scanout: Option<bool>,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            empty_frames: 0,
            scanout_frames: 0,
            composition_frames: 0,
            overlay_frames: 0,
            cursor_plane_frames: 0,
            render_time_total: Duration::ZERO,
            render_time_max: Duration::ZERO,
            last_primary_was_scanout: None,
        }
    }
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

/// Surface the most common misconfiguration that prevents beewm from
/// acquiring DRM master: NVIDIA proprietary driver loaded without
/// `nvidia-drm.modeset=1`. Without that flag the kernel does not expose
/// KMS for the device and any GBM-based Wayland compositor is forced into
/// unprivileged mode — direct scan-out and hardware planes both fail and
/// games end up capped at ~20 fps. We can't fix this from inside the
/// process (it's a kernel-module parameter read at module load), but we
/// can tell the user exactly what to do instead of silently degrading.
///
/// Detection goes via `/proc/modules` (world-readable) rather than
/// `/sys/module/nvidia_drm/parameters/modeset` alone, because that sysfs
/// file is root-only on recent NVIDIA driver releases — a naive
/// `read_to_string` returns `Permission denied` and silently misses the
/// problem.
fn warn_if_nvidia_modeset_disabled() {
    let proc_modules = match std::fs::read_to_string("/proc/modules") {
        Ok(s) => s,
        Err(_) => return,
    };
    let nvidia_drm_loaded = proc_modules
        .lines()
        .any(|line| line.starts_with("nvidia_drm "));
    if !nvidia_drm_loaded {
        return;
    }

    let param_path = "/sys/module/nvidia_drm/parameters/modeset";
    let fix_instructions = "    sudo sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=\"[^\"]*/& \
        nvidia-drm.modeset=1 nvidia-drm.fbdev=1/' /etc/default/grub\n    \
        sudo grub-mkconfig -o /boot/grub/grub.cfg\n    \
        sudo reboot";

    match std::fs::read_to_string(param_path) {
        Ok(value) if value.trim() == "Y" => { /* modeset enabled, nothing to do */ }
        Ok(value) => {
            tracing::error!(
                "nvidia-drm modeset is disabled (modeset={}). Beewm will run in \
                 unprivileged DRM mode: no direct scan-out, software cursor, and \
                 game framerates capped to a fraction of display refresh. To fix \
                 (GRUB):\n\n{}\n\nVerify after reboot with: sudo cat {}",
                value.trim(),
                fix_instructions,
                param_path,
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                "NVIDIA proprietary driver detected but the modeset parameter \
                 ({}) is not readable as your user. If beewm logs \"Unable to \
                 become drm master\" below, modeset is likely disabled. To fix \
                 (GRUB):\n\n{}\n\nVerify with: sudo cat {}",
                param_path,
                fix_instructions,
                param_path,
            );
        }
        Err(_) => { /* file missing or other unrelated error */ }
    }
}

/// Run the compositor on real hardware from a TTY using DRM/KMS.
pub fn run_udev(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    warn_if_nvidia_modeset_disabled();

    let mut event_loop: EventLoop<UdevData> = EventLoop::try_new()?;
    let display: Display<Beewm> = Display::new()?;
    let display_handle = display.handle();

    let state = Beewm::new(&display, config);
    let (_ipc_server, ipc_channel) = ipc::start()?;
    let (_event_server, event_channel) = ipc::start_event_listener()?;

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

    event_loop
        .handle()
        .insert_source(event_channel, |event, _, data| match event {
            ChannelEvent::Msg(stream) => {
                // Build the initial-state snapshot on the main thread (safe,
                // no I/O), then hand the stream + snapshot to the broadcaster
                // thread which does all socket writes.
                let title = data
                    .state
                    .active_workspace_focused_window()
                    .map(crate::compositor::state::focused_window_title)
                    .unwrap_or_default();
                let workspace_num = data.state.active_workspace + 1;
                let initial = format!("window>>{title}\nworkspace>>{workspace_num}\n");
                data.state.event_broadcaster.add_subscriber(stream, initial);
            }
            ChannelEvent::Closed => {
                tracing::warn!("Event socket channel closed");
            }
        })?;

    // Watch the config file for changes and reload automatically on save.
    match config_watcher::make_config_watch_fd(&Config::config_path()) {
        Ok((watch_fd, config_filename)) => {
            event_loop
                .handle()
                .insert_source(
                    Generic::new(
                        watch_fd,
                        Interest::READ,
                        smithay::reexports::calloop::Mode::Level,
                    ),
                    move |_, fd, data: &mut UdevData| {
                        use std::os::fd::AsFd;
                        if config_watcher::drain_config_event(fd.as_fd(), &config_filename) {
                            data.state.apply_config_reload();
                        }
                        Ok(PostAction::Continue)
                    },
                )
                .map_err(|e| tracing::warn!("Failed to register config watcher: {}", e))
                .ok();
        }
        Err(e) => {
            tracing::warn!("Failed to set up config file watcher: {}", e);
        }
    }

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
                    if crate::compositor::runtime_flags::flags().dmabuf_disabled {
                        tracing::warn!(
                            "Dmabuf global skipped by BEEWM_NO_DMABUF; clients will fall back to shm"
                        );
                    } else {
                        data.state._dmabuf_global = Some(
                            data.state
                                .dmabuf_state
                                .create_global::<Beewm>(&display_handle, dmabuf_formats),
                        );
                    }
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
        // All event sources (DRM VBlank, Wayland fd, libinput, IPC, config
        // watcher) wake calloop immediately when they have data, so the
        // dispatch timeout is just an upper bound when the compositor is
        // genuinely idle.
        //
        // - During an interactive grab we want 1 ms ticks so the cursor
        //   tracks tightly even if a frame's worth of input gets coalesced.
        // - When there is pending damage waiting for VBlank, we know the DRM
        //   source will wake us; a 16 ms ceiling is just a safety net.
        // - Otherwise sleep up to 100 ms so the compositor uses essentially
        //   no CPU when the user isn't doing anything (the previous 20 ms
        //   default woke calloop ~50 times/sec for nothing).
        let timeout = if data.state.active_grab.is_some() {
            Duration::from_millis(1)
        } else if data.state.needs_render {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(100)
        };
        event_loop.dispatch(Some(timeout), &mut data)?;
        // Run deferred work that was queued from inside dispatch callbacks
        // and that cannot safely run there (would deadlock cached_state).
        data.state.flush_pending_focus_publish();
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
            // Clear the flag *before* rendering so subsequent damage that
            // arrives while we wait for VBlank re-arms the next frame.
            // Without this, every successful queue caused a redundant empty
            // render on the following iteration just to clear the flag.
            data.state.needs_render = false;
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

    let frame_start = Instant::now();

    let window_elements =
        window_render_elements(&mut gpu.renderer, &data.state.space, &gpu.output, 1.0);

    // True when an xdg-shell fullscreen or a fullscreen-sized X11 game covers
    // the output. Both should suppress top-layers so the game can be promoted
    // onto the primary plane by smithay's DrmCompositor.
    let fullscreen_active = data.state.screen_owned_by_window();

    let border_elements = data.state.border_elements();
    // When a game owns the screen, drop the compositor cursor element so it
    // doesn't sit on top of the primary plane and block direct scanout. The
    // game still gets to draw its own cursor through its surface (or use a
    // relative pointer / hidden cursor), so the user-visible change is just
    // "no compositor pointer overlay while a fullscreen app is up".
    let cursor_elements = if fullscreen_active {
        Vec::new()
    } else {
        data.state.cursor_elements(&mut gpu.renderer)
    };

    // Render layer-shell surfaces (waybar, beebar, etc.) at the correct Z-order.
    // Clone output so we can borrow it for layer_map while also using gpu.renderer.
    let output = gpu.output.clone();

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

    let count_windows = window_elements.len();
    let count_borders = border_elements.len();
    let count_cursor = cursor_elements.len();
    let count_layers_above = layers_above.len();
    let count_layers_below = layers_below.len();

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
            let is_scanout = matches!(result.primary_element, PrimaryPlaneElement::Element(_));
            let overlay_count = result.overlay_elements.len();
            let cursor_plane_used = result.cursor_element.is_some();
            let is_empty = result.is_empty;
            update_primary_scanout_output(&data.state, &output, &render_states);

            if is_empty {
                // No damage — nothing to scan out. The caller already
                // cleared `needs_render`; re-allow the next render and send
                // frame callbacks now since no VBlank will fire to do it.
                gpu.can_render = true;
                gpu.pending_presentation_feedback = None;
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
                // Frame was never queued — no VBlank coming; unblock clients
                // and re-arm the render so the next dispatch retries.
                data.state.needs_render = true;
                let elapsed = data.state.start_time.elapsed();
                send_frame_callbacks(&data.state, &output, elapsed, None);
            } else {
                gpu.pending_presentation_feedback = Some(collect_presentation_feedback(
                    &data.state,
                    &output,
                    &render_states,
                ));
            }

            record_frame_stats(
                &mut gpu.frame_stats,
                frame_start.elapsed(),
                is_scanout,
                is_empty,
                overlay_count,
                cursor_plane_used,
                fullscreen_active,
                count_windows,
                count_borders,
                count_cursor,
                count_layers_above,
                count_layers_below,
            );
            // For the normal non-empty case, frame callbacks are sent from the
            // VBlank handler once the hardware confirms the frame is on screen.
        }
        Err(e) => {
            tracing::error!("Render error: {:?}", e);
            gpu.can_render = true;
            gpu.pending_presentation_feedback = None;
            // Render failed — no VBlank coming; unblock clients and re-arm
            // so we retry on the next iteration instead of getting stuck.
            data.state.needs_render = true;
            let elapsed = data.state.start_time.elapsed();
            send_frame_callbacks(&data.state, &output, elapsed, None);
        }
    }
}

/// Update per-frame counters and emit `beewm::frame` log lines at digestible
/// rates: one line on every scanout↔composition transition (with the full
/// element breakdown of *that* frame) and one summary every ~1 second.
#[allow(clippy::too_many_arguments)]
fn record_frame_stats(
    stats: &mut FrameStats,
    render_time: Duration,
    is_scanout: bool,
    is_empty: bool,
    overlay_count: usize,
    cursor_plane_used: bool,
    fullscreen_active: bool,
    count_windows: usize,
    count_borders: usize,
    count_cursor: usize,
    count_layers_above: usize,
    count_layers_below: usize,
) {
    stats.frames += 1;
    if is_empty {
        stats.empty_frames += 1;
    }
    if is_scanout {
        stats.scanout_frames += 1;
    } else {
        stats.composition_frames += 1;
    }
    if overlay_count > 0 {
        stats.overlay_frames += 1;
    }
    if cursor_plane_used {
        stats.cursor_plane_frames += 1;
    }
    stats.render_time_total += render_time;
    if render_time > stats.render_time_max {
        stats.render_time_max = render_time;
    }

    let transition = stats
        .last_primary_was_scanout
        .map(|prev| prev != is_scanout)
        .unwrap_or(true);
    stats.last_primary_was_scanout = Some(is_scanout);
    if transition {
        tracing::info!(
            target: "beewm::frame",
            is_scanout,
            fullscreen_active,
            overlay_count,
            cursor_plane_used,
            count_windows,
            count_borders,
            count_cursor,
            count_layers_above,
            count_layers_below,
            render_us = render_time.as_micros() as u64,
            "primary-plane path changed: {}",
            if is_scanout { "DIRECT SCANOUT" } else { "GPU COMPOSITION" },
        );
    }

    let elapsed = stats.window_start.elapsed();
    if elapsed >= Duration::from_secs(1) {
        let frames = stats.frames as f64;
        let secs = elapsed.as_secs_f64();
        let avg_us = if stats.frames > 0 {
            (stats.render_time_total.as_micros() / stats.frames as u128) as u64
        } else {
            0
        };
        tracing::info!(
            target: "beewm::frame",
            fps = frames / secs,
            frames = stats.frames,
            scanout = stats.scanout_frames,
            composition = stats.composition_frames,
            empty = stats.empty_frames,
            overlay = stats.overlay_frames,
            cursor_plane = stats.cursor_plane_frames,
            avg_render_us = avg_us,
            max_render_us = stats.render_time_max.as_micros() as u64,
            fullscreen_active,
            "frame-stats over {:.2}s",
            secs,
        );
        *stats = FrameStats::new();
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

    let syncobj_state = if crate::compositor::runtime_flags::flags().explicit_sync_disabled {
        tracing::warn!("Explicit sync disabled by BEEWM_NO_EXPLICIT_SYNC");
        None
    } else if supports_syncobj_eventfd(&drm_fd) {
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
            frame_stats: FrameStats::new(),
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
