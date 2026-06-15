//! Compositor-level window animations.
//!
//! This module implements a small, self-contained *visual* animation layer
//! that is kept strictly separate from the logical tiling layout. The layout
//! engine always computes the real, final geometry immediately (see
//! [`crate::compositor::state::Beewm::relayout`]); animations only interpolate
//! the *rectangle a window is rendered into* over a short duration. Input,
//! focus and pointer hit-testing all continue to use the real geometry from the
//! `Space`, so the layout stays deterministic while the picture catches up.
//!
//! Three animation kinds are supported:
//!
//! * [`WindowAnimationKind::Open`] — a freshly mapped tiled window grows from a
//!   tiny rectangle anchored at the final top-left corner out to its full tile.
//!   Rendered with `CutOff` (clip/reveal): the window content is progressively
//!   *uncovered* from the top-left rather than scaled, which is the requested
//!   "expand from top-left" effect.
//! * [`WindowAnimationKind::GeometryChange`] — an existing tiled window whose
//!   assigned geometry changed (because a sibling opened/closed, the layout or
//!   master ratio changed, …) smoothly interpolates x/y/width/height from its
//!   previous visual rectangle to the new tile. Rendered with `Stretch` so the
//!   client buffer is scaled into the interpolated rectangle (no gaps while the
//!   client catches up to the new configured size).
//! * [`WindowAnimationKind::Close`] — reserved. A true closing animation needs
//!   a GPU snapshot of the surface that outlives the destroyed client buffer,
//!   which Smithay does not provide out of the box; see the module docs in
//!   `render.rs` and the README of this change for the limitation. We still get
//!   the "remaining windows expand into the freed space" half of the effect for
//!   free via `GeometryChange` on the surviving windows.
//!
//! Everything here is keyed by the *root* `WlSurface` of a window so the state
//! survives buffer commits and is trivially pruned when the surface dies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Rectangle};

use crate::config::Config;

/// Fraction of the final size the opening rectangle starts at. A new window
/// becomes visible as a small rectangle of this size anchored at the final
/// top-left corner and grows out to full size.
const OPEN_START_SCALE: f64 = 0.05;

/// Easing curves. `Linear` is only a fallback; the defaults are cubic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Easing {
    Linear,
    EaseInCubic,
    EaseOutCubic,
    EaseInOutCubic,
}

impl Easing {
    /// Parse a config string (case-insensitive). Unknown values yield `None`
    /// so the caller can keep its current/default curve.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "linear" => Some(Self::Linear),
            "ease_in" | "ease_in_cubic" | "in" => Some(Self::EaseInCubic),
            "ease_out" | "ease_out_cubic" | "out" => Some(Self::EaseOutCubic),
            "ease_in_out" | "ease_in_out_cubic" | "in_out" => Some(Self::EaseInOutCubic),
            _ => None,
        }
    }

    /// Map a normalized time `t` to an eased value. `t` is clamped to `[0, 1]`,
    /// and every curve satisfies `apply(0.0) == 0.0` and `apply(1.0) == 1.0`.
    pub fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::EaseInCubic => t * t * t,
            Self::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            Self::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
                }
            }
        }
    }
}

/// Linearly interpolate between two logical rectangles. Width and height are
/// clamped to at least 1 so a degenerate rectangle never disappears or makes
/// the renderer divide by zero.
pub fn lerp_rect(
    from: Rectangle<i32, Logical>,
    to: Rectangle<i32, Logical>,
    t: f64,
) -> Rectangle<i32, Logical> {
    let lerp = |a: i32, b: i32| (a as f64 + (b as f64 - a as f64) * t).round() as i32;
    Rectangle::new(
        (lerp(from.loc.x, to.loc.x), lerp(from.loc.y, to.loc.y)).into(),
        (
            lerp(from.size.w, to.size.w).max(1),
            lerp(from.size.h, to.size.h).max(1),
        )
            .into(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAnimationKind {
    Open,
    Close,
    GeometryChange,
}

/// A single in-flight visual animation for one window.
#[derive(Debug, Clone, Copy)]
pub struct WindowAnimation {
    pub kind: WindowAnimationKind,
    pub from: Rectangle<i32, Logical>,
    pub to: Rectangle<i32, Logical>,
    pub started_at: Instant,
    pub duration: Duration,
    pub easing: Easing,
}

impl WindowAnimation {
    /// Normalized, un-eased progress in `[0, 1]`.
    fn progress(&self, now: Instant) -> f64 {
        if self.duration.is_zero() {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.started_at).as_secs_f64();
        (elapsed / self.duration.as_secs_f64()).clamp(0.0, 1.0)
    }

    /// True once the wall-clock duration has elapsed.
    pub fn is_finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.duration
    }

    /// The interpolated visual rectangle at `now`.
    pub fn current_rect(&self, now: Instant) -> Rectangle<i32, Logical> {
        let t = self.easing.apply(self.progress(now));
        lerp_rect(self.from, self.to, t)
    }

    /// Reveal animations (`Open`/`Close`) clip the window to the visual rect
    /// (content uncovered/covered from the top-left). Non-reveal animations
    /// (`GeometryChange`) scale the buffer into the visual rect instead.
    pub fn is_reveal(&self) -> bool {
        matches!(
            self.kind,
            WindowAnimationKind::Open | WindowAnimationKind::Close
        )
    }
}

/// The visual rectangle a window should currently be rendered into, plus how to
/// fill it (reveal/clip vs. scale).
#[derive(Debug, Clone, Copy)]
pub struct VisualRect {
    pub rect: Rectangle<i32, Logical>,
    pub reveal: bool,
}

/// Owns all active window animations and the per-window resting targets used to
/// detect geometry changes. Keyed by root `WlSurface`.
pub struct AnimationManager {
    enabled: bool,
    open_enabled: bool,
    close_enabled: bool,
    layout_enabled: bool,
    disable_for_fullscreen: bool,
    open_duration: Duration,
    close_duration: Duration,
    layout_duration: Duration,
    open_easing: Easing,
    close_easing: Easing,
    layout_easing: Easing,
    anims: HashMap<WlSurface, WindowAnimation>,
    /// Last target (resting) rectangle laid out for each tracked root. Used to
    /// tell "brand new window" from "geometry changed" in [`Self::reconcile`].
    targets: HashMap<WlSurface, Rectangle<i32, Logical>>,
}

impl AnimationManager {
    /// Build from config, honouring the `BEEWM_DISABLE_ANIMATIONS` runtime
    /// kill-switch (which forces everything off regardless of config).
    pub fn from_config(config: &Config) -> Self {
        let mut manager = Self {
            enabled: true,
            open_enabled: true,
            close_enabled: true,
            layout_enabled: true,
            disable_for_fullscreen: true,
            open_duration: Duration::from_millis(180),
            close_duration: Duration::from_millis(150),
            layout_duration: Duration::from_millis(200),
            open_easing: Easing::EaseOutCubic,
            close_easing: Easing::EaseInOutCubic,
            layout_easing: Easing::EaseInOutCubic,
            anims: HashMap::new(),
            targets: HashMap::new(),
        };
        manager.update_from_config(config);
        manager
    }

    /// Re-read animation-related config values in place (called on config
    /// reload). Does not touch in-flight animations.
    pub fn update_from_config(&mut self, config: &Config) {
        let runtime_disabled = crate::compositor::runtime_flags::flags().animations_disabled;
        self.enabled = config.enable_animations && !runtime_disabled;
        self.open_enabled = config.window_open_animation;
        self.close_enabled = config.window_close_animation;
        self.layout_enabled = config.layout_animation;
        self.disable_for_fullscreen = config.disable_animations_for_fullscreen;
        self.open_duration = Duration::from_millis(config.open_animation_duration_ms);
        self.close_duration = Duration::from_millis(config.close_animation_duration_ms);
        self.layout_duration = Duration::from_millis(config.layout_animation_duration_ms);
        if let Some(easing) = Easing::parse(&config.animation_easing) {
            self.open_easing = easing;
            self.close_easing = easing;
            self.layout_easing = easing;
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn disable_for_fullscreen(&self) -> bool {
        self.disable_for_fullscreen
    }

    /// Reconcile a tiled window's freshly computed `target` rectangle with any
    /// existing animation/target state, starting an `Open` or `GeometryChange`
    /// animation as appropriate. Called once per tiled window from `relayout`.
    ///
    /// * unknown root → `Open` (tiny rect at the final top-left → full tile)
    /// * known root, target moved/resized → `GeometryChange`, starting from the
    ///   *current interpolated* visual rect if an animation is already running
    ///   (so rapidly-opened windows never jump back to an old start rect)
    /// * known root, unchanged target → leave any running animation alone
    pub fn reconcile(
        &mut self,
        root: &WlSurface,
        target: Rectangle<i32, Logical>,
        now: Instant,
        app_id: &str,
    ) {
        if !self.enabled {
            // Still track the resting target so re-enabling later does not snap.
            self.targets.insert(root.clone(), target);
            return;
        }

        match self.targets.get(root).copied() {
            None => {
                if self.open_enabled {
                    let from = open_start_rect(target);
                    self.start(root, WindowAnimationKind::Open, from, target, now, app_id);
                }
            }
            Some(prev) if prev != target => {
                if self.layout_enabled {
                    let interrupted = self.anims.contains_key(root);
                    let from = retarget_from(self.anims.get(root), prev, now);
                    if interrupted {
                        tracing::trace!(
                            target = "beewm::animation",
                            id = root_id(root),
                            app_id,
                            ?from,
                            ?target,
                            "geometry animation interrupted by new target",
                        );
                    }
                    self.start(
                        root,
                        WindowAnimationKind::GeometryChange,
                        from,
                        target,
                        now,
                        app_id,
                    );
                } else {
                    self.anims.remove(root);
                }
            }
            Some(_) => {}
        }

        self.targets.insert(root.clone(), target);
    }

    fn start(
        &mut self,
        root: &WlSurface,
        kind: WindowAnimationKind,
        from: Rectangle<i32, Logical>,
        to: Rectangle<i32, Logical>,
        now: Instant,
        app_id: &str,
    ) {
        let (duration, easing) = match kind {
            WindowAnimationKind::Open => (self.open_duration, self.open_easing),
            WindowAnimationKind::Close => (self.close_duration, self.close_easing),
            WindowAnimationKind::GeometryChange => (self.layout_duration, self.layout_easing),
        };
        if duration.is_zero() {
            self.anims.remove(root);
            return;
        }
        tracing::debug!(
            target = "beewm::animation",
            id = root_id(root),
            app_id,
            ?kind,
            ?from,
            ?to,
            duration_ms = duration.as_millis() as u64,
            "starting window animation",
        );
        self.anims.insert(
            root.clone(),
            WindowAnimation {
                kind,
                from,
                to,
                started_at: now,
                duration,
                easing,
            },
        );
    }

    /// Record a resting target for `root` without starting any animation, and
    /// cancel any in-flight one. Used when animations are situationally
    /// suppressed (e.g. a fullscreen game owns the screen) so that re-enabling
    /// later does not snap from a stale rectangle.
    pub fn track_target(&mut self, root: &WlSurface, target: Rectangle<i32, Logical>) {
        self.anims.remove(root);
        self.targets.insert(root.clone(), target);
    }

    /// The visual rectangle for `root` right now, or `None` if it is not being
    /// animated (render/borders should use the real geometry then).
    pub fn active_rect(&self, root: &WlSurface, now: Instant) -> Option<VisualRect> {
        let anim = self.anims.get(root)?;
        Some(VisualRect {
            rect: anim.current_rect(now),
            reveal: anim.is_reveal(),
        })
    }

    pub fn has_active(&self) -> bool {
        !self.anims.is_empty()
    }

    /// Drop animation + resting-target state for a root (window closed/unmapped
    /// or transitioned to floating). Safe to call for unknown roots.
    pub fn forget(&mut self, root: &WlSurface) {
        if self.anims.remove(root).is_some() {
            tracing::trace!(
                target = "beewm::animation",
                id = root_id(root),
                "forgetting window animation state",
            );
        }
        self.targets.remove(root);
    }

    /// Advance time: drop finished animations and any whose surface has died.
    /// Returns `true` if at least one animation is still active (the caller
    /// should keep scheduling frames).
    pub fn tick(&mut self, now: Instant) -> bool {
        self.anims.retain(|root, anim| {
            if !root.alive() {
                return false;
            }
            if anim.is_finished(now) {
                tracing::trace!(
                    target = "beewm::animation",
                    id = root_id(root),
                    ?anim.kind,
                    "window animation finished",
                );
                return false;
            }
            true
        });
        self.targets.retain(|root, _| root.alive());
        !self.anims.is_empty()
    }
}

/// Pick the `from` rectangle for a re-target. If an animation is already in
/// flight we continue from its *current interpolated* rectangle so the window
/// never jumps back to an old start; otherwise we start from the previous
/// resting target.
fn retarget_from(
    existing: Option<&WindowAnimation>,
    prev_target: Rectangle<i32, Logical>,
    now: Instant,
) -> Rectangle<i32, Logical> {
    existing
        .map(|anim| anim.current_rect(now))
        .unwrap_or(prev_target)
}

/// The tiny opening rectangle: anchored at `target`'s top-left, sized to a
/// small fraction of the final tile.
fn open_start_rect(target: Rectangle<i32, Logical>) -> Rectangle<i32, Logical> {
    let w = ((target.size.w as f64 * OPEN_START_SCALE).round() as i32).max(1);
    let h = ((target.size.h as f64 * OPEN_START_SCALE).round() as i32).max(1);
    Rectangle::new(target.loc, (w, h).into())
}

fn root_id(root: &WlSurface) -> u32 {
    use smithay::reexports::wayland_server::Resource;
    root.id().protocol_id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn easing_endpoints() {
        for easing in [
            Easing::Linear,
            Easing::EaseInCubic,
            Easing::EaseOutCubic,
            Easing::EaseInOutCubic,
        ] {
            assert!((easing.apply(0.0) - 0.0).abs() < 1e-9, "{easing:?} at 0");
            assert!((easing.apply(1.0) - 1.0).abs() < 1e-9, "{easing:?} at 1");
        }
    }

    #[test]
    fn easing_clamps_out_of_range() {
        for easing in [
            Easing::Linear,
            Easing::EaseInCubic,
            Easing::EaseOutCubic,
            Easing::EaseInOutCubic,
        ] {
            assert_eq!(easing.apply(-5.0), easing.apply(0.0));
            assert_eq!(easing.apply(5.0), easing.apply(1.0));
        }
    }

    #[test]
    fn ease_out_cubic_is_monotonic_and_front_loaded() {
        // ease-out cubic should be past the half-way point well before t=0.5.
        assert!(Easing::EaseOutCubic.apply(0.5) > 0.5);
    }

    #[test]
    fn lerp_rect_endpoints_and_midpoint() {
        let a = rect(0, 0, 100, 200);
        let b = rect(40, 60, 300, 400);
        assert_eq!(lerp_rect(a, b, 0.0), a);
        assert_eq!(lerp_rect(a, b, 1.0), b);
        let mid = lerp_rect(a, b, 0.5);
        assert_eq!(mid, rect(20, 30, 200, 300));
    }

    #[test]
    fn lerp_rect_clamps_size_to_one() {
        let a = rect(10, 10, 0, 0);
        let b = rect(10, 10, 0, 0);
        let mid = lerp_rect(a, b, 0.5);
        assert_eq!(mid.size.w, 1);
        assert_eq!(mid.size.h, 1);
    }

    #[test]
    fn open_animation_keeps_top_left_fixed() {
        let target = rect(960, 0, 960, 1080);
        let from = open_start_rect(target);
        assert_eq!(from.loc, target.loc, "open starts anchored at top-left");
        assert!(from.size.w < target.size.w && from.size.h < target.size.h);

        let now = Instant::now();
        let anim = WindowAnimation {
            kind: WindowAnimationKind::Open,
            from,
            to: target,
            started_at: now,
            duration: Duration::from_millis(200),
            easing: Easing::EaseOutCubic,
        };
        // Top-left stays put throughout, end matches target exactly.
        for ms in [0u64, 50, 100, 150, 200] {
            let r = anim.current_rect(now + Duration::from_millis(ms));
            assert_eq!(r.loc, target.loc);
        }
        let end = anim.current_rect(now + Duration::from_millis(200));
        assert_eq!(end, target);
    }

    #[test]
    fn close_animation_keeps_top_left_fixed() {
        let from = rect(0, 0, 1920, 1080);
        let to = open_start_rect(from); // shrink toward top-left
        let now = Instant::now();
        let anim = WindowAnimation {
            kind: WindowAnimationKind::Close,
            from,
            to,
            started_at: now,
            duration: Duration::from_millis(150),
            easing: Easing::EaseInOutCubic,
        };
        for ms in [0u64, 50, 100, 150] {
            let r = anim.current_rect(now + Duration::from_millis(ms));
            assert_eq!(r.loc, from.loc);
        }
    }

    #[test]
    fn geometry_transition_interpolates_all_components() {
        let from = rect(0, 0, 1920, 1080);
        let to = rect(0, 0, 960, 1080);
        let now = Instant::now();
        let anim = WindowAnimation {
            kind: WindowAnimationKind::GeometryChange,
            from,
            to,
            started_at: now,
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        };
        let mid = anim.current_rect(now + Duration::from_millis(100));
        assert_eq!(mid.size.w, 1440); // halfway 1920 -> 960
        assert_eq!(mid.size.h, 1080);
        assert_eq!(anim.current_rect(now + Duration::from_millis(200)), to);
    }

    #[test]
    fn animation_reports_finished_at_or_past_duration() {
        let now = Instant::now();
        let anim = WindowAnimation {
            kind: WindowAnimationKind::Open,
            from: rect(0, 0, 1, 1),
            to: rect(0, 0, 100, 100),
            started_at: now,
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        assert!(!anim.is_finished(now));
        assert!(!anim.is_finished(now + Duration::from_millis(99)));
        assert!(anim.is_finished(now + Duration::from_millis(100)));
        assert!(anim.is_finished(now + Duration::from_millis(250)));
    }

    #[test]
    fn interrupted_animation_starts_from_current_visual_geometry() {
        // Without a running animation, a re-target starts from the previous
        // resting target.
        let prev = rect(0, 0, 1920, 1080);
        let now = Instant::now();
        assert_eq!(retarget_from(None, prev, now), prev);

        // With a running geometry animation, a re-target continues from the
        // *current interpolated* rect, not from the original `from`.
        let running = WindowAnimation {
            kind: WindowAnimationKind::GeometryChange,
            from: rect(0, 0, 1920, 1080),
            to: rect(0, 0, 960, 1080),
            started_at: now,
            duration: Duration::from_millis(200),
            easing: Easing::Linear,
        };
        let at = now + Duration::from_millis(100);
        let expected_current = running.current_rect(at); // 1440 wide
        let from = retarget_from(Some(&running), prev, at);
        assert_eq!(from, expected_current);
        assert_ne!(from, running.from);
        assert_ne!(from, prev);
    }
}
