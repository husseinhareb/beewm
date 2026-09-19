use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use beewm::{config::Config, run_udev, run_winit};

/// A log writer that line-buffers to the page cache, and additionally `fsync`s
/// every line when `BEEWM_WEDGE_TRACE=1`.
///
/// The fsync exists so the last line before a hard GPU wedge survives an
/// unclean reboot. It must NOT be on by default: `sync_data` on a busy or
/// near-full filesystem blocks for tens to thousands of milliseconds, and it
/// runs on the compositor's main event-loop thread. At the ~100 lines/sec this
/// logs while a screencopy client is capturing, a single slow fsync freezes the
/// loop long enough that a key release is not read from libinput — the focused
/// client keeps its own repeat timer running and emits dozens of extra
/// characters ("hellooooooooo").
#[derive(Clone)]
struct SyncWriter {
    file: Arc<Mutex<std::io::LineWriter<std::fs::File>>>,
    fsync: bool,
}

impl Write for SyncWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self.file.lock().unwrap();
        let n = file.write(buf)?;
        if self.fsync {
            file.flush()?;
            let _ = file.get_ref().sync_data();
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self.file.lock().unwrap();
        file.flush()?;
        if self.fsync {
            let _ = file.get_ref().sync_data();
        }
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

fn default_log_filter(wedge_trace: bool) -> &'static str {
    if wedge_trace {
        "warn,beewm=trace"
    } else {
        "warn,beewm=info,\
         beewm::commit=off,\
         beewm::frame=off,\
         beewm::dmabuf=off,\
         beewm::presentation=off,\
         beewm::sync=off"
    }
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
    // BEEWM_WEDGE_TRACE turns on both the per-line fsync and the trace-level
    // firehose that goes with it; neither is affordable in normal use.
    // RUST_LOG still overrides the level either way.
    //
    // The `beewm::{commit,frame,dmabuf,presentation,sync}` targets are periodic
    // telemetry: each emits a line per surface per second (some at `warn!`)
    // for as long as the compositor runs, which is what grew this log to 1.4 GB
    // over a month. They are off unless explicitly asked for — turn one back on
    // with e.g. `RUST_LOG=warn,beewm=info,beewm::frame=info`.
    let wedge_trace = std::env::var_os("BEEWM_WEDGE_TRACE").is_some_and(|v| !v.is_empty());
    let default_level = default_log_filter(wedge_trace);
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| tracing_subscriber::EnvFilter::try_new(raw).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(SyncWriter {
            file: Arc::new(Mutex::new(std::io::LineWriter::new(log_file))),
            fsync: wedge_trace,
        })
        .with_ansi(false)
        .init();
    tracing::warn!(target: "beewm::wedge", "log file: {}", log_path.display());

    tracing::info!("Starting beewm");

    // Automatically reap terminated child processes (spawned terminals,
    // launchers, autostart programs) so they do not linger as zombies (<defunct>).
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        sa.sa_flags = libc::SA_NOCLDWAIT | libc::SA_RESTART;
        libc::sigaction(libc::SIGCHLD, &sa, std::ptr::null_mut());
    }

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

    /// The compositor's event loop calls this writer inline. A per-line
    /// `sync_data` there is what froze the loop (and produced runaway client
    /// key-repeat), so the default must be buffered-only.
    #[test]
    fn writer_only_fsyncs_under_wedge_trace() {
        use std::fs::{self, OpenOptions};
        let path = std::env::temp_dir().join("beewm-syncwriter-test.log");
        let _ = fs::remove_file(&path);
        let open = || {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap()
        };

        let mut buffered = SyncWriter {
            file: Arc::new(Mutex::new(std::io::LineWriter::new(open()))),
            fsync: false,
        };
        assert!(!buffered.fsync);
        buffered.write_all(b"buffered line\n").unwrap();

        let mut synced = SyncWriter {
            file: Arc::new(Mutex::new(std::io::LineWriter::new(open()))),
            fsync: true,
        };
        synced.write_all(b"synced line\n").unwrap();
        synced.flush().unwrap();

        // Both paths must still actually reach the file.
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("buffered line"), "{written:?}");
        assert!(written.contains("synced line"), "{written:?}");
        let _ = fs::remove_file(&path);
    }

    /// The periodic telemetry targets emit a line per surface per second
    /// forever (some at `warn!`), which is what grew the log to 1.4 GB. They
    /// must be off by default, without silencing the rest of the crate.
    #[test]
    fn default_filter_silences_periodic_telemetry() {
        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(default_log_filter(
                false,
            )))
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            // Periodic telemetry — must be dropped, including at warn level.
            tracing::warn!(target: "beewm::dmabuf", "TELEMETRY_dmabuf");
            tracing::warn!(target: "beewm::frame", "TELEMETRY_frame");
            tracing::info!(target: "beewm::commit", "TELEMETRY_commit");
            tracing::info!(target: "beewm::presentation", "TELEMETRY_presentation");
            tracing::info!(target: "beewm::sync", "TELEMETRY_sync");
            // The screencopy firehose is debug/trace — also dropped.
            tracing::debug!(target: "beewm::compositor::screencopy", "TELEMETRY_screencopy");
            // Real events must survive.
            tracing::info!(target: "beewm::compositor::input", "KEEP_input");
            tracing::warn!(target: "beewm::wedge", "KEEP_wedge");
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            !out.contains("TELEMETRY_"),
            "telemetry leaked through: {out}"
        );
        assert!(out.contains("KEEP_input"), "{out}");
        assert!(out.contains("KEEP_wedge"), "{out}");
        assert!(default_log_filter(true).contains("beewm=trace"));
    }

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
