//! Geometry primitives and the recovery invariant.
//!
//! Every normal window must keep a compositor-owned **recovery band** (its top strip) visible on
//! at least one active output, after every drag, output change, scale change, resume and
//! application geometry request. All coordinates are global compositor-logical coordinates.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width.max(0))
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height.max(0))
    }

    pub fn center(self) -> Point {
        Point {
            x: self.x.saturating_add(self.width / 2),
            y: self.y.saturating_add(self.height / 2),
        }
    }

    pub fn intersection(self, other: Rect) -> Option<Rect> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());

        (right > left && bottom > top).then_some(Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }

    pub fn intersection_area(self, other: Rect) -> i64 {
        self.intersection(other)
            .map_or(0, |rect| i64::from(rect.width) * i64::from(rect.height))
    }
}

/// One active output in the single continuous scene.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub name: String,

    /// Global compositor-logical coordinates, not physical pixels.
    pub logical: Rect,

    /// Informational. Dragging remains in logical coordinates; scale is applied at rendering
    /// and input-conversion boundaries only.
    pub scale_milli: u32,
}

/// Minimum visible width of the recovery band.
pub const MIN_RECOVERY_WIDTH: i32 = 96;
/// Height of the compositor-owned recovery band at the top of a window.
pub const RECOVERY_BAND_HEIGHT: i32 = 32;

/// The compositor-owned strip through which a window can always be grabbed.
pub fn recovery_band(window: Rect) -> Rect {
    Rect {
        x: window.x,
        y: window.y,
        width: window.width.max(1),
        height: window.height.clamp(1, RECOVERY_BAND_HEIGHT),
    }
}

/// True when enough of the recovery band is visible on some output.
pub fn is_recoverable(window: Rect, outputs: &[Output]) -> bool {
    let band = recovery_band(window);
    let required_width = window.width.clamp(1, MIN_RECOVERY_WIDTH);

    outputs.iter().any(|output| {
        band.intersection(output.logical)
            .is_some_and(|visible| visible.width >= required_width && visible.height >= band.height)
    })
}

/// Move `window` the minimum distance needed to satisfy [`is_recoverable`]. Size is preserved.
/// With no outputs the window is returned unchanged.
pub fn repair_window(window: Rect, outputs: &[Output]) -> Rect {
    if outputs.is_empty() || is_recoverable(window, outputs) {
        return window;
    }

    let target = choose_target_output(window, outputs);
    let output = target.logical;

    let visible_width = window
        .width
        .clamp(1, MIN_RECOVERY_WIDTH)
        .min(output.width.max(1));
    let visible_height = window
        .height
        .clamp(1, RECOVERY_BAND_HEIGHT)
        .min(output.height.max(1));

    // Permit most of a large window to remain outside the output, but guarantee that a
    // compositor-owned recovery band remains reachable.
    let minimum_x = output
        .x
        .saturating_sub(window.width.saturating_sub(visible_width));
    let maximum_x = output.right().saturating_sub(visible_width);
    let minimum_y = output.y;
    let maximum_y = output.bottom().saturating_sub(visible_height);

    Rect {
        x: clamp(window.x, minimum_x, maximum_x),
        y: clamp(window.y, minimum_y, maximum_y),
        ..window
    }
}

fn choose_target_output(window: Rect, outputs: &[Output]) -> &Output {
    let most_intersection = outputs
        .iter()
        .max_by_key(|output| window.intersection_area(output.logical))
        .expect("outputs is non-empty");

    if window.intersection_area(most_intersection.logical) > 0 {
        return most_intersection;
    }

    let center = window.center();
    outputs
        .iter()
        .min_by_key(|output| distance_squared_to_rect(center, output.logical))
        .expect("outputs is non-empty")
}

fn distance_squared_to_rect(point: Point, rect: Rect) -> i64 {
    let dx = axis_distance(point.x, rect.x, rect.right());
    let dy = axis_distance(point.y, rect.y, rect.bottom());
    dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
}

fn axis_distance(value: i32, minimum: i32, maximum: i32) -> i64 {
    if value < minimum {
        i64::from(minimum - value)
    } else if value > maximum {
        i64::from(value - maximum)
    } else {
        0
    }
}

fn clamp(value: i32, minimum: i32, maximum: i32) -> i32 {
    if minimum > maximum {
        minimum
    } else {
        value.clamp(minimum, maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn edp() -> Vec<Output> {
        vec![Output {
            name: "eDP-1".to_owned(),
            logical: Rect::new(0, 0, 1920, 1080),
            scale_milli: 1000,
        }]
    }

    #[test]
    fn offscreen_window_is_recovered() {
        let lost = Rect::new(4000, 3000, 1200, 900);
        let repaired = repair_window(lost, &edp());
        assert!(is_recoverable(repaired, &edp()));
        assert_eq!((repaired.width, repaired.height), (1200, 900));
    }

    #[test]
    fn visible_window_is_not_moved() {
        let outputs = vec![Output {
            name: "eDP-1".to_owned(),
            logical: Rect::new(0, 0, 1920, 1080),
            scale_milli: 1250,
        }];
        let original = Rect::new(100, 100, 900, 700);
        assert_eq!(repair_window(original, &outputs), original);
    }

    #[test]
    fn removing_an_output_migrates_to_the_nearest_remaining() {
        let two = vec![
            Output {
                name: "eDP-1".into(),
                logical: Rect::new(0, 0, 1920, 1080),
                scale_milli: 1000,
            },
            Output {
                name: "HDMI-1".into(),
                logical: Rect::new(1920, 0, 2560, 1440),
                scale_milli: 1000,
            },
        ];
        let on_external = Rect::new(2500, 300, 800, 600);
        assert!(is_recoverable(on_external, &two));

        let one = vec![two[0].clone()];
        assert!(!is_recoverable(on_external, &one));
        let repaired = repair_window(on_external, &one);
        assert!(is_recoverable(repaired, &one));
        // Nearest edge: it should land at the right edge of eDP-1, not jump to the origin.
        assert!(repaired.x > 1000);
    }

    #[test]
    fn no_outputs_leaves_geometry_alone() {
        let w = Rect::new(-500, -500, 100, 100);
        assert_eq!(repair_window(w, &[]), w);
    }

    fn arb_output() -> impl Strategy<Value = Output> {
        (
            -8000i32..8000,
            -8000i32..8000,
            200i32..7680,
            200i32..4320,
            prop_oneof![Just(1000u32), Just(1250), Just(1500), Just(2000)],
        )
            .prop_map(|(x, y, w, h, scale)| Output {
                name: String::from("out"),
                logical: Rect::new(x, y, w, h),
                scale_milli: scale,
            })
    }

    fn arb_window() -> impl Strategy<Value = Rect> {
        (-20000i32..20000, -20000i32..20000, 1i32..6000, 1i32..4000)
            .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// The core invariant: after repair, every window is recoverable on the current topology,
        /// its size is untouched, and repair is idempotent.
        #[test]
        fn repair_makes_every_window_recoverable(
            window in arb_window(),
            outputs in prop::collection::vec(arb_output(), 1..4),
        ) {
            let repaired = repair_window(window, &outputs);
            prop_assert!(is_recoverable(repaired, &outputs), "window {window:?} → {repaired:?} not recoverable on {outputs:?}");
            prop_assert_eq!((repaired.width, repaired.height), (window.width, window.height));
            prop_assert_eq!(repair_window(repaired, &outputs), repaired);
        }

        /// A window that is already recoverable is never moved.
        #[test]
        fn recoverable_windows_are_left_alone(
            window in arb_window(),
            outputs in prop::collection::vec(arb_output(), 1..4),
        ) {
            if is_recoverable(window, &outputs) {
                prop_assert_eq!(repair_window(window, &outputs), window);
            }
        }
    }
}
