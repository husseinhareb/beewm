//! Lightweight runtime instrumentation for diagnosing frame-pacing problems
//! (most importantly: games stuck well below the display refresh rate).
//!
//! Everything here is rolled up into short time windows and emitted under
//! dedicated `beewm::*` tracing targets, so `RUST_LOG` can enable exactly the
//! signal you need without producing one log line per frame at high refresh
//! rates. None of these helpers allocate or touch the GPU on the hot path.
//!
//! Targets and what they answer:
//!
//! - `beewm::presentation` — are vblanks/page-flips actually arriving at the
//!   monitor refresh rate? (compositor present cadence)
//! - `beewm::commit` — how fast is each client committing, how long after we
//!   send a frame callback does it respond, and is it using dmabuf or shm?
//! - `beewm::dmabuf` — buffer type breakdown per surface (shm fallback is the
//!   classic "everything is slow" cause).
//! - `beewm::sync` — explicit-sync acquire-fence activity and how long the
//!   compositor waits on client fences.
//! - `beewm::frame` — direct-scanout vs GPU-composition path (see udev.rs).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How the buffer a client just committed is backed. SHM means a software
/// (CPU) buffer that has to be uploaded to the GPU every frame — if a game
/// shows up as `Shm` it has fallen back off the fast path and that alone
/// explains terrible performance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BufferKind {
    Shm,
    Dmabuf,
    /// Single-pixel / EGL / unknown — lumped together; not interesting for the
    /// FPS investigation but kept so the counts add up.
    Other,
    /// Commit carried no new buffer (e.g. a subsurface-only or state-only
    /// commit). Not counted against shm/dmabuf totals.
    None,
}

/// Per-root-surface commit statistics aggregated over a ~1s window.
#[derive(Debug)]
struct CommitWindow {
    commits: u32,
    window_start: Instant,
    /// Sum/max of "time between the compositor sending frame callbacks and this
    /// surface committing its next buffer". Small ⇒ the client is responsive
    /// and any FPS cap is on the compositor side; large ⇒ the client (its GPU,
    /// its buffer release, or an explicit-sync fence) is the bottleneck.
    cb_latency_total: Duration,
    cb_latency_max: Duration,
    cb_latency_samples: u32,
    shm: u32,
    dmabuf: u32,
}

impl CommitWindow {
    fn new(now: Instant) -> Self {
        Self {
            commits: 0,
            window_start: now,
            cb_latency_total: Duration::ZERO,
            cb_latency_max: Duration::ZERO,
            cb_latency_samples: 0,
            shm: 0,
            dmabuf: 0,
        }
    }
}

/// Tracks commit rate + responsiveness + buffer type per root surface and emits
/// one `beewm::commit` line per surface per second.
#[derive(Debug, Default)]
pub(crate) struct CommitTracker {
    windows: HashMap<u32, CommitWindow>,
}

impl CommitTracker {
    /// Record a commit for the root surface identified by `surface_id`.
    ///
    /// `cb_latency` is `Some(elapsed)` when we know how long ago we last sent
    /// frame callbacks (the responsiveness signal); `buffer` classifies the
    /// buffer that was attached.
    pub fn record(&mut self, surface_id: u32, cb_latency: Option<Duration>, buffer: BufferKind) {
        let now = Instant::now();
        let win = self
            .windows
            .entry(surface_id)
            .or_insert_with(|| CommitWindow::new(now));
        win.commits = win.commits.saturating_add(1);
        if let Some(latency) = cb_latency {
            win.cb_latency_total += latency;
            win.cb_latency_samples += 1;
            if latency > win.cb_latency_max {
                win.cb_latency_max = latency;
            }
        }
        match buffer {
            BufferKind::Shm => win.shm += 1,
            BufferKind::Dmabuf => win.dmabuf += 1,
            BufferKind::Other | BufferKind::None => {}
        }

        let elapsed = now.duration_since(win.window_start);
        if elapsed >= Duration::from_secs(1) {
            let avg_cb_us = if win.cb_latency_samples > 0 {
                (win.cb_latency_total.as_micros() / win.cb_latency_samples as u128) as u64
            } else {
                0
            };
            tracing::info!(
                target: "beewm::commit",
                surface = surface_id,
                commits = win.commits,
                commits_per_s = win.commits as f64 / elapsed.as_secs_f64(),
                shm = win.shm,
                dmabuf = win.dmabuf,
                cb_to_commit_avg_us = avg_cb_us,
                cb_to_commit_max_us = win.cb_latency_max.as_micros() as u64,
                "surface commit rate"
            );
            if win.shm > 0 {
                tracing::warn!(
                    target: "beewm::dmabuf",
                    surface = surface_id,
                    shm = win.shm,
                    dmabuf = win.dmabuf,
                    "surface committed shm buffers — not on the dmabuf fast path",
                );
            }
            *win = CommitWindow::new(now);
        }
    }
}

/// Vblank cadence tracker. Proves whether the compositor is actually putting
/// frames on screen at the monitor refresh rate, independent of what any client
/// is doing.
#[derive(Debug)]
pub(crate) struct PresentStats {
    window_start: Instant,
    last_vblank: Option<Instant>,
    vblanks: u32,
    interval_total: Duration,
    interval_min: Duration,
    interval_max: Duration,
    feedback_presented: u32,
}

impl PresentStats {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            last_vblank: None,
            vblanks: 0,
            interval_total: Duration::ZERO,
            interval_min: Duration::MAX,
            interval_max: Duration::ZERO,
            feedback_presented: 0,
        }
    }

    /// Call once per vblank. `expected_interval` is the output's nominal refresh
    /// interval so the log can show the achieved vs expected present rate.
    pub fn record_vblank(&mut self, expected_interval: Duration, feedback_presented: bool) {
        let now = Instant::now();
        self.vblanks = self.vblanks.saturating_add(1);
        if feedback_presented {
            self.feedback_presented += 1;
        }
        if let Some(prev) = self.last_vblank {
            let interval = now.duration_since(prev);
            self.interval_total += interval;
            self.interval_min = self.interval_min.min(interval);
            self.interval_max = self.interval_max.max(interval);
        }
        self.last_vblank = Some(now);

        let elapsed = now.duration_since(self.window_start);
        if elapsed >= Duration::from_secs(2) {
            // Intervals are measured between consecutive vblanks, so there is
            // one fewer sample than vblanks in the window.
            let samples = self.vblanks.saturating_sub(1).max(1);
            let avg = self.interval_total / samples;
            let present_fps = self.vblanks as f64 / elapsed.as_secs_f64();
            tracing::info!(
                target: "beewm::presentation",
                present_fps,
                vblanks = self.vblanks,
                expected_hz = 1.0 / expected_interval.as_secs_f64(),
                avg_interval_us = avg.as_micros() as u64,
                min_interval_us = if self.interval_min == Duration::MAX {
                    0
                } else {
                    self.interval_min.as_micros() as u64
                },
                max_interval_us = self.interval_max.as_micros() as u64,
                expected_interval_us = expected_interval.as_micros() as u64,
                feedback_presented = self.feedback_presented,
                "present cadence over {:.2}s",
                elapsed.as_secs_f64(),
            );
            *self = PresentStats::new();
        }
    }
}

impl Default for PresentStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Explicit-sync (linux-drm-syncobj) acquire-fence activity. A commit that
/// arrives with an *unsignalled* acquire fence is held (blocked) until the
/// client's GPU finishes rendering into the buffer; this tracks how often that
/// happens and how long we wait.
#[derive(Debug)]
pub(crate) struct SyncStats {
    window_start: Instant,
    installed: u32,
    cleared: u32,
    wait_total: Duration,
    wait_max: Duration,
    /// Running count of blockers installed but not yet cleared. If this grows
    /// without bound, fences are never signalling and clients are wedged.
    pending: i64,
    last_install_at: Option<Instant>,
}

impl SyncStats {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            installed: 0,
            cleared: 0,
            wait_total: Duration::ZERO,
            wait_max: Duration::ZERO,
            pending: 0,
            last_install_at: None,
        }
    }

    /// A commit arrived with an unsignalled acquire fence; we just added a
    /// blocker for it.
    pub fn record_install(&mut self) {
        self.installed = self.installed.saturating_add(1);
        self.pending += 1;
        self.last_install_at = Some(Instant::now());
    }

    /// An acquire fence signalled and its blocker was cleared. Returns nothing;
    /// the approximate wait is derived from the most recent install.
    pub fn record_clear(&mut self) {
        self.cleared = self.cleared.saturating_add(1);
        self.pending -= 1;
        if let Some(install) = self.last_install_at {
            let wait = install.elapsed();
            self.wait_total += wait;
            self.wait_max = self.wait_max.max(wait);
        }
        self.maybe_log();
    }

    fn maybe_log(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(2) {
            return;
        }
        let avg_wait_us = if self.cleared > 0 {
            (self.wait_total.as_micros() / self.cleared as u128) as u64
        } else {
            0
        };
        tracing::info!(
            target: "beewm::sync",
            installed = self.installed,
            cleared = self.cleared,
            pending = self.pending,
            avg_wait_us,
            max_wait_us = self.wait_max.as_micros() as u64,
            "explicit-sync fence activity over {:.2}s",
            elapsed.as_secs_f64(),
        );
        *self = SyncStats::new();
    }
}

impl Default for SyncStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_tracker_aggregates_per_surface_within_window() {
        let mut tracker = CommitTracker::default();
        // Within the 1s window nothing is logged yet, so the running totals are
        // still in the per-surface windows and we can assert on them.
        tracker.record(1, Some(Duration::from_millis(3)), BufferKind::Dmabuf);
        tracker.record(1, Some(Duration::from_millis(5)), BufferKind::Dmabuf);
        tracker.record(1, None, BufferKind::None); // state-only commit
        tracker.record(2, Some(Duration::from_millis(40)), BufferKind::Shm);

        let s1 = &tracker.windows[&1];
        assert_eq!(s1.commits, 3);
        assert_eq!(s1.dmabuf, 2);
        assert_eq!(s1.shm, 0);
        // The `None` commit carried no callback latency sample.
        assert_eq!(s1.cb_latency_samples, 2);
        assert_eq!(s1.cb_latency_total, Duration::from_millis(8));
        assert_eq!(s1.cb_latency_max, Duration::from_millis(5));

        let s2 = &tracker.windows[&2];
        assert_eq!(s2.commits, 1);
        assert_eq!(s2.shm, 1);
        assert_eq!(s2.dmabuf, 0);
    }

    #[test]
    fn sync_pending_returns_to_zero_when_balanced() {
        let mut stats = SyncStats::new();
        stats.record_install();
        stats.record_install();
        assert_eq!(stats.pending, 2);
        stats.record_clear();
        stats.record_clear();
        assert_eq!(stats.pending, 0);
    }

    #[test]
    fn sync_clear_without_install_is_safe() {
        // A clear with no matching install (e.g. a fence source firing for a
        // commit whose blocker we never counted) must not panic; `pending`
        // simply dips negative, which is itself a useful diagnostic signal.
        let mut stats = SyncStats::new();
        stats.record_clear();
        assert_eq!(stats.pending, -1);
    }

    #[test]
    fn present_stats_handles_first_vblank_with_no_prior_interval() {
        let mut stats = PresentStats::new();
        // First vblank: no previous timestamp, so no interval is recorded but it
        // must not divide by zero or panic.
        stats.record_vblank(Duration::from_micros(16_666), true);
        stats.record_vblank(Duration::from_micros(16_666), true);
    }
}
