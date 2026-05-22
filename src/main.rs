use beewm::{config::Config, run_udev, run_winit};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let has_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    // Always tee logs to /tmp/beewm.log. Default filter is verbose for the
    // beewm crate (so trace-level events show up) and quiet for smithay /
    // wayland-server (so the file doesn't explode). RUST_LOG overrides
    // everything if set — e.g. `RUST_LOG=beewm::frame=info` to see ONLY the
    // FPS instrumentation, or `RUST_LOG=warn,beewm=debug` for less noise.
    use std::fs::OpenOptions;
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/beewm.log")?;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| tracing_subscriber::EnvFilter::try_new(raw).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("warn,beewm=trace"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(log_file))
        .with_ansi(false)
        .init();

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
