//! Runtime kill-switches for diagnosing freezes.
//!
//! Each suspect feature can be disabled at startup via an environment
//! variable, so users can A/B-test which subsystem is responsible for a hang
//! without rebuilding. Set the variable to any non-empty value to disable.
//!
//! | Env var                        | Disables                                   |
//! |--------------------------------|--------------------------------------------|
//! | `BEEWM_NO_FOCUS_IPC`           | Window-name file write + event-socket push |
//! | `BEEWM_NO_WORKSPACE_PUBLISH`   | Workspace state file writes + push         |
//! | `BEEWM_NO_EVENT_BROADCASTER`   | Background broadcaster thread spawn        |
//! | `BEEWM_NO_EXPLICIT_SYNC`       | DRM syncobj / explicit-sync surface hooks  |
//! | `BEEWM_NO_DMABUF`              | wp_linux_dmabuf global (force shm clients) |
//! | `BEEWM_DISABLE_ANIMATIONS`     | All compositor-driven window animations    |
//! | `BEEWM_NO_SESSION_ENV_EXPORT`  | Pushing session env to D-Bus/systemd (portals) |
//! | `BEEWM_NO_KEYBOARD_LEDS`       | Lock-key LED writes to physical keyboards  |
//!
//! These exist purely as a debugging aid; once the freeze is root-caused
//! they should all be removed (or promoted to real config options).
//!
//! There is also one *behaviour* toggle (not a kill-switch) used to A/B test
//! the frame-pacing fix for low in-game FPS:
//!
//! | Env var                          | Effect                                      |
//! |----------------------------------|---------------------------------------------|
//! | `BEEWM_FRAME_CALLBACK_AT_VBLANK` | Send `wl_surface.frame` callbacks at vblank |
//!
//! By default beewm sends frame callbacks as soon as a frame has been queued
//! for scan-out, so a client can render its next frame *in parallel* with the
//! current one being displayed (pipelined). Setting this variable restores the
//! older "send callbacks only after the vblank fires" behaviour, which couples
//! client render + compositor composite into a single refresh interval and can
//! cap fast clients (games) well below the display refresh rate. Keeping the
//! toggle lets users confirm on their own hardware that the pacing change is
//! what restored full frame rate.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeFlags {
    pub focus_ipc_disabled: bool,
    pub workspace_publish_disabled: bool,
    pub event_broadcaster_disabled: bool,
    pub explicit_sync_disabled: bool,
    pub dmabuf_disabled: bool,
    /// When true, disable all compositor-driven window animations regardless of
    /// the config. A debugging aid: animations interact with damage/frame
    /// scheduling, so being able to turn them off without editing the config
    /// helps isolate rendering bugs.
    pub animations_disabled: bool,
    /// When true, do NOT push the session environment (WAYLAND_DISPLAY,
    /// XDG_CURRENT_DESKTOP, …) into the D-Bus activation / systemd user
    /// environment at startup. Exporting it is what lets the screen-sharing
    /// portal find the display; this switch exists only to isolate problems
    /// (e.g. a hanging `systemctl`/`dbus-update-activation-environment`).
    pub session_env_export_disabled: bool,
    /// When true, never write lock-key LED state (Caps/Num/Scroll) to
    /// physical keyboards. The writes are tiny libinput calls made while
    /// processing key events; this switch isolates them when debugging
    /// input-path stalls or misbehaving keyboard firmware.
    pub keyboard_leds_disabled: bool,
    /// When true, restore the legacy behaviour of sending `wl_surface.frame`
    /// callbacks from the vblank handler instead of right after the frame is
    /// queued for scan-out. Used to A/B test the frame-pacing fix.
    pub frame_callback_at_vblank: bool,
    /// Opt-in: drive *every* connected connector as its own output instead of
    /// only the first one. Off by default while the multi-head DRM backend
    /// (per-CRTC `DrmCompositor` + per-vblank routing) is validated on real
    /// dual-monitor hardware — with it unset beewm enumerates a single
    /// connector exactly as before. Set `BEEWM_MULTI_OUTPUT=1` to enable.
    pub multi_output_enabled: bool,
    /// Opt-in: force-enable the StatusNotifier settings tray even when the
    /// config has not set `tray enable`. The two are OR'd, so this is a quick way
    /// to try the tray without editing the config. Set `BEEWM_TRAY=1`.
    pub tray_enabled: bool,
    /// Opt-in: allow the tray's Resolution/Refresh menu to perform a *live* DRM
    /// modeset (rebuilds the connector's surface). Off by default because the
    /// runtime modeset path is not yet hardware-validated; when unset the
    /// request is logged and skipped. Set `BEEWM_LIVE_MODESET=1`.
    pub live_modeset_enabled: bool,
    /// Diagnostic: emit fsync'd breadcrumb logs around each DRM operation
    /// (render / queue_frame / vblank) so the last line before a hard GPU wedge
    /// survives an unclean reboot and names the hung call. Set `BEEWM_WEDGE_TRACE=1`.
    pub wedge_trace: bool,
}

impl RuntimeFlags {
    fn from_env() -> Self {
        let on = |key: &str| {
            std::env::var_os(key)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
        };
        let flags = Self {
            focus_ipc_disabled: on("BEEWM_NO_FOCUS_IPC"),
            workspace_publish_disabled: on("BEEWM_NO_WORKSPACE_PUBLISH"),
            event_broadcaster_disabled: on("BEEWM_NO_EVENT_BROADCASTER"),
            explicit_sync_disabled: on("BEEWM_NO_EXPLICIT_SYNC"),
            dmabuf_disabled: on("BEEWM_NO_DMABUF"),
            animations_disabled: on("BEEWM_DISABLE_ANIMATIONS"),
            session_env_export_disabled: on("BEEWM_NO_SESSION_ENV_EXPORT"),
            keyboard_leds_disabled: on("BEEWM_NO_KEYBOARD_LEDS"),
            frame_callback_at_vblank: on("BEEWM_FRAME_CALLBACK_AT_VBLANK"),
            multi_output_enabled: on("BEEWM_MULTI_OUTPUT"),
            tray_enabled: on("BEEWM_TRAY"),
            live_modeset_enabled: on("BEEWM_LIVE_MODESET"),
            wedge_trace: on("BEEWM_WEDGE_TRACE"),
        };
        if flags.focus_ipc_disabled
            || flags.workspace_publish_disabled
            || flags.event_broadcaster_disabled
            || flags.explicit_sync_disabled
            || flags.dmabuf_disabled
            || flags.animations_disabled
            || flags.session_env_export_disabled
            || flags.keyboard_leds_disabled
        {
            tracing::warn!(
                "BEEWM runtime kill-switches active: focus_ipc_disabled={} workspace_publish_disabled={} event_broadcaster_disabled={} explicit_sync_disabled={} dmabuf_disabled={} animations_disabled={} session_env_export_disabled={} keyboard_leds_disabled={}",
                flags.focus_ipc_disabled,
                flags.workspace_publish_disabled,
                flags.event_broadcaster_disabled,
                flags.explicit_sync_disabled,
                flags.dmabuf_disabled,
                flags.animations_disabled,
                flags.session_env_export_disabled,
                flags.keyboard_leds_disabled,
            );
        }
        if flags.frame_callback_at_vblank {
            tracing::warn!(
                "BEEWM_FRAME_CALLBACK_AT_VBLANK set: using legacy at-vblank frame \
                 callbacks (no pipelining) — expect lower in-game FPS"
            );
        }
        if flags.multi_output_enabled {
            tracing::warn!(
                "BEEWM_MULTI_OUTPUT set: enabling experimental multi-head output \
                 enumeration (per-connector outputs) — validate scan-out/FPS on your hardware"
            );
        }
        if flags.tray_enabled {
            tracing::warn!("BEEWM_TRAY set: force-enabling the experimental settings tray icon");
        }
        if flags.live_modeset_enabled {
            tracing::warn!(
                "BEEWM_LIVE_MODESET set: enabling experimental runtime resolution/refresh \
                 changes from the tray (rebuilds the output surface) — unvalidated on hardware"
            );
        }
        flags
    }
}

static FLAGS: OnceLock<RuntimeFlags> = OnceLock::new();

pub fn flags() -> RuntimeFlags {
    *FLAGS.get_or_init(RuntimeFlags::from_env)
}
