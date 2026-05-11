use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

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

    fn apply(&self, command: &mut Command) {
        if self.sanitize_display {
            command.env_remove("DISPLAY");
        }

        for (key, value) in &self.vars {
            command.env(key, value);
        }
    }
}

/// Heuristic: can this command be executed directly with argv splitting on
/// whitespace, without invoking a shell? If so we skip the extra `sh` fork and
/// shave a noticeable amount of latency off `bindsym ... exec kitty` style
/// keybinds (5–30 ms in practice on a busy compositor).
fn shell_metacharacter(byte: u8) -> bool {
    matches!(
        byte,
        b'$' | b'`'
            | b'\\'
            | b'\''
            | b'"'
            | b'|'
            | b'&'
            | b';'
            | b'<'
            | b'>'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b'*'
            | b'?'
            | b'~'
            | b'#'
            | b'!'
            | b'\n'
            | b'\r'
            | b'='
    )
}

fn try_split_simple_command(cmd: &str) -> Option<Vec<&str>> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().any(shell_metacharacter) {
        return None;
    }
    let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
    if parts.is_empty() { None } else { Some(parts) }
}

fn configure_child(command: &mut Command, child_env: &ChildEnvironment) {
    command.env_remove("WAYLAND_SOCKET");
    child_env.apply(command);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Detach the child from the compositor's process group / session so
    // closing/killing the compositor doesn't take spawned terminals down with
    // it, and so SIGINT on the foreground TTY doesn't propagate. This also
    // prevents the parent from accidentally reaping signals destined for
    // children and is the same approach Hyprland/sway take. `setsid` is a
    // single syscall and runs in the child between fork and exec.
    unsafe {
        command.pre_exec(|| {
            // SAFETY: only async-signal-safe code runs here.
            libc::setsid();
            Ok(())
        });
    }
}

pub fn spawn_shell_command(cmd: &str, child_env: &ChildEnvironment) -> std::io::Result<()> {
    let mut command = if let Some(parts) = try_split_simple_command(cmd) {
        // Direct exec: one fork+exec, no intermediate sh process.
        let mut c = Command::new(parts[0]);
        if parts.len() > 1 {
            c.args(&parts[1..]);
        }
        c
    } else {
        // Fall back to the shell for commands that need globbing, env
        // expansion, pipes, redirection, etc.
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };

    configure_child(&mut command, child_env);
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

#[cfg(test)]
mod tests {
    use super::try_split_simple_command;

    #[test]
    fn simple_command_takes_direct_path() {
        assert_eq!(try_split_simple_command("kitty"), Some(vec!["kitty"]));
        assert_eq!(
            try_split_simple_command("wofi --show drun"),
            Some(vec!["wofi", "--show", "drun"])
        );
    }

    #[test]
    fn empty_or_whitespace_is_rejected() {
        assert_eq!(try_split_simple_command(""), None);
        assert_eq!(try_split_simple_command("   "), None);
    }

    #[test]
    fn shell_metacharacters_force_sh_fallback() {
        assert!(try_split_simple_command("foo | bar").is_none());
        assert!(try_split_simple_command("foo && bar").is_none());
        assert!(try_split_simple_command("foo $bar").is_none());
        assert!(try_split_simple_command("VAR=1 cmd").is_none());
        assert!(try_split_simple_command("echo > /tmp/x").is_none());
    }
}
