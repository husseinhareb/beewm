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
//!
//! These exist purely as a debugging aid; once the freeze is root-caused
//! they should all be removed (or promoted to real config options).

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub struct RuntimeFlags {
    pub focus_ipc_disabled: bool,
    pub workspace_publish_disabled: bool,
    pub event_broadcaster_disabled: bool,
    pub explicit_sync_disabled: bool,
    pub dmabuf_disabled: bool,
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
        };
        if flags.focus_ipc_disabled
            || flags.workspace_publish_disabled
            || flags.event_broadcaster_disabled
            || flags.explicit_sync_disabled
            || flags.dmabuf_disabled
        {
            tracing::warn!(
                "BEEWM runtime kill-switches active: focus_ipc_disabled={} workspace_publish_disabled={} event_broadcaster_disabled={} explicit_sync_disabled={} dmabuf_disabled={}",
                flags.focus_ipc_disabled,
                flags.workspace_publish_disabled,
                flags.event_broadcaster_disabled,
                flags.explicit_sync_disabled,
                flags.dmabuf_disabled,
            );
        }
        flags
    }
}

static FLAGS: OnceLock<RuntimeFlags> = OnceLock::new();

pub fn flags() -> RuntimeFlags {
    *FLAGS.get_or_init(RuntimeFlags::from_env)
}
