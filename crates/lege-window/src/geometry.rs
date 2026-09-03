//! Geometry primitives and the recovery invariant.
//!
//! Every normal window must keep a compositor-owned **recovery band** (its top strip) visible on
//! at least one active output, after every drag, output change, scale change, resume and
//! application geometry request. All coordinates are global compositor-logical coordinates.

use crate::layers::Layer;
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

/// Where the compositor should place a shell surface on `output`.
///
/// xdg-shell gives a client no way to position itself, and that is correct rather than a gap: a
/// surface that could place itself anywhere would be a window that could place itself anywhere,
/// and the whole point of this crate is that placement is the compositor's decision. So a dock
/// does not ask to sit at the bottom of the screen — it declares that it *is* a dock reserving
/// the bottom edge, and where that puts it follows.
///
/// * A surface with an [`Exclusive`] claim is placed flush against that edge, spanning the
///   output's full extent along it. It is measured against `logical`, not [`Output::usable`],
///   because the claim is what *creates* the usable area — insetting it by its own reservation
///   would walk it off the screen one frame at a time.
/// * A [`Layer::HomeSurface`] surface with no claim fills the output: it is the backdrop, the
///   thing visible wherever no window is.
/// * Anything else keeps its requested size and is moved wholly inside the usable area, so a
///   launcher or a notification cannot open underneath the dock that reserved space above it.
pub fn shell_surface_placement(
    layer: Layer,
    exclusive: Option<Exclusive>,
    output: &Output,
    requested: Rect,
) -> Rect {
    let full = output.logical;
    if let Some(zone) = exclusive {
        let size = zone.size.max(0);
        return match zone.edge {
            Edge::Top => Rect::new(full.x, full.y, full.width, size.min(full.height)),
            Edge::Bottom => {
                let height = size.min(full.height);
                Rect::new(
                    full.x,
                    full.bottom().saturating_sub(height),
                    full.width,
                    height,
                )
            }
            Edge::Left => Rect::new(full.x, full.y, size.min(full.width), full.height),
            Edge::Right => {
                let width = size.min(full.width);
                Rect::new(
                    full.right().saturating_sub(width),
                    full.y,
                    width,
                    full.height,
                )
            }
        };
    }
    if layer == Layer::HomeSurface {
        return full;
    }
    if layer == Layer::Notification {
        // A toast anchors to the top-right of the usable area: visible over whatever is being
        // worked on, clear of any reserved edge, and where every desktop has taught people to
        // glance. The client still owns only its size.
        let usable = output.usable();
        let margin = 12;
        let width = requested.width.clamp(1, usable.width);
        let height = requested.height.clamp(1, usable.height);
        return Rect::new(
            usable.right() - width - margin.min(usable.width - width),
            usable.y + margin.min(usable.height - height),
            width,
            height,
        );
    }
    move_into_view(requested, std::slice::from_ref(output))
}

/// How far each successive cascaded window is stepped down-right from centre
/// ([`place_new_window`]). One recovery-band height: exactly enough that the previous window's
/// band stays visible above the new window.
pub const PLACEMENT_CASCADE_STEP: i32 = 32;

/// Where a newly mapped client window goes.
///
/// xdg-shell gives a client no way to position itself, so the location of `requested` carries no
/// intent — it is almost always `(0, 0)` — and honouring it verbatim stacked every new window
/// flush into the top-left corner of the first output, each exactly over the last. Placement is
/// the compositor's decision (the same argument as [`shell_surface_placement`]): an
/// origin-located request is centred in the first output's usable area, stepped down-right by
/// [`PLACEMENT_CASCADE_STEP`] per window already open so consecutive windows stay individually
/// visible, wrapping back to centre before the cascade could reach the area's edge. The size is
/// the client's, clamped to the usable area so nothing maps bigger than the screen.
///
/// A request located *away* from the origin is a real position — a restored session window
/// mapping with its saved geometry, or an X11 client that placed itself — and is honoured,
/// repaired through [`repair_window`] like any other geometry.
pub fn place_new_window(requested: Rect, existing_windows: usize, outputs: &[Output]) -> Rect {
    let Some(output) = outputs.first() else {
        return requested;
    };
    if (requested.x, requested.y) != (0, 0) {
        return repair_window(requested, outputs);
    }
    let usable = output.usable();
    let width = requested.width.clamp(1, usable.width.max(1));
    let height = requested.height.clamp(1, usable.height.max(1));
    let centered_x = usable.x + (usable.width - width) / 2;
    let centered_y = usable.y + (usable.height - height) / 2;
    // Room below-right of centre before the window's far edge would leave the usable area; the
    // cascade cycles within it rather than clamping, so window N+k never buries window N in the
    // corner.
    let room = (usable.right() - width - centered_x).min(usable.bottom() - height - centered_y);
    let wrap = usize::try_from(room / PLACEMENT_CASCADE_STEP).unwrap_or(0);
    let steps = i32::try_from(existing_windows % (wrap + 1)).unwrap_or(0);
    let placed = Rect::new(
        centered_x + steps * PLACEMENT_CASCADE_STEP,
        centered_y + steps * PLACEMENT_CASCADE_STEP,
        width,
        height,
    );
    repair_window(placed, outputs)
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

/// Side of the compositor-owned resize grip's square, in logical pixels. Deliberately the same
/// order of size as [`RECOVERY_BAND_HEIGHT`]: the two are the compositor's whole management
/// vocabulary for a window that draws no frame of its own, and a grip smaller than the band it
/// pairs with would be the harder of the two to hit.
pub const RESIZE_GRIP_SIZE: i32 = 24;

/// The smallest a window may be resized to. Not an arbitrary floor: it is exactly the size at
/// which the recovery band is still grabbable ([`MIN_RECOVERY_WIDTH`]) *and* there is still room
/// for the resize grip underneath it ([`RECOVERY_BAND_HEIGHT`] + [`RESIZE_GRIP_SIZE`]). A window
/// shrunk to this size can therefore still be moved and still be grown again, which is the
/// property that makes an interactive resize safe to offer at all.
pub const MIN_WINDOW_WIDTH: i32 = MIN_RECOVERY_WIDTH;
/// See [`MIN_WINDOW_WIDTH`].
pub const MIN_WINDOW_HEIGHT: i32 = RECOVERY_BAND_HEIGHT + RESIZE_GRIP_SIZE;

/// The compositor-owned corner through which any window can be resized, whoever draws its frame
/// — the resize counterpart to [`recovery_band`]. Placed at the bottom-right, never overlapping
/// the band, so a press can only ever mean one of the two gestures.
///
/// `None` when the window is too short for a grip to fit below its band. That cannot happen to a
/// window this module resized ([`MIN_WINDOW_HEIGHT`] leaves exactly enough room); it can happen
/// to one a client sized itself, and the honest answer there is to offer no grip rather than one
/// that steals the band.
pub fn resize_grip(window: Rect) -> Option<Rect> {
    let width = RESIZE_GRIP_SIZE.min(window.width);
    let height = RESIZE_GRIP_SIZE.min(window.height.saturating_sub(RECOVERY_BAND_HEIGHT));
    (width > 0 && height > 0).then(|| Rect {
        x: window.right().saturating_sub(width),
        y: window.bottom().saturating_sub(height),
        width,
        height,
    })
}

/// The rectangle a framed window's client surface occupies: everything under the recovery
/// band. The band is the compositor's, drawn *above* the client rather than over it — a band
/// painted over the top of a client's own pixels hid the first line of every terminal and the
/// close button of every client-decorated window.
///
/// Never empty: a frame shorter than the band still yields a one-pixel client area, so a caller
/// can always configure it.
pub fn client_area(frame: Rect) -> Rect {
    Rect {
        x: frame.x,
        y: frame.y.saturating_add(RECOVERY_BAND_HEIGHT),
        width: frame.width.max(1),
        height: frame.height.saturating_sub(RECOVERY_BAND_HEIGHT).max(1),
    }
}

/// The frame a client surface of `client` needs — the inverse of [`client_area`].
pub fn frame_around(client: Rect) -> Rect {
    Rect {
        x: client.x,
        y: client.y.saturating_sub(RECOVERY_BAND_HEIGHT),
        width: client.width.max(1),
        height: client.height.max(1).saturating_add(RECOVERY_BAND_HEIGHT),
    }
}

/// How far outside a window's frame a press still grabs its edge, in logical pixels. Outside,
/// never inside: an edge zone inside the frame would take the client's own outermost pixels —
/// its scrollbar, its own resize border — and the band already owns the top.
pub const RESIZE_MARGIN: i32 = 8;

/// How far along an edge from a corner a press still grabs the corner as well.
pub const RESIZE_CORNER: i32 = 24;

/// Which edges a press at `point`, just outside `frame`, grabs — `None` for a point inside the
/// frame (that is the band's, the grip's or the client's) or further than [`RESIZE_MARGIN`]
/// from it. Within [`RESIZE_CORNER`] of a corner, both of the corner's edges.
pub fn resize_edges_at(frame: Rect, point: Point) -> Option<ResizeEdges> {
    let inside = point.x >= frame.x
        && point.x < frame.right()
        && point.y >= frame.y
        && point.y < frame.bottom();
    if inside {
        return None;
    }
    let near = point.x >= frame.x.saturating_sub(RESIZE_MARGIN)
        && point.x < frame.right().saturating_add(RESIZE_MARGIN)
        && point.y >= frame.y.saturating_sub(RESIZE_MARGIN)
        && point.y < frame.bottom().saturating_add(RESIZE_MARGIN);
    if !near {
        return None;
    }
    let mut edges = ResizeEdges {
        left: point.x < frame.x,
        right: point.x >= frame.right(),
        top: point.y < frame.y,
        bottom: point.y >= frame.bottom(),
    };
    if edges.left || edges.right {
        edges.top |= point.y < frame.y.saturating_add(RESIZE_CORNER);
        edges.bottom |= point.y >= frame.bottom().saturating_sub(RESIZE_CORNER);
    }
    if edges.top || edges.bottom {
        edges.left |= point.x < frame.x.saturating_add(RESIZE_CORNER);
        edges.right |= point.x >= frame.right().saturating_sub(RESIZE_CORNER);
    }
    (!edges.is_empty()).then_some(edges)
}

/// Which edges an interactive resize moves. Held as four flags rather than a bitmask so a
/// corner is expressible without a table of sixteen names, and so this crate never has to
/// depend on the protocol enum the compositor translates from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ResizeEdges {
    pub left: bool,
    pub right: bool,
    pub top: bool,
    pub bottom: bool,
}

impl ResizeEdges {
    /// The corner the compositor's own grip drives.
    pub const BOTTOM_RIGHT: ResizeEdges = ResizeEdges {
        left: false,
        right: true,
        top: false,
        bottom: true,
    };

    /// Whether this names no edge at all — `xdg_toplevel.resize` permits `none`, which asks for
    /// a resize that could not change anything.
    pub fn is_empty(self) -> bool {
        !(self.left || self.right || self.top || self.bottom)
    }
}

/// `origin` resized by a pointer that has moved `(dx, dy)` since the grab began.
///
/// The edges that are *not* being dragged do not move: growing leftwards anchors the right edge
/// and moves the origin, which is what makes a resize feel like a resize rather than a move.
/// Every result is at least [`MIN_WINDOW_WIDTH`] × [`MIN_WINDOW_HEIGHT`]; a pointer that runs
/// past that simply stops having an effect, the same clamp-rather-than-refuse rule the field's
/// tiles follow.
pub fn resize_rect(origin: Rect, edges: ResizeEdges, dx: i32, dy: i32) -> Rect {
    let mut resized = origin;
    if edges.right {
        resized.width = origin.width.saturating_add(dx).max(MIN_WINDOW_WIDTH);
    }
    if edges.left {
        let width = origin.width.saturating_sub(dx).max(MIN_WINDOW_WIDTH);
        resized.x = origin.right().saturating_sub(width);
        resized.width = width;
    }
    if edges.bottom {
        resized.height = origin.height.saturating_add(dy).max(MIN_WINDOW_HEIGHT);
    }
    if edges.top {
        let height = origin.height.saturating_sub(dy).max(MIN_WINDOW_HEIGHT);
        resized.y = origin.bottom().saturating_sub(height);
        resized.height = height;
    }
    resized
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

/// How far apart two windows may sit and still count as [`is_beside`] each other — the same
/// order of size as [`RESIZE_MARGIN`], so a pair a person set down "touching" stays touching
/// after a client rounded its own size by a pixel or two.
pub const BESIDE_TOLERANCE: i32 = 8;

/// Whether `window` is already set down beside `target`: not overlapping it, within
/// [`BESIDE_TOLERANCE`] of one of its edges, and sharing some length of that edge. This is the
/// question [`place_beside`] asks before it moves anything, so grouping two windows that are
/// already neighbours leaves them exactly where the user put them.
pub fn is_beside(window: Rect, target: Rect) -> bool {
    if window.intersection(target).is_some() {
        return false;
    }
    // In `i64`: a window record's coordinates come from clients and from restored sessions, and
    // the difference of two extremes does not fit in an `i32`.
    let gap = |a: i32, b: i32| (i64::from(a) - i64::from(b)).abs();
    let overlaps = |low_a: i32, high_a: i32, low_b: i32, high_b: i32| {
        i64::from(high_a.min(high_b)) - i64::from(low_a.max(low_b)) > 0
    };
    let horizontal_overlap = overlaps(window.x, window.right(), target.x, target.right());
    let vertical_overlap = overlaps(window.y, window.bottom(), target.y, target.bottom());
    let horizontal_gap = gap(window.x, target.right()).min(gap(target.x, window.right()));
    let vertical_gap = gap(window.y, target.bottom()).min(gap(target.y, window.bottom()));
    let tolerance = i64::from(BESIDE_TOLERANCE);
    (vertical_overlap && horizontal_gap <= tolerance)
        || (horizontal_overlap && vertical_gap <= tolerance)
}

/// Where `window` goes when it is set down beside `target` — the placement half of "grouping
/// places, it never merges" (planning doc §2 rule 6).
///
/// The side `window` is already nearest to — of those with room for it on `target`'s output —
/// wins: edge to edge, aligned with the target on the other axis and clamped so the whole window
/// stays on that output. Ties go to right, then left, then below, then above. Nothing is stacked,
/// tabbed or folded, and the size is never changed.
///
/// When no side has room — a target that already fills its output, a window wider than the
/// screen — the side with the most room is used anyway and the result goes through
/// [`repair_window`], so the window stays reachable even though it then overlaps the target.
/// Recoverability is the invariant; "beside" is the intent.
pub fn place_beside(window: Rect, target: Rect, outputs: &[Output]) -> Rect {
    let area = (!outputs.is_empty()).then(|| choose_target_output(target, outputs).usable());

    let mut candidates = [
        // Right, then left: `y` aligned with the target.
        Rect {
            x: target.right(),
            y: target.y,
            ..window
        },
        Rect {
            x: target.x.saturating_sub(window.width),
            y: target.y,
            ..window
        },
        // Below, then above: `x` aligned with the target.
        Rect {
            x: target.x,
            y: target.bottom(),
            ..window
        },
        Rect {
            x: target.x,
            y: target.y.saturating_sub(window.height),
            ..window
        },
    ];
    if let Some(area) = area {
        for (index, candidate) in candidates.iter_mut().enumerate() {
            // Slide along the shared edge only: a candidate is never moved across the edge it
            // was placed against, so clamping can align it with the output without ever putting
            // it back on top of the target.
            if index < 2 {
                candidate.y = clamp(
                    candidate.y,
                    area.y,
                    area.bottom().saturating_sub(candidate.height.max(1)),
                );
            } else {
                candidate.x = clamp(
                    candidate.x,
                    area.x,
                    area.right().saturating_sub(candidate.width.max(1)),
                );
            }
        }
    }

    // Which side the window is *already* nearest, from its offset from the target's centre: the
    // dominant axis first (its near side, then its far side), then the other axis. Measuring the
    // distance the window would travel instead would make the choice depend on the window's
    // proportions rather than on where the user left it — a wide, short window would always be
    // pushed below a target it was sitting beside, which is not what "the nearest side" means to
    // anyone looking at the screen. An equal offset, and a window on top of its target, fall
    // through to the declared preference order: right, left, below, above.
    let dx = i64::from(window.center().x) - i64::from(target.center().x);
    let dy = i64::from(window.center().y) - i64::from(target.center().y);
    let preference: [usize; 4] = match (dx.abs() >= dy.abs(), dx >= 0, dy >= 0) {
        (true, true, _) => [0, 1, 2, 3],
        (true, false, _) => [1, 0, 2, 3],
        (false, _, true) => [2, 3, 0, 1],
        (false, _, false) => [3, 2, 0, 1],
    };

    let fits =
        |candidate: Rect| area.is_none_or(|area| area.intersection(candidate) == Some(candidate));
    let chosen = preference
        .into_iter()
        .find(|index| fits(candidates[*index]))
        .or_else(|| {
            // Nothing fits — a target that already fills its output, or a window wider than the
            // screen. Take the side with the most room (`min_by_key` keeps the first of equals,
            // so ties still fall through the preference order) and let `repair_window` have the
            // last word: staying reachable outranks staying beside.
            preference.into_iter().min_by_key(|index| {
                std::cmp::Reverse(
                    area.map_or(i64::MAX, |area| area.intersection_area(candidates[*index])),
                )
            })
        })
        .unwrap_or(0);
    repair_window(candidates[chosen], outputs)
}

/// The smallest rectangle containing every rectangle given — the group outline's shape
/// (`WindowService::group_outline`). `None` for an empty slice.
pub fn bounding_rect(rects: &[Rect]) -> Option<Rect> {
    let mut rects = rects.iter();
    let first = *rects.next()?;
    Some(rects.fold(first, |union, rect| {
        let x = union.x.min(rect.x);
        let y = union.y.min(rect.y);
        Rect {
            x,
            y,
            width: union.right().max(rect.right()).saturating_sub(x),
            height: union.bottom().max(rect.bottom()).saturating_sub(y),
        }
    }))
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

/// Which way a spatial step goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The window nearest `from` in `direction`, by centre, weighting the primary axis so one
/// straight ahead beats a nearer one off to the side.
pub fn nearest_window<Id: Copy + Ord>(
    windows: &[(Id, Rect)],
    from: Id,
    direction: Direction,
) -> Option<Id> {
    let (_, origin) = windows.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = (origin.x + origin.width / 2, origin.y + origin.height / 2);
    let mut best: Option<(i64, Id)> = None;
    for (id, rect) in windows {
        if *id == from {
            continue;
        }
        let (cx, cy) = (rect.x + rect.width / 2, rect.y + rect.height / 2);
        let (ahead, aside) = match direction {
            Direction::Left => (ox - cx, (cy - oy).abs()),
            Direction::Right => (cx - ox, (cy - oy).abs()),
            Direction::Up => (oy - cy, (cx - ox).abs()),
            Direction::Down => (cy - oy, (cx - ox).abs()),
        };
        if ahead <= 0 {
            continue;
        }
        let score = i64::from(ahead) + 2 * i64::from(aside);
        if best.is_none_or(|(s, b)| (score, *id) < (s, b)) {
            best = Some((score, *id));
        }
    }
    best.map(|(_, id)| id)
}

/// The window `hops` contiguous steps from `from` in `direction`.
///
/// This is what `Super+Left`/`Super+Right` move between: the windows of one contiguous group,
/// where "contiguous" is `hops` spatial steps and the next group begins after that.
///
/// It is computed here, from window geometry, rather than from the Home Field's tile layout, and
/// that choice is load-bearing in three ways. It keeps the chord working with `lege-shell` dead,
/// like every other compositor-reserved binding. It needs no round trip to the shell and no wire
/// protocol change. And it stays inside the compositor's own remit — `docs/homefield.md` gives
/// the shell the tiles and the compositor the windows, and this reasons only about windows.
///
/// Nothing is stored. The relation is recomputed per keypress and discarded, so no window
/// becomes *related* to another by sitting next to it — the proximity rule `docs/homefield.md`
/// sets out ("do not infer hidden meaning from proximity") is about meaning, and this changes
/// none: it decides where a keypress goes and nothing else.
///
/// Stops at the edge, returning the furthest window actually reached, so the chord does nothing
/// at a boundary rather than wrapping to the far side of the screen.
pub fn contiguous_window<Id: Copy + Ord>(
    windows: &[(Id, Rect)],
    from: Id,
    direction: Direction,
    hops: u8,
) -> Option<Id> {
    let mut current = from;
    let mut reached = None;
    for _ in 0..hops {
        match nearest_window(windows, current, direction) {
            Some(next) => {
                current = next;
                reached = Some(next);
            }
            None => break,
        }
    }
    reached
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

pub(crate) fn choose_target_output(window: Rect, outputs: &[Output]) -> &Output {
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
mod contiguous_tests {
    use super::*;

    /// Four windows in a row, 100 wide with 20 between them.
    fn row() -> Vec<(u8, Rect)> {
        (0..4u8)
            .map(|i| (i + 1, Rect::new(i32::from(i) * 120, 0, 100, 80)))
            .collect()
    }

    #[test]
    fn one_hop_reaches_the_next_window_along() {
        let row = row();
        assert_eq!(nearest_window(&row, 1, Direction::Right), Some(2));
        assert_eq!(nearest_window(&row, 3, Direction::Left), Some(2));
        assert_eq!(nearest_window(&row, 1, Direction::Left), None);
    }

    #[test]
    fn two_hops_is_the_group_rule_and_the_edge_stops_it() {
        let row = row();
        assert_eq!(contiguous_window(&row, 1, Direction::Right, 2), Some(3));
        // Past the end it reports the furthest it got, never wrapping around.
        assert_eq!(contiguous_window(&row, 3, Direction::Right, 2), Some(4));
        assert_eq!(contiguous_window(&row, 4, Direction::Right, 2), None);
        // Zero hops does nothing at all rather than returning the window you are on.
        assert_eq!(contiguous_window(&row, 1, Direction::Right, 0), None);
    }

    #[test]
    fn a_window_straight_ahead_beats_a_nearer_one_off_to_the_side() {
        let windows = [
            (1u8, Rect::new(0, 0, 100, 80)),
            // Slightly nearer by raw distance, but well off the axis.
            (2, Rect::new(90, 300, 100, 80)),
            (3, Rect::new(200, 0, 100, 80)),
        ];
        assert_eq!(nearest_window(&windows, 1, Direction::Right), Some(3));
    }

    #[test]
    fn the_relation_is_symmetric_enough_to_navigate_back() {
        let row = row();
        let there = contiguous_window(&row, 1, Direction::Right, 1).unwrap();
        assert_eq!(contiguous_window(&row, there, Direction::Left, 1), Some(1));
    }

    #[test]
    fn an_unknown_window_is_not_a_panic() {
        assert_eq!(nearest_window(&row(), 99, Direction::Right), None);
        assert_eq!(contiguous_window::<u8>(&[], 1, Direction::Right, 2), None);
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

    /// An origin-located map request has no position intent: the first window is centred in the
    /// usable area, wholly on screen.
    #[test]
    fn a_new_window_is_centred_not_cornered() {
        let placed = place_new_window(Rect::new(0, 0, 800, 600), 0, &edp());
        assert_eq!(placed, Rect::new(560, 240, 800, 600));
    }

    /// Consecutive windows cascade rather than mapping exactly over one another, and every one
    /// of them stays wholly inside the usable area.
    #[test]
    fn consecutive_windows_cascade_and_stay_on_screen() {
        let usable = edp()[0].usable();
        let mut seen = Vec::new();
        for existing in 0..40 {
            let placed = place_new_window(Rect::new(0, 0, 800, 600), existing, &edp());
            assert!(placed.x >= usable.x && placed.y >= usable.y);
            assert!(placed.right() <= usable.right() && placed.bottom() <= usable.bottom());
            if let Some(previous) = seen.last() {
                assert_ne!(
                    placed, *previous,
                    "window {existing} buried its predecessor"
                );
            }
            seen.push(placed);
        }
    }

    /// A request bigger than the screen is clamped to the usable area, not mapped hanging off.
    #[test]
    fn an_oversized_window_is_clamped_to_the_usable_area() {
        let placed = place_new_window(Rect::new(0, 0, 5000, 4000), 0, &edp());
        assert_eq!(placed, Rect::new(0, 0, 1920, 1080));
    }

    /// A located request is a real position — a restored session window — and is honoured,
    /// repaired only as any geometry would be.
    #[test]
    fn a_positioned_request_is_honoured() {
        assert_eq!(
            place_new_window(Rect::new(300, 200, 640, 480), 5, &edp()),
            Rect::new(300, 200, 640, 480)
        );
        let lost = place_new_window(Rect::new(4000, 3000, 640, 480), 0, &edp());
        assert!(is_recoverable(lost, &edp()));
    }

    #[test]
    fn a_shell_surface_is_placed_by_what_it_is() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let output = Output::new("eDP-1", screen, 1000).with_work_area(work_area_for(
            screen,
            &[Exclusive {
                edge: Edge::Bottom,
                size: 64,
            }],
        ));
        let asked_for = Rect::new(7000, 7000, 900, 64);

        // A dock claiming the bottom edge lands on the bottom edge, spanning the output —
        // measured against the whole output, not the usable area its own claim created.
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive {
                    edge: Edge::Bottom,
                    size: 64
                }),
                &output,
                asked_for,
            ),
            Rect::new(0, 1016, 1920, 64)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive {
                    edge: Edge::Top,
                    size: 40
                }),
                &output,
                asked_for,
            ),
            Rect::new(0, 0, 1920, 40)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive {
                    edge: Edge::Left,
                    size: 72
                }),
                &output,
                asked_for,
            ),
            Rect::new(0, 0, 72, 1080)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive {
                    edge: Edge::Right,
                    size: 72
                }),
                &output,
                asked_for,
            ),
            Rect::new(1848, 0, 72, 1080)
        );

        // The Home Surface is the backdrop, so it fills the output whatever it asked for.
        assert_eq!(
            shell_surface_placement(Layer::HomeSurface, None, &output, asked_for),
            screen
        );

        // Anything else keeps its size and is pulled wholly inside the usable area, so a
        // launcher cannot open underneath the dock.
        let launcher = shell_surface_placement(
            Layer::ShellOverlay,
            None,
            &output,
            Rect::new(7000, 7000, 400, 300),
        );
        assert_eq!((launcher.width, launcher.height), (400, 300));
        assert_eq!(output.usable().intersection(launcher), Some(launcher));

        // A claim larger than the output is clamped rather than placed off-screen.
        let greedy = shell_surface_placement(
            Layer::ShellOverlay,
            Some(Exclusive {
                edge: Edge::Bottom,
                size: 99_999,
            }),
            &output,
            asked_for,
        );
        assert_eq!(greedy, screen);
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

    #[test]
    fn the_client_area_sits_under_the_band_and_the_frame_is_its_inverse() {
        let frame = Rect::new(100, 200, 640, 480);
        let client = client_area(frame);
        assert_eq!(
            client,
            Rect::new(
                100,
                200 + RECOVERY_BAND_HEIGHT,
                640,
                480 - RECOVERY_BAND_HEIGHT
            )
        );
        assert_eq!(frame_around(client), frame);
        assert!(
            client.intersection(recovery_band(frame)).is_none(),
            "the band is above the client, never over it"
        );
        // A frame too short for a client still yields one the compositor can configure.
        assert_eq!(client_area(Rect::new(0, 0, 100, 10)).height, 1);
        assert_eq!(client_area(Rect::new(0, 0, 0, 0)).width, 1);
    }

    #[test]
    fn edges_are_grabbed_just_outside_the_frame_and_nowhere_inside_it() {
        let frame = Rect::new(100, 100, 400, 300);
        let at = |x, y| resize_edges_at(frame, Point { x, y });
        // Inside is the client's, the band's or the grip's.
        assert_eq!(at(300, 250), None);
        assert_eq!(at(100, 100), None);
        assert_eq!(at(499, 399), None);
        // Just outside each edge, away from the corners.
        assert_eq!(
            at(96, 250),
            Some(ResizeEdges {
                left: true,
                ..ResizeEdges::default()
            })
        );
        assert_eq!(
            at(503, 250),
            Some(ResizeEdges {
                right: true,
                ..ResizeEdges::default()
            })
        );
        assert_eq!(
            at(300, 405),
            Some(ResizeEdges {
                bottom: true,
                ..ResizeEdges::default()
            })
        );
        assert_eq!(
            at(300, 95),
            Some(ResizeEdges {
                top: true,
                ..ResizeEdges::default()
            })
        );
        // A corner, reached from either of its edges.
        assert_eq!(at(503, 390), Some(ResizeEdges::BOTTOM_RIGHT));
        assert_eq!(at(490, 403), Some(ResizeEdges::BOTTOM_RIGHT));
        assert_eq!(
            at(95, 105),
            Some(ResizeEdges {
                left: true,
                top: true,
                ..ResizeEdges::default()
            })
        );
        // Too far away is nothing at all.
        assert_eq!(at(80, 250), None);
        assert_eq!(at(300, 420), None);
    }

    #[test]
    fn the_grip_sits_in_the_corner_and_never_reaches_the_recovery_band() {
        let window = Rect::new(100, 200, 800, 600);
        let grip = resize_grip(window).expect("a large window has a grip");
        assert_eq!(
            grip,
            Rect::new(
                900 - RESIZE_GRIP_SIZE,
                800 - RESIZE_GRIP_SIZE,
                RESIZE_GRIP_SIZE,
                RESIZE_GRIP_SIZE
            )
        );
        assert!(
            grip.intersection(recovery_band(window)).is_none(),
            "the move band and the resize grip must never be the same pixel"
        );
    }

    #[test]
    fn a_window_at_the_minimum_size_still_has_both_a_band_and_a_grip() {
        // The property that makes shrinking safe: it is always undoable by pointer.
        let window = Rect::new(0, 0, MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
        let grip = resize_grip(window).expect("the minimum size still affords a grip");
        assert_eq!(grip.height, RESIZE_GRIP_SIZE);
        assert!(grip.intersection(recovery_band(window)).is_none());
    }

    #[test]
    fn a_window_shorter_than_its_own_band_offers_no_grip_rather_than_stealing_it() {
        assert_eq!(
            resize_grip(Rect::new(0, 0, 400, RECOVERY_BAND_HEIGHT)),
            None
        );
        assert_eq!(resize_grip(Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn dragging_the_bottom_right_corner_grows_without_moving_the_origin() {
        let origin = Rect::new(10, 20, 400, 300);
        let grown = resize_rect(origin, ResizeEdges::BOTTOM_RIGHT, 50, 40);
        assert_eq!(grown, Rect::new(10, 20, 450, 340));
    }

    #[test]
    fn dragging_a_left_or_top_edge_anchors_the_opposite_one() {
        let origin = Rect::new(100, 100, 400, 300);
        let edges = ResizeEdges {
            left: true,
            top: true,
            ..ResizeEdges::default()
        };
        let resized = resize_rect(origin, edges, -50, -30);
        assert_eq!(resized, Rect::new(50, 70, 450, 330));
        assert_eq!(resized.right(), origin.right());
        assert_eq!(resized.bottom(), origin.bottom());
    }

    #[test]
    fn a_resize_clamps_at_the_minimum_instead_of_inverting_the_window() {
        let origin = Rect::new(0, 0, 400, 300);
        let squashed = resize_rect(origin, ResizeEdges::BOTTOM_RIGHT, -10_000, -10_000);
        assert_eq!(squashed.width, MIN_WINDOW_WIDTH);
        assert_eq!(squashed.height, MIN_WINDOW_HEIGHT);
        // Clamping from the left keeps the anchored edge exactly where it was.
        let edges = ResizeEdges {
            left: true,
            ..ResizeEdges::default()
        };
        let squashed = resize_rect(origin, edges, 10_000, 0);
        assert_eq!(squashed.width, MIN_WINDOW_WIDTH);
        assert_eq!(squashed.right(), origin.right());
    }

    #[test]
    fn naming_no_edge_changes_nothing() {
        let origin = Rect::new(1, 2, 300, 200);
        assert!(ResizeEdges::default().is_empty());
        assert!(!ResizeEdges::BOTTOM_RIGHT.is_empty());
        assert_eq!(resize_rect(origin, ResizeEdges::default(), 99, 99), origin);
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

    /// A Notification-layer shell surface keeps its requested size and anchors to the
    /// top-right of the output's usable area, clear of any reserved edge.
    #[test]
    fn notification_surfaces_anchor_top_right_of_the_usable_area() {
        let output = Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)
            .with_work_area(Rect::new(0, 40, 1920, 1040));
        let placed =
            shell_surface_placement(Layer::Notification, None, &output, Rect::new(0, 0, 360, 88));
        assert_eq!(placed, Rect::new(1920 - 360 - 12, 40 + 12, 360, 88));
        // A toast wider than the output is clamped rather than walked off-screen.
        let huge = shell_surface_placement(
            Layer::Notification,
            None,
            &output,
            Rect::new(0, 0, 4000, 4000),
        );
        assert!(huge.width <= 1920 && huge.height <= 1040);
        assert!(huge.x >= 0 && huge.y >= 40);
    }

    /// The side the window is already on wins, and the placement is edge to edge.
    #[test]
    fn place_beside_uses_the_side_the_window_is_already_nearest() {
        let outputs = edp();
        let target = Rect::new(700, 400, 400, 300);
        let from_the_right = place_beside(Rect::new(900, 420, 300, 200), target, &outputs);
        assert_eq!(from_the_right, Rect::new(1100, 400, 300, 200));
        let from_the_left = place_beside(Rect::new(500, 420, 300, 200), target, &outputs);
        assert_eq!(from_the_left, Rect::new(400, 400, 300, 200));
        let from_below = place_beside(Rect::new(720, 650, 300, 200), target, &outputs);
        assert_eq!(from_below, Rect::new(700, 700, 300, 200));
        let from_above = place_beside(Rect::new(720, 200, 300, 200), target, &outputs);
        assert_eq!(from_above, Rect::new(700, 200, 300, 200));
    }

    /// A side with no room on the output is not chosen: a target against the right edge sends the
    /// window to its left instead, still edge to edge and still on screen.
    #[test]
    fn place_beside_skips_a_side_with_no_room_on_the_output() {
        let outputs = edp();
        let target = Rect::new(1500, 400, 400, 300);
        let placed = place_beside(Rect::new(1700, 420, 300, 200), target, &outputs);
        assert_eq!(placed, Rect::new(1200, 400, 300, 200));
        assert!(placed.intersection(target).is_none());
        assert!(is_recoverable(placed, &outputs));
    }

    #[test]
    fn windows_that_touch_are_beside_each_other_and_overlapping_ones_are_not() {
        let target = Rect::new(700, 400, 400, 300);
        assert!(is_beside(Rect::new(1100, 400, 300, 200), target));
        assert!(
            is_beside(Rect::new(1104, 400, 300, 200), target),
            "a few pixels of gap is still beside"
        );
        assert!(!is_beside(Rect::new(1200, 400, 300, 200), target));
        assert!(!is_beside(Rect::new(900, 420, 300, 200), target));
        assert!(
            !is_beside(Rect::new(1100, 900, 300, 200), target),
            "flush with the target's right edge but sharing none of it"
        );
    }

    #[test]
    fn a_bounding_rect_covers_every_rectangle_and_nothing_covers_none() {
        assert_eq!(bounding_rect(&[]), None);
        assert_eq!(
            bounding_rect(&[Rect::new(100, 100, 200, 200), Rect::new(400, 300, 100, 100)]),
            Some(Rect::new(100, 100, 400, 300))
        );
    }
}
