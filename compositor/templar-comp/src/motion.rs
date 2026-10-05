//! The park/unpark motion: a window shrinking into its card on Home when it is shelved, and
//! growing back out of it when it is unshelved.
//!
//! The shell draws one card per open or parked window on its Home surface and tells the
//! compositor where each one is (`templar_shell_toplevel_v1.set_minimize_target`, version 12).
//! That rectangle is a *hint for this motion only*: it never changes a window's geometry, layout,
//! stacking or input. [`MotionState`] keeps the hints, the motions in flight, and the shelf and
//! visibility each window had at the last `space_sync` resync — which is how a transition is
//! noticed at all: every shelve and unshelve, whichever command, keybinding, protocol request or
//! group operation caused it, ends in `space_sync::sync_after_change`, and that is where
//! [`transition`] compares the two.
//!
//! Everything here is pure: rectangles, nanoseconds and maps. `space_sync` resolves a hint to
//! global coordinates and starts motions; `scene` draws them; the backends prune finished ones.
//! Both backends already redraw every frame interval (udev: page-flip completion, or
//! `schedule_retry`'s poll on a damage-free frame; winit: `request_redraw` after every redraw),
//! so a motion needs no timer of its own — each frame draws it at the next `rect_at`, and a
//! finished one simply stops being drawn.

use std::collections::{HashMap, HashSet};
use templar_window::{Rect, WindowId};

/// How long a park or an unpark takes.
pub const MOTION_DURATION_NS: u64 = 200_000_000;

/// The opacity a parking window ends at (and an unparking one starts from): faded enough to read
/// as "going away", not so faded that the eye loses it before it reaches the card.
pub const PARKED_ALPHA: f32 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionKind {
    /// Shelved: from the window's own rectangle into its card.
    Park,
    /// Unshelved: from its card back to the window's own rectangle.
    Unpark,
}

/// Where a window lives on Home: `rect`, local to the shell surface `surface_window`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MinimizeTarget {
    pub surface_window: WindowId,
    pub rect: Rect,
}

/// One window moving between its own rectangle and its card, in global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowMotion {
    pub window: WindowId,
    pub from: Rect,
    pub to: Rect,
    pub started_ns: u64,
    pub duration_ns: u64,
    pub kind: MotionKind,
}

impl WindowMotion {
    /// Linear progress in `[0, 1]`: 0 at (or before) the start, 1 from the end on.
    fn progress(&self, now_ns: u64) -> f64 {
        if self.duration_ns == 0 {
            return 1.0;
        }
        let elapsed = now_ns.saturating_sub(self.started_ns);
        (elapsed as f64 / self.duration_ns as f64).min(1.0)
    }

    /// Quintic smoothstep has zero velocity and acceleration at both ends.
    fn eased(&self, now_ns: u64) -> f64 {
        let t = self.progress(now_ns);
        t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
    }

    /// The rectangle to draw the window into at `now_ns`: exactly `from` at the start, exactly
    /// `to` from the end on, and never moving backwards in between.
    #[must_use]
    pub fn rect_at(&self, now_ns: u64) -> Rect {
        let t = self.eased(now_ns);
        if t <= 0.0 {
            return self.from;
        }
        if t >= 1.0 {
            return self.to;
        }
        let lerp = |a: f64, b: f64| a + (b - a) * t;
        // Geometric scale settles uniformly; the aspect approaches the target gently.
        let dimension = |a: i32, b: i32| {
            lerp(f64::from(a.max(1)).ln(), f64::from(b.max(1)).ln())
                .exp()
                .round() as i32
        };
        let width = dimension(self.from.width, self.to.width);
        let height = dimension(self.from.height, self.to.height);
        Rect::new(
            lerp(f64::from(self.from.x), f64::from(self.to.x)).round() as i32,
            lerp(f64::from(self.from.y), f64::from(self.to.y)).round() as i32,
            width,
            height,
        )
    }

    /// The opacity to draw the window with at `now_ns`: a park fades from 1.0 to
    /// [`PARKED_ALPHA`], an unpark the other way.
    #[must_use]
    pub fn alpha_at(&self, now_ns: u64) -> f32 {
        let t = self.eased(now_ns) as f32;
        let faded = (1.0 - PARKED_ALPHA) * t;
        match self.kind {
            MotionKind::Park => 1.0 - faded,
            MotionKind::Unpark => PARKED_ALPHA + faded,
        }
    }

    #[must_use]
    pub fn done(&self, now_ns: u64) -> bool {
        now_ns >= self.started_ns.saturating_add(self.duration_ns)
    }
}

/// The largest rectangle with a `width`:`height` aspect that fits in `rect`, centred in it.
///
/// A card rarely has its window's aspect, and the surface tree is drawn with one uniform scale;
/// without centring, the content would shrink into the card's top-left corner.
#[must_use]
pub fn fit_centered(rect: Rect, width: i32, height: i32) -> Rect {
    if width <= 0 || height <= 0 || rect.width <= 0 || rect.height <= 0 {
        return rect;
    }
    let k =
        (f64::from(rect.width) / f64::from(width)).min(f64::from(rect.height) / f64::from(height));
    let fitted_width = ((f64::from(width) * k).round() as i32).clamp(1, rect.width);
    let fitted_height = ((f64::from(height) * k).round() as i32).clamp(1, rect.height);
    Rect::new(
        rect.x + (rect.width - fitted_width) / 2,
        rect.y + (rect.height - fitted_height) / 2,
        fitted_width,
        fitted_height,
    )
}

/// What one window looked like at a resync: on the shelf or not, and on screen or not
/// (`WindowService::is_visible`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    pub shelved: bool,
    pub visible: bool,
}

/// The motion a change from `before` to `after` calls for, if any.
///
/// A park needs the window to have been on screen — a window hidden behind a Home reveal that
/// gets parked from its card has nowhere visible to shrink *from*. An unpark needs it to be on
/// screen now, for the same reason in reverse.
#[must_use]
pub fn transition(before: Seen, after: Seen) -> Option<MotionKind> {
    match (before.shelved, after.shelved) {
        (false, true) if before.visible => Some(MotionKind::Park),
        (true, false) if after.visible => Some(MotionKind::Unpark),
        _ => None,
    }
}

/// The park/unpark bookkeeping the compositor keeps beside `WindowService`.
#[derive(Debug, Default)]
pub struct MotionState {
    /// Keyed by the client window the card stands for.
    targets: HashMap<WindowId, MinimizeTarget>,
    motions: HashMap<WindowId, WindowMotion>,
    /// Every window's [`Seen`] at the last resync.
    seen: HashMap<WindowId, Seen>,
    reduced_motion: bool,
}

impl MotionState {
    pub fn set_reduced_motion(&mut self, enabled: bool) {
        self.reduced_motion = enabled;
        if enabled {
            self.motions.clear();
        }
    }

    /// Store (`Some`) or clear (`None`) `window`'s card.
    pub fn set_target(&mut self, window: WindowId, target: Option<MinimizeTarget>) {
        match target {
            Some(target) => {
                self.targets.insert(window, target);
            }
            None => {
                self.targets.remove(&window);
            }
        }
    }

    #[must_use]
    pub fn target(&self, window: WindowId) -> Option<MinimizeTarget> {
        self.targets.get(&window).copied()
    }

    /// Start `kind` for `window` from `from` to `to`. A motion already in flight for the same
    /// window is replaced, and the new one starts from wherever that one had got to, so a park
    /// reversed half-way grows back from the half-shrunk rectangle instead of jumping.
    pub fn start(&mut self, window: WindowId, kind: MotionKind, from: Rect, to: Rect, now_ns: u64) {
        if self.reduced_motion {
            self.motions.remove(&window);
            return;
        }
        let from = self
            .motions
            .get(&window)
            .filter(|motion| !motion.done(now_ns))
            .map_or(from, |motion| motion.rect_at(now_ns));
        self.motions.insert(
            window,
            WindowMotion {
                window,
                from,
                to,
                started_ns: now_ns,
                duration_ns: MOTION_DURATION_NS,
                kind,
            },
        );
    }

    /// Stop drawing `window`'s motion, if it has one.
    pub fn cancel(&mut self, window: WindowId) {
        self.motions.remove(&window);
    }

    /// `window`'s motion, if one is still running at `now_ns`.
    #[must_use]
    pub fn active(&self, window: WindowId, now_ns: u64) -> Option<&WindowMotion> {
        self.motions
            .get(&window)
            .filter(|motion| !motion.done(now_ns))
    }

    /// Drop every motion finished by `now_ns`. Returns whether any is still running.
    pub fn prune(&mut self, now_ns: u64) -> bool {
        self.motions.retain(|_, motion| !motion.done(now_ns));
        !self.motions.is_empty()
    }

    /// Record this resync's [`Seen`] for every window, returning the previous resync's.
    pub fn replace_seen(&mut self, seen: HashMap<WindowId, Seen>) -> HashMap<WindowId, Seen> {
        std::mem::replace(&mut self.seen, seen)
    }

    /// Forget everything about `window`: its card, its motion, its last [`Seen`], and every card
    /// that lives on it (when `window` is the shell's Home surface).
    pub fn forget_window(&mut self, window: WindowId) {
        self.targets.remove(&window);
        self.targets
            .retain(|_, target| target.surface_window != window);
        self.motions.remove(&window);
        self.seen.remove(&window);
    }

    /// [`Self::forget_window`] for every window not in `live`.
    pub fn retain_windows(&mut self, live: &HashSet<WindowId>) {
        self.targets.retain(|window, target| {
            live.contains(window) && live.contains(&target.surface_window)
        });
        self.motions.retain(|window, _| live.contains(window));
        self.seen.retain(|window, _| live.contains(window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_cancels_in_flight_and_refuses_new_motion() {
        let mut state = MotionState::default();
        state.start(
            WindowId(1),
            MotionKind::Park,
            Rect::new(0, 0, 800, 600),
            Rect::new(0, 0, 100, 80),
            0,
        );
        state.set_reduced_motion(true);
        assert!(state.active(WindowId(1), 1).is_none());
        state.start(
            WindowId(2),
            MotionKind::Unpark,
            Rect::new(0, 0, 100, 80),
            Rect::new(0, 0, 800, 600),
            1,
        );
        assert!(state.active(WindowId(2), 2).is_none());
    }

    fn motion(kind: MotionKind) -> WindowMotion {
        WindowMotion {
            window: WindowId(1),
            from: Rect::new(100, 200, 800, 600),
            to: Rect::new(20, 900, 160, 90),
            started_ns: 1_000,
            duration_ns: MOTION_DURATION_NS,
            kind,
        }
    }

    #[test]
    fn the_endpoints_are_exact() {
        let park = motion(MotionKind::Park);
        assert_eq!(park.rect_at(0), park.from, "before the start");
        assert_eq!(park.rect_at(park.started_ns), park.from);
        assert_eq!(park.rect_at(park.started_ns + park.duration_ns), park.to);
        assert_eq!(park.rect_at(u64::MAX), park.to, "long after the end");
    }

    #[test]
    fn easing_never_moves_backwards_and_scale_settles_geometrically() {
        let park = motion(MotionKind::Park);
        let mut previous = park.rect_at(park.started_ns);
        for step in 1..=200 {
            let now = park.started_ns + park.duration_ns * step / 200;
            let rect = park.rect_at(now);
            assert!(rect.x <= previous.x && rect.y >= previous.y, "step {step}");
            assert!(rect.width <= previous.width && rect.height <= previous.height);
            previous = rect;
        }
        // Geometric scale has already lost more than half its size halfway through.
        let half = park.rect_at(park.started_ns + park.duration_ns / 2);
        assert!(half.width * 2 < park.from.width + park.to.width);
    }

    #[test]
    fn a_park_fades_out_and_an_unpark_fades_in_within_range() {
        let park = motion(MotionKind::Park);
        let unpark = motion(MotionKind::Unpark);
        let end = park.started_ns + park.duration_ns;
        assert!((park.alpha_at(park.started_ns) - 1.0).abs() < f32::EPSILON);
        assert!((park.alpha_at(end) - PARKED_ALPHA).abs() < 1e-6);
        assert!((unpark.alpha_at(unpark.started_ns) - PARKED_ALPHA).abs() < 1e-6);
        assert!((unpark.alpha_at(end) - 1.0).abs() < 1e-6);
        for step in 0..=50 {
            let now = park.started_ns + park.duration_ns * step / 50;
            for alpha in [park.alpha_at(now), unpark.alpha_at(now)] {
                assert!(
                    (PARKED_ALPHA - 1e-6..=1.0 + 1e-6).contains(&alpha),
                    "{alpha}"
                );
            }
        }
    }

    #[test]
    fn a_motion_is_done_exactly_at_its_end() {
        let park = motion(MotionKind::Park);
        assert!(!park.done(park.started_ns));
        assert!(!park.done(park.started_ns + park.duration_ns - 1));
        assert!(park.done(park.started_ns + park.duration_ns));
        let instant = WindowMotion {
            duration_ns: 0,
            ..park
        };
        assert!(instant.done(instant.started_ns));
        assert_eq!(instant.rect_at(instant.started_ns), instant.to);
    }

    #[test]
    fn a_reversed_motion_starts_where_the_old_one_had_got_to() {
        let mut state = MotionState::default();
        let window = WindowId(7);
        let own = Rect::new(0, 0, 1000, 500);
        let card = Rect::new(500, 900, 100, 50);
        state.start(window, MotionKind::Park, own, card, 0);
        let midway = MOTION_DURATION_NS / 2;
        let reached = state.active(window, midway).unwrap().rect_at(midway);
        state.start(window, MotionKind::Unpark, card, own, midway);
        let unpark = state.active(window, midway).unwrap();
        assert_eq!(unpark.kind, MotionKind::Unpark);
        assert_eq!(unpark.from, reached);
        assert_eq!(unpark.to, own);

        assert!(state.prune(midway + 1));
        assert!(!state.prune(midway + MOTION_DURATION_NS));
        assert!(state.active(window, midway).is_none());
    }

    #[test]
    fn only_a_visible_park_or_a_visible_unpark_is_a_transition() {
        let on_screen = Seen {
            shelved: false,
            visible: true,
        };
        let hidden = Seen {
            shelved: false,
            visible: false,
        };
        let parked = Seen {
            shelved: true,
            visible: false,
        };
        assert_eq!(transition(on_screen, parked), Some(MotionKind::Park));
        assert_eq!(transition(parked, on_screen), Some(MotionKind::Unpark));
        assert_eq!(transition(hidden, parked), None, "nothing to shrink from");
        assert_eq!(transition(parked, hidden), None, "nothing to grow into");
        assert_eq!(transition(on_screen, on_screen), None);
        assert_eq!(transition(parked, parked), None);
    }

    #[test]
    fn content_is_fitted_into_the_middle_of_a_card() {
        assert_eq!(
            fit_centered(Rect::new(0, 0, 200, 200), 1600, 900),
            Rect::new(0, 43, 200, 113)
        );
        assert_eq!(
            fit_centered(Rect::new(10, 10, 160, 90), 1600, 900),
            Rect::new(10, 10, 160, 90)
        );
        assert_eq!(
            fit_centered(Rect::new(1, 2, 3, 4), 0, 9),
            Rect::new(1, 2, 3, 4)
        );
    }

    #[test]
    fn forgetting_a_window_drops_its_card_its_motion_and_every_card_on_it() {
        let mut state = MotionState::default();
        let home = WindowId(1);
        let app = WindowId(2);
        let other = WindowId(3);
        let target = MinimizeTarget {
            surface_window: home,
            rect: Rect::new(0, 0, 10, 10),
        };
        state.set_target(app, Some(target));
        state.set_target(other, Some(target));
        state.start(app, MotionKind::Park, Rect::new(0, 0, 9, 9), target.rect, 0);
        state.forget_window(app);
        assert!(state.target(app).is_none());
        assert!(state.active(app, 0).is_none());
        assert_eq!(state.target(other), Some(target));
        state.forget_window(home);
        assert!(state.target(other).is_none());
    }
}
