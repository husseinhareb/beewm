pub fn spawn_shell_command(cmd: &str, sanitize_display: bool) -> std::io::Result<()> {
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(cmd);

    command.env_remove("WAYLAND_SOCKET");
    if sanitize_display {
        command.env_remove("DISPLAY");
    }

    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    command.spawn().map(|_| ())
}

pub fn spawn_startup_commands(commands: &[String], sanitize_display: bool) {
    for cmd in commands {
        tracing::info!("Running startup command: {}", cmd);
        if let Err(error) = spawn_shell_command(cmd, sanitize_display) {
            tracing::error!("Failed to run startup command '{}': {}", cmd, error);
        }
    }
}
