use beewm_core::config::Config;
use beewm_wayland::WaylandBackend;

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

    // Initialize and run Wayland compositor
    let backend = WaylandBackend::new()?;
    backend.run(config)?;

    tracing::info!("beewm exited");
    Ok(())
}
