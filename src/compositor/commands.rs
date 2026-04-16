use std::collections::BTreeMap;
use std::ffi::OsString;

#[derive(Debug, Clone, Default)]
pub(crate) struct ChildEnvironment {
    vars: BTreeMap<OsString, OsString>,
    sanitize_display: bool,
}

impl ChildEnvironment {
    pub(crate) fn wayland(socket_name: impl Into<OsString>) -> Self {
        let mut env = Self::default();
        env.set("WAYLAND_DISPLAY", socket_name);
        env.set("XDG_SESSION_TYPE", "wayland");
        env.set("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
        env.set("NIXOS_OZONE_WL", "1");
        env
    }

    pub(crate) fn set(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) {
        self.vars.insert(key.into(), value.into());
    }

    pub(crate) fn set_sanitize_display(&mut self, sanitize_display: bool) {
        self.sanitize_display = sanitize_display;
    }

    fn apply(&self, command: &mut std::process::Command) {
        if self.sanitize_display {
            command.env_remove("DISPLAY");
        }

        for (key, value) in &self.vars {
            command.env(key, value);
        }
    }
}

pub fn spawn_shell_command(cmd: &str, child_env: &ChildEnvironment) -> std::io::Result<()> {
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(cmd);

    command.env_remove("WAYLAND_SOCKET");
    child_env.apply(&mut command);

    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    command.spawn().map(|_| ())
}

pub fn spawn_startup_commands(commands: &[String], child_env: &ChildEnvironment) {
    for cmd in commands {
        tracing::info!("Running startup command: {}", cmd);
        if let Err(error) = spawn_shell_command(cmd, child_env) {
            tracing::error!("Failed to run startup command '{}': {}", cmd, error);
        }
    }
}
