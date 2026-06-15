use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

// NOTE: Do NOT add `pre_exec` hooks to spawned commands. Rust's
// `Command::spawn()` only uses the fork-safe `posix_spawn()` path when there
// are no pre_exec callbacks; setting one forces the unsafe fork+exec path.
// beewm runs several background threads (event broadcaster, IPC accept,
// XWayland, libseat), and `fork()` in a multi-threaded program copies all
// libc mutexes — including the malloc mutex — in their currently-held state,
// which can deadlock the child before `exec` runs. When that happens the
// parent blocks forever on the internal CLOEXEC error pipe and the entire
// compositor (input, rendering, VT-switch hotkeys) freezes. Use
// `process_group` / posix_spawn-compatible methods only.

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
        // Desktop identity. xdg-desktop-portal selects a backend config file
        // named `<desktop>-portals.conf` by matching the (colon-separated)
        // XDG_CURRENT_DESKTOP list, so this is what lets beewm route the
        // ScreenCast portal to xdg-desktop-portal-wlr (see portal/ in the repo).
        env.set("XDG_CURRENT_DESKTOP", "beewm");
        env.set("XDG_SESSION_DESKTOP", "beewm");
        env.set("ELECTRON_OZONE_PLATFORM_HINT", "wayland");
        env.set("NIXOS_OZONE_WL", "1");
        env
    }

    pub(crate) fn set(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) {
        self.vars.insert(key.into(), value.into());
    }

    /// The subset of session variables that D-Bus/systemd-activated services
    /// (xdg-desktop-portal, xdg-desktop-portal-wlr, PipeWire) need in order to
    /// see the live Wayland session. These are pushed into the D-Bus activation
    /// and systemd user-manager environments at startup so screen sharing works
    /// (those services are *not* children of beewm and otherwise never inherit
    /// `WAYLAND_DISPLAY`/`XDG_CURRENT_DESKTOP`).
    pub(crate) fn session_activation_pairs(&self) -> Vec<(OsString, OsString)> {
        const KEYS: &[&str] = &[
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_DESKTOP",
            "XDG_SESSION_TYPE",
            "XDG_RUNTIME_DIR",
        ];
        KEYS.iter()
            .filter_map(|key| {
                let key = OsString::from(key);
                self.vars.get(&key).map(|value| (key, value.clone()))
            })
            .collect()
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

    // Detach the child from the compositor's process group so closing/killing
    // the compositor doesn't take spawned terminals down with it, and so
    // SIGINT on the foreground TTY doesn't propagate to children. We use
    // `process_group(0)` rather than a `pre_exec(setsid)` hook because the
    // latter disables Rust's posix_spawn fast path and forces the unsafe
    // fork+exec path; in a multi-threaded compositor that risks the child
    // deadlocking on inherited libc mutexes (e.g. malloc) before `exec`,
    // which would block this `spawn()` call forever and freeze the WM.
    // `process_group` is implemented via `posix_spawnattr_setpgroup`, so it
    // stays on the safe path.
    command.process_group(0);
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

/// Push the live session environment into the D-Bus activation environment and
/// the systemd user manager, so D-Bus/systemd-activated services started later
/// (xdg-desktop-portal, xdg-desktop-portal-wlr, pipewire, wireplumber) inherit
/// `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`, etc.
///
/// This is the fix for "screen sharing finds no monitor": the portal that
/// browsers/OBS/Electron talk to is bus-activated and would otherwise have no
/// idea which Wayland display to capture. We run this once at startup, before
/// autostart clients, mirroring what sway/Hyprland/niri sessions do.
///
/// Fire-and-forget and best-effort: both helpers are tiny one-shots, and a
/// missing `dbus-update-activation-environment`/`systemctl` is logged but never
/// fatal. Values are passed explicitly as `KEY=VALUE` so the result does not
/// depend on what the helper itself happens to inherit.
pub(crate) fn export_session_environment(child_env: &ChildEnvironment) {
    if crate::compositor::runtime_flags::flags().session_env_export_disabled {
        tracing::warn!(
            target = "beewm::portal",
            "BEEWM_NO_SESSION_ENV_EXPORT set: not exporting session env to D-Bus/systemd; \
             screen sharing portals may not find the display",
        );
        return;
    }

    let pairs = child_env.session_activation_pairs();
    if pairs.is_empty() {
        return;
    }

    let keys: Vec<String> = pairs
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    tracing::info!(
        target = "beewm::portal",
        ?keys,
        "exporting session environment to D-Bus activation + systemd user manager",
    );

    // `dbus-update-activation-environment --systemd KEY=VALUE …` updates both
    // the D-Bus activation environment and (with --systemd) the systemd user
    // manager in one call — the canonical incantation for Wayland sessions.
    let kv_args: Vec<OsString> = pairs
        .iter()
        .map(|(k, v)| {
            let mut arg = k.clone();
            arg.push("=");
            arg.push(v);
            arg
        })
        .collect();

    let mut dbus = Command::new("dbus-update-activation-environment");
    dbus.arg("--systemd").args(&kv_args);
    configure_child(&mut dbus, child_env);
    match dbus.spawn() {
        Ok(_) => tracing::debug!(
            target = "beewm::portal",
            "spawned dbus-update-activation-environment",
        ),
        Err(error) => tracing::warn!(
            target = "beewm::portal",
            %error,
            "failed to run dbus-update-activation-environment (install dbus); \
             portals may not see the Wayland session",
        ),
    }

    // Also import into the systemd user manager directly, in case
    // dbus-update-activation-environment is unavailable but systemd is present.
    let key_args: Vec<OsString> = pairs.iter().map(|(k, _)| k.clone()).collect();
    let mut systemctl = Command::new("systemctl");
    systemctl
        .arg("--user")
        .arg("import-environment")
        .args(&key_args);
    configure_child(&mut systemctl, child_env);
    if let Err(error) = systemctl.spawn() {
        tracing::debug!(
            target = "beewm::portal",
            %error,
            "systemctl --user import-environment unavailable (non-systemd session?)",
        );
    }

    // Activate the graphical session target now that the environment is in
    // place. `xdg-desktop-portal.service` (and other session units) carry a
    // hard `Requisite=graphical-session.target`, which `RefuseManualStart=yes`
    // forbids starting directly — it can only be pulled in as a dependency.
    // `beewm-session.target` (installed by `portal/install.sh`) `BindsTo` it,
    // so starting our target is what legally activates graphical-session and
    // unblocks the ScreenCast portal. Best-effort: a missing unit (target not
    // installed) or non-systemd session is logged, never fatal.
    let mut session = Command::new("systemctl");
    session
        .arg("--user")
        .arg("start")
        .arg("beewm-session.target");
    configure_child(&mut session, child_env);
    if let Err(error) = session.spawn() {
        tracing::debug!(
            target = "beewm::portal",
            %error,
            "systemctl --user start beewm-session.target unavailable (non-systemd session?)",
        );
    }
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
    use super::{ChildEnvironment, try_split_simple_command};

    #[test]
    fn wayland_env_sets_desktop_identity() {
        let env = ChildEnvironment::wayland("wayland-1");
        let pairs = env.session_activation_pairs();
        let lookup = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string_lossy().into_owned())
        };
        // The session env that must reach the bus-activated portal.
        assert_eq!(lookup("WAYLAND_DISPLAY").as_deref(), Some("wayland-1"));
        assert_eq!(lookup("XDG_CURRENT_DESKTOP").as_deref(), Some("beewm"));
        assert_eq!(lookup("XDG_SESSION_DESKTOP").as_deref(), Some("beewm"));
        assert_eq!(lookup("XDG_SESSION_TYPE").as_deref(), Some("wayland"));
    }

    #[test]
    fn session_pairs_only_include_set_keys() {
        // DISPLAY is only present once XWayland is up; absent by default.
        let env = ChildEnvironment::wayland("wayland-1");
        assert!(
            !env.session_activation_pairs()
                .iter()
                .any(|(k, _)| k == "DISPLAY")
        );

        let mut env = env;
        env.set("DISPLAY", ":1");
        assert!(
            env.session_activation_pairs()
                .iter()
                .any(|(k, v)| k == "DISPLAY" && v == ":1")
        );
        // Non-session vars are never exported to the activation environment.
        assert!(
            !env.session_activation_pairs()
                .iter()
                .any(|(k, _)| k == "NIXOS_OZONE_WL")
        );
    }

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
