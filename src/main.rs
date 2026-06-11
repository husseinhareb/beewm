use std::io::Write;
use std::sync::{Arc, Mutex};

use beewm::{config::Config, run_udev, run_winit};

/// A log writer that `fsync`s every line to disk. Used so that when the DRM
/// backend hard-wedges the GPU (frozen session, power-button reboot) the very
/// last line written before the wedge survives the unclean reboot — the only
/// way to pin down which DRM operation hung without a second machine. Slower
/// than buffered logging, but only matters while debugging the wedge.
#[derive(Clone)]
struct SyncWriter(Arc<Mutex<std::fs::File>>);

impl Write for SyncWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.0.lock().unwrap();
        let n = file.write(buf)?;
        let _ = file.flush();
        let _ = file.sync_data();
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self.0.lock().unwrap();
        file.flush()?;
        let _ = file.sync_data();
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SyncWriter {
    type Writer = SyncWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let has_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    // Log to a PERSISTENT, fsync'd file under $HOME so the trail survives a hard
    // reboot after a GPU wedge (/tmp is usually tmpfs and is lost on reboot).
    // Default filter is verbose for the beewm crate, quiet for smithay; RUST_LOG
    // overrides — e.g. `RUST_LOG=warn,beewm=debug`.
    use std::fs::OpenOptions;
    let log_path = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|mut p| {
            p.push("beewm-debug.log");
            p
        })
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/beewm.log"));
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| tracing_subscriber::EnvFilter::try_new(raw).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("warn,beewm=trace"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(SyncWriter(Arc::new(Mutex::new(log_file))))
        .with_ansi(false)
        .init();
    tracing::warn!(target: "beewm::wedge", "log file: {}", log_path.display());

    tracing::info!("Starting beewm");

    // Load configuration
    let config = Config::load()?;
    tracing::info!(
        "Config loaded: {} workspaces, border_width={}, gap={}, tray_enabled={}",
        config.num_workspaces,
        config.border_width,
        config.gap,
        config.tray_enabled,
    );

    if has_display {
        tracing::info!(
            "Detected existing session, using winit backend; the settings tray publishes to the host StatusNotifier tray"
        );
        run_winit(config)?;
    } else {
        tracing::info!("No display session detected, using DRM/udev backend");
        run_udev(config)?;
    }

    tracing::info!("beewm exited");
    Ok(())
}
