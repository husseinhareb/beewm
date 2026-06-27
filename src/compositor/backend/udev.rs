use std::collections::HashSet;
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::{Config, OutputConfig, OutputModeSpec};

use smithay::backend::allocator::Format;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmEventTime, DrmNode, NodeType};
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
use smithay::reexports::drm::Device as BasicDrmDevice;
use smithay::reexports::drm::control::{
    Device as ControlDevice, Mode as DrmMode, ModeTypeFlags, connector, crtc,
};
use smithay::reexports::input::{
    Device as LibinputDevice, DeviceCapability, Libinput, ScrollMethod,
};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Logical, Point, Transform};
use smithay::utils::{Monotonic, Time};
use smithay::wayland::dmabuf::{DmabufFeedback, DmabufFeedbackBuilder};
use smithay::wayland::drm_syncobj::{DrmSyncobjState, supports_syncobj_eventfd};
use smithay::wayland::presentation::Refresh;
use smithay::wayland::socket::ListeningSocketSource;

use crate::compositor::commands::ChildEnvironment;
use crate::compositor::config_watcher;
use crate::compositor::diagnostics::PresentStats;
use crate::compositor::feedback::{
    collect_presentation_feedback, output_frame_interval, send_frame_callbacks,
    update_primary_scanout_output,
};
use crate::compositor::input::leds::LedDeviceRegistry;
use crate::compositor::ipc;
use crate::compositor::layering::{layers_rendered_above_windows, layers_rendered_below_windows};
use crate::compositor::power::{PowerEvent, PowerState, ResumeSource};
use crate::compositor::render::{
    OutputRenderElement, layer_render_elements, lock_render_elements, window_render_elements,
};
use crate::compositor::screencopy::process_pending_screencopies;
use crate::compositor::state::{
    BackendRequest, Beewm, ClientState, OutputModes, lookup_client_compositor_state,
};
use crate::xwayland::{delegate_backend_xwayland, start_xwayland};

/// How the `wp_linux_dmabuf` global should be advertised.
///
/// Modern clients — Mesa EGL and especially XWayland/glamor (which is how Steam
/// games render) — use the dmabuf *feedback* protocol (v4) to discover the
/// render device and the format/modifier tranches the compositor can import. If
/// we only expose the legacy v3 global they cannot negotiate a shared buffer and
/// silently fall back to slow shared-memory buffers, which tanks the frame rate
/// and GPU utilisation. We therefore build feedback whenever possible and only
/// fall back to v3 if the device id / feedback can't be obtained.
enum DmabufSetup {
    /// v4 global with a default feedback tranche (preferred). Boxed because the
    /// feedback owns the full format/modifier tables.
    Feedback(Box<DmabufFeedback>),
    /// v3 global with a flat format list (fallback).
    Legacy(Vec<Format>),
}

type DrmCompositorImpl =
    DrmCompositor<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, (), DrmDeviceFd>;

const FRAME_FAILURE_RETRY_BASE: Duration = Duration::from_millis(100);
const FRAME_FAILURE_RETRY_MAX: Duration = Duration::from_secs(2);

/// How often a `Degraded` resume re-attempts DRM activation. On NVIDIA laptops
/// the watchdog can fire `handle_resume` before logind/libseat has handed DRM
/// master back, so the first `activate(true)` fails with EACCES; we retry until
/// master returns instead of latching black. Each failed `activate` ioctl can
/// itself stall ~5s, so the effective cadence is governed by that, not this.
const RESUME_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// One rendered output: a single CRTC's `DrmCompositor` plus its `Output` and
/// per-output pacing/feedback bookkeeping. Every surface on a device shares that
/// device's single `GlesRenderer` (see [`GpuData::renderer`]).
struct SurfaceData {
    /// The connector (physical display) this surface drives — used to diff
    /// against the live connector set on hotplug.
    connector: connector::Handle,
    /// The CRTC this surface scans out on — used to route vblank events.
    crtc: crtc::Handle,
    output: Output,
    /// Top-left of this output in the global Space coordinate space; mirrored
    /// into the compositor's `OutputCtx` via `Beewm::add_output`.
    position: Point<i32, Logical>,
    /// `None` only transiently across suspend/resume, while the old KMS surface
    /// has been torn down and a fresh one not yet rebuilt (see `activate_drm`).
    compositor: Option<DrmCompositorImpl>,
    /// True when this surface's vblank has fired and it may render again.
    can_render: bool,
    /// Backoff deadline after a failed render/queue. Without this, a bad DRM
    /// atomic state can spin the compositor hard enough to starve input/session
    /// events, which makes a recoverable resume problem feel like a total freeze.
    retry_after: Option<Instant>,
    consecutive_frame_failures: u32,
    pending_presentation_feedback: Option<smithay::desktop::utils::OutputPresentationFeedback>,
    /// Rolling counters for the `beewm::frame` instrumentation. Reset whenever
    /// a summary line is emitted so the file stays digestible at high refresh
    /// rates instead of growing one log line per frame.
    frame_stats: FrameStats,
    /// Vblank-cadence counters for the `beewm::presentation` instrumentation —
    /// proves whether the compositor presents at the real refresh rate.
    present_stats: PresentStats,
}

/// Per-GPU state for the DRM backend. One renderer drives every connected
/// connector's [`SurfaceData`]. Multi-head (more than one surface) is gated on
/// `BEEWM_MULTI_OUTPUT`; by default exactly one surface is created.
struct GpuData {
    drm_device: DrmDevice,
    /// Cloned control fd for connector/encoder queries (DrmDevice itself doesn't
    /// expose the `ControlDevice` surface we need for hotplug rescans).
    drm_fd: DrmDeviceFd,
    _drm_notifier_token: RegistrationToken,
    gbm_device: GbmDevice<DrmDeviceFd>,
    renderer: GlesRenderer,
    surfaces: Vec<SurfaceData>,
    // Device-level resources retained so surfaces can be created at runtime when
    // a display is hot-plugged (see `rescan_connectors`).
    renderer_formats: Vec<Format>,
    color_formats: [smithay::backend::allocator::Fourcc; 2],
    cursor_size: smithay::utils::Size<u32, smithay::utils::Buffer>,
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
    power_state: PowerState,
    resume_lock_pending: bool,
    /// Throttles `Degraded` resume retries from the main loop. `None` means a
    /// retry may run on the next iteration.
    resume_retry_at: Option<Instant>,
    /// The VT beewm runs on, read once at startup. Used to re-assert the VT on a
    /// stuck resume (NVIDIA's resume `chvt`-back doesn't reliably reactivate us).
    session_vt: Option<i32>,
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

/// The active VT number, e.g. `1` for `tty1`. Read at startup while beewm is the
/// foreground session, so it identifies beewm's own VT.
fn current_vt() -> Option<i32> {
    std::fs::read_to_string("/sys/class/tty/tty0/active")
        .ok()?
        .trim()
        .strip_prefix("tty")?
        .parse()
        .ok()
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
    let (power_tx, power_rx) = smithay::reexports::calloop::channel::channel::<PowerEvent>();

    // Clone the display fd before moving display into UdevData — used to
    // wake calloop when clients send data.
    let display_fd = display
        .as_fd()
        .try_clone_to_owned()
        .expect("Failed to clone wayland display fd");

    let mut data = UdevData {
        state,
        gpu: None,
        power_state: PowerState::Awake,
        resume_lock_pending: false,
        resume_retry_at: None,
        // Read while we are the foreground VT, so this is our own VT number.
        session_vt: current_vt(),
        display,
    };

    start_xwayland(event_loop.handle(), &display_handle, &mut data.state);

    let loop_handle = event_loop.handle();
    data.state
        .install_syncobj_blocker_source(Box::new(move |source, client| {
            let client = client.clone();
            if let Err(error) =
                loop_handle.insert_source(source, move |(), _, data: &mut UdevData| {
                    // An acquire fence signalled; release the held commit(s).
                    data.state.sync_stats.record_clear();
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
                handle_session_paused(data);
            }
            SessionEvent::ActivateSession => {
                handle_session_activated(data);
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
    // udev is the real TTY session: beewm owns the seat, so it is safe (and
    // necessary) to push the session env to D-Bus/systemd so the screen-sharing
    // portal and PipeWire can find the display.
    data.state.session_env_managed = true;

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
                let workspace_num = data.state.active_workspace() + 1;
                let mut initial = format!("window>>{title}\nworkspace>>{workspace_num}\n");
                if let Some(status) = data.state.keyboard_status() {
                    initial.push_str(&status.event_payload());
                    initial.push('\n');
                }
                data.state.event_broadcaster.add_subscriber(stream, initial);
            }
            ChannelEvent::Closed => {
                tracing::warn!("Event socket channel closed");
            }
        })?;

    if let Err(error) = crate::compositor::power::start_resume_watchdog(power_tx.clone()) {
        tracing::debug!(
            target: "beewm::power",
            %error,
            "failed to start resume watchdog thread",
        );
    }
    if let Err(error) = crate::compositor::power::start_logind_sleep_monitor(power_tx) {
        tracing::debug!(
            target: "beewm::power",
            %error,
            "failed to start logind sleep monitor thread",
        );
    }
    event_loop
        .handle()
        .insert_source(power_rx, |event, _, data: &mut UdevData| match event {
            ChannelEvent::Msg(PowerEvent::PrepareSuspend { ack }) => {
                handle_prepare_suspend(data, "logind");
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            ChannelEvent::Msg(PowerEvent::Resume { source }) => {
                handle_resume(data, source);
            }
            ChannelEvent::Closed => {
                tracing::debug!(target: "beewm::power", "logind sleep monitor channel closed");
            }
        })?;

    // Settings tray: a StatusNotifierItem D-Bus thread sends chosen menu actions
    // back here over a channel; config reload can start/stop the tray thread.
    let (tray_tx, tray_rx) =
        smithay::reexports::calloop::channel::channel::<crate::compositor::tray::MenuAction>();
    event_loop
        .handle()
        .insert_source(tray_rx, |event, _, data: &mut UdevData| {
            if let ChannelEvent::Msg(action) = event {
                data.state.apply_menu_action(action);
                data.state.needs_render = true;
            }
        })?;
    data.state.install_tray_action_sender(tray_tx);

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

    // Track every libinput keyboard so lock-LED updates (Caps/Num/Scroll)
    // reach all of them. The hotplug arms below maintain the device list
    // through one clone of the registry; the clone installed in compositor
    // state is what `SeatHandler::led_state_changed` writes through.
    let keyboards: LedDeviceRegistry<LibinputDevice> = LedDeviceRegistry::new();
    let leds_enabled = !crate::compositor::runtime_flags::flags().keyboard_leds_disabled;
    if leds_enabled {
        data.state
            .keyboard_leds
            .install_controller(Box::new(keyboards.clone()));
    } else {
        tracing::warn!("Keyboard LED sync disabled by BEEWM_NO_KEYBOARD_LEDS");
    }

    event_loop
        .handle()
        .insert_source(libinput_backend, move |event, _, data| match event {
            InputEvent::DeviceAdded { mut device } => {
                // A keyboard that just (re)appeared carries whatever LED
                // state firmware or a previous VT owner left; sync it to the
                // compositor's XKB state and track it for future updates.
                if leds_enabled && device.has_capability(DeviceCapability::Keyboard) {
                    let current = data
                        .state
                        .seat
                        .get_keyboard()
                        .map(|keyboard| keyboard.led_state().into())
                        .unwrap_or_default();
                    keyboards.add_device(device.clone(), current);
                }

                // Tap-to-click is a libinput-specific feature; configure it as
                // devices appear (e.g. touchpad at startup or on hotplug).
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
            }
            InputEvent::DeviceRemoved { device } => {
                if leds_enabled && device.has_capability(DeviceCapability::Keyboard) {
                    keyboards.remove_device(&device);
                }
            }
            event => crate::compositor::input::handle_input(&mut data.state, event),
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
                data.state.config.refresh_rate,
                &data.state.config.outputs,
            ) {
                Ok((gd, dmabuf_setup, syncobj_state)) => {
                    for surface in &gd.surfaces {
                        data.state
                            .add_output(surface.output.clone(), surface.position);
                        if let Ok(conn_info) = gd.drm_fd.get_connector(surface.connector, false) {
                            let modes = output_modes_for(&conn_info, &surface.output);
                            data.state.set_output_modes(surface.output.clone(), modes);
                        }
                    }
                    data.gpu = Some(gd);
                    data.state.drm_syncobj_state = syncobj_state;
                    let display_handle = data.state.display_handle.clone();
                    if crate::compositor::runtime_flags::flags().dmabuf_disabled {
                        tracing::warn!(
                            "Dmabuf global skipped by BEEWM_NO_DMABUF; clients will fall back to shm"
                        );
                    } else {
                        let global = match dmabuf_setup {
                            DmabufSetup::Feedback(feedback) => data
                                .state
                                .dmabuf_state
                                .create_global_with_default_feedback::<Beewm>(
                                    &display_handle,
                                    &feedback,
                                ),
                            DmabufSetup::Legacy(formats) => data
                                .state
                                .dmabuf_state
                                .create_global::<Beewm>(&display_handle, formats),
                        };
                        data.state._dmabuf_global = Some(global);
                    }
                }
                Err(e) => tracing::warn!("Failed to init GPU {}: {}", path.display(), e),
            }
        }
    }

    // Insert udev for hotplug (we don't handle hotplug in detail yet)
    event_loop
        .handle()
        .insert_source(udev, |event, _, data| match event {
            UdevEvent::Added { device_id, path } => {
                tracing::info!("DRM device added: {} at {}", device_id, path.display());
                // Multi-GPU hot-add is not handled yet (Phase 3 covers connectors
                // on the already-initialized device, not adding a second GPU).
            }
            UdevEvent::Changed { device_id } => {
                tracing::info!("DRM device changed: {}", device_id);
                // A connector (monitor) was plugged or unplugged. Reconcile the
                // surface set on the active device. Gated so the default
                // single-output path keeps the legacy log-only behavior.
                if crate::compositor::runtime_flags::flags().multi_output_enabled {
                    rescan_connectors(data);
                }
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
        // Advance window animations first: while any animation is active this
        // sets `needs_render` so we keep repainting (gated by `can_render` /
        // VBlank, so no busy loop), and clears back to idle when they finish.
        data.state.tick_animations(Instant::now());

        // A resume left the DRM device unusable (typically EACCES because we
        // didn't hold DRM master yet). Keep re-attempting activation — master
        // comes back once logind/libseat reactivates the session, which on some
        // NVIDIA laptops never arrives as a libseat ActivateSession event, so
        // the main loop is the only thing that drives recovery.
        // ponytail: no retry cap — if master never returns the panel is dead
        // anyway; the user can VT-switch out. Add a cap if it ever wedges.
        if data.power_state == PowerState::Degraded
            && data
                .resume_retry_at
                .is_none_or(|at| Instant::now() >= at)
        {
            data.resume_retry_at = Some(Instant::now() + RESUME_RETRY_INTERVAL);
            // If our session is inactive, NVIDIA's resume `chvt`-back left the
            // kernel on its scratch VT and never reactivated us — so we never
            // regain DRM master and every commit EACCESes. Re-assert our VT to
            // force logind/libseat to reactivate, the programmatic equivalent of
            // the Ctrl+Alt+Fn switch that recovers it by hand.
            if let (Some(vt), Some(session)) = (data.session_vt, data.state.session.as_mut())
                && !session.is_active()
            {
                match session.change_vt(vt) {
                    Ok(()) => tracing::info!(
                        target: "beewm::power", vt,
                        "re-asserting VT to recover DRM master after resume",
                    ),
                    Err(error) => tracing::warn!(
                        target: "beewm::power", %error, vt, "resume VT re-assert failed",
                    ),
                }
            }
            handle_resume(&mut data, ResumeSource::Watchdog);
        }

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
        // Evaluate the screen-timeout deadline and apply any queued backend work
        // (DPMS blank/wake) before rendering this iteration.
        data.state.update_idle(Instant::now());
        process_backend_requests(&mut data);
        // Run deferred work that was queued from inside dispatch callbacks
        // and that cannot safely run there (would deadlock cached_state).
        data.state.apply_pending_map_focus();
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
        // Only render when something visual has actually changed. Each surface
        // renders only if its own vblank has fired (`can_render`); a surface
        // still waiting re-arms `needs_render` from inside `render_frame` so it
        // is retried next iteration. Rendering after dispatch keeps live resizes
        // close to the latest pointer/commit state.
        if data.gpu.is_some() && data.state.needs_render && !data.state.blanked {
            // Clear the flag *before* rendering so subsequent damage that
            // arrives while we wait for VBlank re-arms the next frame.
            // Without this, every successful queue caused a redundant empty
            // render on the following iteration just to clear the flag.
            data.state.needs_render = false;
            render_frame(&mut data);
            // render_frame queued this frame's wl_surface.frame callbacks (the
            // pipelining fix). Flush them now so the client starts its next
            // frame immediately, in parallel with this one scanning out —
            // otherwise they wait for the next dispatch cycle, which at high
            // refresh rates is a whole frame interval and defeats the point.
            if let Err(err) = data.display.flush_clients() {
                tracing::warn!("Failed to flush Wayland clients after render: {}", err);
            }
        }
    }

    Ok(())
}

fn handle_session_paused(data: &mut UdevData) {
    tracing::info!(target: "beewm::power", "session paused");
    data.power_state = data.power_state.suspended();
    pause_drm(data);
}

fn handle_session_activated(data: &mut UdevData) {
    tracing::info!(target: "beewm::power", "session activated");
    handle_resume(data, ResumeSource::Libseat);
}

fn handle_prepare_suspend(data: &mut UdevData, source: &'static str) {
    data.power_state = data.power_state.prepare_suspend();
    tracing::info!(
        target: "beewm::power",
        source,
        state = ?data.power_state,
        "preparing suspend/session pause",
    );

    if data.state.config.lock_on_suspend {
        data.state.secure_lock("suspend");
        data.resume_lock_pending = true;
        if let Err(err) = data.display.flush_clients() {
            tracing::warn!("Failed to flush Wayland clients before suspend: {}", err);
        }
    } else if data.state.config.lock_on_resume || data.state.locked {
        data.resume_lock_pending = true;
    }

    // Draw one (now-locked) frame while the GPU is still up, then release the
    // device. logind delays the actual suspend until we ack this handler, so the
    // GPU is guaranteed alive here — but once it powers down, an in-flight atomic
    // commit blocks forever and wedges the whole event loop (the nvidia
    // no-freeze-session setup keeps us running straight into suspend). Pausing
    // now means the main loop issues no commits during the suspend window and
    // stays responsive to drive recovery on resume.
    force_frame(data);
    pause_drm(data);
}

fn handle_resume(data: &mut UdevData, source: ResumeSource) {
    let next = data.power_state.resume();
    if next == PowerState::Awake {
        return;
    }
    data.power_state = next;
    tracing::info!(
        target: "beewm::power",
        source = source.as_str(),
        state = ?data.power_state,
        "resuming session",
    );

    let resume_lock_pending = data.resume_lock_pending;
    data.resume_lock_pending = false;
    data.state.secure_resume_lock(resume_lock_pending);
    let force_drm_reset = matches!(source, ResumeSource::Logind | ResumeSource::Watchdog);
    let activated = activate_drm(data, force_drm_reset);
    // `secure_resume_lock` may have queued DPMS-on work if the screen timed out
    // before suspend. Apply it before the first forced render.
    process_backend_requests(data);

    if activated {
        if crate::compositor::runtime_flags::flags().multi_output_enabled {
            rescan_connectors(data);
        }
        data.power_state = PowerState::Awake;
        data.resume_retry_at = None;
        tracing::info!(target: "beewm::power", "resume completed");
    } else {
        data.power_state = PowerState::Degraded;
        tracing::error!(
            target: "beewm::power",
            "resume degraded: DRM activation failed; compositor remains alive and locked",
        );
    }

    force_frame(data);
    if let Err(err) = data.display.flush_clients() {
        tracing::warn!("Failed to flush Wayland clients after resume: {}", err);
    }
}

fn pause_drm(data: &mut UdevData) {
    if let Some(gpu) = data.gpu.as_mut() {
        tracing::info!(
            target: "beewm::power",
            active = gpu.drm_device.is_active(),
            "pausing DRM device",
        );
        gpu.drm_device.pause();
        tracing::info!(
            target: "beewm::power",
            active = gpu.drm_device.is_active(),
            "DRM device paused",
        );
        for surface in &mut gpu.surfaces {
            surface.can_render = false;
            surface.retry_after = None;
            surface.pending_presentation_feedback = None;
        }
    }

    data.state.needs_render = false;
}

fn activate_drm(data: &mut UdevData, force_reset: bool) -> bool {
    // Read before borrowing `gpu` (both live on `data`); needed to rebuild
    // surfaces below.
    let refresh_rate = data.state.config.refresh_rate;
    let output_configs = data.state.config.outputs.clone();
    let Some(gpu) = data.gpu.as_mut() else {
        return true;
    };

    let was_active = gpu.drm_device.is_active();
    tracing::info!(
        target: "beewm::power",
        was_active,
        force_reset,
        "activating DRM device",
    );

    if let Err(err) = gpu.drm_device.activate(true) {
        tracing::error!(target: "beewm::power", "failed to reactivate DRM device: {}", err);
        for surface in &mut gpu.surfaces {
            surface.can_render = false;
            surface.retry_after = Some(Instant::now() + FRAME_FAILURE_RETRY_MAX);
        }
        return false;
    }
    if force_reset && was_active {
        tracing::info!(
            target: "beewm::power",
            "forcing DRM state reset after sleep while device remained active",
        );
        if let Err(err) = gpu.drm_device.reset_state() {
            tracing::error!(
                target: "beewm::power",
                "failed to reset active DRM device after resume: {}",
                err,
            );
            for surface in &mut gpu.surfaces {
                surface.can_render = false;
                surface.retry_after = Some(Instant::now() + FRAME_FAILURE_RETRY_MAX);
            }
            return false;
        }
    }
    tracing::info!(
        target: "beewm::power",
        active = gpu.drm_device.is_active(),
        "DRM device activated",
    );

    // Rebuild each surface's DrmCompositor from scratch rather than resetting the
    // suspended one — the stale KMS state otherwise hangs the first commit (see
    // `rebuild_surface_compositor`). A failure here means master/KMS isn't truly
    // back yet, so report degraded and let the caller retry.
    let GpuData {
        drm_device,
        gbm_device,
        drm_fd,
        renderer_formats,
        color_formats,
        cursor_size,
        surfaces,
        ..
    } = gpu;
    let color_formats = *color_formats;
    let cursor_size = *cursor_size;
    let mut all_ok = true;
    for surface in surfaces.iter_mut() {
        match rebuild_surface_compositor(
            drm_device,
            gbm_device,
            drm_fd,
            renderer_formats,
            color_formats,
            cursor_size,
            surface,
            refresh_rate,
            &output_configs,
        ) {
            Ok(()) => {
                surface.can_render = true;
                surface.retry_after = None;
                surface.consecutive_frame_failures = 0;
            }
            Err(err) => {
                tracing::error!(
                    target: "beewm::power",
                    output = %surface.output.name(),
                    "failed to rebuild compositor after resume: {}",
                    err,
                );
                surface.can_render = false;
                surface.retry_after = Some(Instant::now() + FRAME_FAILURE_RETRY_MAX);
                all_ok = false;
            }
        }
        surface.pending_presentation_feedback = None;
        surface.frame_stats = FrameStats::new();
        surface.present_stats = PresentStats::new();
    }
    if !all_ok {
        return false;
    }

    // Whoever owned the VT meanwhile (firmware, agetty, another compositor)
    // may have rewritten keyboard LEDs; force one re-push.
    data.state.keyboard_leds.invalidate();
    data.state.sync_keyboard_leds();
    data.state.needs_render = true;
    true
}

fn force_frame(data: &mut UdevData) {
    if data.gpu.is_some() && !data.state.blanked {
        data.state.needs_render = false;
        render_frame(data);
    }
}

/// Apply queued backend work that needs DRM access (currently DPMS for the
/// screen-timeout blank). Drained once per loop iteration.
fn process_backend_requests(data: &mut UdevData) {
    while let Some(request) = data.state.backend_requests.pop_front() {
        match request {
            BackendRequest::SetDpms { on } => {
                let Some(gpu) = data.gpu.as_mut() else {
                    continue;
                };
                for surface in gpu.surfaces.iter_mut() {
                    if on {
                        // Re-arm so the next render re-enables the CRTC (queueing
                        // a frame brings DPMS back on per DrmCompositor::clear).
                        surface.can_render = true;
                        surface.retry_after = None;
                    } else if let Some(Err(err)) =
                        surface.compositor.as_mut().map(|c| c.clear())
                    {
                        tracing::warn!(
                            target: "beewm::idle",
                            "DPMS off (clear) failed: {:?}", err
                        );
                    }
                }
                if on {
                    data.state.needs_render = true;
                }
            }
            BackendRequest::SetOutputMode { output, mode } => {
                apply_output_mode(data, &output, mode);
            }
        }
    }
}

/// Build the [`OutputModes`] list the tray's Resolution/Refresh menu reads, from
/// a connector's advertised modes plus the output's current mode.
fn output_modes_for(conn_info: &connector::Info, output: &Output) -> OutputModes {
    let mut available: Vec<OutputModeSpec> = conn_info
        .modes()
        .iter()
        .map(|m| {
            let (w, h) = m.size();
            OutputModeSpec {
                width: w as i32,
                height: h as i32,
                refresh: Some(m.vrefresh()),
            }
        })
        .collect();
    available.sort_unstable_by(|a, b| {
        (b.width, b.height, b.refresh).cmp(&(a.width, a.height, a.refresh))
    });
    available.dedup();
    let current = output.current_mode().map(|m| OutputModeSpec {
        width: m.size.w,
        height: m.size.h,
        refresh: Some((m.refresh / 1000) as u32),
    });
    OutputModes { available, current }
}

/// Apply a live resolution/refresh change to `output` by rebuilding its DRM
/// surface with the requested mode (reusing the existing `Output` so workspaces
/// and position are preserved). Gated on `BEEWM_LIVE_MODESET`; the runtime
/// modeset path is not yet hardware-validated.
fn apply_output_mode(data: &mut UdevData, output: &Output, spec: OutputModeSpec) {
    if !crate::compositor::runtime_flags::flags().live_modeset_enabled {
        tracing::warn!(
            target: "beewm::tray",
            "live modeset is gated off; set BEEWM_LIVE_MODESET=1 to change resolution/refresh \
             from the tray (the runtime modeset path is not yet hardware-validated)",
        );
        return;
    }

    // All DRM work happens here; we return the data the compositor side needs.
    let rebuilt = {
        let Some(gpu) = data.gpu.as_mut() else {
            return;
        };
        let Some(idx) = gpu.surfaces.iter().position(|s| &s.output == output) else {
            tracing::warn!(target: "beewm::tray", "modeset: no surface for the focused output");
            return;
        };
        let connector = gpu.surfaces[idx].connector;
        let crtc = gpu.surfaces[idx].crtc;
        let position = gpu.surfaces[idx].position;

        let conn_info = match gpu.drm_fd.get_connector(connector, false) {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!(target: "beewm::tray", "modeset: get_connector failed: {}", error);
                return;
            }
        };
        let Some(drm_mode) = find_mode_for_spec(&conn_info, spec) else {
            tracing::warn!(
                target: "beewm::tray",
                "modeset: {}x{}{} not available on this output",
                spec.width,
                spec.height,
                spec.refresh.map(|hz| format!("@{hz}")).unwrap_or_default(),
            );
            return;
        };

        // Drop the old surface first so its CRTC is free before we re-create.
        let reused_output = gpu.surfaces[idx].output.clone();
        gpu.surfaces.remove(idx);

        let drm_surface = match gpu.drm_device.create_surface(crtc, drm_mode, &[connector]) {
            Ok(surface) => surface,
            Err(error) => {
                tracing::error!(target: "beewm::tray", "modeset: create_surface failed: {}", error);
                return;
            }
        };
        let gbm_allocator = GbmAllocator::new(
            gpu.gbm_device.clone(),
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );
        let gbm_exporter = GbmFramebufferExporter::new(gpu.gbm_device.clone(), None);

        let output_mode = OutputMode {
            size: (drm_mode.size().0 as i32, drm_mode.size().1 as i32).into(),
            refresh: (drm_mode.vrefresh() * 1000) as i32,
        };
        reused_output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            None,
            Some(position),
        );
        reused_output.set_preferred(output_mode);

        let compositor = match DrmCompositor::new(
            &reused_output,
            drm_surface,
            None,
            gbm_allocator,
            gbm_exporter,
            gpu.color_formats,
            gpu.renderer_formats.to_vec(),
            gpu.cursor_size,
            Some(gpu.gbm_device.clone()),
        ) {
            Ok(compositor) => compositor,
            Err(error) => {
                tracing::error!(target: "beewm::tray", "modeset: DrmCompositor::new failed: {}", error);
                return;
            }
        };

        gpu.surfaces.push(SurfaceData {
            connector,
            crtc,
            output: reused_output.clone(),
            position,
            compositor: Some(compositor),
            can_render: true,
            retry_after: None,
            consecutive_frame_failures: 0,
            pending_presentation_feedback: None,
            frame_stats: FrameStats::new(),
            present_stats: PresentStats::new(),
        });
        tracing::info!(
            target: "beewm::tray",
            width = output_mode.size.w,
            height = output_mode.size.h,
            refresh_hz = output_mode.refresh as f64 / 1000.0,
            "applied live output mode",
        );
        (reused_output, conn_info)
    };

    let (output, conn_info) = rebuilt;
    let modes = output_modes_for(&conn_info, &output);
    let applied_mode = modes.current;
    let output_name = output.name().to_string();
    data.state.set_output_modes(output, modes);
    if let Some(mode) = applied_mode {
        data.state.record_runtime_output_mode(output_name, mode);
    }
    data.state.needs_render = true;
    // Reflow tiling/floats against the output's new size.
    data.state.handle_output_geometry_changed();
}

/// Render every renderable surface (each connected output) and queue its frame.
/// A surface still waiting on its vblank is skipped and `needs_render` is
/// re-armed so it is retried on the next loop iteration.
fn render_frame(data: &mut UdevData) {
    let UdevData { state, gpu, .. } = data;
    let Some(gpu) = gpu.as_mut() else {
        return;
    };
    // Split-borrow: the device's single renderer is shared by every surface.
    let GpuData {
        renderer, surfaces, ..
    } = gpu;
    let mut any_skipped = false;
    let now = Instant::now();
    for surface in surfaces.iter_mut() {
        if let Some(retry_after) = surface.retry_after {
            if retry_after > now {
                any_skipped = true;
                continue;
            }
            surface.retry_after = None;
            surface.can_render = true;
        }
        if surface.can_render {
            render_one_surface(state, renderer, surface);
        } else {
            any_skipped = true;
        }
    }
    if any_skipped {
        // At least one surface is still waiting on its page-flip; keep the dirty
        // flag set so it renders once its vblank fires.
        state.needs_render = true;
    }
}

fn retry_delay_for_frame_failure(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(5);
    let delay = FRAME_FAILURE_RETRY_BASE.saturating_mul(1 << shift);
    std::cmp::min(delay, FRAME_FAILURE_RETRY_MAX)
}

fn record_frame_success(surface: &mut SurfaceData, output: &Output) {
    if surface.consecutive_frame_failures > 0 {
        tracing::info!(
            target: "beewm::frame",
            output = %output.name(),
            failures = surface.consecutive_frame_failures,
            "DRM frame output recovered",
        );
    }
    surface.retry_after = None;
    surface.consecutive_frame_failures = 0;
}

fn record_frame_failure<E: std::fmt::Debug>(
    state: &mut Beewm,
    surface: &mut SurfaceData,
    output: &Output,
    stage: &'static str,
    error: &E,
) {
    surface.consecutive_frame_failures = surface.consecutive_frame_failures.saturating_add(1);
    let failures = surface.consecutive_frame_failures;
    let retry_delay = retry_delay_for_frame_failure(failures);

    surface.can_render = false;
    surface.retry_after = Some(Instant::now() + retry_delay);
    surface.pending_presentation_feedback = None;
    state.needs_render = true;

    let elapsed = state.start_time.elapsed();
    send_frame_callbacks(state, output, elapsed, None);
    state.last_frame_callbacks_sent_at = Some(Instant::now());

    if failures == 1 || failures.is_power_of_two() {
        tracing::error!(
            target: "beewm::frame",
            output = %output.name(),
            stage,
            failures,
            retry_ms = retry_delay.as_millis() as u64,
            "DRM frame output failed; backing off before retry: {:?}",
            error,
        );
    } else {
        tracing::debug!(
            target: "beewm::frame",
            output = %output.name(),
            stage,
            failures,
            retry_ms = retry_delay.as_millis() as u64,
            "DRM frame output still failing: {:?}",
            error,
        );
    }
}

/// Render the current state into one output's DRM framebuffer and queue it.
fn render_one_surface(state: &mut Beewm, renderer: &mut GlesRenderer, surface: &mut SurfaceData) {
    surface.can_render = false;

    // No KMS surface right now (torn down across suspend, awaiting rebuild).
    if surface.compositor.is_none() {
        return;
    }

    let frame_start = Instant::now();

    let wedge_trace = crate::compositor::runtime_flags::flags().wedge_trace;
    if wedge_trace {
        tracing::warn!(
            target: "beewm::wedge",
            animating = state.animations.has_active(),
            scale = surface.output.current_scale().fractional_scale(),
            "render_one_surface: begin (building window elements)",
        );
    }
    let window_elements = window_render_elements(
        renderer,
        &state.space,
        &surface.output,
        1.0,
        &state.animations,
        Instant::now(),
    );
    if wedge_trace {
        tracing::warn!(
            target: "beewm::wedge",
            count = window_elements.len(),
            "window_render_elements: done",
        );
    }

    // True when an xdg-shell fullscreen or a fullscreen-sized X11 game covers
    // the output. Both should suppress top-layers so the game can be promoted
    // onto the primary plane by smithay's DrmCompositor.
    let fullscreen_active = state.screen_owned_by_window();
    // Whether the screen-owning window is an XWayland (X11) surface — many
    // games run through XWayland, and `beewm::frame` surfaces this so the log
    // shows which path is being exercised.
    let fullscreen_is_x11 = state
        .active_fullscreen()
        .and_then(|w| w.x11_surface())
        .is_some()
        || state.screen_owned_by_x11_window();

    let border_elements = state.border_elements();
    // Cursor visibility is driven entirely by Wayland client/pointer state, not
    // by fullscreen presentation. `effective_cursor_icon()` returns `None` (so
    // `cursor_elements` is empty) exactly when the focused client hid the cursor
    // (e.g. a null cursor surface) or when an active pointer-lock constraint
    // requires it. Fullscreen alone never hides the pointer here.
    //
    // The element is tagged `Kind::Cursor`, so smithay's `DrmCompositor`
    // promotes it onto the hardware cursor plane; a fullscreen game can still be
    // direct-scanned-out onto the primary plane with the cursor on its own
    // plane, so keeping the element does not block direct scanout.
    let cursor_elements = state.cursor_elements(renderer);

    // Render layer-shell surfaces (waybar, beebar, etc.) at the correct Z-order.
    // Clone output so we can borrow it for layer_map while also using the renderer.
    let output = surface.output.clone();

    let layers_below = layer_render_elements(
        renderer,
        &output,
        layers_rendered_below_windows(fullscreen_active),
        1.0,
    );
    let layers_above = layer_render_elements(
        renderer,
        &output,
        layers_rendered_above_windows(fullscreen_active),
        1.0,
    );

    process_pending_screencopies(state, renderer, &output);

    // When the session is locked, the only thing on screen is the lock surface
    // (plus the cursor). Everything else — windows, layer-shell surfaces,
    // borders — is omitted, and outputs without a live lock surface fall back to
    // the solid-black clear color. This is what guarantees a locked session
    // never leaks content, even if the lock client has died.
    let locked = state.locked;
    let lock_elements = if locked {
        let lock_surface = state.lock_surfaces.get(&output);
        lock_render_elements(renderer, &output, lock_surface, 1.0)
    } else {
        Vec::new()
    };

    let count_windows = window_elements.len();
    let count_borders = border_elements.len();
    let count_cursor = cursor_elements.len();
    let count_layers_above = layers_above.len();
    let count_layers_below = layers_below.len();

    // Build final element list front-to-back (first = topmost).
    let mut elements: Vec<OutputRenderElement> = Vec::new();
    elements.extend(cursor_elements.into_iter().map(OutputRenderElement::from));
    if locked {
        elements.extend(lock_elements.into_iter().map(OutputRenderElement::from));
    } else {
        elements.extend(layers_above.into_iter().map(OutputRenderElement::from));
        elements.extend(border_elements.into_iter().map(OutputRenderElement::from));
        elements.extend(window_elements.into_iter().map(OutputRenderElement::from));
        elements.extend(layers_below.into_iter().map(OutputRenderElement::from));
    }

    let clear_color = if locked {
        [0.0, 0.0, 0.0, 1.0]
    } else {
        [0.1, 0.1, 0.1, 1.0]
    };

    if wedge_trace {
        tracing::warn!(
            target: "beewm::wedge",
            crtc = ?surface.crtc,
            elements = elements.len(),
            "render_frame: begin",
        );
    }
    let result = surface.compositor.as_mut().unwrap().render_frame::<_, OutputRenderElement>(
        renderer,
        &elements,
        clear_color,
        FrameFlags::DEFAULT,
    );
    if wedge_trace {
        tracing::warn!(target: "beewm::wedge", ok = result.is_ok(), "render_frame: returned");
    }

    match result {
        Ok(result) => {
            let render_states = result.states.clone();
            let is_scanout = matches!(result.primary_element, PrimaryPlaneElement::Element(_));
            let overlay_count = result.overlay_elements.len();
            let cursor_plane_used = result.cursor_element.is_some();
            let is_empty = result.is_empty;
            update_primary_scanout_output(state, &output, &render_states);

            if is_empty {
                // No damage — nothing to scan out. The caller already
                // cleared `needs_render`; re-allow the next render and send
                // frame callbacks now since no VBlank will fire to do it.
                record_frame_success(surface, &output);
                surface.can_render = true;
                surface.pending_presentation_feedback = None;
                let elapsed = state.start_time.elapsed();
                send_frame_callbacks(
                    state,
                    &output,
                    elapsed,
                    Some(output_frame_interval(&output)),
                );
                state.last_frame_callbacks_sent_at = Some(Instant::now());
            } else if let Err(e) = {
                if wedge_trace {
                    tracing::warn!(target: "beewm::wedge", "queue_frame: begin (atomic commit)");
                }
                let r = surface.compositor.as_mut().unwrap().queue_frame(());
                if wedge_trace {
                    tracing::warn!(target: "beewm::wedge", ok = r.is_ok(), "queue_frame: returned");
                }
                r
            } {
                record_frame_failure(state, surface, &output, "queue_frame", &e);
            } else {
                record_frame_success(surface, &output);
                surface.pending_presentation_feedback = Some(collect_presentation_feedback(
                    state,
                    &output,
                    &render_states,
                ));
                // Frame-pacing fix: invite clients to draw their *next* frame
                // as soon as this one is queued for scan-out, so the client
                // renders in parallel with the current frame being displayed
                // instead of serializing client-render + our composite into the
                // single refresh interval that follows the vblank. This is what
                // lets fast clients (games, native or XWayland) actually reach
                // the monitor refresh rate. `wp_presentation` feedback is still
                // emitted at the real vblank below, so reported timing stays
                // accurate. Set BEEWM_FRAME_CALLBACK_AT_VBLANK to restore the
                // legacy at-vblank behaviour for comparison.
                if !crate::compositor::runtime_flags::flags().frame_callback_at_vblank {
                    let elapsed = state.start_time.elapsed();
                    send_frame_callbacks(
                        state,
                        &output,
                        elapsed,
                        Some(output_frame_interval(&output)),
                    );
                    state.last_frame_callbacks_sent_at = Some(Instant::now());
                }
            }

            record_frame_stats(
                &mut surface.frame_stats,
                FrameStatsSample {
                    render_time: frame_start.elapsed(),
                    is_scanout,
                    is_empty,
                    overlay_count,
                    cursor_plane_used,
                    fullscreen_active,
                    fullscreen_is_x11,
                    count_windows,
                    count_borders,
                    count_cursor,
                    count_layers_above,
                    count_layers_below,
                },
            );
            // For the normal non-empty case, frame callbacks are sent from the
            // VBlank handler once the hardware confirms the frame is on screen.
        }
        Err(e) => {
            record_frame_failure(state, surface, &output, "render_frame", &e);
        }
    }
}

/// Update per-frame counters and emit `beewm::frame` log lines at digestible
/// rates: one line on every scanout↔composition transition (with the full
/// element breakdown of *that* frame) and one summary every ~1 second.
struct FrameStatsSample {
    render_time: Duration,
    is_scanout: bool,
    is_empty: bool,
    overlay_count: usize,
    cursor_plane_used: bool,
    fullscreen_active: bool,
    fullscreen_is_x11: bool,
    count_windows: usize,
    count_borders: usize,
    count_cursor: usize,
    count_layers_above: usize,
    count_layers_below: usize,
}

fn record_frame_stats(stats: &mut FrameStats, sample: FrameStatsSample) {
    let FrameStatsSample {
        render_time,
        is_scanout,
        is_empty,
        overlay_count,
        cursor_plane_used,
        fullscreen_active,
        fullscreen_is_x11,
        count_windows,
        count_borders,
        count_cursor,
        count_layers_above,
        count_layers_below,
    } = sample;

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
            fullscreen_is_x11,
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
            fullscreen_is_x11,
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
    refresh_rate: Option<u32>,
    output_configs: &[OutputConfig],
) -> Result<(GpuData, DmabufSetup, Option<DrmSyncobjState>), Box<dyn std::error::Error>> {
    // Open DRM device via session
    let fd = session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
    let device_fd: DeviceFd = fd.into();
    let drm_fd = DrmDeviceFd::new(device_fd);

    let (mut drm_device, drm_notifier) = DrmDevice::new(drm_fd.clone(), false)?;

    let resources = drm_fd.resource_handles()?;

    // ── Device-level resources, shared by every connector's surface ──────────
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

    // Decide how to advertise the dmabuf global. Prefer v4 feedback built from
    // the render device + the formats the GL renderer can actually import, so
    // Mesa/XWayland clients use the GPU fast path instead of falling back to
    // shm (the classic cause of ~20-30 fps games). Fall back to the legacy v3
    // global only if we can't read the device id or build the feedback.
    let dmabuf_formats = renderer_formats.clone();
    let format_count = dmabuf_formats.len();
    // Prefer the *render* node's device id as the feedback main device: it is
    // the unprivileged node (e.g. /dev/dri/renderD128) clients open to allocate
    // GBM buffers. Fall back to the primary node, then to the raw fd dev id.
    let main_device = DrmNode::from_path(path)
        .ok()
        .map(|node| {
            node.node_with_type(NodeType::Render)
                .and_then(Result::ok)
                .unwrap_or(node)
                .dev_id()
        })
        .or_else(|| drm_fd.dev_id().ok());
    let dmabuf_setup = match main_device {
        Some(dev_id) => match DmabufFeedbackBuilder::new(dev_id, dmabuf_formats.clone()).build() {
            Ok(feedback) => {
                tracing::info!(
                    target: "beewm::dmabuf",
                    main_device = dev_id,
                    formats = format_count,
                    "advertising dmabuf with default feedback (v4); clients can use the GPU fast path",
                );
                if format_count == 0 {
                    tracing::warn!(
                        target: "beewm::dmabuf",
                        "renderer reported zero dmabuf import formats — clients will be forced to shm",
                    );
                }
                DmabufSetup::Feedback(Box::new(feedback))
            }
            Err(e) => {
                tracing::warn!(
                    target: "beewm::dmabuf",
                    "failed to build dmabuf feedback ({}); falling back to legacy v3 global",
                    e,
                );
                DmabufSetup::Legacy(dmabuf_formats)
            }
        },
        None => {
            tracing::warn!(
                target: "beewm::dmabuf",
                "could not determine DRM device id; falling back to legacy v3 dmabuf global",
            );
            DmabufSetup::Legacy(dmabuf_formats)
        }
    };

    let renderer = unsafe { GlesRenderer::new(egl_context)? };
    let cursor_size = drm_device.cursor_size();

    use smithay::backend::allocator::Fourcc;
    let color_formats = [Fourcc::Argb8888, Fourcc::Xrgb8888];

    // ── One surface per connected connector ──────────────────────────────────
    // Multi-head is gated on BEEWM_MULTI_OUTPUT; with the gate off we drive only
    // the first connected connector, exactly as before.
    let multi_output = crate::compositor::runtime_flags::flags().multi_output_enabled;
    let mut surfaces: Vec<SurfaceData> = Vec::new();
    let mut used_crtcs: HashSet<crtc::Handle> = HashSet::new();
    let mut x_offset: i32 = 0;

    for conn_handle in resources.connectors() {
        let conn_info = match drm_fd.get_connector(*conn_handle, false) {
            Ok(info) => info,
            Err(_) => continue,
        };
        if conn_info.state() != connector::State::Connected || conn_info.modes().is_empty() {
            continue;
        }

        let name = connector_name(&conn_info);
        let cfg = config_for_output(output_configs, &name);
        if cfg.map(|c| !c.enabled).unwrap_or(false) {
            tracing::info!("Output {} disabled by config", name);
            continue;
        }

        let Some(drm_mode) = resolve_connector_mode(&conn_info, cfg, refresh_rate) else {
            continue;
        };

        let crtc_handle =
            match find_crtc_for_connector(&drm_fd, &resources, *conn_handle, &used_crtcs) {
                Ok(crtc) => crtc,
                Err(error) => {
                    tracing::warn!("Skipping connector {}: {}", name, error);
                    continue;
                }
            };

        // Honor a configured position; otherwise auto-pack left-to-right.
        let position = cfg
            .and_then(|c| c.position)
            .map(|(x, y)| Point::<i32, Logical>::from((x, y)))
            .unwrap_or_else(|| Point::from((x_offset, 0)));

        match build_surface_for_connector(
            &mut drm_device,
            &gbm_device,
            SurfaceBuildParams {
                renderer_formats: &renderer_formats,
                color_formats,
                cursor_size,
                display_handle,
                name: &name,
                connector_handle: *conn_handle,
                conn_info: &conn_info,
                drm_mode,
                crtc_handle,
                position,
            },
        ) {
            Ok(surface) => {
                used_crtcs.insert(crtc_handle);
                // Keep the auto cursor past any explicitly-placed output so the
                // next auto-positioned output doesn't overlap it.
                x_offset = (position.x + drm_mode.size().0 as i32).max(x_offset);
                surfaces.push(surface);
            }
            Err(error) => {
                tracing::warn!("Failed to create surface for connector {}: {}", name, error);
                continue;
            }
        }

        // Default (gate off): drive only the first connected connector.
        if !multi_output {
            break;
        }
    }

    if surfaces.is_empty() {
        return Err("No connected display with a usable CRTC found".into());
    }

    let nvidia_drm = is_nvidia_drm_device(&drm_fd);
    let syncobj_state = if crate::compositor::runtime_flags::flags().explicit_sync_disabled {
        tracing::warn!("Explicit sync disabled by BEEWM_NO_EXPLICIT_SYNC");
        None
    } else if nvidia_drm {
        tracing::warn!(
            target: "beewm::sync",
            "explicit sync disabled on NVIDIA DRM device; IN_FENCE_FD atomic commits are unreliable across suspend/resume",
        );
        None
    } else if supports_syncobj_eventfd(&drm_fd) {
        tracing::info!(
            target: "beewm::sync",
            "explicit sync enabled (linux-drm-syncobj-v1 with eventfd waits)"
        );
        Some(DrmSyncobjState::new::<Beewm>(
            display_handle,
            drm_fd.clone(),
        ))
    } else {
        tracing::info!(
            target: "beewm::sync",
            "DRM syncobj eventfd unsupported on {} — explicit sync off",
            path.display()
        );
        None
    };

    tracing::info!(
        "Initialized {} output surface(s) on {}",
        surfaces.len(),
        path.display()
    );

    // VBlank: route the page-flip completion to the surface whose CRTC fired,
    // acknowledge it, emit presentation feedback, and re-arm that surface.
    let drm_notifier_token = event_loop.handle().insert_source(
        drm_notifier,
        |event, metadata, data: &mut UdevData| match event {
            DrmEvent::VBlank(crtc) => {
                if crate::compositor::runtime_flags::flags().wedge_trace {
                    tracing::warn!(target: "beewm::wedge", ?crtc, "vblank: page-flip completed");
                }
                let mut vblank_output = None;
                let mut feedback_presented = false;
                if let Some(gpu) = data.gpu.as_mut()
                    && let Some(surface) =
                        gpu.surfaces.iter_mut().find(|surface| surface.crtc == crtc)
                {
                    if let Some(Err(e)) = surface.compositor.as_mut().map(|c| c.frame_submitted()) {
                        tracing::error!("frame_submitted error: {:?}", e);
                    }
                    surface.can_render = true;
                    let interval = output_frame_interval(&surface.output);
                    let refresh = Refresh::fixed(interval);
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
                    if let Some(mut feedback) = surface.pending_presentation_feedback.take() {
                        feedback.presented(
                            presentation_time,
                            refresh,
                            sequence,
                            smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                        );
                        feedback_presented = true;
                    }
                    // beewm::presentation — real interval between page-flips.
                    surface
                        .present_stats
                        .record_vblank(interval, feedback_presented);
                    vblank_output = Some(surface.output.clone());
                }

                // Legacy at-vblank frame-callback pacing (opt-in A/B test).
                if crate::compositor::runtime_flags::flags().frame_callback_at_vblank
                    && let Some(output) = vblank_output
                {
                    let elapsed = data.state.start_time.elapsed();
                    send_frame_callbacks(
                        &data.state,
                        &output,
                        elapsed,
                        Some(output_frame_interval(&output)),
                    );
                    data.state.last_frame_callbacks_sent_at = Some(Instant::now());
                }
            }
            DrmEvent::Error(e) => tracing::error!("DRM error: {:?}", e),
        },
    )?;

    Ok((
        GpuData {
            drm_device,
            drm_fd,
            _drm_notifier_token: drm_notifier_token,
            gbm_device,
            renderer,
            surfaces,
            renderer_formats,
            color_formats,
            cursor_size,
        },
        dmabuf_setup,
        syncobj_state,
    ))
}

fn is_nvidia_drm_device(drm_fd: &DrmDeviceFd) -> bool {
    match drm_fd.get_driver() {
        Ok(driver) => {
            let name = driver.name().to_string_lossy().to_lowercase();
            let description = driver.description().to_string_lossy().to_lowercase();
            name.contains("nvidia") || description.contains("nvidia")
        }
        Err(error) => {
            tracing::debug!(
                target: "beewm::sync",
                %error,
                "could not query DRM driver while deciding explicit-sync support",
            );
            false
        }
    }
}

/// Build a render surface (its own `DrmCompositor` + `Output`) for one connected
/// connector on `crtc_handle`, placed at `position`. Shared by initial
/// enumeration and runtime hotplug so the two paths can never diverge.
struct SurfaceBuildParams<'a> {
    renderer_formats: &'a [Format],
    color_formats: [smithay::backend::allocator::Fourcc; 2],
    cursor_size: smithay::utils::Size<u32, smithay::utils::Buffer>,
    display_handle: &'a smithay::reexports::wayland_server::DisplayHandle,
    name: &'a str,
    connector_handle: connector::Handle,
    conn_info: &'a connector::Info,
    drm_mode: DrmMode,
    crtc_handle: crtc::Handle,
    position: Point<i32, Logical>,
}

/// Replace a surface's `DrmCompositor` with a freshly created one (new KMS
/// surface, new GBM swapchain), reusing the existing `Output`.
///
/// This is the resume recovery path. After suspend the NVIDIA driver leaves the
/// old `DrmCompositor`'s KMS state stale: `reset_state()` is not enough, and the
/// first atomic commit on it hangs the whole event loop (the panel never lights
/// — "atomic commits unreliable across suspend/resume"). Building a brand-new
/// compositor reproduces the boot path, which always does a clean modeset, while
/// keeping the same `Output` so wl_output clients aren't disturbed.
fn rebuild_surface_compositor(
    drm_device: &mut DrmDevice,
    gbm_device: &GbmDevice<DrmDeviceFd>,
    drm_fd: &DrmDeviceFd,
    renderer_formats: &[Format],
    color_formats: [smithay::backend::allocator::Fourcc; 2],
    cursor_size: smithay::utils::Size<u32, smithay::utils::Buffer>,
    surface: &mut SurfaceData,
    refresh_rate: Option<u32>,
    output_configs: &[OutputConfig],
) -> Result<(), Box<dyn std::error::Error>> {
    let conn_info = drm_fd.get_connector(surface.connector, false)?;
    let name = connector_name(&conn_info);
    let cfg = config_for_output(output_configs, &name);
    let drm_mode = resolve_connector_mode(&conn_info, cfg, refresh_rate)
        .ok_or("no usable mode for connector on resume")?;

    // Drop the old compositor first: it holds the CRTC's primary-plane claim, and
    // `create_surface` would fail with NoPlane while that claim is live.
    surface.compositor = None;
    let drm_surface = drm_device.create_surface(surface.crtc, drm_mode, &[surface.connector])?;
    let gbm_allocator = GbmAllocator::new(
        gbm_device.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let gbm_exporter = GbmFramebufferExporter::new(gbm_device.clone(), None);
    let compositor = DrmCompositor::new(
        &surface.output,
        drm_surface,
        None,
        gbm_allocator,
        gbm_exporter,
        color_formats,
        renderer_formats.to_vec(),
        cursor_size,
        Some(gbm_device.clone()),
    )?;
    surface.compositor = Some(compositor);
    Ok(())
}

fn build_surface_for_connector(
    drm_device: &mut DrmDevice,
    gbm_device: &GbmDevice<DrmDeviceFd>,
    params: SurfaceBuildParams<'_>,
) -> Result<SurfaceData, Box<dyn std::error::Error>> {
    let SurfaceBuildParams {
        renderer_formats,
        color_formats,
        cursor_size,
        display_handle,
        name,
        connector_handle,
        conn_info,
        drm_mode,
        crtc_handle,
        position,
    } = params;

    let drm_surface = drm_device.create_surface(crtc_handle, drm_mode, &[connector_handle])?;

    // Per-surface allocator/exporter, both backed by the shared GBM device.
    let gbm_allocator = GbmAllocator::new(
        gbm_device.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let gbm_exporter = GbmFramebufferExporter::new(gbm_device.clone(), None);

    let (phys_w, phys_h) = conn_info
        .size()
        .map(|(w, h)| (w as i32, h as i32))
        .unwrap_or((0, 0));

    let output = Output::new(
        name.to_string(),
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
        Some(position),
    );
    output.set_preferred(output_mode);

    let frame_interval = output_frame_interval(&output);
    tracing::info!(
        target: "beewm::presentation",
        connector = ?connector_handle,
        width = output_mode.size.w,
        height = output_mode.size.h,
        refresh_mhz = output_mode.refresh,
        refresh_hz = output_mode.refresh as f64 / 1000.0,
        frame_interval_us = frame_interval.as_micros() as u64,
        position_x = position.x,
        "configured output mode",
    );

    let compositor = DrmCompositor::new(
        &output,
        drm_surface,
        None,
        gbm_allocator,
        gbm_exporter,
        color_formats,
        renderer_formats.to_vec(),
        cursor_size,
        Some(gbm_device.clone()),
    )?;

    Ok(SurfaceData {
        connector: connector_handle,
        crtc: crtc_handle,
        output,
        position,
        compositor: Some(compositor),
        can_render: true, // allow the first frame immediately
        retry_after: None,
        consecutive_frame_failures: 0,
        pending_presentation_feedback: None,
        frame_stats: FrameStats::new(),
        present_stats: PresentStats::new(),
    })
}

/// Re-scan a device's connectors after a hotplug event and reconcile surfaces:
/// build a surface for any newly-connected display, and tear down + migrate any
/// surface whose connector has gone away. Only active under `BEEWM_MULTI_OUTPUT`.
fn rescan_connectors(data: &mut UdevData) {
    // Snapshot the live connector set (immutable borrow of the device fd).
    let resources = match data.gpu.as_ref() {
        Some(gpu) => match gpu.drm_fd.resource_handles() {
            Ok(resources) => resources,
            Err(error) => {
                tracing::warn!("Hotplug rescan failed to read DRM resources: {}", error);
                return;
            }
        },
        None => return,
    };
    let connected: Vec<connector::Handle> = {
        let Some(gpu) = data.gpu.as_ref() else { return };
        resources
            .connectors()
            .iter()
            .copied()
            .filter(|conn| {
                gpu.drm_fd
                    .get_connector(*conn, false)
                    .map(|info| {
                        info.state() == connector::State::Connected && !info.modes().is_empty()
                    })
                    .unwrap_or(false)
            })
            .collect()
    };

    // ── Tear down surfaces whose connector disappeared ───────────────────────
    let gone: Vec<(connector::Handle, Output)> = data
        .gpu
        .as_ref()
        .map(|gpu| {
            gpu.surfaces
                .iter()
                .filter(|surface| !connected.contains(&surface.connector))
                .map(|surface| (surface.connector, surface.output.clone()))
                .collect()
        })
        .unwrap_or_default();
    for (connector, output) in gone {
        tracing::info!("Output disconnected: connector {:?}", connector);
        if let Some(gpu) = data.gpu.as_mut() {
            gpu.surfaces
                .retain(|surface| surface.connector != connector);
        }
        data.state.remove_output(&output);
    }

    // ── Build surfaces for newly-connected connectors ────────────────────────
    let refresh_rate = data.state.config.refresh_rate;
    let output_configs = data.state.config.outputs.clone();
    let display_handle = data.state.display_handle.clone();
    for conn_handle in connected {
        // Skip connectors that already have a surface.
        let already_driven = data
            .gpu
            .as_ref()
            .map(|gpu| gpu.surfaces.iter().any(|s| s.connector == conn_handle))
            .unwrap_or(true);
        if already_driven {
            continue;
        }

        // Resolve name/config/mode/CRTC under an immutable device borrow.
        // `conn_info`/`name`/`cfg` are owned so they survive the later mutable
        // borrow for surface creation.
        let prepared = data.gpu.as_ref().and_then(|gpu| {
            let conn_info = gpu.drm_fd.get_connector(conn_handle, false).ok()?;
            let name = connector_name(&conn_info);
            let cfg = config_for_output(&output_configs, &name).cloned();
            if cfg.as_ref().map(|c| !c.enabled).unwrap_or(false) {
                tracing::info!("Output {} disabled by config; not adding", name);
                return None;
            }
            let drm_mode = resolve_connector_mode(&conn_info, cfg.as_ref(), refresh_rate)?;
            let used: HashSet<crtc::Handle> =
                gpu.surfaces.iter().map(|surface| surface.crtc).collect();
            let crtc = find_crtc_for_connector(&gpu.drm_fd, &resources, conn_handle, &used).ok()?;
            Some((conn_info, name, cfg, drm_mode, crtc))
        });
        let Some((conn_info, name, cfg, drm_mode, crtc_handle)) = prepared else {
            continue;
        };

        // Honor a configured position; otherwise place to the right of every
        // existing output.
        let auto_x: i32 = data
            .gpu
            .as_ref()
            .map(|gpu| {
                gpu.surfaces
                    .iter()
                    .filter_map(|surface| {
                        let geo = data.state.space.output_geometry(&surface.output)?;
                        Some(geo.loc.x + geo.size.w)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let position = cfg
            .as_ref()
            .and_then(|c| c.position)
            .map(|(x, y)| Point::<i32, Logical>::from((x, y)))
            .unwrap_or_else(|| Point::from((auto_x, 0)));

        // Create the surface (mutable device borrow), then register the output.
        let built = if let Some(gpu) = data.gpu.as_mut() {
            let GpuData {
                drm_device,
                gbm_device,
                renderer_formats,
                color_formats,
                cursor_size,
                surfaces,
                ..
            } = gpu;
            match build_surface_for_connector(
                drm_device,
                gbm_device,
                SurfaceBuildParams {
                    renderer_formats,
                    color_formats: *color_formats,
                    cursor_size: *cursor_size,
                    display_handle: &display_handle,
                    name: &name,
                    connector_handle: conn_handle,
                    conn_info: &conn_info,
                    drm_mode,
                    crtc_handle,
                    position,
                },
            ) {
                Ok(surface) => {
                    let registered = (surface.output.clone(), surface.position);
                    surfaces.push(surface);
                    Some(registered)
                }
                Err(error) => {
                    tracing::warn!(
                        "Failed to build surface for hotplugged connector {}: {}",
                        name,
                        error
                    );
                    None
                }
            }
        } else {
            None
        };

        if let Some((output, position)) = built {
            tracing::info!("Output connected: {}", name);
            let modes = output_modes_for(&conn_info, &output);
            data.state.add_output(output.clone(), position);
            data.state.set_output_modes(output, modes);
        }
    }

    // Repack + relayout against the new output set.
    data.state.handle_output_geometry_changed();
}

/// Human-readable connector name (`DP-3`, `eDP-1`, `HDMI-A-1`, …) for config
/// matching, bars, and logs — instead of the opaque connector-handle debug
/// string. Built from the Debug form of the interface so it stays correct even
/// for connector kinds not explicitly mapped here.
fn connector_name(info: &connector::Info) -> String {
    let raw = format!("{:?}", info.interface());
    let kind = match raw.as_str() {
        "DisplayPort" => "DP",
        "HDMIA" => "HDMI-A",
        "HDMIB" => "HDMI-B",
        "EmbeddedDisplayPort" => "eDP",
        "DVID" => "DVI-D",
        "DVII" => "DVI-I",
        "DVIA" => "DVI-A",
        "VGA" => "VGA",
        "Virtual" => "Virtual",
        "DSI" => "DSI",
        other => other,
    };
    format!("{}-{}", kind, info.interface_id())
}

/// The config stanza matching a connector name, if any.
fn config_for_output<'a>(configs: &'a [OutputConfig], name: &str) -> Option<&'a OutputConfig> {
    configs.iter().find(|cfg| cfg.name == name)
}

/// Find the DRM mode matching a configured `WxH[@Hz]` spec, if the connector
/// advertises one. Returns `None` so the caller can fall back to the preferred
/// mode when the requested mode is unavailable.
fn find_mode_for_spec(conn_info: &connector::Info, spec: OutputModeSpec) -> Option<DrmMode> {
    conn_info
        .modes()
        .iter()
        .find(|m| {
            let (w, h) = m.size();
            w as i32 == spec.width
                && h as i32 == spec.height
                && spec.refresh.map(|hz| m.vrefresh() == hz).unwrap_or(true)
        })
        .copied()
}

/// Resolve the mode for a connector honoring an `output … mode` override, else
/// the configured global refresh rate, else the preferred mode.
fn resolve_connector_mode(
    conn_info: &connector::Info,
    cfg: Option<&OutputConfig>,
    refresh_rate: Option<u32>,
) -> Option<DrmMode> {
    if let Some(spec) = cfg.and_then(|c| c.mode) {
        if let Some(mode) = find_mode_for_spec(conn_info, spec) {
            return Some(mode);
        }
        tracing::warn!(
            "Configured mode {}x{}{} not available on this output; using preferred",
            spec.width,
            spec.height,
            spec.refresh.map(|hz| format!("@{hz}")).unwrap_or_default(),
        );
    }
    select_connector_mode(conn_info, refresh_rate)
}

/// Choose the mode to drive a connector with: the PREFERRED mode (or the first
/// available), optionally narrowed to a configured refresh rate at the same
/// resolution.
fn select_connector_mode(
    conn_info: &connector::Info,
    refresh_rate: Option<u32>,
) -> Option<DrmMode> {
    let preferred = conn_info
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .or_else(|| conn_info.modes().first().copied())?;

    let mode = match refresh_rate {
        Some(target_hz) => conn_info
            .modes()
            .iter()
            .find(|m| m.size() == preferred.size() && m.vrefresh() == target_hz)
            .copied()
            .unwrap_or(preferred),
        None => preferred,
    };
    Some(mode)
}

/// Find a CRTC that can drive the given connector.
fn find_crtc_for_connector(
    drm: &DrmDeviceFd,
    resources: &smithay::reexports::drm::control::ResourceHandles,
    connector: connector::Handle,
    used: &HashSet<crtc::Handle>,
) -> Result<crtc::Handle, Box<dyn std::error::Error>> {
    let conn_info = drm.get_connector(connector, false)?;

    for encoder_handle in conn_info.encoders() {
        if let Ok(encoder_info) = drm.get_encoder(*encoder_handle) {
            let crtcs = resources.filter_crtcs(encoder_info.possible_crtcs());
            // Skip CRTCs already claimed by an earlier connector so each output
            // scans out on its own pipe.
            if let Some(&crtc_handle) = crtcs.iter().find(|crtc| !used.contains(crtc)) {
                return Ok(crtc_handle);
            }
        }
    }

    Err("No unused CRTC available for connector".into())
}
