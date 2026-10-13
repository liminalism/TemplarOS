//! A gesture rail: the bar or the cluster drawn outward from the bottom centre, a clip at a time.
//!
//! The compositor recognises a downward approach at the bottom centre or a horizontal bottom-row
//! swipe and says which side opened; the shell draws the rail on that side. Everything here is
//! the part both screens share: which way the rail runs, how much of it the pointer has revealed,
//! and which actions leave it open.
//!
//! **Items never move.** A screen lays its chips out once per opening at fixed positions and the
//! pointer only moves a clip over them. A chip that slid along under the pointer would be a
//! target that runs away from the hand aiming at it; a clip that only grows is a curtain being
//! drawn, and whatever it has shown stays where it was shown.
//!
//! **The reveal is max-only**, and runs [`RAIL_LEAD`] ahead of the pointer — half a chip — so the
//! thing about to be under the pointer is already on screen when it gets there. Going back never
//! hides anything: the gesture is forgiving, and a hand that overshoots and returns should find
//! what it overshot still there.

use pixelkit_raster::Rect;

use crate::surface::bar_screen::BarAction;
use crate::surface::field_screen::FieldAction;

/// Which way a rail runs from its origin. Left runs toward x = 0, Right toward the output's width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailSide {
    Left,
    Right,
}

impl RailSide {
    /// `-1` for Left, `1` for Right: the sign that turns "distance outward" into an x offset.
    #[must_use]
    pub fn dir(self) -> i32 {
        match self {
            RailSide::Left => -1,
            RailSide::Right => 1,
        }
    }
}

/// What a screen needs to draw itself as a rail. Logical pixels in the surface's own coordinates;
/// the rail surface spans the whole output width with x = 0 at the output's left edge, so these
/// are output x too.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RailView {
    /// The rail's start: the gate's centre line, the output's horizontal centre.
    pub origin: i32,
    pub side: RailSide,
    /// How far outward from `origin` is on screen.
    pub revealed: i32,
}

impl RailView {
    /// The revealed interval as `[x0, x1)` in logical surface coordinates.
    #[must_use]
    pub fn interval(&self) -> (i32, i32) {
        let revealed = self.revealed.max(0);
        match self.side {
            RailSide::Left => (self.origin - revealed, self.origin),
            RailSide::Right => (self.origin, self.origin + revealed),
        }
    }

    /// The revealed interval in physical pixels, for a screen whose `area` starts at physical
    /// `area_x` and whose logical-to-physical conversion is `px` (`Ui::px`).
    #[must_use]
    pub(crate) fn physical_interval(&self, area_x: i32, px: impl Fn(i32) -> i32) -> (i32, i32) {
        let origin = area_x + px(self.origin);
        let revealed = px(self.revealed.max(0));
        match self.side {
            RailSide::Left => (origin - revealed, origin),
            RailSide::Right => (origin, origin + revealed),
        }
    }
}

/// Whether a cell counts as revealed: its centre is inside `[x0, x1)`. A chip the curtain has only
/// half drawn over is on screen but not yet aimable — the pointer is still on its way to it.
#[must_use]
pub(crate) fn centre_revealed(rect: Rect, (x0, x1): (i32, i32)) -> bool {
    let centre = rect.x + rect.w / 2;
    centre >= x0 && centre < x1
}

/// `rect` cut to the columns `[x0, x1)`; `None` when nothing of it is left.
#[must_use]
pub(crate) fn clip_to(rect: Rect, (x0, x1): (i32, i32)) -> Option<Rect> {
    let left = rect.x.max(x0);
    let right = rect.right().min(x1);
    (right > left && rect.h > 0).then(|| Rect::new(left, rect.y, right - left, rect.h))
}

/// The row shapes of a rail frame clipped to its revealed columns, keeping each shape's corner
/// radius no larger than half of what is left of it.
pub(crate) fn clipped_shapes(
    shapes: impl IntoIterator<Item = (Rect, i32)>,
    interval: (i32, i32),
) -> impl Iterator<Item = (Rect, i32)> {
    shapes.into_iter().filter_map(move |(rect, radius)| {
        clip_to(rect, interval).map(|clipped| (clipped, radius.min(clipped.w / 2)))
    })
}

/// How far ahead of the pointer the reveal runs: half a chip.
pub const RAIL_LEAD: i32 = 28;
/// The gap between the origin line and the first item.
pub const RAIL_GAP: i32 = 8;

/// One opening's reveal. Pure; max-only.
#[derive(Clone, Debug, PartialEq)]
pub struct RailReveal {
    origin: i32,
    side: RailSide,
    length: i32,
    lead: i32,
    visible: i32,
    /// The first pointer report can follow the opening in a later protocol dispatch.
    awaiting_entry: bool,
}

impl RailReveal {
    /// A fresh opening. `initial` is enough to show the first item, so something appears at once.
    #[must_use]
    pub fn new(origin: i32, side: RailSide, length: i32, initial: i32) -> Self {
        let length = length.max(0);
        Self {
            origin,
            side,
            length,
            lead: RAIL_LEAD,
            visible: initial.clamp(0, length),
            awaiting_entry: true,
        }
    }

    /// Follow the pointer to output-local logical `pointer_x`. Returns whether the reveal grew.
    /// Non-finite input is ignored; the reveal never shrinks.
    pub fn advance(&mut self, pointer_x: f64) -> bool {
        if !pointer_x.is_finite() {
            return false;
        }
        let outward = f64::from(self.side.dir()) * (pointer_x - f64::from(self.origin));
        if self.awaiting_entry {
            self.awaiting_entry = false;
            // A bottom-row swipe can open the right rail from the output's far left, or the
            // left rail from its far right. Keep the established centred chip positions, but
            // expose the whole rail so the hand can travel to it along the bottom row.
            if outward < 0.0 {
                let grew = self.visible < self.length;
                self.show_all();
                return grew;
            }
        }
        let wanted = (outward + f64::from(self.lead))
            .clamp(0.0, f64::from(self.length))
            .round() as i32;
        if wanted > self.visible {
            self.visible = wanted;
            true
        } else {
            false
        }
    }

    /// Reduced motion removes the reveal curtain, including an opening already in progress.
    pub fn show_all(&mut self) {
        self.visible = self.length;
    }

    #[must_use]
    pub fn visible(&self) -> i32 {
        self.visible
    }

    #[must_use]
    pub fn view(&self) -> RailView {
        RailView {
            origin: self.origin,
            side: self.side,
            revealed: self.visible,
        }
    }
}

/// Whether performing `action` from a rail leaves the rail open: the level controls and the
/// native clock popup, which remain on this surface. Actions that open another surface dismiss it.
#[must_use]
pub fn action_keeps_rail_open(action: &BarAction) -> bool {
    match action {
        BarAction::Field(
            FieldAction::SetVolume(_) | FieldAction::SetMuted(_) | FieldAction::SetBrightness(_),
        ) => true,
        BarAction::Field(FieldAction::RunAction(id)) => {
            id.starts_with("volume.")
                || id.starts_with("brightness.")
                || *id == "clock.open-calendar"
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_reveal_starts_at_initial_clamped() {
        assert_eq!(RailReveal::new(960, RailSide::Right, 300, 72).visible(), 72);
        assert_eq!(RailReveal::new(960, RailSide::Right, 50, 72).visible(), 50);
        assert_eq!(RailReveal::new(960, RailSide::Right, 300, -5).visible(), 0);
    }

    #[test]
    fn rail_reveal_runs_ahead_of_pointer_by_lead() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 600, 0);
        assert!(reveal.advance(1060.0));
        assert_eq!(reveal.visible(), 100 + RAIL_LEAD);
    }

    #[test]
    fn rail_reveal_never_retracts() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 600, 0);
        reveal.advance(1200.0);
        let high = reveal.visible();
        assert!(!reveal.advance(970.0));
        assert!(!reveal.advance(100.0));
        assert_eq!(reveal.visible(), high);
    }

    #[test]
    fn rail_reveal_clamps_to_length() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 200, 0);
        reveal.advance(1900.0);
        assert_eq!(reveal.visible(), 200);
        assert!(!reveal.advance(1920.0));
    }

    #[test]
    fn reduced_motion_finishes_an_existing_reveal_without_pointer_travel() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 200, 20);
        reveal.show_all();
        assert_eq!(reveal.visible(), 200);
        assert!(!reveal.advance(960.0));
    }

    #[test]
    fn rail_left_mirrors_right() {
        let mut left = RailReveal::new(960, RailSide::Left, 600, 0);
        let mut right = RailReveal::new(960, RailSide::Right, 600, 0);
        for x in [950.0, 900.0, 700.0] {
            left.advance(x);
            right.advance(1920.0 - x);
            assert_eq!(left.visible(), right.visible());
        }
        assert_eq!(left.view().interval(), (960 - left.visible(), 960));
        assert_eq!(right.view().interval(), (960, 960 + right.visible()));
    }

    #[test]
    fn rail_reveal_ignores_non_finite() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 600, 40);
        assert!(!reveal.advance(f64::NAN));
        assert!(!reveal.advance(f64::INFINITY));
        assert!(!reveal.advance(f64::NEG_INFINITY));
        assert_eq!(reveal.visible(), 40);
    }

    #[test]
    fn rail_remote_bottom_opening_exposes_controls_without_moving_them() {
        for (side, entry) in [(RailSide::Right, 200.0), (RailSide::Left, 1720.0)] {
            let mut reveal = RailReveal::new(960, side, 400, 72);
            assert!(reveal.advance(entry));
            assert_eq!(reveal.visible(), 400);
            assert_eq!(reveal.view().origin, 960);
            // Returning through the centre must not make controls disappear.
            for x in [entry, 960.0, 960.0 + f64::from(side.dir()) * 40.0] {
                assert!(!reveal.advance(x));
                assert_eq!(reveal.visible(), 400);
            }
        }
    }

    #[test]
    fn rail_centred_opening_keeps_the_progressive_reveal() {
        for side in [RailSide::Left, RailSide::Right] {
            let mut reveal = RailReveal::new(960, side, 400, 72);
            assert!(!reveal.advance(960.0));
            assert_eq!(reveal.visible(), 72);
            assert!(reveal.advance(960.0 + f64::from(side.dir()) * 100.0));
            assert_eq!(reveal.visible(), 100 + RAIL_LEAD);
        }
    }

    #[test]
    fn rail_delayed_entry_ignores_invalid_pointer_reports() {
        let mut reveal = RailReveal::new(960, RailSide::Right, 400, 72);
        assert!(!reveal.advance(f64::NAN));
        assert!(reveal.advance(200.0));
        assert_eq!(reveal.visible(), 400);
    }

    #[test]
    fn rail_action_keeps_open_only_for_level_controls() {
        let keeps = [
            BarAction::Field(FieldAction::SetVolume(40)),
            BarAction::Field(FieldAction::SetMuted(true)),
            BarAction::Field(FieldAction::SetBrightness(70)),
            BarAction::Field(FieldAction::RunAction("volume.mute")),
            BarAction::Field(FieldAction::RunAction("volume.up")),
            BarAction::Field(FieldAction::RunAction("brightness.down")),
            BarAction::Field(FieldAction::RunAction("clock.open-calendar")),
        ];
        for action in &keeps {
            assert!(action_keeps_rail_open(action), "{action:?}");
        }
        let closes = [
            BarAction::Activate(templar_window::WindowId(1)),
            BarAction::Overview,
            BarAction::Field(FieldAction::RunAction("settings.open")),
            BarAction::Field(FieldAction::RunAction("session.lock")),
            BarAction::Field(FieldAction::RunAction("network.open-control")),
        ];
        for action in &closes {
            assert!(!action_keeps_rail_open(action), "{action:?}");
        }
    }
}
