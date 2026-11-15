//! Which clients are capturing the screen right now, published on the event
//! socket as `screencast>>name1,name2` (empty after `>>` when nobody is) so a
//! bar can show a recording indicator.
//!
//! A client counts as capturing from its first screencopy frame request until
//! [`CAPTURE_LINGER`] after its last one. Streams request a frame every few
//! milliseconds, so they stay listed for as long as they run; a one-shot
//! screenshot (grim) shows briefly, which is still worth knowing about.
//!
//! When an app shares the screen through the portal, the client beewm sees is
//! `xdg-desktop-portal-wlr`, not the app. Naming the real consumer is left to
//! the bar, which can look it up in PipeWire.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::backend::ClientId;

use crate::compositor::state::Beewm;

/// How long a client stays listed after its last frame request.
const CAPTURE_LINGER: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct CaptureClient {
    name: String,
    last_frame: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct ScreencastStatus {
    clients: HashMap<ClientId, CaptureClient>,
    /// The last payload pushed, so only changes go out.
    published: String,
}

impl ScreencastStatus {
    /// The current `screencast>>...` line.
    pub(crate) fn event_payload(&self) -> String {
        format_payload(self.clients.values().map(|client| client.name.as_str()))
    }
}

impl Beewm {
    /// Note that `client` just asked for a screencopy frame.
    pub(crate) fn note_capture_frame(&mut self, client: &Client) {
        let now = Instant::now();
        let id = client.id();
        if let Some(entry) = self.screencast_status.clients.get_mut(&id) {
            entry.last_frame = now;
            return;
        }
        let name = client
            .get_credentials(&self.display_handle)
            .ok()
            .and_then(|credentials| process_name(credentials.pid))
            .unwrap_or_else(|| "unknown".to_string());
        tracing::info!(target: "beewm::screencast", client = name, "screen capture started");
        self.screencast_status.clients.insert(
            id,
            CaptureClient {
                name,
                last_frame: now,
            },
        );
        self.publish_screencast_status();
    }

    /// Drop clients that stopped capturing and publish any change. Called once
    /// per main-loop turn, which wakes at least every idle tick.
    pub fn tick_screencast(&mut self, now: Instant) {
        if self.screencast_status.clients.is_empty() && self.screencast_status.published.is_empty()
        {
            return;
        }
        self.screencast_status.clients.retain(|_, client| {
            let active = now.duration_since(client.last_frame) < CAPTURE_LINGER;
            if !active {
                tracing::info!(
                    target: "beewm::screencast",
                    client = client.name,
                    "screen capture stopped",
                );
            }
            active
        });
        self.publish_screencast_status();
    }

    fn publish_screencast_status(&mut self) {
        let payload = self.screencast_status.event_payload();
        if payload == self.screencast_status.published {
            return;
        }
        self.event_broadcaster.push_event(&payload);
        self.screencast_status.published = payload;
    }
}

/// The executable name of `pid`, e.g. `xdg-desktop-portal-wlr` or `grim`.
/// `/proc/<pid>/exe` gives the full name; `comm` is the fallback (it is cut
/// to 15 characters, but readable even when `exe` is not).
fn process_name(pid: i32) -> Option<String> {
    let proc_dir = Path::new("/proc").join(pid.to_string());
    std::fs::read_link(proc_dir.join("exe"))
        .ok()
        .and_then(|exe| {
            exe.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        // A binary replaced by an upgrade while running reads as "name (deleted)".
        .map(|name| name.trim_end_matches(" (deleted)").to_string())
        .or_else(|| {
            std::fs::read_to_string(proc_dir.join("comm"))
                .ok()
                .map(|comm| comm.trim().to_string())
        })
        .filter(|name| !name.is_empty())
}

/// `screencast>>a,b`: names sorted and deduplicated so the line only changes
/// when the set of capturing programs does.
fn format_payload<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut names: Vec<&str> = names.collect();
    names.sort_unstable();
    names.dedup();
    format!("screencast>>{}", names.join(","))
}

#[cfg(test)]
mod tests {
    use super::{format_payload, process_name};

    #[test]
    fn payload_is_sorted_deduplicated_and_empty_when_idle() {
        assert_eq!(format_payload(std::iter::empty()), "screencast>>");
        assert_eq!(
            format_payload(["obs", "grim", "obs"].into_iter()),
            "screencast>>grim,obs"
        );
    }

    #[test]
    fn names_our_own_process() {
        let name = process_name(std::process::id() as i32).expect("own process is readable");
        assert!(!name.is_empty() && !name.contains('/'), "{name}");
    }
}
