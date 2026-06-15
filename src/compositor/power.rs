use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use smithay::reexports::calloop::channel::Sender;

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
    Resume,
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
            let _ = sender.send(PowerEvent::Resume);
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
    use super::PowerState;

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
}
