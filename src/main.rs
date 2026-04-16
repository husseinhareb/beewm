use beewm::{config::Config, run_udev, run_winit};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // When running from a bare TTY (DRM backend) stdout/stderr aren't visible,
    // so write logs to /tmp/beewm.log for post-hoc debugging.
    let has_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    if has_display {
        // Interactive session: honour RUST_LOG, default to debug for beewm crates.
        let filter =
            tracing_subscriber::EnvFilter::from_default_env().add_directive("beewm=debug".parse()?);
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        // DRM/TTY session: write to /tmp/beewm.log with a hardcoded conservative
        // filter so the file never bloats from smithay internals or RUST_LOG.
        let filter = tracing_subscriber::EnvFilter::new("warn,beewm=debug");
        use std::fs::OpenOptions;
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/beewm.log")?;
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::sync::Mutex::new(log_file))
            .init();
    }

    tracing::info!("Starting beewm");

    // Load configuration
    let config = Config::load()?;
    tracing::info!(
        "Config loaded: {} workspaces, border_width={}, gap={}",
        config.num_workspaces,
        config.border_width,
        config.gap
    );

    if has_display {
        tracing::info!("Detected existing session, using winit backend");
        run_winit(config)?;
    } else {
        tracing::info!("No display session detected, using DRM/udev backend");
        run_udev(config)?;
    }

    tracing::info!("beewm exited");
    Ok(())
}
