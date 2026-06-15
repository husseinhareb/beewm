use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
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

fn absolute_nonempty_path(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    path.is_absolute().then_some(path)
}

fn log_dir_from_env(xdg_state_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    if let Some(mut path) = absolute_nonempty_path(xdg_state_home) {
        path.push("beewm");
        path.push("log");
        return path;
    }

    if let Some(mut path) = absolute_nonempty_path(home) {
        path.push(".local");
        path.push("state");
        path.push("beewm");
        path.push("log");
        return path;
    }

    PathBuf::from("/var/tmp/beewm/log")
}

fn default_log_dir() -> PathBuf {
    log_dir_from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn default_log_path() -> PathBuf {
    let mut path = default_log_dir();
    path.push("beewm-debug.log");
    path
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let has_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();

    // Log to a persistent, fsync'd XDG state log so the trail survives a hard
    // reboot after a GPU wedge (/tmp is usually tmpfs and is lost on reboot).
    // Default filter is verbose for the beewm crate, quiet for smithay; RUST_LOG
    // overrides — e.g. `RUST_LOG=warn,beewm=debug`.
    use std::fs::{self, OpenOptions};
    let log_path = default_log_path();
    if let Some(log_dir) = log_path.parent() {
        fs::create_dir_all(log_dir)?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_prefers_xdg_state_home() {
        let dir = log_dir_from_env(Some("/run/user-state".into()), Some("/home/alice".into()));

        assert_eq!(dir, PathBuf::from("/run/user-state/beewm/log"));
    }

    #[test]
    fn log_dir_falls_back_to_xdg_state_default_under_home() {
        let dir = log_dir_from_env(None, Some("/home/alice".into()));

        assert_eq!(dir, PathBuf::from("/home/alice/.local/state/beewm/log"));
    }

    #[test]
    fn relative_xdg_state_home_is_ignored() {
        let dir = log_dir_from_env(Some("relative-state".into()), Some("/home/alice".into()));

        assert_eq!(dir, PathBuf::from("/home/alice/.local/state/beewm/log"));
    }
}
