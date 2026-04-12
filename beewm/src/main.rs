use beewm_core::config::Config;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("beewm=info".parse()?),
        )
        .init();

    tracing::info!("Starting beewm");

    // Load configuration
    let config = Config::load()?;
    tracing::info!("Config loaded: {} workspaces, border_width={}, gap={}", 
        config.num_workspaces, config.border_width, config.gap);

    // Auto-detect backend: if WAYLAND_DISPLAY or DISPLAY is set, use winit (nested).
    // Otherwise, run on DRM/TTY directly.
    let has_display = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("DISPLAY").is_some();

    if has_display {
        tracing::info!("Detected existing session, using winit backend");
        beewm_wayland::run_winit(config)?;
    } else {
        tracing::info!("No display session detected, using DRM/udev backend");
        beewm_wayland::run_udev(config)?;
    }

    tracing::info!("beewm exited");
    Ok(())
}
