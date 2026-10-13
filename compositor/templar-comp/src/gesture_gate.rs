//! The gesture gate: an invisible rectangle centred on each output's bottom edge that turns
//! "came down into it from above, then went sideways" or a horizontal bottom-row swipe into one of the shell's two
//! rails (`templar-shell-v2.xml`'s `set_gesture_gate`).
//!
//! Compositor state, not a surface: nothing here takes input or installs a grab, so a pointer
//! passing over the gate is delivered to whatever is underneath exactly as it would be without
//! it. The recognizer only *watches* the motion stream — both pointer paths feed it
//! (`input::update_gesture_gate`) with the segment between consecutive samples in global logical
//! coordinates — and it owns no clock, no timer and no allocation: each sample is a handful of
//! comparisons, because it runs on every motion report there is.
//!
//! The user's governing rule is that the gesture can be *very forgiving*, so every threshold here
//! leans towards opening:
//!
//! - **Arm** by entering the gate through any edge, or with one motion segment that jumps across
//!   it, having been above its top edge (by [`ABOVE_MARGIN`]) within [`ABOVE_MEMORY_MS`]. A
//!   segment, not a point: a fast flick can cross the whole gate between two reports. Starting
//!   inside, or sliding in sideways low on the screen with no recent "above", arms nothing unless
//!   the pointer is in the narrow bottom row, where horizontal swipes have their own entry path.
//! - **Open** once the pointer is [`COMMIT_DISTANCE`] sideways from where it entered, whatever it
//!   did vertically. The side is latched for the opening; reversing never switches rails.
//! - **Disarm** on rising [`RISE_CANCEL`] above the gate, on a held button, after
//!   [`ARM_TIMEOUT_MS`] without committing, or on moving to another output.
//! - After a disarm that was not the pointer rising, and after every close, the pointer has to be
//!   above the gate again before a diagonal can arm. A bottom-row swipe can rearm after leaving
//!   that row or a brief pause, so closing never reopens the rail in the same motion stream.

/// How far above the gate's top edge the pointer must be for the approach to count as "from
/// above", logical pixels. Small: the rule is about direction, not distance.
pub const ABOVE_MARGIN: f64 = 4.0;
/// How long being above the gate is remembered, milliseconds. Long enough for a slow, deliberate
/// descent that dawdles just over the top edge; short enough that a pointer parked low on the
/// screen for a while no longer counts as having come from above.
pub const ABOVE_MEMORY_MS: u64 = 700;
/// Sideways travel from the entry point that opens a rail, logical pixels. Vertical motion is
/// ignored entirely — a diagonal, a hook and a straight line down-then-across all commit.
pub const COMMIT_DISTANCE: f64 = 16.0;
/// A horizontal swipe can start anywhere in this narrow output-bottom band, logical pixels.
pub const BOTTOM_ROW_HEIGHT: f64 = 8.0;
/// A fresh swipe may start at the bottom after this pause in motion reports, milliseconds.
pub const BOTTOM_REARM_MS: u64 = 250;
/// How far above the gate's top edge an armed pointer may wander before it is taken back,
/// logical pixels. Generous, so wobbling out of the top of the gate on the way in is not a cancel.
pub const RISE_CANCEL: f64 = 40.0;
/// How long an arm may sit uncommitted, milliseconds.
pub const ARM_TIMEOUT_MS: u64 = 3_000;
/// The gate's size when the shell does not say (`shell.rails.gate-width`/`gate-height`).
pub const DEFAULT_WIDTH: u32 = 240;
pub const DEFAULT_HEIGHT: u32 = 96;

/// What the shell configured (`set_gesture_gate`). Disabled until a shell says otherwise, and
/// again after it disconnects — the classic bar and its Peek are the fallback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateConfig {
    pub enabled: bool,
    pub width: u32,
    pub height: u32,
    /// Draw the gate's outline (`chrome::collect_gesture_gate`).
    pub learning: bool,
    /// Canonical application ids under whose windows the gate never arms.
    pub exceptions: Vec<String>,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            learning: false,
            exceptions: Vec::new(),
        }
    }
}

impl GateConfig {
    /// Whether `app_id` is one of the exceptions.
    pub fn is_exception(&self, app_id: &str) -> bool {
        self.exceptions.iter().any(|exception| exception == app_id)
    }
}

/// A point in global logical coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// A rectangle in global logical coordinates; `y` grows downward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Area {
    pub fn right(self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(self) -> f64 {
        self.y + self.height
    }

    fn contains(self, point: Pos) -> bool {
        point.x >= self.x
            && point.x <= self.right()
            && point.y >= self.y
            && point.y <= self.bottom()
    }
}

/// Which way the pointer went from the gate. Which rail lives on which side is the shell's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Where the recognizer is. `output` is the index of the output in
/// `WindowService::outputs` the caller fed — opaque here, only compared.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    Armed {
        entry: Pos,
        since: u64,
        output: usize,
    },
    Open {
        output: usize,
        side: Side,
    },
}

/// Why an arm was taken back, for the diagnostic log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisarmReason {
    Rose,
    Button,
    Stale,
    OutputChanged,
}

/// What one sample changed. At most one per sample: a flick that arms and commits in the same
/// segment reports only `Opened`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Armed,
    Opened(Side),
    Disarmed(DisarmReason),
}

/// One motion report: the segment `from` → `to` on the output `output` whose rectangle is
/// `screen`, at `now_ms`, with or without a button held.
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub output: usize,
    pub screen: Area,
    pub from: Pos,
    pub to: Pos,
    pub now_ms: u64,
    pub button_held: bool,
}

#[derive(Debug, Default)]
pub struct GestureGate {
    config: GateConfig,
    phase: Phase,
    /// The output and time the pointer was last seen above the gate. `None` is the "await exit"
    /// state: nothing can arm until the pointer has been above again.
    above: Option<(usize, u64)>,
    /// Suppress the remainder of a swipe after dismissing a rail or cancelling an arm.
    bottom_blocked: bool,
    last_motion_ms: Option<u64>,
}

impl GestureGate {
    pub fn config(&self) -> &GateConfig {
        &self.config
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Take a new configuration. Disabling drops any arm or opening and the "above" memory; the
    /// caller closes an open rail first (`TemplarCompState::close_rail`).
    pub fn configure(&mut self, config: GateConfig) {
        if !config.enabled {
            self.close();
        }
        self.config = config;
    }

    /// The opening (or arm) ended, whoever ended it: back to idle, and the pointer has to go
    /// above the gate before it can arm again.
    pub fn close(&mut self) {
        self.phase = Phase::Idle;
        self.above = None;
        self.bottom_blocked = true;
    }

    /// The gate on the output whose rectangle is `screen`: the configured size, clamped to the
    /// screen, centred on its bottom edge.
    pub fn area(&self, screen: Area) -> Area {
        let width = f64::from(self.config.width).min(screen.width);
        let height = f64::from(self.config.height).min(screen.height);
        Area {
            x: screen.x + (screen.width - width) / 2.0,
            y: screen.bottom() - height,
            width,
            height,
        }
    }

    /// Feed one motion sample. `may_arm` is asked only at the moment the gate would arm — the
    /// exception list's check, which walks windows and is not worth doing on every report.
    pub fn feed(
        &mut self,
        sample: Sample,
        may_arm: impl FnOnce(&GateConfig) -> bool,
    ) -> Option<Transition> {
        if !self.config.enabled {
            return None;
        }
        let at_bottom = self.note_bottom_motion(sample);
        let gate = self.area(sample.screen);
        let above_line = gate.y - ABOVE_MARGIN;
        match self.phase {
            // The rail is the shell's while it is open; closing it is `close`.
            Phase::Open { .. } => None,
            Phase::Armed {
                entry,
                since,
                output,
            } => {
                let reason = if output != sample.output {
                    Some(DisarmReason::OutputChanged)
                } else if sample.button_held {
                    Some(DisarmReason::Button)
                } else if sample.to.y < gate.y - RISE_CANCEL {
                    Some(DisarmReason::Rose)
                } else if sample.now_ms.saturating_sub(since) > ARM_TIMEOUT_MS {
                    Some(DisarmReason::Stale)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    self.phase = Phase::Idle;
                    self.bottom_blocked = true;
                    // Rising out of the top *is* being above: coming straight back down is a
                    // fresh approach. Every other cancel needs the pointer to go up again.
                    self.above =
                        (reason == DisarmReason::Rose).then_some((sample.output, sample.now_ms));
                    return Some(Transition::Disarmed(reason));
                }
                self.commit(entry, sample.to, output)
            }
            Phase::Idle => {
                // Starting in the bottom row is intentional horizontal input: it needs neither
                // a descent nor the central gate. Keep the same direction latch and distance as
                // the diagonal route; exceptions and held-button rules apply to both.
                if at_bottom
                    && !self.bottom_blocked
                    && (sample.to.x - sample.from.x).abs() > f64::EPSILON
                    && !sample.button_held
                {
                    if !may_arm(&self.config) {
                        return None;
                    }
                    self.phase = Phase::Armed {
                        entry: sample.from,
                        since: sample.now_ms,
                        output: sample.output,
                    };
                    return Some(
                        self.commit(sample.from, sample.to, sample.output)
                            .unwrap_or(Transition::Armed),
                    );
                }
                if sample.from.y < above_line || sample.to.y < above_line {
                    self.above = Some((sample.output, sample.now_ms));
                }
                // A segment that ends above the gate is leaving it upward, not entering it.
                if sample.button_held || gate.contains(sample.from) || sample.to.y < gate.y {
                    return None;
                }
                let recently_above = self.above.is_some_and(|(output, at)| {
                    output == sample.output && sample.now_ms.saturating_sub(at) <= ABOVE_MEMORY_MS
                });
                if !recently_above {
                    return None;
                }
                let entry = segment_entry(gate, sample.from, sample.to)?;
                if !may_arm(&self.config) {
                    return None;
                }
                self.phase = Phase::Armed {
                    entry,
                    since: sample.now_ms,
                    output: sample.output,
                };
                Some(
                    self.commit(entry, sample.to, sample.output)
                        .unwrap_or(Transition::Armed),
                )
            }
        }
    }

    fn note_bottom_motion(&mut self, sample: Sample) -> bool {
        let bottom_row = Area {
            y: sample.screen.bottom() - BOTTOM_ROW_HEIGHT.min(sample.screen.height),
            height: BOTTOM_ROW_HEIGHT.min(sample.screen.height),
            ..sample.screen
        };
        let at_bottom = bottom_row.contains(sample.from) && bottom_row.contains(sample.to);
        if !at_bottom
            || self
                .last_motion_ms
                .is_none_or(|at| sample.now_ms.saturating_sub(at) >= BOTTOM_REARM_MS)
        {
            self.bottom_blocked = false;
        }
        self.last_motion_ms = Some(sample.now_ms);
        if sample.button_held {
            self.bottom_blocked = true;
        }
        at_bottom
    }

    fn commit(&mut self, entry: Pos, to: Pos, output: usize) -> Option<Transition> {
        let travel = to.x - entry.x;
        if travel.abs() < COMMIT_DISTANCE {
            return None;
        }
        let side = if travel < 0.0 {
            Side::Left
        } else {
            Side::Right
        };
        self.phase = Phase::Open { output, side };
        Some(Transition::Opened(side))
    }
}

/// Where the segment `from` → `to` first touches `area`, if it does (Liang–Barsky clipping). A
/// fixed four-edge loop over a stack array — no allocation, constant time.
fn segment_entry(area: Area, from: Pos, to: Pos) -> Option<Pos> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let mut enter = 0.0_f64;
    let mut leave = 1.0_f64;
    for (p, q) in [
        (-dx, from.x - area.x),
        (dx, area.right() - from.x),
        (-dy, from.y - area.y),
        (dy, area.bottom() - from.y),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
            continue;
        }
        let t = q / p;
        if p < 0.0 {
            if t > leave {
                return None;
            }
            enter = enter.max(t);
        } else {
            if t < enter {
                return None;
            }
            leave = leave.min(t);
        }
    }
    Some(Pos {
        x: from.x + enter * dx,
        y: from.y + enter * dy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Area = Area {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };
    // The default gate on that screen: x 840..1080, y 984..1080.
    const LEFT: f64 = 840.0;
    const RIGHT: f64 = 1080.0;
    const TOP: f64 = 984.0;
    const CENTRE: f64 = 960.0;

    fn enabled() -> GestureGate {
        let mut gate = GestureGate::default();
        gate.configure(GateConfig {
            enabled: true,
            ..GateConfig::default()
        });
        gate
    }

    fn at(x: f64, y: f64) -> Pos {
        Pos { x, y }
    }

    /// Drives a gate along a polyline, one report per point, 8 ms apart.
    struct Pointer {
        gate: GestureGate,
        pos: Pos,
        now: u64,
        button: bool,
        output: usize,
    }

    impl Pointer {
        fn new(start: Pos) -> Self {
            Self {
                gate: enabled(),
                pos: start,
                now: 10_000,
                button: false,
                output: 0,
            }
        }

        fn to(&mut self, next: Pos) -> Option<Transition> {
            self.now += 8;
            let sample = Sample {
                output: self.output,
                screen: SCREEN,
                from: self.pos,
                to: next,
                now_ms: self.now,
                button_held: self.button,
            };
            self.pos = next;
            self.gate.feed(sample, |_| true)
        }

        /// Walk in `steps` equal steps to `target`, collecting every transition.
        fn walk(&mut self, target: Pos, steps: u32) -> Vec<Transition> {
            let start = self.pos;
            (1..=steps)
                .filter_map(|step| {
                    let t = f64::from(step) / f64::from(steps);
                    self.to(at(
                        start.x + (target.x - start.x) * t,
                        start.y + (target.y - start.y) * t,
                    ))
                })
                .collect()
        }
    }

    #[test]
    fn slow_straight_down_then_left_opens_left() {
        let mut pointer = Pointer::new(at(CENTRE, 800.0));
        assert_eq!(pointer.walk(at(CENTRE, 1030.0), 115), [Transition::Armed]);
        assert_eq!(
            pointer.walk(at(CENTRE - 40.0, 1030.0), 20),
            [Transition::Opened(Side::Left)]
        );
        assert!(matches!(
            pointer.gate.phase(),
            Phase::Open {
                side: Side::Left,
                ..
            }
        ));
    }

    #[test]
    fn down_then_right_opens_right() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        assert_eq!(
            pointer.walk(at(CENTRE + 5.0, 1060.0), 20),
            [Transition::Armed]
        );
        assert_eq!(
            pointer.walk(at(CENTRE + 60.0, 1075.0), 10),
            [Transition::Opened(Side::Right)]
        );
    }

    #[test]
    fn a_diagonal_through_a_side_edge_from_above_opens() {
        // Above and to the left of the gate, then down past its left edge into it.
        let mut pointer = Pointer::new(at(LEFT - 60.0, TOP - 30.0));
        assert_eq!(
            pointer.to(at(LEFT - 10.0, TOP + 20.0)),
            None,
            "beside, not in"
        );
        assert_eq!(
            pointer.to(at(LEFT + 5.0, TOP + 35.0)),
            Some(Transition::Armed)
        );
        assert_eq!(
            pointer.to(at(LEFT + 25.0, TOP + 50.0)),
            Some(Transition::Opened(Side::Right))
        );
    }

    #[test]
    fn one_fast_segment_jumping_past_the_gate_opens() {
        let mut pointer = Pointer::new(at(LEFT - 100.0, TOP - 100.0));
        // Straight across the gate to the bottom-right, beyond its far edge, in one report.
        assert_eq!(
            pointer.to(at(RIGHT + 200.0, 1079.0)),
            Some(Transition::Opened(Side::Right))
        );
    }

    #[test]
    fn a_horizontal_crossing_low_on_the_screen_never_arms() {
        let mut pointer = Pointer::new(at(100.0, 1050.0));
        assert!(pointer.walk(at(1800.0, 1050.0), 200).is_empty());
        assert!(
            pointer.walk(at(100.0, 1050.0), 1).is_empty(),
            "nor one jump"
        );
        assert_eq!(pointer.gate.phase(), Phase::Idle);
    }

    #[test]
    fn starting_inside_and_moving_sideways_never_opens() {
        let mut pointer = Pointer::new(at(CENTRE, 1040.0));
        assert!(pointer.walk(at(LEFT + 2.0, 1040.0), 30).is_empty());
        assert!(pointer.walk(at(RIGHT - 2.0, 1060.0), 60).is_empty());
        assert_eq!(pointer.gate.phase(), Phase::Idle);
    }

    #[test]
    fn horizontal_bottom_row_swipes_open_in_their_direction() {
        // Both routes also work far outside the central gate.
        for (start, end, side) in [(1600.0, 1500.0, Side::Left), (200.0, 300.0, Side::Right)] {
            let mut pointer = Pointer::new(at(start, SCREEN.bottom() - 1.0));
            assert_eq!(
                pointer.walk(at(end, SCREEN.bottom() - 1.0), 20),
                [Transition::Armed, Transition::Opened(side)]
            );
        }
    }

    #[test]
    fn bottom_row_jitter_and_a_stationary_pointer_do_not_open() {
        let mut pointer = Pointer::new(at(CENTRE, SCREEN.bottom() - 1.0));
        assert_eq!(pointer.to(pointer.pos), None);
        assert_eq!(
            pointer.to(at(CENTRE + 3.0, pointer.pos.y)),
            Some(Transition::Armed)
        );
        assert_eq!(pointer.to(at(CENTRE - 3.0, pointer.pos.y)), None);
        assert_eq!(pointer.to(at(CENTRE, pointer.pos.y)), None);
        assert!(matches!(pointer.gate.phase(), Phase::Armed { .. }));
    }

    #[test]
    fn closing_at_bottom_requires_a_pause_or_leaving_the_row() {
        let y = SCREEN.bottom() - 1.0;
        let mut pointer = Pointer::new(at(CENTRE, y));
        assert_eq!(
            pointer.to(at(CENTRE - 30.0, y)),
            Some(Transition::Opened(Side::Left))
        );
        pointer.gate.close();
        assert!(pointer.walk(at(CENTRE + 100.0, y), 20).is_empty());
        pointer.now += BOTTOM_REARM_MS;
        assert_eq!(
            pointer.to(at(CENTRE + 130.0, y)),
            Some(Transition::Opened(Side::Right))
        );
        pointer.gate.close();
        pointer.to(at(CENTRE, y - BOTTOM_ROW_HEIGHT));
        pointer.to(at(CENTRE, y));
        assert_eq!(
            pointer.to(at(CENTRE - 30.0, y)),
            Some(Transition::Opened(Side::Left))
        );
    }

    #[test]
    fn bottom_row_swipes_respect_buttons_exceptions_and_output_coordinates() {
        let mut pointer = Pointer::new(at(CENTRE, SCREEN.bottom() - 1.0));
        pointer.button = true;
        assert!(
            pointer
                .walk(at(CENTRE - 80.0, pointer.pos.y), 20)
                .is_empty()
        );
        pointer.button = false;
        assert!(
            pointer
                .walk(at(CENTRE + 80.0, pointer.pos.y), 20)
                .is_empty()
        );
        let mut gate = enabled();
        let screen = Area {
            x: -1920.0,
            y: 500.0,
            ..SCREEN
        };
        let sample = Sample {
            output: 1,
            screen,
            from: at(-900.0, screen.bottom() - 1.0),
            to: at(-950.0, screen.bottom() - 1.0),
            now_ms: 1,
            button_held: false,
        };
        assert_eq!(gate.feed(sample, |_| false), None);
        assert_eq!(gate.phase(), Phase::Idle);
        assert_eq!(
            gate.feed(sample, |_| true),
            Some(Transition::Opened(Side::Left))
        );
    }

    #[test]
    fn a_button_held_while_entering_never_arms() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        pointer.button = true;
        assert!(pointer.walk(at(CENTRE, 1040.0), 20).is_empty());
        // Letting go inside does not turn the drag into an approach.
        pointer.button = false;
        assert!(pointer.walk(at(CENTRE - 80.0, 1040.0), 20).is_empty());
        assert_eq!(pointer.gate.phase(), Phase::Idle);
    }

    #[test]
    fn a_button_pressed_while_armed_disarms() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        assert_eq!(pointer.walk(at(CENTRE, 1040.0), 20), [Transition::Armed]);
        pointer.button = true;
        assert_eq!(
            pointer.to(at(CENTRE, 1041.0)),
            Some(Transition::Disarmed(DisarmReason::Button))
        );
    }

    #[test]
    fn reversing_after_open_never_switches_side() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        pointer.walk(at(CENTRE, 1040.0), 20);
        assert_eq!(
            pointer.walk(at(CENTRE - 30.0, 1040.0), 5),
            [Transition::Opened(Side::Left)]
        );
        assert!(pointer.walk(at(CENTRE + 300.0, 1040.0), 30).is_empty());
        assert!(matches!(
            pointer.gate.phase(),
            Phase::Open {
                side: Side::Left,
                ..
            }
        ));
    }

    #[test]
    fn after_close_the_pointer_must_go_above_before_arming_again() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        pointer.walk(at(CENTRE, 1040.0), 20);
        pointer.walk(at(CENTRE + 30.0, 1040.0), 5);
        pointer.gate.close();
        // Resting at the bottom and wandering in and out of the gate sideways: nothing.
        assert!(pointer.walk(at(RIGHT + 100.0, 1040.0), 20).is_empty());
        assert!(pointer.walk(at(CENTRE, 1040.0), 20).is_empty());
        assert!(pointer.walk(at(LEFT - 100.0, 1070.0), 20).is_empty());
        // Up out of it, then back down: a fresh approach.
        pointer.walk(at(LEFT - 100.0, TOP - 50.0), 10);
        pointer.walk(at(CENTRE, TOP - 50.0), 10);
        assert_eq!(pointer.walk(at(CENTRE, 1040.0), 10), [Transition::Armed]);
    }

    #[test]
    fn a_stale_arm_expires() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        assert_eq!(pointer.walk(at(CENTRE, 1040.0), 20), [Transition::Armed]);
        pointer.now += ARM_TIMEOUT_MS;
        assert_eq!(
            pointer.to(at(CENTRE + 1.0, 1040.0)),
            Some(Transition::Disarmed(DisarmReason::Stale))
        );
        // And it needs a new approach: going sideways now opens nothing.
        assert!(pointer.walk(at(CENTRE + 100.0, 1040.0), 10).is_empty());
    }

    #[test]
    fn rising_out_of_the_top_disarms_and_can_come_straight_back() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        pointer.walk(at(CENTRE, 1000.0), 20);
        assert_eq!(
            pointer.to(at(CENTRE, TOP - RISE_CANCEL - 1.0)),
            Some(Transition::Disarmed(DisarmReason::Rose))
        );
        assert_eq!(pointer.walk(at(CENTRE, 1000.0), 5), [Transition::Armed]);
    }

    #[test]
    fn coming_from_above_expires_while_resting_beside_the_gate() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        // Down beside the gate, then a long rest there before sliding in sideways.
        assert_eq!(pointer.to(at(LEFT - 50.0, 900.0)), None);
        assert_eq!(pointer.to(at(LEFT - 50.0, 1040.0)), None);
        pointer.now += ABOVE_MEMORY_MS;
        assert_eq!(pointer.to(at(LEFT + 10.0, 1040.0)), None);
        assert_eq!(pointer.gate.phase(), Phase::Idle);
    }

    #[test]
    fn moving_up_through_the_gate_arms_nothing() {
        let mut pointer = Pointer::new(at(LEFT - 50.0, 1060.0));
        pointer.walk(at(LEFT - 50.0, 900.0), 10);
        // Recently above, but now crossing the gate upward in one segment and in small steps.
        pointer.pos = at(LEFT - 20.0, 1070.0);
        assert!(pointer.walk(at(RIGHT + 20.0, 900.0), 1).is_empty());
        pointer.pos = at(CENTRE, 1079.0);
        let transitions = pointer.walk(at(CENTRE, 900.0), 40);
        assert!(!transitions.contains(&Transition::Opened(Side::Left)));
        assert!(!transitions.contains(&Transition::Opened(Side::Right)));
    }

    #[test]
    fn another_output_disarms() {
        let mut pointer = Pointer::new(at(CENTRE, 900.0));
        pointer.walk(at(CENTRE, 1000.0), 20);
        pointer.output = 1;
        assert_eq!(
            pointer.to(at(CENTRE, 1000.0)),
            Some(Transition::Disarmed(DisarmReason::OutputChanged))
        );
    }

    #[test]
    fn an_exception_refuses_the_arm_and_a_disabled_gate_sees_nothing() {
        let mut gate = enabled();
        let sample = Sample {
            output: 0,
            screen: SCREEN,
            from: at(CENTRE, 900.0),
            to: at(CENTRE, 1000.0),
            now_ms: 1,
            button_held: false,
        };
        assert_eq!(gate.feed(sample, |_| false), None);
        assert_eq!(gate.phase(), Phase::Idle);
        gate.configure(GateConfig::default());
        assert_eq!(gate.feed(sample, |_| true), None);
    }

    #[test]
    fn the_gate_is_centred_on_the_bottom_edge_and_clamped_to_the_screen() {
        let gate = enabled();
        assert_eq!(
            gate.area(SCREEN),
            Area {
                x: LEFT,
                y: TOP,
                width: 240.0,
                height: 96.0
            }
        );
        let small = Area {
            x: 1920.0,
            y: 0.0,
            width: 200.0,
            height: 50.0,
        };
        assert_eq!(gate.area(small), small);
    }
}
