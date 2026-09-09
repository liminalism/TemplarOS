//! Geometry primitives and the recovery invariant.
//!
//! Every normal window must keep a compositor-owned **tag strip** (its bottom strip, where the
//! name tag and the control tags are drawn) visible on at least one active output, after every
//! drag, output change, scale change, resume and application geometry request. All coordinates are global compositor-logical coordinates.

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
            Some(area) if area.width >= MIN_RECOVERY_WIDTH && area.height >= TAG_HEIGHT => area,
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

/// Where along its edge an anchored shell surface sits
/// (`templar-shell-v1.xml`'s `set_edge_alignment`).
///
/// Placement only: the alignment moves the surface along its edge and changes nothing about what
/// it reserves, so moving between alignments reflows no window — the same property as growing a
/// strip along its edge.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Alignment {
    /// The left end of a top/bottom edge, the top end of a left/right one. The default, and what
    /// every surface got before the shell could ask for anything else.
    #[default]
    Start,
    Centre,
    End,
}

/// One shell surface's claim on an output's space: a dock of `size` logical pixels along `edge`,
/// sitting at `alignment` along it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Exclusive {
    pub edge: Edge,
    pub size: i32,
    /// Where along `edge` the surface sits. `#[serde(default)]` because a persisted claim written
    /// before this field existed is a claim at the edge's start.
    #[serde(default)]
    pub alignment: Alignment,
}

impl Exclusive {
    /// A claim at the edge's start — every claim that never asked for an alignment.
    #[must_use]
    pub fn new(edge: Edge, size: i32) -> Self {
        Self {
            edge,
            size,
            alignment: Alignment::Start,
        }
    }
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
/// * A surface with an [`Exclusive`] claim is placed flush against that edge, at the claim's
///   [`Alignment`] along it ([`Alignment::Start`] unless the shell said otherwise), as thick as
///   the greater of its claim and its requested thickness, and as long *along* the edge as it
///   asked to be. It is measured against `logical`, not [`Output::usable`], because the claim is
///   what *creates* the usable area — insetting it by its own reservation would walk it off the
///   screen one frame at a time.
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
        // Across the edge, the zone is a floor on the surface's thickness, not its size
        // (`templar-shell-v1.xml`'s `set_exclusive_zone`): a bar that opens a row above itself grows
        // over the windows beside it without moving them, because `work_area_for` follows the
        // zone alone.
        //
        // Along the edge, the surface's extent is its own. A chip strip narrower than the screen
        // reserves exactly what a full-width row reserves, so growing one into the other reflows
        // nothing — the same property as the thickness floor, on the other axis. Stretching the
        // surface to fill its edge instead would make the two shapes different reservations, and
        // every toggle between them a work-area change that moves every maximized window.
        let size = zone.size.max(0);
        let thickness =
            |requested_side: i32, available: i32| requested_side.max(size).min(available);
        // Where along the edge is the shell's to say, not the compositor's — the same argument as
        // before, now with a way for the shell to answer it (`templar-shell-v1.xml`'s
        // `set_edge_alignment`). `Start` is what a surface that never asked for anything gets, so
        // this is the behaviour every existing claim already had. The offset is placement alone:
        // `work_area_for` reads `size` and never this, so sliding a strip along its edge reflows
        // nothing.
        let along = |requested_side: i32, available: i32| {
            let extent = requested_side.clamp(1, available.max(1));
            let offset = match zone.alignment {
                Alignment::Start => 0,
                Alignment::Centre => (available - extent) / 2,
                Alignment::End => available - extent,
            };
            (offset.max(0), extent)
        };
        return match zone.edge {
            Edge::Top => {
                let (offset, width) = along(requested.width, full.width);
                Rect::new(
                    full.x + offset,
                    full.y,
                    width,
                    thickness(requested.height, full.height),
                )
            }
            Edge::Bottom => {
                let height = thickness(requested.height, full.height);
                let (offset, width) = along(requested.width, full.width);
                Rect::new(
                    full.x + offset,
                    full.bottom().saturating_sub(height),
                    width,
                    height,
                )
            }
            Edge::Left => {
                let (offset, height) = along(requested.height, full.height);
                Rect::new(
                    full.x,
                    full.y + offset,
                    thickness(requested.width, full.width),
                    height,
                )
            }
            Edge::Right => {
                let width = thickness(requested.width, full.width);
                let (offset, height) = along(requested.height, full.height);
                Rect::new(
                    full.right().saturating_sub(width),
                    full.y + offset,
                    width,
                    height,
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

/// One tag height. The cascade steps down-right, so each new window leaves the previous one's
/// left edge and its name tag's leading pixels showing; the recovery invariant is about
/// reachability on an output, not visibility above other windows, which is the switcher's job.
pub const PLACEMENT_CASCADE_STEP: i32 = 32;

/// The largest frame `area` can hold and still leave a [`RESIZE_MARGIN`] band outside every side.
///
/// [`resize_edges_at`] puts a window's resize zone strictly *outside* its frame, so a frame flush
/// with the edge of the usable area has no grabbable side there — the band is off-screen and the
/// pointer can never reach it. A client asking for more than the screen (a great many do: a
/// remembered size from a larger monitor, a toolkit default) was clamped to exactly the usable
/// rectangle, which is that case on every side at once; clamped on one axis only, it is the
/// window that resizes from the left and the right and from neither the top nor the bottom.
///
/// Never below the minimum: an output smaller than two margins plus a minimum window yields the
/// minimum, and such a window simply overhangs.
fn graspable_size(area: Rect) -> (i32, i32) {
    let inset = RESIZE_MARGIN.saturating_mul(2);
    (
        area.width.saturating_sub(inset).max(MIN_WINDOW_WIDTH),
        area.height.saturating_sub(inset).max(MIN_WINDOW_HEIGHT),
    )
}

/// Where a newly mapped client window goes.
///
/// xdg-shell gives a client no way to position itself, so the location of `requested` carries no
/// intent — it is almost always `(0, 0)` — and honouring it verbatim stacked every new window
/// flush into the top-left corner of the first output, each exactly over the last. Placement is
/// the compositor's decision (the same argument as [`shell_surface_placement`]): an
/// origin-located request is centred in the first output's usable area, stepped down-right by
/// [`PLACEMENT_CASCADE_STEP`] per window already open so consecutive windows stay individually
/// visible, wrapping back to centre before the cascade could reach the area's edge. The size is
/// the client's, clamped by [`graspable_size`] so nothing maps bigger than the screen and every
/// window keeps a resize band on all four sides.
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
    let (widest, tallest) = graspable_size(usable);
    let width = requested.width.clamp(1, widest);
    let height = requested.height.clamp(1, tallest);
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

/// Minimum visible width of the tag strip.
pub const MIN_RECOVERY_WIDTH: i32 = 96;
/// Height of the compositor-owned tag strip at the bottom of a window: the name tag at its left
/// and the control tags at its right are this tall.
pub const TAG_HEIGHT: i32 = 32;

/// The compositor-owned strip along a window's bottom edge, through which it can always be
/// grabbed (by its name tag, `chrome::layout::name_tag`) and controlled (by its control tags).
/// The one derivation of where the tags are: everything else is relative to this rectangle.
pub fn tag_strip(window: Rect) -> Rect {
    let height = window.height.clamp(1, TAG_HEIGHT);
    Rect {
        x: window.x,
        // `max` keeps a degenerate (zero-height) window's strip at its own origin.
        y: window.bottom().saturating_sub(height).max(window.y),
        width: window.width.max(1),
        height,
    }
}

/// The compositor's grab affordance for one window: the tag strip, plus whether the compositor
/// must actively paint a temporary tab over it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoveryAffordance {
    pub strip: Rect,
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
        strip: tag_strip(geometry),
        requires_compositor_paint: client_owns_band,
    }
}

/// True when enough of the tag strip is visible on some output's usable area.
///
/// Against [`Output::usable`], not [`Output::logical`]: a band hidden underneath a dock is not
/// something the user can grab, so counting it would make "recoverable" mean "drawn somewhere"
/// rather than "reachable".
pub fn is_recoverable(window: Rect, outputs: &[Output]) -> bool {
    let strip = tag_strip(window);
    let required_width = window.width.clamp(1, MIN_RECOVERY_WIDTH);

    outputs.iter().any(|output| {
        strip.intersection(output.usable()).is_some_and(|visible| {
            visible.width >= required_width && visible.height >= strip.height
        })
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
    let visible_height = window.height.clamp(1, TAG_HEIGHT).min(output.height.max(1));

    // Permit most of a large window to remain outside the output, but guarantee that the
    // compositor-owned tag strip along its bottom edge remains reachable: a tall window may hang
    // off the top of the output, never off the bottom.
    let minimum_x = output
        .x
        .saturating_sub(window.width.saturating_sub(visible_width));
    let maximum_x = output.right().saturating_sub(visible_width);
    let minimum_y = output
        .y
        .saturating_sub(window.height.saturating_sub(visible_height));
    let maximum_y = output.bottom().saturating_sub(window.height);

    Rect {
        x: clamp(window.x, minimum_x, maximum_x),
        y: clamp(window.y, minimum_y, maximum_y),
        ..window
    }
}

/// The smallest a window may be resized to. Not an arbitrary floor: it is exactly the size at
/// which the tag strip is still grabbable ([`MIN_RECOVERY_WIDTH`]) *and* there is a client area
/// at least as tall as the strip above it. A window shrunk to this size can therefore still be
/// moved and still be grown again — from any edge or corner ([`resize_edges_at`]) — which is the
/// property that makes an interactive resize safe to offer at all.
pub const MIN_WINDOW_WIDTH: i32 = MIN_RECOVERY_WIDTH;
/// See [`MIN_WINDOW_WIDTH`].
pub const MIN_WINDOW_HEIGHT: i32 = 2 * TAG_HEIGHT;

/// The rectangle a framed window's client surface occupies: everything above the tag strip.
/// The strip is the compositor's, drawn *below* the client rather than over it — chrome painted
/// over a client's own pixels hid the first line of every terminal and the close button of every
/// client-decorated window.
///
/// Never empty: a frame shorter than the strip still yields a one-pixel client area, so a caller
/// can always configure it.
pub fn client_area(frame: Rect) -> Rect {
    Rect {
        x: frame.x,
        y: frame.y,
        width: frame.width.max(1),
        height: frame.height.saturating_sub(TAG_HEIGHT).max(1),
    }
}

/// The frame a client surface of `client` needs — the inverse of [`client_area`]: the same
/// origin, one tag strip taller.
pub fn frame_around(client: Rect) -> Rect {
    Rect {
        x: client.x,
        y: client.y,
        width: client.width.max(1),
        height: client.height.max(1).saturating_add(TAG_HEIGHT),
    }
}

/// How far outside a window's frame a press still grabs its edge, in logical pixels. Outside,
/// never inside: an edge zone inside the frame would take the client's own outermost pixels —
/// its scrollbar, its own resize border — and the tags own the bottom strip. Every edge and every
/// corner resizes; there is no separate grip.
pub const RESIZE_MARGIN: i32 = 8;

/// Which edges a press at `point`, just outside `frame`, grabs — `None` for a point inside the
/// frame (that is the tags' or the client's) or further than [`RESIZE_MARGIN`] from it.
///
/// **Every point on the perimeter takes a corner**: one horizontal edge and one vertical edge,
/// chosen by which half of the side was pressed. Touch the left side above its midpoint and you
/// get the top-left corner; below it, bottom-left. There is no middle third that resizes one axis
/// only, because there is no way to see where such a band would end — the previous rule (both
/// edges within a fixed distance of a corner, one edge otherwise) meant the same press produced a
/// different gesture on a small window than on a large one. A halfway rule is the one rule a
/// person can predict without being told it.
///
/// Exact midpoints go to `right`/`bottom`, so every perimeter point yields exactly one horizontal
/// and one vertical edge and none is ambiguous. Single-axis resize is still reachable — it is
/// what `xdg_toplevel.resize` asks for, and that path does not come through here.
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
    // Measured from the frame's own origin, so a window at a negative coordinate splits at its
    // own middle rather than at zero. A corner press already sits in the half its own edges name,
    // so these two never contradict the flags above.
    let (on_a_side, on_an_end) = (edges.left || edges.right, edges.top || edges.bottom);
    if on_a_side {
        let past_middle = point.y >= frame.y.saturating_add(frame.height / 2);
        edges.top |= !past_middle;
        edges.bottom |= past_middle;
    }
    if on_an_end {
        let past_middle = point.x >= frame.x.saturating_add(frame.width / 2);
        edges.left |= !past_middle;
        edges.right |= past_middle;
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
    /// The bottom-right corner, named because it is the one a great many tests reach for.
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

    /// The cursor shape this pair of edges asks for, by its `cursor-spec` name — what the
    /// compositor hands to `CursorImageStatus::Named` and to the nested backend's host window.
    ///
    /// `None` only for edges that name no axis, which is what `xdg_toplevel.resize(none)`
    /// produces; [`resize_edges_at`] always names one edge per axis, so every point on a
    /// window's perimeter has a cursor. Total over all sixteen flag combinations, including the
    /// four that cannot occur (both edges of one axis): those simply have no shape to show.
    pub fn cursor(self) -> Option<&'static str> {
        match (self.left != self.right, self.top != self.bottom) {
            // A corner: the diagonal runs north-west/south-east when the two edges are on the
            // same side of the window (left-with-top, right-with-bottom), north-east/south-west
            // when they are opposite.
            (true, true) => Some(if self.left == self.top {
                "nwse-resize"
            } else {
                "nesw-resize"
            }),
            (true, false) => Some("ew-resize"),
            (false, true) => Some("ns-resize"),
            (false, false) => None,
        }
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
    // A window taller than the output cannot fit; it is set flush with the bottom edge, so the
    // tag strip along that edge stays reachable and the excess hangs off the top.
    let maximum_y = output.bottom().saturating_sub(window.height.max(1));
    let minimum_y = output.y.min(maximum_y);

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
/// that choice is load-bearing in three ways. It keeps the chord working with `templar-shell` dead,
/// like every other compositor-reserved binding. It needs no round trip to the shell and no wire
/// protocol change. And it stays inside the compositor's own remit —
/// `@templaros.decision.home-field-stable-bounded-semantic-grammar` gives
/// the shell the tiles and the compositor the windows, and this reasons only about windows.
///
/// Nothing is stored. The relation is recomputed per keypress and discarded, so no window
/// becomes *related* to another by sitting next to it — the proximity rule in
/// `@templaros.decision.home-field-stable-bounded-semantic-grammar`
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

    /// A request bigger than the screen is clamped into the usable area, not mapped hanging off —
    /// and never flush with it, so the resize zone outside the frame stays on screen.
    #[test]
    fn an_oversized_window_is_clamped_to_the_usable_area() {
        let placed = place_new_window(Rect::new(0, 0, 5000, 4000), 0, &edp());
        assert_eq!(
            placed,
            Rect::new(
                RESIZE_MARGIN,
                RESIZE_MARGIN,
                1920 - 2 * RESIZE_MARGIN,
                1080 - 2 * RESIZE_MARGIN
            )
        );
    }

    /// The regression this rule exists for: a client whose remembered height exceeds the screen
    /// used to be clamped to exactly the usable height, which put its top and bottom edges flush
    /// with the output. `resize_edges_at` only ever answers *outside* a frame, so those two sides
    /// had no reachable band and the window resized from the left and the right alone.
    #[test]
    fn a_window_as_tall_as_the_screen_still_has_a_top_and_a_bottom_to_grab() {
        let outputs = edp();
        let placed = place_new_window(Rect::new(0, 0, 1200, 1700), 0, &outputs);
        let usable = outputs[0].usable();
        assert!(
            placed.y >= usable.y + RESIZE_MARGIN,
            "no band above {placed:?}"
        );
        assert!(
            placed.bottom() <= usable.bottom() - RESIZE_MARGIN,
            "no band below {placed:?}"
        );
        let middle_x = placed.x + placed.width / 2;
        for y in [placed.y - 1, placed.bottom()] {
            let edges = resize_edges_at(placed, Point { x: middle_x, y })
                .expect("a point beside the top or the bottom is that side's");
            assert!(edges.top || edges.bottom, "{edges:?} at y={y}");
        }
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
        let output = Output::new("eDP-1", screen, 1000)
            .with_work_area(work_area_for(screen, &[Exclusive::new(Edge::Bottom, 64)]));
        let asked_for = Rect::new(7000, 7000, 900, 64);

        // A dock claiming the bottom edge lands on the bottom edge, at the start of it, as long
        // as it asked to be — measured against the whole output, not the usable area its own
        // claim created.
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive::new(Edge::Bottom, 64)),
                &output,
                asked_for,
            ),
            Rect::new(0, 1016, 900, 64)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive::new(Edge::Top, 40)),
                &output,
                asked_for,
            ),
            // The request is thicker than its zone, and the surface keeps its thickness: the
            // zone is a floor (a bar with its hover row open), and only the work area follows it.
            Rect::new(0, 0, 900, 64)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive::new(Edge::Left, 72)),
                &output,
                asked_for,
            ),
            // Along a side edge the extent is the request's height.
            Rect::new(0, 0, 900, 64)
        );
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive::new(Edge::Right, 72)),
                &output,
                asked_for,
            ),
            Rect::new(1020, 0, 900, 64)
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

        // A claim larger than the output is clamped rather than placed off-screen — thickness
        // and extent alike.
        let greedy = shell_surface_placement(
            Layer::ShellOverlay,
            Some(Exclusive::new(Edge::Bottom, 99_999)),
            &output,
            Rect::new(7000, 7000, 99_999, 64),
        );
        assert_eq!(greedy, screen);
    }

    /// A zone of size 0 is a live claim that reserves nothing: it still anchors the surface to
    /// its edge, at every height the surface may commit, while `usable()` stays the whole output
    /// stays the whole output — so a maximized window fills the screen. This is the bar
    /// collapsed to a hint.
    #[test]
    fn a_zero_zone_anchors_the_surface_and_reserves_nothing() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let zero = Exclusive::new(Edge::Bottom, 0);
        let output =
            Output::new("eDP-1", screen, 1000).with_work_area(work_area_for(screen, &[zero]));

        assert_eq!(work_area_for(screen, &[zero]), screen);
        assert_eq!(output.usable(), screen);

        // Whatever height it commits — a 6px hint, a 48px row, an open hover row — its bottom is
        // the output's bottom and its left end is the output's left end.
        for height in [1, 6, 48, 96, 240] {
            let placed = shell_surface_placement(
                Layer::ShellOverlay,
                Some(zero),
                &output,
                Rect::new(7000, 7000, 900, height),
            );
            assert_eq!(
                placed,
                Rect::new(0, 1080 - height, 900, height),
                "at {height}"
            );
            assert_eq!(placed.bottom(), screen.bottom(), "at {height}");
        }

        // The other edges anchor the same way at 0.
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive::new(Edge::Top, 0)),
                &output,
                Rect::new(0, 0, 900, 6),
            ),
            Rect::new(0, 0, 900, 6)
        );
    }

    #[test]
    fn exclusive_zones_shrink_the_work_area_and_stack() {
        let screen = Rect::new(0, 0, 1920, 1080);
        assert_eq!(
            work_area_for(screen, &[Exclusive::new(Edge::Top, 40)]),
            Rect::new(0, 40, 1920, 1040)
        );
        assert_eq!(
            work_area_for(screen, &[Exclusive::new(Edge::Bottom, 64)]),
            Rect::new(0, 0, 1920, 1016)
        );
        // A panel at the top and a dock at the bottom take from opposite ends.
        assert_eq!(
            work_area_for(
                screen,
                &[
                    Exclusive::new(Edge::Top, 40),
                    Exclusive::new(Edge::Bottom, 64),
                ]
            ),
            Rect::new(0, 40, 1920, 976)
        );
        // Two claims on the same edge stack rather than overlap.
        let stacked = work_area_for(
            screen,
            &[
                Exclusive::new(Edge::Left, 50),
                Exclusive::new(Edge::Left, 30),
            ],
        );
        assert_eq!(stacked, Rect::new(80, 0, 1840, 1080));
        // A negative claim is not a way to grow the work area past the output.
        assert_eq!(
            work_area_for(screen, &[Exclusive::new(Edge::Top, -500)]),
            screen
        );
    }

    #[test]
    fn a_shell_cannot_make_windows_unreachable() {
        let screen = Rect::new(0, 0, 1920, 1080);
        let ordinary = Output::new("eDP-1", screen, 1000)
            .with_work_area(work_area_for(screen, &[Exclusive::new(Edge::Bottom, 64)]));
        assert_eq!(ordinary.usable(), Rect::new(0, 0, 1920, 1016));

        // A dock taller than the screen, a dock that leaves a sliver too small for a recovery
        // band, and a work area that has wandered off the output entirely: each claim is
        // refused and the whole output is returned, because a restartable shell process does
        // not get to decide whether the user can reach their windows.
        for hostile in [
            work_area_for(screen, &[Exclusive::new(Edge::Top, 5000)]),
            work_area_for(screen, &[Exclusive::new(Edge::Top, 1060)]),
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
        // A bar occupying the bottom 80 logical pixels.
        let output = Output::new("eDP-1", screen, 1000)
            .with_work_area(work_area_for(screen, &[Exclusive::new(Edge::Bottom, 80)]));
        let outputs = vec![output];

        // A window whose whole tag strip sits beneath the bar: visible, but not grabbable.
        let buried = Rect::new(200, 370, 900, 700);
        assert!(
            !is_recoverable(buried, &outputs),
            "a strip hidden under the bar must not count as reachable"
        );

        let repaired = repair_window(buried, &outputs);
        assert!(is_recoverable(repaired, &outputs));
        assert!(
            repaired.bottom() <= 1000,
            "repaired to {repaired:?}, still under the bar"
        );
        assert_eq!((repaired.width, repaired.height), (900, 700));

        // A panel at the top buries nothing that matters: the tags are at the bottom.
        let panelled = Output::new("eDP-1", screen, 1000)
            .with_work_area(work_area_for(screen, &[Exclusive::new(Edge::Top, 80)]));
        assert!(is_recoverable(Rect::new(200, 10, 900, 700), &[panelled]));
    }

    /// A tall window may hang off the top of its output, never off the bottom: the repair keeps
    /// the whole tag strip reachable and moves nothing else.
    #[test]
    fn a_window_dragged_off_the_bottom_is_pulled_back_until_its_tags_show() {
        let outputs = vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)];
        let dropped = Rect::new(300, 1070, 800, 600);
        let repaired = repair_window(dropped, &outputs);
        assert_eq!(repaired, Rect::new(300, 480, 800, 600));
        assert!(is_recoverable(repaired, &outputs));
        let hanging = Rect::new(300, -500, 800, 600);
        assert_eq!(
            repair_window(hanging, &outputs),
            hanging,
            "the strip is on screen"
        );
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

    /// A bar that grows above its zone (a hover row) keeps its edge and its full width, and the
    /// work area does not follow it — nothing beside the bar moves when the row opens.
    #[test]
    fn an_exclusive_surface_may_be_thicker_than_its_zone_without_moving_the_work_area() {
        let output = Output::new("primary", Rect::new(0, 0, 1600, 1000), 1000);
        let zone = Exclusive::new(Edge::Bottom, 48);
        let resting = shell_surface_placement(
            Layer::ShellOverlay,
            Some(zone),
            &output,
            Rect::new(0, 0, 1600, 48),
        );
        assert_eq!(resting, Rect::new(0, 952, 1600, 48));
        let grown = shell_surface_placement(
            Layer::ShellOverlay,
            Some(zone),
            &output,
            Rect::new(0, 0, 1600, 88),
        );
        assert_eq!(
            grown,
            Rect::new(0, 912, 1600, 88),
            "anchored to the edge, thicker"
        );
        assert_eq!(
            work_area_for(output.logical, &[zone]),
            Rect::new(0, 0, 1600, 952),
            "the zone, not the surface, is what windows are placed beside"
        );
    }

    /// A chip strip narrower than the screen stays narrower than the screen, anchored at the
    /// start of its edge. Stretching it to fill the edge would be the compositor deciding the
    /// shape of the shell's own chrome — and would make chips and a full row two different
    /// reservations, so every toggle between them moved every maximized window.
    #[test]
    fn an_exclusive_surface_keeps_its_requested_extent_along_its_edge() {
        let output = Output::new("primary", Rect::new(0, 0, 1600, 1000), 1000);
        let bottom = Exclusive::new(Edge::Bottom, 48);
        let chips = shell_surface_placement(
            Layer::ShellOverlay,
            Some(bottom),
            &output,
            Rect::new(0, 0, 420, 48),
        );
        assert_eq!(chips, Rect::new(0, 952, 420, 48));
        assert_eq!(
            work_area_for(output.logical, &[bottom]),
            Rect::new(0, 0, 1600, 952),
            "a narrow strip reserves exactly what a full-width one does"
        );

        let left = Exclusive::new(Edge::Left, 60);
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(left),
                &output,
                Rect::new(0, 0, 60, 300)
            ),
            Rect::new(0, 0, 60, 300),
            "a side strip is as long as it asked and starts at the top of its edge"
        );
        let right = Exclusive::new(Edge::Right, 60);
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(right),
                &output,
                Rect::new(0, 0, 60, 300)
            ),
            Rect::new(1540, 0, 60, 300)
        );
    }

    /// Where along its edge the surface sits is the shell's to say
    /// (`templar-shell-v1.xml`'s `set_edge_alignment`), on all four edges. Every other test in this
    /// module builds its claim with [`Exclusive::new`] — the start alignment — so they are the
    /// proof that a surface which never asks is placed exactly where it always was.
    #[test]
    fn an_exclusive_surface_sits_where_along_its_edge_it_asked_to() {
        let output = Output::new("primary", Rect::new(0, 0, 1600, 1000), 1000);
        let place = |edge, alignment, requested| {
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(Exclusive {
                    alignment,
                    ..Exclusive::new(edge, 48)
                }),
                &output,
                requested,
            )
        };
        let strip = Rect::new(0, 0, 420, 48);
        for (edge, y) in [(Edge::Top, 0), (Edge::Bottom, 952)] {
            assert_eq!(
                place(edge, Alignment::Start, strip),
                Rect::new(0, y, 420, 48)
            );
            assert_eq!(
                place(edge, Alignment::Centre, strip),
                Rect::new(590, y, 420, 48),
                "centred along a horizontal edge"
            );
            assert_eq!(
                place(edge, Alignment::End, strip),
                Rect::new(1180, y, 420, 48)
            );
        }
        let side = Rect::new(0, 0, 48, 300);
        for (edge, x) in [(Edge::Left, 0), (Edge::Right, 1552)] {
            assert_eq!(
                place(edge, Alignment::Start, side),
                Rect::new(x, 0, 48, 300)
            );
            assert_eq!(
                place(edge, Alignment::Centre, side),
                Rect::new(x, 350, 48, 300),
                "centred along a vertical edge"
            );
            assert_eq!(
                place(edge, Alignment::End, side),
                Rect::new(x, 700, 48, 300)
            );
        }
    }

    /// Alignment is placement, never reservation: the island the shell centres claims a zone of
    /// 0, so it moves along its edge and the work area is still the whole output. Nothing
    /// reflows on the way — which is the only reason the shell may move it at all.
    #[test]
    fn a_centred_zero_zone_surface_reserves_nothing() {
        let output = Output::new("primary", Rect::new(0, 0, 1600, 1000), 1000);
        let island = Exclusive {
            alignment: Alignment::Centre,
            ..Exclusive::new(Edge::Bottom, 0)
        };
        assert_eq!(
            shell_surface_placement(
                Layer::ShellOverlay,
                Some(island),
                &output,
                Rect::new(0, 0, 420, 48)
            ),
            Rect::new(590, 952, 420, 48),
            "centred, and flush to its edge with only its own thickness"
        );
        assert_eq!(
            work_area_for(output.logical, &[island]),
            output.logical,
            "a zone of 0 reserves nothing, wherever along the edge it sits"
        );
        assert_eq!(
            work_area_for(output.logical, &[island]),
            work_area_for(output.logical, &[Exclusive::new(Edge::Bottom, 0)]),
            "the same claim at the start of the edge reserves the same nothing"
        );
    }

    /// The extent is the surface's, but the output is still the compositor's: a claim longer than
    /// the edge is cut to it, and one of nothing is still one pixel — the clamp is what stops
    /// `apply_surface_claims` re-configuring a client with a size it will never commit.
    #[test]
    fn an_exclusive_surface_extent_is_clamped_to_the_output() {
        let output = Output::new("primary", Rect::new(0, 0, 1600, 1000), 1000);
        let zone = Exclusive::new(Edge::Bottom, 48);
        let place = |requested| {
            shell_surface_placement(Layer::ShellOverlay, Some(zone), &output, requested)
        };
        assert_eq!(
            place(Rect::new(0, 0, 9000, 48)),
            Rect::new(0, 952, 1600, 48)
        );
        assert_eq!(place(Rect::new(0, 0, 0, 48)), Rect::new(0, 952, 1, 48));
        assert_eq!(
            place(Rect::new(0, 0, 1600, 4000)),
            Rect::new(0, 0, 1600, 1000),
            "a surface thicker than the output is the output"
        );
        // Idempotence, which is what `protocols_shell::apply_surface_claims` relies on to stop
        // re-configuring a surface whose committed size the placement clamps or floors.
        for requested in [
            Rect::new(0, 0, 9000, 48),
            Rect::new(0, 0, 0, 10),
            Rect::new(0, 0, 420, 88),
        ] {
            let once = place(requested);
            assert_eq!(place(once), once, "placing {once:?} again moved it");
        }
    }

    #[test]
    fn the_client_area_sits_above_the_tags_and_the_frame_is_its_inverse() {
        let frame = Rect::new(100, 200, 640, 480);
        let client = client_area(frame);
        assert_eq!(client, Rect::new(100, 200, 640, 480 - TAG_HEIGHT));
        assert_eq!(frame_around(client), frame);
        let strip = tag_strip(frame);
        assert_eq!(strip, Rect::new(100, 680 - TAG_HEIGHT, 640, TAG_HEIGHT));
        assert!(
            client.intersection(strip).is_none(),
            "the tags are below the client, never over it"
        );
        // A frame too short for a client still yields one the compositor can configure.
        assert_eq!(client_area(Rect::new(0, 0, 100, 10)).height, 1);
        assert_eq!(client_area(Rect::new(0, 0, 0, 0)).width, 1);
    }

    /// Eight side halves, four midpoints, three frames: every press on the perimeter takes the
    /// corner of the half it landed in, and nothing inside the frame is a resize.
    #[test]
    fn every_side_half_takes_that_half_s_corner() {
        let corner = |left, right, top, bottom| {
            Some(ResizeEdges {
                left,
                right,
                top,
                bottom,
            })
        };
        let (top_left, top_right) = (
            corner(true, false, true, false),
            corner(false, true, true, false),
        );
        let (bottom_left, bottom_right) = (
            corner(true, false, false, true),
            corner(false, true, false, true),
        );

        // A normal window, one at the minimum size (where the halves are two tag-heights apart),
        // and one at a negative origin (the split is the frame's own middle, never zero).
        for frame in [
            Rect::new(100, 100, 400, 300),
            Rect::new(0, 0, MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT),
            Rect::new(-300, -200, 200, 120),
        ] {
            let at = |x, y| resize_edges_at(frame, Point { x, y });
            let (mid_x, mid_y) = (frame.x + frame.width / 2, frame.y + frame.height / 2);
            let (before_x, before_y) = (mid_x - 1, mid_y - 1);
            let (outside_left, outside_right) = (frame.x - 1, frame.right());
            let (outside_top, outside_bottom) = (frame.y - 1, frame.bottom());

            // The two halves of each of the four sides.
            assert_eq!(
                at(outside_left, before_y),
                top_left,
                "left side, upper half of {frame:?}"
            );
            assert_eq!(
                at(outside_left, mid_y),
                bottom_left,
                "left side, lower half of {frame:?}"
            );
            assert_eq!(
                at(outside_right, before_y),
                top_right,
                "right side, upper half of {frame:?}"
            );
            assert_eq!(
                at(outside_right, mid_y),
                bottom_right,
                "right side, lower half of {frame:?}"
            );
            assert_eq!(
                at(before_x, outside_top),
                top_left,
                "top side, left half of {frame:?}"
            );
            assert_eq!(
                at(mid_x, outside_top),
                top_right,
                "top side, right half of {frame:?}"
            );
            assert_eq!(
                at(before_x, outside_bottom),
                bottom_left,
                "bottom side, left half of {frame:?}"
            );
            assert_eq!(
                at(mid_x, outside_bottom),
                bottom_right,
                "bottom side, right half of {frame:?}"
            );

            // The four corners still name themselves, reached diagonally.
            assert_eq!(at(outside_left, outside_top), top_left);
            assert_eq!(at(outside_right, outside_top), top_right);
            assert_eq!(at(outside_left, outside_bottom), bottom_left);
            assert_eq!(at(outside_right, outside_bottom), bottom_right);

            // Inside is the client's and the tags', and far outside is nobody's.
            assert_eq!(at(mid_x, mid_y), None);
            assert_eq!(at(frame.x, frame.y), None);
            assert_eq!(at(frame.right() - 1, frame.bottom() - 1), None);
            assert_eq!(at(frame.x - RESIZE_MARGIN - 1, mid_y), None);
            assert_eq!(at(mid_x, frame.bottom() + RESIZE_MARGIN), None);
        }
    }

    /// All sixteen combinations, so the four impossible ones (both edges of one axis) are known
    /// to be harmless rather than merely unreached.
    #[test]
    fn every_pair_of_resize_edges_names_its_cursor() {
        let edges = |left, right, top, bottom| ResizeEdges {
            left,
            right,
            top,
            bottom,
        };
        assert_eq!(
            edges(true, false, true, false).cursor(),
            Some("nwse-resize")
        );
        assert_eq!(
            edges(false, true, false, true).cursor(),
            Some("nwse-resize")
        );
        assert_eq!(
            edges(false, true, true, false).cursor(),
            Some("nesw-resize")
        );
        assert_eq!(
            edges(true, false, false, true).cursor(),
            Some("nesw-resize")
        );
        assert_eq!(edges(true, false, false, false).cursor(), Some("ew-resize"));
        assert_eq!(edges(false, true, false, false).cursor(), Some("ew-resize"));
        assert_eq!(edges(false, false, true, false).cursor(), Some("ns-resize"));
        assert_eq!(edges(false, false, false, true).cursor(), Some("ns-resize"));
        assert_eq!(ResizeEdges::default().cursor(), None);

        // A degenerate axis (neither edge, or both) simply contributes no direction: only the
        // four pairs degenerate on *both* axes have nothing left to show.
        let unnameable: Vec<ResizeEdges> = (0u8..16)
            .map(|bits| edges(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0, bits & 8 != 0))
            .filter(|edges| edges.cursor().is_none())
            .collect();
        assert_eq!(unnameable.len(), 4, "{unnameable:?}");
        assert!(
            unnameable
                .iter()
                .all(|edges| edges.left == edges.right && edges.top == edges.bottom)
        );

        // Every point the perimeter can return has one, which is what the compositor relies on.
        let frame = Rect::new(40, 40, 300, 200);
        for (x, y) in [(39, 60), (340, 60), (60, 39), (60, 240)] {
            assert!(
                resize_edges_at(frame, Point { x, y })
                    .and_then(ResizeEdges::cursor)
                    .is_some()
            );
        }
    }

    #[test]
    fn a_window_at_the_minimum_size_still_has_a_full_tag_strip_and_a_client() {
        // The property that makes shrinking safe: it is always undoable by pointer.
        let window = Rect::new(0, 0, MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
        assert_eq!(tag_strip(window).height, TAG_HEIGHT);
        assert_eq!(client_area(window).height, TAG_HEIGHT);
        // A window shorter than its own strip is nothing but strip.
        assert_eq!(tag_strip(Rect::new(0, 0, 400, 10)).height, 10);
        assert_eq!(tag_strip(Rect::new(0, 0, 0, 0)), Rect::new(0, 0, 1, 1));
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

        /// Whatever a shell asks for, a zone-holding surface lands on the output, flush against
        /// its edge, at least as thick as it reserved, and idempotently — and the work area it
        /// creates depends on the zone alone, never on how long the surface made itself. That
        /// last one is what lets a chip strip grow into a full row without moving a window.
        #[test]
        fn an_exclusive_surface_lands_on_its_edge_whatever_it_asks_for(
            output in arb_output(),
            requested in arb_window(),
            edge in prop_oneof![Just(Edge::Top), Just(Edge::Bottom), Just(Edge::Left), Just(Edge::Right)],
            size in 0i32..6000,
        ) {
            let zone = Exclusive::new(edge, size);
            let full = output.logical;
            let placed = shell_surface_placement(Layer::ShellOverlay, Some(zone), &output, requested);

            prop_assert!(placed.intersection(full) == Some(placed), "{placed:?} is not inside {full:?}");
            prop_assert!(placed.width >= 1 && placed.height >= 1);
            let (start, thickness, available) = match edge {
                Edge::Top => (placed.y == full.y, placed.height, full.height),
                Edge::Bottom => (placed.bottom() == full.bottom(), placed.height, full.height),
                Edge::Left => (placed.x == full.x, placed.width, full.width),
                Edge::Right => (placed.right() == full.right(), placed.width, full.width),
            };
            prop_assert!(start, "{placed:?} is not flush against {edge:?} of {full:?}");
            prop_assert_eq!(thickness, size.max(0).max(match edge {
                Edge::Top | Edge::Bottom => requested.height,
                Edge::Left | Edge::Right => requested.width,
            }).min(available), "thickness is the floor, capped by the output");
            // Anchored at the start of the edge, whatever the extent.
            match edge {
                Edge::Top | Edge::Bottom => prop_assert_eq!(placed.x, full.x),
                Edge::Left | Edge::Right => prop_assert_eq!(placed.y, full.y),
            }
            prop_assert_eq!(
                shell_surface_placement(Layer::ShellOverlay, Some(zone), &output, placed),
                placed,
                "placement is not idempotent"
            );
            // Growing the surface *along* its edge — chips into a full row — moves it not at all
            // across the edge, so `work_area_for` (which never sees the surface) cannot notice.
            let stretched = match edge {
                Edge::Top | Edge::Bottom => Rect { width: full.width, ..requested },
                Edge::Left | Edge::Right => Rect { height: full.height, ..requested },
            };
            let stretched = shell_surface_placement(Layer::ShellOverlay, Some(zone), &output, stretched);
            prop_assert_eq!(
                match edge {
                    Edge::Top | Edge::Bottom => (placed.y, placed.height),
                    Edge::Left | Edge::Right => (placed.x, placed.width),
                },
                match edge {
                    Edge::Top | Edge::Bottom => (stretched.y, stretched.height),
                    Edge::Left | Edge::Right => (stretched.x, stretched.width),
                },
                "the extent moved the surface across its own edge"
            );
        }

        /// Every point on the perimeter is a corner: exactly one horizontal edge and exactly one
        /// vertical edge, for any frame at any coordinate. No point is ambiguous, none is a
        /// single-axis resize, and none is empty — which is what makes `cursor_for` total over
        /// what this can return.
        #[test]
        fn every_perimeter_point_selects_exactly_one_edge_per_axis(
            frame in arb_window(),
            offset in -20000i32..20000,
            margin in 0i32..RESIZE_MARGIN,
            side in 0u8..4,
        ) {
            let point = match side {
                0 => Point { x: frame.x - 1 - margin, y: frame.y + offset.rem_euclid(frame.height.max(1)) },
                1 => Point { x: frame.right() + margin, y: frame.y + offset.rem_euclid(frame.height.max(1)) },
                2 => Point { x: frame.x + offset.rem_euclid(frame.width.max(1)), y: frame.y - 1 - margin },
                _ => Point { x: frame.x + offset.rem_euclid(frame.width.max(1)), y: frame.bottom() + margin },
            };
            let edges = resize_edges_at(frame, point).expect("a point beside a side is that side's");
            prop_assert!(edges.left != edges.right, "{edges:?} names no single horizontal edge");
            prop_assert!(edges.top != edges.bottom, "{edges:?} names no single vertical edge");

            // Whatever is dragged from there, the rectangle never inverts and never goes below
            // the minima — the property that makes shrinking a window undoable by pointer.
            for (dx, dy) in [(0, 0), (-100_000, -100_000), (100_000, 100_000), (-7, 9)] {
                let resized = resize_rect(frame, edges, dx, dy);
                prop_assert!(resized.width >= MIN_WINDOW_WIDTH, "{resized:?} is narrower than the minimum");
                prop_assert!(resized.height >= MIN_WINDOW_HEIGHT, "{resized:?} is shorter than the minimum");
                // The edges not being dragged stay put.
                if !edges.left {
                    prop_assert_eq!(resized.x, frame.x);
                }
                if !edges.top {
                    prop_assert_eq!(resized.y, frame.y);
                }
            }
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
