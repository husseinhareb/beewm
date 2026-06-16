use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use smithay::reexports::calloop::channel::Sender;

const RESUME_WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);
const RESUME_WATCHDOG_THRESHOLD: Duration = Duration::from_secs(2);

/// Backend-visible suspend/resume lifecycle.
///
/// The state machine is intentionally small and idempotent: logind and libseat
/// may report overlapping parts of the same transition, and either signal must
/// be safe to process more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowerState {
    Awake,
    PreparingSuspend,
    Suspended,
    Resuming,
    Degraded,
}

impl PowerState {
    pub(crate) fn prepare_suspend(self) -> Self {
        match self {
            Self::PreparingSuspend | Self::Suspended => self,
            Self::Awake | Self::Resuming | Self::Degraded => Self::PreparingSuspend,
        }
    }

    pub(crate) fn suspended(self) -> Self {
        match self {
            Self::Awake | Self::PreparingSuspend | Self::Resuming | Self::Degraded => {
                Self::Suspended
            }
            Self::Suspended => Self::Suspended,
        }
    }

    pub(crate) fn resume(self) -> Self {
        match self {
            Self::Awake => Self::Awake,
            Self::PreparingSuspend | Self::Suspended | Self::Degraded => Self::Resuming,
            Self::Resuming => Self::Resuming,
        }
    }
}

#[derive(Debug)]
pub(crate) enum PowerEvent {
    PrepareSuspend { ack: Option<mpsc::Sender<()>> },
    Resume { source: ResumeSource },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeSource {
    Logind,
    Watchdog,
    Libseat,
}

impl ResumeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Logind => "logind",
            Self::Watchdog => "resume-watchdog",
            Self::Libseat => "libseat",
        }
    }
}

/// Start a best-effort logind sleep monitor.
///
/// This complements libseat's pause/activate events. logind tells us *before*
/// suspend, which lets beewm enter its compositor-enforced locked state before
/// the kernel freezes the system. If logind or D-Bus is unavailable, the caller
/// still has libseat fallback handling.
pub(crate) fn start_logind_sleep_monitor(sender: Sender<PowerEvent>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("beewm-logind-sleep".into())
        .spawn(move || logind_sleep_monitor(sender))
        .map(|_| ())
}

/// Start a fallback resume detector based on wall-clock time.
///
/// Some systems resume userspace without delivering a logind
/// `PrepareForSleep(false)` signal to this process and without a libseat
/// activate event. A large wall-clock jump compared to `Instant` means the
/// process was suspended; sending a best-effort resume event wakes calloop and
/// lets the backend revalidate DRM even in that case.
pub(crate) fn start_resume_watchdog(sender: Sender<PowerEvent>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("beewm-resume-watchdog".into())
        .spawn(move || resume_watchdog(sender))
        .map(|_| ())
}

fn resume_watchdog(sender: Sender<PowerEvent>) {
    let mut last_instant = Instant::now();
    let mut last_wall = SystemTime::now();

    loop {
        thread::sleep(RESUME_WATCHDOG_INTERVAL);

        let now_instant = Instant::now();
        let now_wall = SystemTime::now();
        let instant_elapsed = now_instant.saturating_duration_since(last_instant);
        let wall_elapsed = now_wall.duration_since(last_wall).unwrap_or(Duration::ZERO);

        if wall_clock_resume_gap(wall_elapsed, instant_elapsed) {
            tracing::info!(
                target: "beewm::power",
                wall_ms = wall_elapsed.as_millis() as u64,
                monotonic_ms = instant_elapsed.as_millis() as u64,
                "resume watchdog detected suspend gap",
            );
            if sender
                .send(PowerEvent::Resume {
                    source: ResumeSource::Watchdog,
                })
                .is_err()
            {
                return;
            }
        }

        last_instant = now_instant;
        last_wall = now_wall;
    }
}

fn wall_clock_resume_gap(wall_elapsed: Duration, instant_elapsed: Duration) -> bool {
    wall_elapsed > instant_elapsed.saturating_add(RESUME_WATCHDOG_THRESHOLD)
}

fn logind_sleep_monitor(sender: Sender<PowerEvent>) {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(
                target: "beewm::power",
                %error,
                "logind sleep monitor unavailable: cannot connect to system bus",
            );
            return;
        }
    };

    let proxy = match zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    ) {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::debug!(
                target: "beewm::power",
                %error,
                "logind sleep monitor unavailable: cannot create login1 proxy",
            );
            return;
        }
    };

    let mut inhibitor = acquire_sleep_delay_inhibitor(&proxy);
    let mut signals = match proxy.receive_signal("PrepareForSleep") {
        Ok(signals) => signals,
        Err(error) => {
            tracing::debug!(
                target: "beewm::power",
                %error,
                "logind sleep monitor unavailable: cannot subscribe to PrepareForSleep",
            );
            return;
        }
    };

    tracing::info!(target: "beewm::power", "logind sleep monitor active");
    for message in &mut signals {
        let sleeping = match message.body().deserialize::<bool>() {
            Ok(sleeping) => sleeping,
            Err(error) => {
                tracing::warn!(
                    target: "beewm::power",
                    %error,
                    "ignored malformed PrepareForSleep signal",
                );
                continue;
            }
        };

        if sleeping {
            let (ack_tx, ack_rx) = mpsc::channel();
            if sender
                .send(PowerEvent::PrepareSuspend { ack: Some(ack_tx) })
                .is_err()
            {
                return;
            }

            if ack_rx.recv_timeout(Duration::from_millis(750)).is_err() {
                tracing::warn!(
                    target: "beewm::power",
                    "timed out waiting for compositor suspend preparation; releasing inhibitor",
                );
            }

            // Releasing this fd lets logind continue the suspend.
            inhibitor.take();
        } else {
            let _ = sender.send(PowerEvent::Resume {
                source: ResumeSource::Logind,
            });
            inhibitor = acquire_sleep_delay_inhibitor(&proxy);
        }
    }
}

fn acquire_sleep_delay_inhibitor(
    proxy: &zbus::blocking::Proxy<'_>,
) -> Option<zbus::zvariant::OwnedFd> {
    match proxy.call(
        "Inhibit",
        &(
            "sleep",
            "beewm",
            "lock the compositor and quiesce DRM before suspend",
            "delay",
        ),
    ) {
        Ok(fd) => {
            tracing::debug!(target: "beewm::power", "acquired logind sleep delay inhibitor");
            Some(fd)
        }
        Err(error) => {
            tracing::debug!(
                target: "beewm::power",
                %error,
                "could not acquire logind sleep delay inhibitor",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{PowerState, wall_clock_resume_gap};

    #[test]
    fn suspend_transition_is_idempotent() {
        assert_eq!(
            PowerState::Awake.prepare_suspend(),
            PowerState::PreparingSuspend
        );
        assert_eq!(
            PowerState::PreparingSuspend.prepare_suspend(),
            PowerState::PreparingSuspend,
        );
        assert_eq!(
            PowerState::Suspended.prepare_suspend(),
            PowerState::Suspended
        );
    }

    #[test]
    fn suspended_transition_accepts_repeated_pause_events() {
        assert_eq!(
            PowerState::PreparingSuspend.suspended(),
            PowerState::Suspended
        );
        assert_eq!(PowerState::Suspended.suspended(), PowerState::Suspended);
    }

    #[test]
    fn resume_transition_only_runs_from_non_awake_states() {
        assert_eq!(PowerState::Awake.resume(), PowerState::Awake);
        assert_eq!(PowerState::Suspended.resume(), PowerState::Resuming);
        assert_eq!(PowerState::Degraded.resume(), PowerState::Resuming);
    }

    #[test]
    fn wall_clock_resume_gap_detects_suspend_jump() {
        assert!(wall_clock_resume_gap(
            Duration::from_secs(10),
            Duration::from_secs(1)
        ));
        assert!(!wall_clock_resume_gap(
            Duration::from_millis(1100),
            Duration::from_secs(1)
        ));
    }
}
