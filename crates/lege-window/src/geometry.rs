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

    /// What is left of [`Output::logical`] once shell surfaces that reserve space — a dock, a
    /// panel — have taken theirs. `None` means the whole output, which is the state of every
    /// output until a shell claims an edge.
    ///
    /// Read it through [`Output::usable`], never directly: the raw value is what a shell asked
    /// for, and a shell does not get the last word on whether a window can be reached.
    #[serde(default)]
    pub work_area: Option<Rect>,
}

impl Output {
    /// An output with no reserved space.
    pub fn new(name: impl Into<String>, logical: Rect, scale_milli: u32) -> Self {
        Self {
            name: name.into(),
            logical,
            scale_milli,
            work_area: None,
        }
    }

    /// The same output with `work_area` set — normally computed by [`work_area_for`].
    #[must_use]
    pub fn with_work_area(mut self, work_area: Rect) -> Self {
        self.work_area = Some(work_area);
        self
    }

    /// Where windows may actually be placed.
    ///
    /// **A shell surface must never be able to make windows unreachable.** The shell is a
    /// restartable, replaceable process; if a buggy or hostile one claims an exclusive zone that
    /// swallows the output — or one so large that no recovery band could fit in what remains —
    /// the claim is ignored and the full output is returned. Reserving space is a request, and
    /// like every other client request in this system it is checked rather than obeyed.
    pub fn usable(&self) -> Rect {
        let Some(requested) = self.work_area else {
            return self.logical;
        };
        match requested.intersection(self.logical) {
            Some(area)
                if area.width >= MIN_RECOVERY_WIDTH && area.height >= RECOVERY_BAND_HEIGHT =>
            {
                area
            }
            _ => self.logical,
        }
    }
}

/// Which side of an output a shell surface reserves space against.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// One shell surface's claim on an output's space: a dock of `size` logical pixels along `edge`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Exclusive {
    pub edge: Edge,
    pub size: i32,
}

/// Shrink `logical` by every zone, in the order given.
///
/// Kept here rather than in the compositor because it is geometry, and geometry in this project
/// is tested without a compositor. Zones are applied cumulatively, so two docks on the same edge
/// stack rather than overlap; a negative size is treated as zero.
pub fn work_area_for(logical: Rect, zones: &[Exclusive]) -> Rect {
    zones.iter().fold(logical, |area, zone| {
        let size = zone.size.max(0);
        match zone.edge {
            Edge::Top => Rect {
                y: area.y.saturating_add(size),
                height: area.height.saturating_sub(size),
                ..area
            },
            Edge::Bottom => Rect {
                height: area.height.saturating_sub(size),
                ..area
            },
            Edge::Left => Rect {
                x: area.x.saturating_add(size),
                width: area.width.saturating_sub(size),
                ..area
            },
            Edge::Right => Rect {
                width: area.width.saturating_sub(size),
                ..area
            },
        }
    })
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

/// The compositor's grab affordance for one window: the recovery band, plus whether the
/// compositor must actively paint a temporary tab over it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryAffordance {
    pub band: Rect,
    /// True for client-side-decorated and borderless windows: the client controls that screen
    /// area and may draw over it, so the compositor cannot rely on a client-drawn title bar to
    /// make the window recoverable and must paint its own temporary tab.
    pub requires_compositor_paint: bool,
}

/// Compute the affordance for a window at `geometry`. `client_owns_band` is
/// `Decoration::needs_compositor_recovery_tab()` from `service.rs`, passed in rather than the
/// enum itself so this module stays free of window-record concerns.
pub fn recovery_affordance(geometry: Rect, client_owns_band: bool) -> RecoveryAffordance {
    RecoveryAffordance {
        band: recovery_band(geometry),
        requires_compositor_paint: client_owns_band,
    }
}

/// True when enough of the recovery band is visible on some output's usable area.
///
/// Against [`Output::usable`], not [`Output::logical`]: a band hidden underneath a dock is not
/// something the user can grab, so counting it would make "recoverable" mean "drawn somewhere"
/// rather than "reachable".
pub fn is_recoverable(window: Rect, outputs: &[Output]) -> bool {
    let band = recovery_band(window);
    let required_width = window.width.clamp(1, MIN_RECOVERY_WIDTH);

    outputs.iter().any(|output| {
        band.intersection(output.usable())
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
    let output = target.usable();

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

/// Reposition `window` to be as fully visible as possible on the nearest active output. Unlike
/// [`repair_window`], this does not settle for the minimal recovery band: when the window fits
/// within the target output it ends up entirely on-screen. Size is always preserved. Used by
/// `WindowCommand::MoveToVisibleArea`.
pub fn move_into_view(window: Rect, outputs: &[Output]) -> Rect {
    if outputs.is_empty() {
        return window;
    }

    let output = choose_target_output(window, outputs).usable();

    let minimum_x = output.x;
    let maximum_x = output.right().saturating_sub(window.width.max(1));
    let minimum_y = output.y;
    let maximum_y = output.bottom().saturating_sub(window.height.max(1));

    Rect {
        x: clamp(window.x, minimum_x, maximum_x),
        y: clamp(window.y, minimum_y, maximum_y),
        ..window
    }
}

/// Lay out `windows` across `outputs` as a grid per output, without consulting the shell. Used
/// by the compositor-owned "show every window" recovery view (planning doc 01 §3), which must
/// work even when the shell process is dead. Every returned rectangle is fully contained in the
/// output it was assigned to, so a window can never be scattered somewhere it cannot be seen.
///
/// Outputs are filled round-robin in the order given; within an output, windows tile in reading
/// order (row-major) at their original size, shrunk only as much as needed to fit their cell.
/// With no outputs, nothing can be shown anywhere and the input is returned unchanged.
pub fn scatter_layout<Id: Copy>(windows: &[(Id, Rect)], outputs: &[Output]) -> Vec<(Id, Rect)> {
    if outputs.is_empty() || windows.is_empty() {
        return windows.to_vec();
    }

    let mut per_output: Vec<Vec<(Id, Rect)>> = vec![Vec::new(); outputs.len()];
    for (index, window) in windows.iter().enumerate() {
        per_output[index % outputs.len()].push(*window);
    }

    let mut placed = Vec::with_capacity(windows.len());
    for (output, assigned) in outputs.iter().zip(per_output) {
        let area = output.usable();
        let count = assigned.len();
        if count == 0 {
            continue;
        }

        let columns = (count as f64).sqrt().ceil().max(1.0) as i32;
        let rows = (i32::try_from(count).unwrap_or(i32::MAX) + columns - 1) / columns;
        let rows = rows.max(1);
        let cell_width = (area.width.max(1) / columns.max(1)).max(1);
        let cell_height = (area.height.max(1) / rows.max(1)).max(1);

        for (index, (id, original)) in assigned.into_iter().enumerate() {
            let column = i32::try_from(index).unwrap_or(i32::MAX) % columns;
            let row = i32::try_from(index).unwrap_or(i32::MAX) / columns;

            let width = original.width.max(1).min(cell_width);
            let height = original.height.max(1).min(cell_height);
            let cell_x = area.x.saturating_add(column.saturating_mul(cell_width));
            let cell_y = area.y.saturating_add(row.saturating_mul(cell_height));

            placed.push((
                id,
                Rect {
                    x: cell_x.saturating_add((cell_width - width) / 2),
                    y: cell_y.saturating_add((cell_height - height) / 2),
                    width,
                    height,
                },
            ));
        }
    }

    placed
}

/// A point in one output's own physical pixel space (its framebuffer), as opposed to the
/// global logical scene [`Point`] lives in.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

/// Convert a point in global logical coordinates to `output`'s physical pixel space.
///
/// This, and [`physical_to_logical`], are the *only* places `scale_milli` should ever be
/// applied to a coordinate. Persistent state — window geometry, the drag anchor — stays
/// logical, so a drag crossing outputs of different scale never re-derives its anchor through a
/// lossy physical round-trip (planning doc 02 §6).
pub fn logical_to_physical(point: Point, output: &Output) -> PhysicalPoint {
    PhysicalPoint {
        x: scale_round(
            i64::from(point.x) - i64::from(output.logical.x),
            i64::from(output.scale_milli),
        ),
        y: scale_round(
            i64::from(point.y) - i64::from(output.logical.y),
            i64::from(output.scale_milli),
        ),
    }
}

/// Inverse of [`logical_to_physical`].
pub fn physical_to_logical(point: PhysicalPoint, output: &Output) -> Point {
    Point {
        x: output.logical.x.saturating_add(unscale_round(
            i64::from(point.x),
            i64::from(output.scale_milli),
        )),
        y: output.logical.y.saturating_add(unscale_round(
            i64::from(point.y),
            i64::from(output.scale_milli),
        )),
    }
}

/// `offset * scale_milli / 1000`, rounded half-away-from-zero and saturated to `i32`. Using one
/// fixed rounding rule in both directions is what keeps repeated conversions from drifting: see
/// the `physical_logical_round_trip_stabilises` property test.
fn scale_round(offset: i64, scale_milli: i64) -> i32 {
    div_round(offset.saturating_mul(scale_milli), 1000)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// `offset * 1000 / scale_milli`, saturated to `i32`. Zero scale (not physically meaningful)
/// maps to zero rather than panicking.
fn unscale_round(offset: i64, scale_milli: i64) -> i32 {
    if scale_milli == 0 {
        return 0;
    }
    div_round(offset.saturating_mul(1000), scale_milli)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// Integer division rounded half-away-from-zero, instead of `/`'s truncation-toward-zero.
fn div_round(numerator: i64, denominator: i64) -> i64 {
    if denominator == 0 {
        return 0;
    }
    let half = denominator / 2;
    if (numerator < 0) == (denominator < 0) {
        (numerator + half) / denominator
    } else {
        (numerator - half) / denominator
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
        vec![Output::new(
            "eDP-1".to_owned(),
            Rect::new(0, 0, 1920, 1080),
            1000,
        )]
    }

    #[test]
    fn offscreen_window_is_recovered() {
        let lost = Rect::new(4000, 3000, 1200, 900);
        let repaired = repair_window(lost, &edp());
        assert!(is_recoverable(repaired, &edp()));
        assert_eq!((repaired.width, repaired.height), (1200, 900));
    }

    #[test]
    fn exclusive_zones_shrink_the_work_area_and_stack() {
        let screen = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            work_area_for(
                screen,
                &[Exclusive {
                    edge: Edge::Top,
                    size: 40
                }]
            ),
            Rect::new(0, 40, 1920, 1040)
        );
        assert_eq!(
            work_area_for(
                screen,
                &[Exclusive {
                    edge: Edge::Bottom,
                    size: 64
                }]
            ),
            Rect::new(0, 0, 1920, 1016)
        );
        // A panel at the top and a dock at the bottom take from opposite ends.
        assert_eq!(
            work_area_for(
                screen,
                &[
                    Exclusive {
                        edge: Edge::Top,
                        size: 40
                    },
                    Exclusive {
                        edge: Edge::Bottom,
                        size: 64
                    },
                ]
            ),
            Rect::new(0, 40, 1920, 976)
        );
        // Two claims on the same edge stack rather than overlap.
        let stacked = work_area_for(
            screen,
            &[
                Exclusive {
                    edge: Edge::Left,
                    size: 50,
                },
                Exclusive {
                    edge: Edge::Left,
                    size: 30,
                },
            ],
        );
        assert_eq!(stacked, Rect::new(80, 0, 1840, 1080));
        // A negative claim is not a way to grow the work area past the output.
        assert_eq!(
            work_area_for(
                screen,
                &[Exclusive {
                    edge: Edge::Top,
                    size: -500
                }]
            ),
            screen
        );
    }

    #[test]
    fn a_shell_cannot_make_windows_unreachable() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let ordinary = Output::new("eDP-1", screen, 1000).with_work_area(work_area_for(
            screen,
            &[Exclusive {
                edge: Edge::Bottom,
                size: 64,
            }],
        ));
        assert_eq!(ordinary.usable(), Rect::new(0, 0, 1920, 1016));

        // A dock taller than the screen, a dock that leaves a sliver too small for a recovery
        // band, and a work area that has wandered off the output entirely: each claim is
        // refused and the whole output is returned, because a restartable shell process does
        // not get to decide whether the user can reach their windows.
        for hostile in [
            work_area_for(
                screen,
                &[Exclusive {
                    edge: Edge::Top,
                    size: 5000,
                }],
            ),
            work_area_for(
                screen,
                &[Exclusive {
                    edge: Edge::Top,
                    size: 1060,
                }],
            ),
            Rect::new(9000, 9000, 800, 600),
            Rect::new(0, 0, 0, 0),
        ] {
            let output = Output::new("eDP-1", screen, 1000).with_work_area(hostile);
            assert_eq!(
                output.usable(),
                screen,
                "work area {hostile:?} should have been refused"
            );
            let window = Rect::new(400, 300, 800, 600);
            let outputs = [output];
            assert!(is_recoverable(repair_window(window, &outputs), &outputs));
        }
    }

    #[test]
    fn a_window_under_the_dock_is_not_considered_recoverable() {
        let screen = Rect::new(0, 0, 1920, 1080);
        // A panel occupying the top 80 logical pixels.
        let output = Output::new("eDP-1", screen, 1000).with_work_area(work_area_for(
            screen,
            &[Exclusive {
                edge: Edge::Top,
                size: 80,
            }],
        ));
        let outputs = vec![output];

        // A window whose whole recovery band sits beneath the panel: visible, but not grabbable.
        let buried = Rect::new(200, 10, 900, 700);
        assert!(
            !is_recoverable(buried, &outputs),
            "a band hidden under the panel must not count as reachable"
        );

        let repaired = repair_window(buried, &outputs);
        assert!(is_recoverable(repaired, &outputs));
        assert!(
            repaired.y >= 80,
            "repaired to {repaired:?}, still under the panel"
        );
        assert_eq!((repaired.width, repaired.height), (900, 700));
    }

    #[test]
    fn visible_window_is_not_moved() {
        let outputs = vec![Output::new(
            "eDP-1".to_owned(),
            Rect::new(0, 0, 1920, 1080),
            1250,
        )];
        let original = Rect::new(100, 100, 900, 700);
        assert_eq!(repair_window(original, &outputs), original);
    }

    #[test]
    fn removing_an_output_migrates_to_the_nearest_remaining() {
        let two = vec![
            Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("HDMI-1", Rect::new(1920, 0, 2560, 1440), 1000),
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

    use crate::test_support::{arb_output, arb_window, load_topology};

    #[test]
    fn fixture_topologies_repair_to_recoverable() {
        let probe_windows = [
            Rect::new(-5000, -5000, 400, 300),
            Rect::new(20000, 20000, 800, 600),
            Rect::new(100, 100, 400, 300),
            Rect::new(0, 0, 10000, 10000),
        ];
        for name in [
            "laptop-plus-hdmi",
            "triple-mixed-scale",
            "negative-origin-external",
            "overlapping-outputs",
            "disjoint-outputs",
        ] {
            let outputs = load_topology(name);
            for window in probe_windows {
                let repaired = repair_window(window, &outputs);
                assert!(
                    is_recoverable(repaired, &outputs),
                    "fixture {name}: {window:?} -> {repaired:?} not recoverable on {outputs:?}"
                );
            }
        }
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

        /// `move_into_view` always leaves a recoverable window, and fully contains it when it
        /// fits inside the target output.
        #[test]
        fn move_into_view_is_recoverable_and_contains_when_it_fits(
            window in arb_window(),
            outputs in prop::collection::vec(arb_output(), 1..4),
        ) {
            let moved = move_into_view(window, &outputs);
            prop_assert_eq!((moved.width, moved.height), (window.width, window.height));
            prop_assert!(is_recoverable(moved, &outputs));

            let area = choose_target_output(window, &outputs).usable();
            if window.width <= area.width && window.height <= area.height {
                prop_assert_eq!(area.intersection(moved), Some(moved));
            }
        }

        /// Every window `scatter_layout` places lands fully inside the output it was assigned
        /// to: the recovery view can never lose a window off the edge of the screen.
        #[test]
        fn scattered_windows_stay_within_their_output(
            windows in prop::collection::vec(arb_window(), 1..12),
            outputs in prop::collection::vec(arb_output(), 1..4),
        ) {
            let tagged: Vec<(usize, Rect)> = windows.into_iter().enumerate().collect();
            let placed = scatter_layout(&tagged, &outputs);
            prop_assert_eq!(placed.len(), tagged.len());
            for (_, rect) in &placed {
                prop_assert!(outputs.iter().any(|output| output.usable().intersection(*rect) == Some(*rect)));
            }
        }

        /// Converting a point forward then back is exact when the scale divides evenly.
        #[test]
        fn logical_physical_round_trip_exact_at_unit_scale(
            x in -20000i32..20000, y in -20000i32..20000,
            ox in -8000i32..8000, oy in -8000i32..8000,
        ) {
            let output = Output::new("o", Rect::new(ox, oy, 1920, 1080), 1000);
            let point = Point { x, y };
            let physical = logical_to_physical(point, &output);
            prop_assert_eq!(physical_to_logical(physical, &output), point);
        }

        /// The forward/backward conversion never drifts: a second round trip through physical
        /// space reproduces exactly the same physical and logical values as the first, instead
        /// of walking further away each time (the failure mode this boundary exists to prevent).
        #[test]
        fn physical_logical_round_trip_stabilises(
            x in -20000i32..20000, y in -20000i32..20000,
            ox in -8000i32..8000, oy in -8000i32..8000,
            scale in prop_oneof![Just(1000u32), Just(1250), Just(1500), Just(1750), Just(2000)],
        ) {
            let output = Output::new("o", Rect::new(ox, oy, 1920, 1080), scale);
            let l1 = Point { x, y };
            let p1 = logical_to_physical(l1, &output);
            let l2 = physical_to_logical(p1, &output);
            let p2 = logical_to_physical(l2, &output);
            let l3 = physical_to_logical(p2, &output);
            prop_assert_eq!(p1, p2, "physical value must stabilise after one lossy hop");
            prop_assert_eq!(l2, l3, "logical value must stabilise after one lossy hop");
        }

        /// The drag anchor is defined purely in logical coordinates and never touches
        /// `scale_milli`; converting the pointer position through two outputs of different
        /// scale must not change the logical anchor offset a drag was started with.
        #[test]
        fn anchor_offset_is_independent_of_output_scale(
            wx in 0i32..1000, wy in 0i32..1000, ww in 1i32..2000, wh in 1i32..2000,
            px in -20000i32..20000, py in -20000i32..20000,
            scale_a in prop_oneof![Just(1000u32), Just(1500), Just(2000)],
            scale_b in prop_oneof![Just(1000u32), Just(1500), Just(2000)],
        ) {
            let window = Rect::new(wx, wy, ww, wh);
            let pointer = Point { x: px, y: py };
            let anchor = Point { x: pointer.x - window.x, y: pointer.y - window.y };

            let output_a = Output::new("a", Rect::new(0, 0, 1920, 1080), scale_a);
            let output_b = Output::new("b", Rect::new(1920, 0, 2560, 1440), scale_b);

            // Round-tripping the pointer through either output's physical space and back must
            // not change the logical point at all when it is not itself the value being
            // quantised — the anchor is computed and applied entirely in logical space.
            let recovered_a = physical_to_logical(logical_to_physical(pointer, &output_a), &output_a);
            let recovered_b = physical_to_logical(logical_to_physical(pointer, &output_b), &output_b);
            let new_window_a = Rect { x: recovered_a.x - anchor.x, y: recovered_a.y - anchor.y, ..window };
            let new_window_b = Rect { x: recovered_b.x - anchor.x, y: recovered_b.y - anchor.y, ..window };
            prop_assert_eq!(new_window_a, new_window_b, "anchor must place the window identically regardless of which output's scale it round-tripped through");
        }

        /// The recorded three-output mixed-scale desk fixture, exercised with arbitrary window
        /// rectangles rather than the four fixed probes in `fixture_topologies_repair_to_recoverable`.
        #[test]
        fn arbitrary_windows_are_recoverable_on_the_triple_mixed_scale_fixture(window in arb_window()) {
            let outputs = load_topology("triple-mixed-scale");
            prop_assert!(is_recoverable(repair_window(window, &outputs), &outputs));
        }
    }
}
