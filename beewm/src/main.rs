use beewm_core::config::Config;
use beewm_core::manager::Manager;
use beewm_core::model::screen::Screen;
use beewm_core::model::window::Geometry;
use beewm_x11::X11Backend;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Initialize X11 backend
    let backend = X11Backend::new()?;

    // Get screen dimensions
    let (width, height) = backend.conn.screen_geometry();
    tracing::info!("Screen: {}x{}", width, height);

    // Create manager
    let mut manager = Manager::new(backend, config);

    // Set up the screen
    manager
        .state
        .screens
        .push(Screen::new(0, Geometry::new(0, 0, width, height)));

    // Run the event loop
    manager.run().await?;

    tracing::info!("beewm exited");
    Ok(())
}
