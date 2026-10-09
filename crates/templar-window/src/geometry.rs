//! Geometry primitives and the recovery invariant.
//!
//! Every normal window must keep a compositor-owned recovery tab reachable on at least one active
//! output, after every drag, output change, scale change, resume and application geometry request.
//! The tab is one horizontal title plus three controls ([`WindowLayout`]) following the nearest of
//! the window's four client corners; the bottom-edge recovery band below is the corridor the
//! repair pass guarantees, which keeps at least the bottom tabs reachable and, through overlay
//! clamping, every other corner's too. All coordinates are global compositor-logical coordinates.

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
/// (`templar-shell-v2.xml`'s `set_edge_alignment`).
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

/// The nonexclusive shell bar/machine strip. Notification placement clears it even though
/// it does not reduce `Output::usable()`.
pub const SHELL_BAR_HEIGHT: i32 = 48;

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
        // (`templar-shell-v2.xml`'s `set_exclusive_zone`): a bar that opens a row above itself grows
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
        // before, now with a way for the shell to answer it (`templar-shell-v2.xml`'s
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
    if layer == Layer::HomeSurface || layer == Layer::Presentation {
        // Both are backdrops for a whole screen: Home is the desktop, and the window overview's
        // chrome has to reach every card on the output. Neither reserves anything — the
        // overview's surface holds no exclusive zone at all — so the whole output is a placement,
        // not a claim, and nothing reflows when it appears or goes away.
        return full;
    }
    if layer == Layer::Notification {
        // Bottom-right, above the nonexclusive bar, machine cluster and bottom gestures.
        // The size remains client-owned; the compositor re-clamps on output changes.
        let usable = output.usable();
        let margin = 12;
        let width = requested.width.clamp(1, usable.width.max(1));
        let height = requested.height.clamp(1, usable.height.max(1));
        let bottom = SHELL_BAR_HEIGHT + margin;
        return Rect::new(
            usable.right() - width - margin.min((usable.width - width).max(0)),
            usable.bottom() - height - bottom.min((usable.height - height).max(0)),
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

/// Minimum visible width of the recovery band.
pub const MIN_RECOVERY_WIDTH: i32 = 96;
/// Height of one horizontal title tab: the title on a window's top or bottom edge is this tall,
/// and the recovery band below is one tab tall too.
pub const TAG_HEIGHT: i32 = 32;

/// The compositor-owned band along a window's bottom edge: the corridor the repair pass
/// guarantees on an output, through which the window's bottom-corner tabs stay reachable. The
/// corner tabs themselves live in [`WindowLayout`]; this rectangle is only the invariant's
/// measure, kept at the bottom edge so a tall window may hang off the top of an output, never
/// off the bottom.
pub fn recovery_band(window: Rect) -> Rect {
    let height = window.height.clamp(1, TAG_HEIGHT);
    Rect {
        x: window.x,
        // `max` keeps a degenerate (zero-height) window's band at its own origin.
        y: window.bottom().saturating_sub(height).max(window.y),
        width: window.width.max(1),
        height,
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
    let visible_height = window.height.clamp(1, TAG_HEIGHT).min(output.height.max(1));

    // Permit most of a large window to remain outside the output, but guarantee that the
    // compositor-owned recovery band along its bottom edge remains reachable: a tall window may
    // hang off the top of the output, never off the bottom.
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
/// which the recovery band is still grabbable ([`MIN_RECOVERY_WIDTH`]) *and* a minimum title tab
/// still fits beside the reserved resize squares. A window shrunk to this size can therefore
/// still be moved and still be grown again — from any edge or corner ([`resize_edges_at`]) —
/// which is the property that makes an interactive resize safe to offer at all.
///
/// There is no frame/client split anymore: a window's geometry *is* its client rectangle, and
/// [`WindowLayout`] tabs live outside it (or overlaid inside it when there is no exterior room).
/// Showing, hiding or relocating tabs never resizes the client.
pub const MIN_WINDOW_WIDTH: i32 = MIN_RECOVERY_WIDTH;
/// See [`MIN_WINDOW_WIDTH`].
pub const MIN_WINDOW_HEIGHT: i32 = 2 * TAG_HEIGHT;

/// How far outside a window a press still grabs its edge, in logical pixels. Outside, never
/// inside: an edge zone inside the window would take the client's own outermost pixels — its
/// scrollbar, its own resize border. Every edge and every corner resizes; there is no separate
/// grip. The corner squares ([`WindowLayout::resize_square`]) take priority over tabs, and tabs
/// take priority over the edge band they sit in.
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
    // recovery band along that edge stays reachable and the excess hangs off the top.
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

/// Output with greatest overlap, or nearest to the rect when it lies offscreen.
/// The caller must supply at least one output.
pub fn choose_target_output(window: Rect, outputs: &[Output]) -> &Output {
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

// ---- Corner-following window tabs ---------------------------------------------------
//
// One horizontal title tab plus three controls (park, maximize/restore, close) per window,
// following the nearest of the window's four client corners as the pointer approaches from any
// side (plan: `docs/planning/homegarden-previews-notifications-window-tabs.md` §3).
// [`WindowLayout`] is the one derivation of where every tab rectangle is: chrome paint, pointer
// hover/press, move/resize, groups, configure, space sync, output repair and recovery all
// consume it, so the drawn tab and the clickable tab cannot drift apart.
//
// The client rectangle is authoritative: tabs live *outside* it when there is room, overlaid
// *inside* it at the same corner when there is not (output edge, maximized, fullscreen, tiny
// window). Moving, showing or hiding tabs never changes the client rectangle and never emits a
// client resize.

/// A window's four client corners. All four are real layouts — never aliases of each other.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    /// Every corner, in deterministic tie-break order.
    pub const ALL: [Corner; 4] = [
        Self::TopLeft,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomRight,
    ];

    /// Whether this is a top (`TopLeft`/`TopRight`) corner: its title sits on the top edge.
    #[must_use]
    pub const fn is_top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight)
    }

    /// Whether this is a left (`TopLeft`/`BottomLeft`) corner.
    #[must_use]
    pub const fn is_left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft)
    }

    /// The corner's point on `client`.
    #[must_use]
    pub fn point(self, client: Rect) -> Point {
        Point {
            x: if self.is_left() {
                client.x
            } else {
                client.right()
            },
            y: if self.is_top() {
                client.y
            } else {
                client.bottom()
            },
        }
    }

    /// The resize edges a press in this corner's square drags.
    #[must_use]
    pub const fn resize_edges(self) -> ResizeEdges {
        ResizeEdges {
            left: self.is_left(),
            right: !self.is_left(),
            top: self.is_top(),
            bottom: !self.is_top(),
        }
    }

    /// The stable name the settings schema and the protocol use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopRight => "top-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomRight => "bottom-right",
        }
    }

    /// Parse [`Self::name`]; `None` for anything else.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|corner| corner.name() == name)
    }
}

/// How a window's title and controls are arranged at its active corner.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabPlacement {
    /// The default right angle: the title on the adjacent horizontal edge and the three
    /// controls stacked vertically on the adjacent side, meeting at the same corner.
    SameCorner,
    /// Title at one end of the top/bottom edge and a horizontal control row at the opposite
    /// end of the same edge. The exact opposite geometry stays a prototype-review point;
    /// same-edge (not diagonal) is the proposed meaning.
    OppositeCorners,
}

impl TabPlacement {
    /// The stable name the settings schema and the protocol use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SameCorner => "same-corner",
            Self::OppositeCorners => "opposite-corners",
        }
    }

    /// Parse [`Self::name`]; `None` for anything else.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "same-corner" => Some(Self::SameCorner),
            "opposite-corners" => Some(Self::OppositeCorners),
            _ => None,
        }
    }
}

/// One of the three window controls, in canonical order.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabControl {
    Park,
    Maximize,
    Close,
}

impl TabControl {
    /// Every control. Placement order (nearest the anchor corner first) is
    /// [`Self::corner_outward`]: Close rides nearest the corner so it is found first, and it
    /// is the one control that must never sit on a resize target.
    pub const ALL: [TabControl; 3] = [Self::Park, Self::Maximize, Self::Close];

    /// Corner-outward order: the first entry sits nearest the anchor corner.
    #[must_use]
    pub const fn corner_outward() -> [TabControl; 3] {
        [Self::Close, Self::Maximize, Self::Park]
    }
}

/// The tab policy: arrangement, follow, visibility and defaults. One value shared by every
/// window; per-window state (active corner, latch, reveal) lives on the service beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TabPolicy {
    /// [`TabPlacement::SameCorner`] unless the person asked for opposite ends.
    pub placement: TabPlacement,
    /// Follow the nearest corner as the pointer approaches; when false every window keeps
    /// [`Self::fixed_corner`].
    pub follow_pointer: bool,
    /// The predictable accessibility fallback, and the corner used while `follow_pointer`
    /// is false.
    pub fixed_corner: Corner,
    /// Show controls always instead of only on hover reveal.
    pub controls_always: bool,
    /// Keep the title visible while its window is visible (the default); when false the
    /// title hides with the controls until hovered.
    pub title_always: bool,
    /// The corner a new window's tabs start at.
    pub default_corner: Corner,
}

impl Default for TabPolicy {
    fn default() -> Self {
        Self {
            placement: TabPlacement::SameCorner,
            follow_pointer: true,
            fixed_corner: Corner::TopRight,
            controls_always: false,
            title_always: true,
            default_corner: Corner::TopRight,
        }
    }
}

impl TabPolicy {
    /// Parse a policy out of `set_tab_policy` wire values (protocol v14): each argument the
    /// enum value the XML declares, unknown values falling back to [`Self::default`] field
    /// by field, so a skewed or future sender cannot corrupt the policy.
    pub fn from_protocol(
        placement: u32,
        follow_pointer: u32,
        fixed_corner: u32,
        controls: u32,
        title: u32,
        default_corner: u32,
    ) -> Self {
        let corner = |raw: u32| {
            Corner::ALL
                .get(usize::try_from(raw).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(Corner::TopRight)
        };
        Self {
            placement: match placement {
                1 => TabPlacement::OppositeCorners,
                _ => TabPlacement::SameCorner,
            },
            follow_pointer: follow_pointer != 0,
            fixed_corner: corner(fixed_corner),
            controls_always: controls == 1,
            title_always: title != 1,
            default_corner: corner(default_corner),
        }
    }

    /// Parse a policy out of committed settings: `lookup` maps a `windows.tabs.*` key to its
    /// stored token (`same-corner`, `true`, `top-right`, …). Missing or unparsable values fall
    /// back to [`Self::default`] field by field, which is what makes Set/Reset/undo/restart
    /// converge: every reader parses the same committed bytes through this one function.
    pub fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let choice = |key: &str| lookup(key);
        let flag = |key: &str, default: bool| {
            choice(key).map_or(default, |value| match value.as_str() {
                "true" => true,
                "false" => false,
                _ => default,
            })
        };
        Self {
            placement: choice("windows.tabs.placement")
                .as_deref()
                .and_then(TabPlacement::from_name)
                .unwrap_or(TabPlacement::SameCorner),
            follow_pointer: flag("windows.tabs.follow-pointer", true),
            fixed_corner: choice("windows.tabs.fixed-corner")
                .as_deref()
                .and_then(Corner::from_name)
                .unwrap_or(Corner::TopRight),
            controls_always: choice("windows.tabs.controls").is_some_and(|value| value == "always"),
            title_always: choice("windows.tabs.title").is_none_or(|value| value != "hover"),
            default_corner: choice("windows.tabs.default-corner")
                .as_deref()
                .and_then(Corner::from_name)
                .unwrap_or(Corner::TopRight),
        }
    }
}

/// How far a title tab extends along its edge at most: a title is a tab, not a bar.
pub const TAB_TITLE_MAX_WIDTH: i32 = 240;
/// How far it extends at least before the layout falls back to overlay.
pub const TAB_TITLE_MIN_WIDTH: i32 = 64;
/// One control is a square of this side.
pub const TAB_CONTROL_SIZE: i32 = 28;
/// The gap between two controls in a row or stack.
pub const TAB_CONTROL_GAP: i32 = 4;
/// Side of the reserved resize square at each client corner, reaching diagonally outward.
/// Twice [`RESIZE_MARGIN`]: the square covers the corner's whole edge band and the diagonal
/// approach to it, so a press aimed at the exact corner always resizes, never closes.
pub const RESIZE_SQUARE: i32 = 2 * RESIZE_MARGIN;
/// How far outside each client edge pointer approach is noticed for corner selection.
pub const TAB_ACTIVATION_DEPTH: i32 = 48;
/// Corner-selection hysteresis in logical pixels: the winning corner keeps the tabs until
/// another corner is nearer by more than this, so crossing a midpoint does not jitter.
pub const TAB_HYSTERESIS: i32 = 8;
/// Grace before hidden controls answer a fresh approach, in milliseconds.
pub const TAB_REVEAL_GRACE_MS: u64 = 120;
/// Grace before controls hide after the pointer leaves their window, in milliseconds. The
/// title stays; only the controls go, and the last corner is retained.
pub const TAB_EXIT_GRACE_MS: u64 = 250;

/// One window's complete tab geometry: client rect (authoritative), active corner, title rect,
/// the three control rects, and whether any chrome had to clamp inside the client.
///
/// Built by [`WindowLayout::new`] — the one constructor every consumer (paint, hit, damage,
/// recovery, configure) shares — and by [`crate::WindowService::window_layout`], which feeds it
/// the window's stored corner and policy. There are no other tab rectangles anywhere.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowLayout {
    /// The client rectangle. Never resized by tab computation: tabs are derived from it.
    pub client: Rect,
    /// The corner the tabs currently follow.
    pub corner: Corner,
    /// The arrangement in force.
    pub placement: TabPlacement,
    /// True when at least one chrome rectangle had no exterior room and clamped inside the
    /// client at the same corner (output edge, maximized/fullscreen, tiny window).
    pub overlay: bool,
    /// The horizontal title tab. Always horizontal, never rotated, on the top or bottom edge.
    pub title: Rect,
    /// The three controls in [`TabControl::corner_outward`] order: index 0 rides nearest the
    /// anchor corner. A fixed array — three controls need no heap.
    pub controls: [(TabControl, Rect); 3],
}

impl WindowLayout {
    /// Derive every tab rectangle for `client` at `corner` under `placement`, placing chrome
    /// outside the client where `output` (the window's usable area) has room and clamping it
    /// inside at the same corner where it has not.
    ///
    /// The client rectangle passes through untouched: deriving a layout never resizes anything,
    /// which is what lets pure tab relocation send no client configure at all.
    pub fn new(client: Rect, corner: Corner, placement: TabPlacement, output: Rect) -> Self {
        let client = Rect::new(
            client.x,
            client.y,
            client.width.max(1),
            client.height.max(1),
        );
        // Leave the output's Home corner and bottom peek/gesture corridor unobstructed.
        // This bounds decoration only: the client still occupies its original geometry.
        let inset_x = RESIZE_SQUARE.min((output.width - MIN_WINDOW_WIDTH).max(0) / 2);
        let inset_y = RESIZE_SQUARE.min((output.height - MIN_WINDOW_HEIGHT).max(0) / 2);
        let output = Rect::new(
            output.x + inset_x,
            output.y + inset_y,
            output.width - 2 * inset_x,
            output.height - 2 * inset_y,
        );

        // Room for chrome between the two reserved corner squares on one edge.
        let room_between_squares = |span: i32| span - 2 * RESIZE_SQUARE;

        // -- Title: horizontal, on the adjacent horizontal edge, inset past the corner square.
        let title_exterior_width =
            room_between_squares(client.width).clamp(TAB_TITLE_MIN_WIDTH, TAB_TITLE_MAX_WIDTH);
        let title_exterior = if corner.is_left() {
            Rect::new(
                client.x + RESIZE_SQUARE,
                Self::title_y(client, corner),
                title_exterior_width.min(room_between_squares(client.width).max(1)),
                TAG_HEIGHT,
            )
        } else {
            let width = title_exterior_width.min(room_between_squares(client.width).max(1));
            Rect::new(
                client.right() - RESIZE_SQUARE - width,
                Self::title_y(client, corner),
                width,
                TAG_HEIGHT,
            )
        };
        let title_fits_exterior = room_between_squares(client.width) >= TAB_TITLE_MIN_WIDTH
            && output.intersection(title_exterior) == Some(title_exterior);
        let exterior_controls = match placement {
            TabPlacement::SameCorner => Self::same_corner_stack(client, corner),
            TabPlacement::OppositeCorners => Self::opposite_row(client, corner),
        };
        let controls_fit = match placement {
            TabPlacement::SameCorner => room_between_squares(client.height) >= Self::stack_span(),
            TabPlacement::OppositeCorners => {
                room_between_squares(client.width)
                    >= title_exterior_width + TAB_CONTROL_GAP + Self::stack_span()
            }
        } && exterior_controls.iter().all(|rect| {
            output.intersection(*rect) == Some(*rect) && rect.intersection(title_exterior).is_none()
        });
        let overlay = !title_fits_exterior || !controls_fit;
        let (title, controls) = if overlay {
            Self::overlay_tabs(
                Self::overlay_area(client, output, corner),
                corner,
                placement,
            )
        } else {
            (title_exterior, exterior_controls)
        };
        let order = TabControl::corner_outward();
        let controls = [
            (order[0], controls[0]),
            (order[1], controls[1]),
            (order[2], controls[2]),
        ];
        Self {
            client,
            corner,
            placement,
            overlay,
            title,
            controls,
        }
    }

    /// The title's exterior y: above the client for top corners, below it for bottom ones.
    fn title_y(client: Rect, corner: Corner) -> i32 {
        if corner.is_top() {
            client.y - TAG_HEIGHT
        } else {
            client.bottom()
        }
    }

    /// Ordinary overlays stay inside visible client pixels. A sub-minimum client may
    /// leave too little room for four distinct targets; give it bounded overflow rather
    /// than three coincident controls. Reserved resize squares remain excluded.
    fn overlay_area(client: Rect, output: Rect, corner: Corner) -> Rect {
        let visible = client.intersection(output).unwrap_or(output);
        if visible.width >= TAB_CONTROL_SIZE + TAB_CONTROL_GAP + RESIZE_SQUARE
            && visible.height >= 24
        {
            return visible;
        }
        let width = MIN_WINDOW_WIDTH.min(output.width).max(1);
        let height = MIN_WINDOW_HEIGHT.min(output.height).max(1);
        let anchored = |anchor: Rect, corner: Corner| {
            Rect::new(
                (if corner.is_left() {
                    anchor.x
                } else {
                    anchor.right() - width
                })
                .clamp(output.x, (output.right() - width).max(output.x)),
                (if corner.is_top() {
                    anchor.y
                } else {
                    anchor.bottom() - height
                })
                .clamp(output.y, (output.bottom() - height).max(output.y)),
                width,
                height,
            )
        };
        let fits = |area: Rect| {
            Self::resize_squares(client)
                .into_iter()
                .all(|square| square.intersection(area).is_none())
        };
        let preferred = anchored(visible, corner);
        if fits(preferred) {
            return preferred;
        }
        // Keep the requested corner if possible; output-corner recovery is only for
        // a client too small to hold the minimum overflow without crossing a resize square.
        std::iter::once(corner)
            .chain(Corner::ALL)
            .map(|corner| anchored(output, corner))
            .find(|area| fits(*area))
            .unwrap_or(preferred)
    }

    fn overlay_tabs(area: Rect, corner: Corner, placement: TabPlacement) -> (Rect, [Rect; 3]) {
        let mut title = Self::overlay_title(area, corner);
        let controls = match placement {
            TabPlacement::SameCorner => {
                let controls = Self::overlay_stack(area, corner);
                let reserved = controls[0].width + TAB_CONTROL_GAP;
                title.width = title.width.min((area.width - reserved).max(1));
                title.x = if corner.is_left() {
                    area.x + reserved
                } else {
                    area.right() - reserved - title.width
                };
                title.height = title.height.min(title.width);
                title.y = if corner.is_top() {
                    area.y
                } else {
                    area.bottom() - title.height
                };
                controls
            }
            TabPlacement::OppositeCorners => {
                if area.width >= TAB_TITLE_MIN_WIDTH + TAB_CONTROL_GAP + Self::stack_span() {
                    title.width = title
                        .width
                        .min(area.width - Self::stack_span() - TAB_CONTROL_GAP);
                    title.x = if corner.is_left() {
                        area.x
                    } else {
                        area.right() - title.width
                    };
                    Self::overlay_row(area, corner)
                } else {
                    // A narrow window uses a second horizontal row rather than putting
                    // controls over the entire drag target.
                    title.height = title.height.min((area.height - TAB_CONTROL_GAP) / 2).max(1);
                    title.y = if corner.is_top() {
                        area.y
                    } else {
                        area.bottom() - title.height
                    };
                    let remaining = Rect::new(
                        area.x,
                        if corner.is_top() {
                            title.bottom() + TAB_CONTROL_GAP
                        } else {
                            area.y
                        },
                        area.width,
                        (area.height - title.height - TAB_CONTROL_GAP).max(1),
                    );
                    Self::overlay_row(remaining, corner)
                }
            }
        };
        (title, controls)
    }

    /// The title clamped inside the client at the same corner's edge: same corner, same
    /// horizontal edge, horizontal text, inside rather than outside.
    fn overlay_title(client: Rect, corner: Corner) -> Rect {
        let width = TAB_TITLE_MAX_WIDTH.min(client.width.max(1));
        let height = TAG_HEIGHT.min(client.height.max(1));
        let x = if corner.is_left() {
            client.x
        } else {
            client.right() - width
        };
        let y = if corner.is_top() {
            client.y
        } else {
            client.bottom() - height
        };
        Rect::new(x, y, width, height)
    }

    /// The length one control row or stack occupies along its axis: three squares, two gaps.
    fn stack_span() -> i32 {
        3 * TAB_CONTROL_SIZE + 2 * TAB_CONTROL_GAP
    }

    /// The same-corner vertical stack, exterior: on the adjacent side, Close nearest the
    /// corner, running away from it.
    fn same_corner_stack(client: Rect, corner: Corner) -> [Rect; 3] {
        let x = if corner.is_left() {
            client.x - TAB_CONTROL_SIZE
        } else {
            client.right()
        };
        std::array::from_fn(|index| {
            let step = i32::try_from(index).unwrap_or(0) * (TAB_CONTROL_SIZE + TAB_CONTROL_GAP);
            let y = if corner.is_top() {
                client.y + RESIZE_SQUARE + step
            } else {
                client.bottom() - RESIZE_SQUARE - TAB_CONTROL_SIZE - step
            };
            Rect::new(x, y, TAB_CONTROL_SIZE, TAB_CONTROL_SIZE)
        })
    }

    /// The same-corner stack clamped inside the client along the same side. On a tiny
    /// client the controls shrink to fit side by side rather than drop one: three small
    /// controls beat two full-size ones and a window that cannot be parked.
    fn overlay_stack(client: Rect, corner: Corner) -> [Rect; 3] {
        let size = Self::overlay_control_size(client.height.max(1), client.width.max(1));
        let x = if corner.is_left() {
            client.x
        } else {
            client.right() - size
        };
        let column = |index: usize| {
            let step = i32::try_from(index).unwrap_or(0) * (size + TAB_CONTROL_GAP);
            let y = if corner.is_top() {
                client.y + step
            } else {
                client.bottom() - size - step
            };
            Rect::new(x, y.clamp(client.y, client.bottom() - size), size, size)
        };
        [column(0), column(1), column(2)]
    }

    /// The control side that fits three controls plus two gaps along `span`: full size
    /// unless the client is too short for it. Never below one pixel, never wider than
    /// `across`.
    fn overlay_control_size(span: i32, across: i32) -> i32 {
        ((span - 2 * TAB_CONTROL_GAP) / 3)
            .clamp(1, TAB_CONTROL_SIZE)
            .min(across.max(1))
    }

    /// The opposite-end horizontal row, exterior: on the same edge as the title, anchored at
    /// the far corner, Close nearest it, running back toward the title's end.
    fn opposite_row(client: Rect, corner: Corner) -> [Rect; 3] {
        let y = Self::title_y(client, corner);
        // Controls are squares of the tab's own proportion: centred on the title's height.
        let control_y = y + (TAG_HEIGHT - TAB_CONTROL_SIZE) / 2;
        std::array::from_fn(|index| {
            let step = i32::try_from(index).unwrap_or(0) * (TAB_CONTROL_SIZE + TAB_CONTROL_GAP);
            let x = if corner.is_left() {
                // Title at the left end; controls run left from the far (right) corner.
                client.right() - RESIZE_SQUARE - TAB_CONTROL_SIZE - step
            } else {
                // Title at the right end; controls run right from the far (left) corner.
                client.x + RESIZE_SQUARE + step
            };
            Rect::new(x, control_y, TAB_CONTROL_SIZE, TAB_CONTROL_SIZE)
        })
    }

    /// A horizontal row within its allocated overlay area. `overlay_tabs` reserves
    /// a separate title region, stacking the row beneath it on a narrow client.
    fn overlay_row(client: Rect, corner: Corner) -> [Rect; 3] {
        let size = Self::overlay_control_size(client.width.max(1), client.height.max(1));
        let y = if corner.is_top() {
            client.y
        } else {
            client.bottom() - size
        }
        .clamp(client.y, client.bottom() - size);
        std::array::from_fn(|index| {
            let step = i32::try_from(index).unwrap_or(0) * (size + TAB_CONTROL_GAP);
            let x = if corner.is_left() {
                client.right() - size - step
            } else {
                client.x + step
            };
            Rect::new(x.clamp(client.x, client.right() - size), y, size, size)
        })
    }

    /// The reserved resize square at one client corner: [`RESIZE_SQUARE`] on a side, reaching
    /// diagonally outward from the corner point. Strictly outside the client, disjoint from
    /// the other three squares on any non-degenerate client, and never holding a control.
    pub fn resize_square(client: Rect, corner: Corner) -> Rect {
        let tip = corner.point(client);
        Rect::new(
            if corner.is_left() {
                tip.x - RESIZE_SQUARE
            } else {
                tip.x
            },
            if corner.is_top() {
                tip.y - RESIZE_SQUARE
            } else {
                tip.y
            },
            RESIZE_SQUARE,
            RESIZE_SQUARE,
        )
    }

    /// All four resize squares, in [`Corner::ALL`] order.
    pub fn resize_squares(client: Rect) -> [Rect; 4] {
        Corner::ALL.map(|corner| Self::resize_square(client, corner))
    }

    /// The four edge activation regions, in top/bottom/left/right order: bands
    /// [`TAB_ACTIVATION_DEPTH`] deep outside each client edge, spanning it. Approach from any
    /// side selects the nearest corner ([`Self::nearest_corner`]); the bands only say that the
    /// pointer is near enough to ask.
    pub fn activation_regions(client: Rect) -> [Rect; 4] {
        [
            Rect::new(
                client.x,
                client.y - TAB_ACTIVATION_DEPTH,
                client.width.max(1),
                TAB_ACTIVATION_DEPTH,
            ),
            Rect::new(
                client.x,
                client.bottom(),
                client.width.max(1),
                TAB_ACTIVATION_DEPTH,
            ),
            Rect::new(
                client.x - TAB_ACTIVATION_DEPTH,
                client.y,
                TAB_ACTIVATION_DEPTH,
                client.height.max(1),
            ),
            Rect::new(
                client.right(),
                client.y,
                TAB_ACTIVATION_DEPTH,
                client.height.max(1),
            ),
        ]
    }

    /// Which corner the tabs follow for a pointer at `point`: the nearest client corner, with
    /// deterministic ties ([`Corner::ALL`] order wins) and hysteresis — `current`, when given,
    /// is kept until another corner is nearer by more than [`TAB_HYSTERESIS`], so crossing a
    /// midpoint does not jitter the tabs. Pure integer arithmetic: the same inputs always
    /// answer the same corner.
    pub fn nearest_corner(client: Rect, point: Point, current: Option<Corner>) -> Corner {
        let corner = |c: Corner| c.point(client);
        let distance_squared = |c: Corner| {
            let tip = corner(c);
            let dx = i64::from(point.x) - i64::from(tip.x);
            let dy = i64::from(point.y) - i64::from(tip.y);
            dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
        };
        let best = Corner::ALL
            .into_iter()
            .min_by_key(|candidate| distance_squared(*candidate))
            .unwrap_or(Corner::TopRight);
        let Some(current) = current else {
            return best;
        };
        // `isqrt` is monotonic, so comparing roots compares distances to a pixel — without it
        // the hysteresis band would be measured in square-pixels, which is not a distance.
        let nearer = distance_squared(best).isqrt();
        let held = distance_squared(current).isqrt();
        if held.saturating_sub(nearer) <= i64::from(TAB_HYSTERESIS) {
            current
        } else {
            best
        }
    }

    /// The control whose rectangle contains `point`, if any — geometry only. Whether a hidden
    /// control answers is the hit test's decision (it does not), not the layout's.
    pub fn control_at(&self, point: Point) -> Option<TabControl> {
        self.controls
            .iter()
            .find(|(_, rect)| {
                point.x >= rect.x
                    && point.x < rect.right()
                    && point.y >= rect.y
                    && point.y < rect.bottom()
            })
            .map(|(control, _)| *control)
    }

    /// Whether `point` is on the title tab.
    pub fn title_at(&self, point: Point) -> bool {
        point.x >= self.title.x
            && point.x < self.title.right()
            && point.y >= self.title.y
            && point.y < self.title.bottom()
    }

    /// This control's rectangle.
    pub fn control_rect(&self, control: TabControl) -> Rect {
        self.controls
            .iter()
            .find(|(candidate, _)| *candidate == control)
            .map_or(self.title, |(_, rect)| *rect)
    }

    /// Everything the pointer can interact with: client, title, controls and resize squares.
    /// Paint and hit share it through this one function.
    pub fn input_bounds(&self) -> Rect {
        let mut bounds = self.client;
        let grow = |bounds: Rect, rect: Rect| {
            let x = bounds.x.min(rect.x);
            let y = bounds.y.min(rect.y);
            Rect {
                x,
                y,
                width: bounds.right().max(rect.right()) - x,
                height: bounds.bottom().max(rect.bottom()) - y,
            }
        };
        bounds = grow(bounds, self.title);
        for (_, rect) in &self.controls {
            bounds = grow(bounds, *rect);
        }
        for square in Self::resize_squares(self.client) {
            bounds = grow(bounds, square);
        }
        bounds
    }

    /// What (re)painting this window's chrome touches: title plus controls. A relocation
    /// damages the union of the old and the new bounds — never the whole output, never the
    /// client beneath exterior chrome.
    pub fn damage_bounds(&self) -> Rect {
        let mut bounds = self.title;
        let grow = |bounds: Rect, rect: Rect| {
            let x = bounds.x.min(rect.x);
            let y = bounds.y.min(rect.y);
            Rect {
                x,
                y,
                width: bounds.right().max(rect.right()) - x,
                height: bounds.bottom().max(rect.bottom()) - y,
            }
        };
        for (_, rect) in &self.controls {
            bounds = grow(bounds, *rect);
        }
        bounds
    }

    /// The rectangle that must stay on an output for this window to stay recoverable: its
    /// title tab, the thing a person grabs.
    pub fn recovery_rect(&self) -> Rect {
        self.title
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

        // A window whose whole recovery band sits beneath the bar: visible, not grabbable.
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
    /// the whole recovery band reachable and moves nothing else.
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
    /// (`templar-shell-v2.xml`'s `set_edge_alignment`), on all four edges. Every other test in this
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
    fn the_recovery_band_is_one_tab_tall_along_the_bottom_edge() {
        let window = Rect::new(100, 200, 640, 480);
        let band = recovery_band(window);
        assert_eq!(band, Rect::new(100, 680 - TAG_HEIGHT, 640, TAG_HEIGHT));
        // A degenerate window still yields a band the compositor can reason about.
        assert_eq!(recovery_band(Rect::new(0, 0, 400, 10)).height, 10);
        assert_eq!(recovery_band(Rect::new(0, 0, 0, 0)), Rect::new(0, 0, 1, 1));
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
    fn a_window_at_the_minimum_size_still_has_a_full_recovery_band() {
        // The property that makes shrinking safe: it is always undoable by pointer.
        let window = Rect::new(0, 0, MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT);
        assert_eq!(recovery_band(window).height, TAG_HEIGHT);
        // A window shorter than its own band is nothing but band.
        assert_eq!(recovery_band(Rect::new(0, 0, 400, 10)).height, 10);
        assert_eq!(recovery_band(Rect::new(0, 0, 0, 0)), Rect::new(0, 0, 1, 1));
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

    /// Alignment moves a surface **along** its edge and never across it: the island (`Centre`)
    /// and the machine cluster (`End`) share one bottom edge, and both are anchored to it by the
    /// same expression. Written down because a cluster whose input sat some pixels above where it
    /// painted was first blamed on `End` alignment — and the y here is not a function of the
    /// alignment at all, so it could not have been.
    #[test]
    fn alignment_moves_a_surface_along_its_edge_and_never_across_it() {
        let output = Output::new("winit", Rect::new(0, 0, 1280, 900), 1000);
        let zone = Exclusive {
            edge: Edge::Bottom,
            size: 48,
            alignment: Alignment::Start,
        };
        // The machine cluster's real numbers, measured nested: 548 wide, 48 tall, and the popover
        // open at 378 tall — every alignment puts every one of them flush on the bottom edge.
        for height in [48, 378] {
            let requested = Rect::new(0, 0, 548, height);
            let mut xs = Vec::new();
            for alignment in [Alignment::Start, Alignment::Centre, Alignment::End] {
                let placed = shell_surface_placement(
                    Layer::ShellOverlay,
                    Some(Exclusive { alignment, ..zone }),
                    &output,
                    requested,
                );
                assert_eq!(
                    (placed.y, placed.height),
                    (900 - height, height),
                    "{alignment:?} moved a bottom-anchored surface across its edge"
                );
                xs.push(placed.x);
            }
            assert_eq!(xs, vec![0, (1280 - 548) / 2, 1280 - 548]);
        }
    }

    /// A Notification-layer shell surface keeps its requested size and anchors to the
    /// bottom-right, clear of reserved edges and the nonexclusive shell strip.
    #[test]
    fn notification_surfaces_anchor_bottom_right_above_the_shell_strip() {
        let output = Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)
            .with_work_area(Rect::new(0, 40, 1920, 1040));
        let placed =
            shell_surface_placement(Layer::Notification, None, &output, Rect::new(0, 0, 360, 88));
        assert_eq!(
            placed,
            Rect::new(1920 - 360 - 12, 1080 - 88 - SHELL_BAR_HEIGHT - 12, 360, 88)
        );
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

    #[test]
    fn notification_growth_shrink_and_output_changes_preserve_bottom_right_anchor() {
        let mut placed = Rect::new(0, 0, 380, 80);
        for output in [
            Output::new("first", Rect::new(0, 0, 1280, 800), 1000),
            Output::new("first", Rect::new(0, 0, 960, 600), 1500),
            Output::new("remaining", Rect::new(-1920, 100, 1920, 1080), 2000),
        ] {
            for height in [80, 200, 320, 80] {
                placed = shell_surface_placement(
                    Layer::Notification,
                    None,
                    &output,
                    Rect::new(placed.x, placed.y, placed.width, height),
                );
                assert_eq!(placed.height, height);
                assert_eq!(
                    placed.bottom(),
                    output.usable().bottom() - SHELL_BAR_HEIGHT - 12
                );
                assert_eq!(placed.right(), output.usable().right() - 12);
                assert!(placed.y >= output.usable().y);
            }
        }
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

#[cfg(test)]
mod tab_layout_tests {
    use super::*;

    /// A roomy output: chrome fits outside the client unless the test says otherwise.
    fn output() -> Rect {
        Rect::new(0, 0, 1920, 1080)
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.intersection(b).is_some()
    }

    /// Child 1 (`targets-separated`): the four resize squares are outside the client,
    /// pairwise disjoint, and no control — Close included — ever touches one, at every
    /// corner, under both arrangements, from minimum size up.
    #[test]
    fn resize_squares_are_outside_disjoint_and_never_hold_a_control() {
        let clients = [
            Rect::new(200, 200, 800, 600),
            Rect::new(0, 0, MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT),
            Rect::new(-400, -300, 150, 90),
            Rect::new(100, 100, 40, 40),
            Rect::new(100, 100, 1, 1),
        ];
        for client in clients {
            let squares = WindowLayout::resize_squares(client);
            for (index, square) in squares.iter().enumerate() {
                assert_eq!(
                    (square.width, square.height),
                    (RESIZE_SQUARE, RESIZE_SQUARE)
                );
                assert!(
                    !overlaps(*square, client),
                    "resize square {index} of {client:?} eats client pixels"
                );
                for other in &squares[index + 1..] {
                    assert!(
                        !overlaps(*square, *other),
                        "resize squares of {client:?} overlap"
                    );
                }
                // The square answers the corner it is named for. Probed 4px out from the
                // tip: the squares (16px) intentionally reach past the 8px edge band to
                // catch diagonal approaches, so their centres sit outside
                // `resize_edges_at`'s band on the right/bottom — the press path checks
                // squares first and routes those (see the compositor's
                // `every_corner_resizes` test). Where band and square overlap they agree.
                let corner = Corner::ALL[index];
                let tip = corner.point(client);
                let probe = Point {
                    x: tip.x + if corner.is_left() { -4 } else { 4 },
                    y: tip.y + if corner.is_top() { -4 } else { 4 },
                };
                assert!(
                    probe.x >= square.x
                        && probe.x < square.right()
                        && probe.y >= square.y
                        && probe.y < square.bottom(),
                    "probe for {corner:?} escaped its own square"
                );
                assert_eq!(
                    resize_edges_at(client, probe),
                    Some(corner.resize_edges()),
                    "square {corner:?} of {client:?} does not resize that corner"
                );
            }
            for corner in Corner::ALL {
                for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
                    let layout = WindowLayout::new(client, corner, placement, output());
                    for (control, rect) in &layout.controls {
                        for square in &squares {
                            assert!(
                                !overlaps(*rect, *square),
                                "{control:?} at {corner:?}/{placement:?} on {client:?} \
                                 sits on a resize target"
                            );
                        }
                    }
                    assert!(
                        !overlaps(layout.title, squares[corner as usize]),
                        "title at {corner:?} covers its own resize square"
                    );
                }
            }
        }
    }

    /// Child 2 (`four-corner-shared-layout`): every corner is a real layout — the title is
    /// always horizontal on the adjacent horizontal edge, exterior chrome never overlaps the
    /// client or itself, and Close rides nearest the anchor corner.
    #[test]
    fn every_corner_lays_out_a_horizontal_title_and_three_separate_controls() {
        let client = Rect::new(300, 300, 640, 480);
        for corner in Corner::ALL {
            for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
                let layout = WindowLayout::new(client, corner, placement, output());
                assert!(
                    !layout.overlay,
                    "{corner:?}/{placement:?} should fit outside"
                );
                assert_eq!(layout.client, client, "layout must not resize the client");
                // Horizontal title on the adjacent edge, never rotated.
                assert_eq!(layout.title.height, TAG_HEIGHT);
                assert!(layout.title.width >= layout.title.height);
                if corner.is_top() {
                    assert_eq!(layout.title.bottom(), client.y);
                } else {
                    assert_eq!(layout.title.y, client.bottom());
                }
                assert!(!overlaps(layout.title, client));
                // Three fixed controls, pairwise disjoint, clear of title and client.
                assert_eq!(layout.controls.len(), 3);
                for (index, (control, rect)) in layout.controls.iter().enumerate() {
                    assert_eq!(*control, TabControl::corner_outward()[index]);
                    assert_eq!(
                        (rect.width, rect.height),
                        (TAB_CONTROL_SIZE, TAB_CONTROL_SIZE)
                    );
                    assert!(!overlaps(*rect, client), "{control:?} overlaps the client");
                    assert!(
                        !overlaps(*rect, layout.title),
                        "{control:?} overlaps the title"
                    );
                    for (_, other) in &layout.controls[index + 1..] {
                        assert!(!overlaps(*rect, *other), "controls overlap each other");
                    }
                }
                // Close nearest the anchor corner: the active corner for the
                // same-corner right angle, the far corner of the same edge for the
                // opposite-end row (its controls anchor there and run back).
                let anchor = match placement {
                    TabPlacement::SameCorner => corner,
                    TabPlacement::OppositeCorners => match corner {
                        Corner::TopLeft => Corner::TopRight,
                        Corner::TopRight => Corner::TopLeft,
                        Corner::BottomLeft => Corner::BottomRight,
                        Corner::BottomRight => Corner::BottomLeft,
                    },
                };
                let tip = anchor.point(client);
                let close = layout.control_rect(TabControl::Close);
                let park = layout.control_rect(TabControl::Park);
                let distance = |rect: Rect| {
                    let dx = i64::from(rect.center().x - tip.x);
                    let dy = i64::from(rect.center().y - tip.y);
                    dx * dx + dy * dy
                };
                assert!(
                    distance(close) < distance(park),
                    "Close is not nearest the {anchor:?} anchor ({corner:?}/{placement:?})"
                );
                // Paint and hit agree: every drawn rect answers itself.
                assert!(layout.title_at(layout.title.center()));
                for (control, rect) in &layout.controls {
                    assert_eq!(layout.control_at(rect.center()), Some(*control));
                }
            }
        }
    }

    /// Child 2: same-corner is a right angle (title horizontal, controls vertical on the
    /// adjacent side); opposite-corners puts a horizontal row at the far end of the same edge.
    #[test]
    fn arrangements_share_no_shape() {
        let client = Rect::new(300, 300, 640, 480);
        for corner in Corner::ALL {
            let same = WindowLayout::new(client, corner, TabPlacement::SameCorner, output());
            let (_, first) = same.controls[0];
            let (_, second) = same.controls[1];
            assert_eq!(first.x, second.x, "same-corner stack is not vertical");
            assert_ne!(first.y, second.y);

            let opposite =
                WindowLayout::new(client, corner, TabPlacement::OppositeCorners, output());
            let (_, first) = opposite.controls[0];
            let (_, second) = opposite.controls[1];
            assert_eq!(first.y, second.y, "opposite row is not horizontal");
            assert_ne!(first.x, second.x);
            // Same edge as the title, opposite end from it.
            assert!(
                first.y >= opposite.title.y && first.bottom() <= opposite.title.bottom(),
                "opposite controls are not on the title's edge"
            );
            if corner.is_left() {
                assert!(
                    first.x > opposite.title.right(),
                    "row is not at the far end"
                );
            } else {
                assert!(
                    first.right() < opposite.title.x,
                    "row is not at the far end"
                );
            }
        }
    }

    /// Child 2 (recovery): with no exterior room — output edge, maximized/fullscreen-sized,
    /// tiny window — chrome clamps inside the client at the same corner instead of leaving
    /// the screen, and every control survives.
    #[test]
    fn without_exterior_room_chrome_overlays_inside_at_the_same_corner() {
        // Flush with the output: a maximized-sized client has no exterior anywhere.
        let client = Rect::new(0, 0, 1920, 1040);
        let tight = Rect::new(0, 0, 1920, 1040);
        for corner in Corner::ALL {
            for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
                let layout = WindowLayout::new(client, corner, placement, tight);
                assert!(layout.overlay, "{corner:?}/{placement:?} should overlay");
                assert_eq!(client.intersection(layout.title), Some(layout.title));
                for (control, rect) in &layout.controls {
                    assert_eq!(
                        client.intersection(*rect),
                        Some(*rect),
                        "{control:?} escapes the client"
                    );
                }
            }
        }
        // Tiny window: all three controls survive, none dropped, none off-output.
        let tiny = Rect::new(500, 500, 60, 40);
        for corner in Corner::ALL {
            let layout = WindowLayout::new(tiny, corner, TabPlacement::SameCorner, output());
            let mut seen = [false; 3];
            for (control, rect) in &layout.controls {
                seen[*control as usize] = true;
                assert_eq!(output().intersection(*rect), Some(*rect));
            }
            assert!(seen.iter().all(|seen| *seen), "a control went missing");
        }
        // Top edge: the title overlays while the side keeps its exterior stack — same
        // corner throughout, nothing offscreen.
        let top_edge = Rect::new(400, 0, 640, 480);
        let layout = WindowLayout::new(
            top_edge,
            Corner::TopRight,
            TabPlacement::SameCorner,
            output(),
        );
        assert!(layout.overlay);
        assert_eq!(output().intersection(layout.title), Some(layout.title));
    }

    /// Child 3 (`follow-and-latch` selection half): nearest-corner choice is deterministic,
    /// ties break in [`Corner::ALL`] order, and hysteresis holds the current corner across
    /// small crossings.
    #[test]
    fn nearest_corner_selection_is_deterministic_with_ties_and_hysteresis() {
        let client = Rect::new(100, 100, 400, 300);
        // Deep in each quadrant, no contest.
        assert_eq!(
            WindowLayout::nearest_corner(client, Point { x: 0, y: 0 }, None),
            Corner::TopLeft
        );
        assert_eq!(
            WindowLayout::nearest_corner(client, Point { x: 600, y: 0 }, None),
            Corner::TopRight
        );
        assert_eq!(
            WindowLayout::nearest_corner(client, Point { x: 0, y: 500 }, None),
            Corner::BottomLeft
        );
        assert_eq!(
            WindowLayout::nearest_corner(client, Point { x: 600, y: 500 }, None),
            Corner::BottomRight
        );
        // Exact ties break in ALL order, every time.
        let middle_top = Point { x: 300, y: 100 };
        for _ in 0..2 {
            assert_eq!(
                WindowLayout::nearest_corner(client, middle_top, None),
                Corner::TopLeft
            );
        }
        let centre = Point { x: 300, y: 250 };
        assert_eq!(
            WindowLayout::nearest_corner(client, centre, None),
            Corner::TopLeft
        );
        // Hysteresis: each pixel past the midpoint is two pixels of corner-distance
        // difference, so half the band past keeps the held corner and past the band switches.
        assert_eq!(
            WindowLayout::nearest_corner(
                client,
                Point {
                    x: 300 + TAB_HYSTERESIS / 2 - 1,
                    y: 100
                },
                Some(Corner::TopLeft)
            ),
            Corner::TopLeft
        );
        assert_eq!(
            WindowLayout::nearest_corner(
                client,
                Point {
                    x: 300 + TAB_HYSTERESIS,
                    y: 100
                },
                Some(Corner::TopLeft)
            ),
            Corner::TopRight
        );
    }

    /// Corner names round-trip through the settings wire spelling.
    #[test]
    fn corner_and_placement_names_round_trip() {
        for corner in Corner::ALL {
            assert_eq!(Corner::from_name(corner.name()), Some(corner));
        }
        assert_eq!(Corner::from_name("middle"), None);
        assert_eq!(
            TabPlacement::from_name(TabPlacement::SameCorner.name()),
            Some(TabPlacement::SameCorner)
        );
        assert_eq!(
            TabPlacement::from_name(TabPlacement::OppositeCorners.name()),
            Some(TabPlacement::OppositeCorners)
        );
        assert_eq!(TabPlacement::from_name("diagonal"), None);
    }

    /// Child 4 (`settings-converge` parsing half): one parser, field-by-field defaults, so
    /// every reader of the committed bytes converges on the same policy.
    #[test]
    fn tab_policy_parses_committed_settings_with_field_defaults() {
        use std::collections::HashMap;
        let lookup = |map: &HashMap<String, String>| {
            let map = map.clone();
            move |key: &str| -> Option<String> { map.get(key).cloned() }
        };
        // Empty store: the proposed defaults.
        let empty: HashMap<String, String> = HashMap::new();
        assert_eq!(
            TabPolicy::from_lookup(&lookup(&empty)),
            TabPolicy::default()
        );
        assert_eq!(TabPolicy::default().default_corner, Corner::TopRight);
        assert!(TabPolicy::default().title_always);
        // Every field set: every field honoured.
        let full: HashMap<String, String> = [
            ("windows.tabs.placement", "opposite-corners"),
            ("windows.tabs.follow-pointer", "false"),
            ("windows.tabs.fixed-corner", "bottom-left"),
            ("windows.tabs.controls", "always"),
            ("windows.tabs.title", "hover"),
            ("windows.tabs.default-corner", "bottom-right"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(
            TabPolicy::from_lookup(&lookup(&full)),
            TabPolicy {
                placement: TabPlacement::OppositeCorners,
                follow_pointer: false,
                fixed_corner: Corner::BottomLeft,
                controls_always: true,
                title_always: false,
                default_corner: Corner::BottomRight,
            }
        );
        // Garbage in one field does not move the others.
        let mut mixed = full.clone();
        mixed.insert("windows.tabs.placement".into(), "diagonal".into());
        mixed.insert("windows.tabs.follow-pointer".into(), "sometimes".into());
        let policy = TabPolicy::from_lookup(&lookup(&mixed));
        assert_eq!(policy.placement, TabPlacement::SameCorner);
        assert!(policy.follow_pointer);
        mixed.insert("windows.tabs.title".into(), "sometimes".into());
        assert!(TabPolicy::from_lookup(&lookup(&mixed)).title_always);
        assert_eq!(policy.fixed_corner, Corner::BottomLeft);
    }

    /// Child 4: the wire parser speaks the XML's values — and a skewed sender's unknown
    /// values land on the defaults, never on a corrupt policy.
    #[test]
    fn tab_policy_parses_protocol_values_with_field_defaults() {
        assert_eq!(
            TabPolicy::from_protocol(0, 1, 1, 0, 0, 1),
            TabPolicy::default()
        );
        assert_eq!(
            TabPolicy::from_protocol(1, 0, 2, 1, 1, 3),
            TabPolicy {
                placement: TabPlacement::OppositeCorners,
                follow_pointer: false,
                fixed_corner: Corner::BottomLeft,
                controls_always: true,
                title_always: false,
                default_corner: Corner::BottomRight,
            }
        );
        assert_eq!(
            TabPolicy::from_protocol(9, 7, 9, 9, 9, 9),
            TabPolicy::default()
        );
    }

    #[test]
    fn clipped_and_tiny_layouts_keep_four_distinct_reachable_targets() {
        let output = Rect::new(-800, -200, 800, 600);
        for client in [
            Rect::new(-1000, -800, 640, 900),
            Rect::new(-90, 100, 640, 280),
            Rect::new(-600, 0, 96, 64),
            Rect::new(-600, 0, 60, 40),
            Rect::new(-600, 0, 24, 100),
            Rect::new(-600, 0, 48, 100),
            Rect::new(-600, 0, 1, 1),
            output,
        ] {
            for corner in Corner::ALL {
                for placement in [TabPlacement::SameCorner, TabPlacement::OppositeCorners] {
                    let layout = WindowLayout::new(client, corner, placement, output);
                    assert_eq!(layout.client, client);
                    let targets = [
                        layout.title,
                        layout.controls[0].1,
                        layout.controls[1].1,
                        layout.controls[2].1,
                    ];
                    for (index, target) in targets.into_iter().enumerate() {
                        assert!(target.width > 0 && target.height > 0);
                        assert_eq!(target.intersection(output), Some(target), "{layout:?}");
                        for other in &targets[..index] {
                            assert!(target.intersection(*other).is_none(), "{layout:?}");
                        }
                        for square in WindowLayout::resize_squares(client) {
                            assert!(target.intersection(square).is_none(), "{layout:?}");
                        }
                    }
                    for (control, rect) in layout.controls {
                        assert_eq!(layout.control_at(rect.center()), Some(control));
                    }
                    assert!(layout.title_at(layout.title.center()));
                    assert!(layout.control_at(layout.title.center()).is_none());
                }
            }
        }
    }
}
