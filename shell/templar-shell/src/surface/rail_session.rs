//! When an open rail goes away: the shell's half of a gesture rail.
//!
//! The compositor opens a rail and closes it on the things only it sees (a press elsewhere,
//! Escape, the lock). Wandering off is the shell's to judge, because only the shell knows what is
//! on its surface: a fan of window cards or the account popover standing above the row is part of
//! the rail as far as the hand is concerned, and a pointer on its way to one must not lose it.
//!
//! So the rule is forgiving on purpose. The rail holds while the pointer is anywhere in its **hold
//! region** — the band over the rail's own side, well above the row; the output's bottom row;
//! the gate it came in through;
//! every solid shape the rail's screen reports above the row, grown by a margin — and it is
//! dismissed only after the pointer has been outside that region for [`RAIL_GRACE`] without
//! coming back. A held button holds it too: a slider dragged off its track is still being dragged.

use std::time::{Duration, Instant};

use crate::surface::bar_screen::BAR_HEIGHT;
use crate::surface::rail::RailSide;

/// How long the pointer may be outside the hold region before the rail goes. Long enough that an
/// overshoot or a diagonal on the way to a popover is not a dismissal, short enough that leaving
/// on purpose feels immediate.
pub const RAIL_GRACE: Duration = Duration::from_millis(350);

/// How far above the row the hold band reaches, logical px. Generous: the hand travelling along
/// the rail is not travelling along a line.
pub const RAIL_HOLD_BAND: i32 = 56;

/// The entry row spans the output, so a remote horizontal swipe can reach centred controls.
pub const RAIL_BOTTOM_ROW: i32 = 8;

/// How far a reported shape (fan card, tooltip, popover) is grown before it holds the rail.
pub const RAIL_SHAPE_MARGIN: i32 = 24;

/// `[x0, x1) × [y0, y1)` in output-local logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Area {
    fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x0)
            && x < f64::from(self.x1)
            && y >= f64::from(self.y0)
            && y < f64::from(self.y1)
    }
}

/// One open rail's dismissal state. Pure: time comes in as an argument, so every transition is a
/// test rather than a sleep.
#[derive(Clone, Debug)]
pub struct RailSession {
    band: Area,
    gate: Area,
    bottom_row: Area,
    shapes: Vec<Area>,
    button_held: bool,
    last_pointer: Option<(f64, f64)>,
    /// When the pointer last left the hold region, if it is outside it now.
    outside_since: Option<Instant>,
}

impl RailSession {
    /// A rail opened on `side` of an output `output` = (width, height) logical px, through a gate
    /// `gate` = (width, height) centred on its bottom edge.
    #[must_use]
    pub fn new(side: RailSide, output: (i32, i32), gate: (i32, i32)) -> Self {
        let (width, height) = output;
        let origin = width / 2;
        let (x0, x1) = match side {
            RailSide::Left => (0, origin),
            RailSide::Right => (origin, width),
        };
        let band = Area {
            x0,
            y0: height - (BAR_HEIGHT + RAIL_HOLD_BAND),
            x1,
            y1: height,
        };
        let gate = Area {
            x0: origin - gate.0 / 2,
            y0: height - gate.1,
            x1: origin + (gate.0 - gate.0 / 2),
            y1: height,
        };
        Self {
            band,
            gate,
            bottom_row: Area {
                x0: 0,
                y0: height - RAIL_BOTTOM_ROW,
                x1: width,
                y1: height,
            },
            shapes: Vec::new(),
            button_held: false,
            last_pointer: None,
            outside_since: None,
        }
    }

    /// The solid shapes the rail's screen reports this frame, `(x, y, w, h)` in output-local
    /// logical px. A shape appearing under a pointer that had wandered out takes it back in.
    pub fn set_shapes(
        &mut self,
        shapes: impl IntoIterator<Item = (i32, i32, i32, i32)>,
        now: Instant,
    ) {
        self.shapes = shapes
            .into_iter()
            .map(|(x, y, w, h)| Area {
                x0: x - RAIL_SHAPE_MARGIN,
                y0: y - RAIL_SHAPE_MARGIN,
                x1: x + w + RAIL_SHAPE_MARGIN,
                y1: y + h + RAIL_SHAPE_MARGIN,
            })
            .collect();
        self.reconsider(now);
    }

    /// Whether a button is down on the rail's surface. Releasing it outside the hold region
    /// starts the grace afresh: the drag just ended, and the hand has not yet had its chance to
    /// come back.
    pub fn set_button_held(&mut self, held: bool, now: Instant) {
        if self.button_held && !held && self.outside_since.is_some() {
            self.outside_since = Some(now);
        }
        self.button_held = held;
    }

    /// The pointer moved to output-local logical (`x`, `y`).
    pub fn pointer(&mut self, x: f64, y: f64, now: Instant) {
        self.last_pointer = Some((x, y));
        self.reconsider(now);
    }

    /// Whether (`x`, `y`) is somewhere that keeps the rail open.
    #[must_use]
    pub fn holds(&self, x: f64, y: f64) -> bool {
        self.band.contains(x, y)
            || self.gate.contains(x, y)
            || self.bottom_row.contains(x, y)
            || self.shapes.iter().any(|shape| shape.contains(x, y))
    }

    /// Whether the rail should be dismissed now.
    #[must_use]
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// When the rail will be dismissed if nothing changes, so the host can wake for it.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        if self.button_held {
            return None;
        }
        self.outside_since.map(|since| since + RAIL_GRACE)
    }

    fn reconsider(&mut self, now: Instant) {
        let inside = self.last_pointer.is_none_or(|(x, y)| self.holds(x, y));
        if inside {
            self.outside_since = None;
        } else if self.outside_since.is_none() {
            self.outside_since = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTPUT: (i32, i32) = (1920, 1080);
    const GATE: (i32, i32) = (240, 96);

    fn opened(side: RailSide) -> (RailSession, Instant) {
        let now = Instant::now();
        let mut session = RailSession::new(side, OUTPUT, GATE);
        session.pointer(900.0, 1070.0, now);
        (session, now)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn rail_session_holds_along_its_own_side_and_the_gate() {
        let (session, _) = opened(RailSide::Left);
        // The row, well above it, and the gate that straddles the centre.
        assert!(session.holds(10.0, 1075.0));
        assert!(session.holds(500.0, f64::from(1080 - BAR_HEIGHT - RAIL_HOLD_BAND)));
        assert!(session.holds(1020.0, 1060.0));
        // The other side's row and the screen above the band are not the rail.
        assert!(!session.holds(1500.0, 1050.0));
        assert!(!session.holds(500.0, 900.0));
    }

    #[test]
    fn rail_session_leaving_for_less_than_the_grace_keeps_it() {
        let (mut session, now) = opened(RailSide::Left);
        session.pointer(500.0, 400.0, now);
        assert!(!session.expired(now + RAIL_GRACE.saturating_sub(ms(1))));
        assert_eq!(session.deadline(), Some(now + RAIL_GRACE));
    }

    #[test]
    fn rail_session_remote_bottom_entry_holds_until_the_pointer_leaves_the_row() {
        for (side, remote_x) in [(RailSide::Right, 200.0), (RailSide::Left, 1720.0)] {
            let now = Instant::now();
            let mut session = RailSession::new(side, OUTPUT, GATE);
            session.pointer(remote_x, 1079.0, now);
            assert_eq!(session.deadline(), None);
            assert!(!session.expired(now + ms(5000)));
            // The entire route from the remote entry to the rail's centre stays held.
            session.pointer(f64::midpoint(remote_x, 960.0), 1079.0, now + ms(5000));
            session.pointer(960.0, 1079.0, now + ms(6000));
            assert_eq!(session.deadline(), None);
            // Above the entry row on the opposite side, the ordinary grace still applies.
            session.pointer(
                remote_x,
                f64::from(1080 - RAIL_BOTTOM_ROW - 1),
                now + ms(7000),
            );
            assert_eq!(session.deadline(), Some(now + ms(7000) + RAIL_GRACE));
            assert!(session.expired(now + ms(7000) + RAIL_GRACE));
        }
    }

    #[test]
    fn rail_session_leaving_for_longer_than_the_grace_dismisses_it() {
        let (mut session, now) = opened(RailSide::Right);
        session.pointer(1500.0, 400.0, now);
        // Moving about outside does not restart the clock.
        session.pointer(1400.0, 300.0, now + ms(200));
        assert!(session.expired(now + RAIL_GRACE + ms(1)));
    }

    #[test]
    fn rail_session_returning_within_the_grace_cancels_the_dismissal() {
        let (mut session, now) = opened(RailSide::Left);
        session.pointer(500.0, 400.0, now);
        session.pointer(500.0, 1070.0, now + ms(200));
        assert_eq!(session.deadline(), None);
        assert!(!session.expired(now + ms(2000)));
        // And leaving again starts a fresh grace, not the old one.
        session.pointer(500.0, 400.0, now + ms(2000));
        assert!(!session.expired(now + ms(2000) + RAIL_GRACE.saturating_sub(ms(1))));
        assert!(session.expired(now + ms(2000) + RAIL_GRACE));
    }

    #[test]
    fn rail_session_popover_shapes_hold_it() {
        let (mut session, now) = opened(RailSide::Right);
        // An account popover standing well above the row, and the pointer on its way to it —
        // just short of it, inside the margin.
        session.set_shapes([(1600, 700, 280, 300)], now);
        session.pointer(1700.0, 700.0 - f64::from(RAIL_SHAPE_MARGIN) + 1.0, now);
        assert!(!session.expired(now + ms(5000)));
        // The popover closes under a pointer that is otherwise nowhere near the rail.
        session.set_shapes([], now + ms(5000));
        assert!(session.expired(now + ms(5000) + RAIL_GRACE));
    }

    #[test]
    fn rail_session_a_held_button_holds_it() {
        let (mut session, now) = opened(RailSide::Right);
        session.set_button_held(true, now);
        session.pointer(1500.0, 200.0, now);
        assert_eq!(session.deadline(), None);
        assert!(!session.expired(now + ms(5000)));
        // Let go out there: the grace starts from the release, not from when it left.
        session.set_button_held(false, now + ms(5000));
        assert!(!session.expired(now + ms(5000) + RAIL_GRACE.saturating_sub(ms(1))));
        assert!(session.expired(now + ms(5000) + RAIL_GRACE));
    }
}
