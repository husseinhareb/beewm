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
    /// When true, restore the legacy behaviour of sending `wl_surface.frame`
    /// callbacks from the vblank handler instead of right after the frame is
    /// queued for scan-out. Used to A/B test the frame-pacing fix.
    pub frame_callback_at_vblank: bool,
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
            frame_callback_at_vblank: on("BEEWM_FRAME_CALLBACK_AT_VBLANK"),
        };
        if flags.focus_ipc_disabled
            || flags.workspace_publish_disabled
            || flags.event_broadcaster_disabled
            || flags.explicit_sync_disabled
            || flags.dmabuf_disabled
            || flags.animations_disabled
            || flags.session_env_export_disabled
        {
            tracing::warn!(
                "BEEWM runtime kill-switches active: focus_ipc_disabled={} workspace_publish_disabled={} event_broadcaster_disabled={} explicit_sync_disabled={} dmabuf_disabled={} animations_disabled={} session_env_export_disabled={}",
                flags.focus_ipc_disabled,
                flags.workspace_publish_disabled,
                flags.event_broadcaster_disabled,
                flags.explicit_sync_disabled,
                flags.dmabuf_disabled,
                flags.animations_disabled,
                flags.session_env_export_disabled,
            );
        }
        if flags.frame_callback_at_vblank {
            tracing::warn!(
                "BEEWM_FRAME_CALLBACK_AT_VBLANK set: using legacy at-vblank frame \
                 callbacks (no pipelining) — expect lower in-game FPS"
            );
        }
        flags
    }
}

static FLAGS: OnceLock<RuntimeFlags> = OnceLock::new();

pub fn flags() -> RuntimeFlags {
    *FLAGS.get_or_init(RuntimeFlags::from_env)
}
