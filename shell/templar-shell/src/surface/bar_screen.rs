//! The bottom bar: **the running windows at the left, the machine at the right.**
//!
//! That division is the whole design (`docs/garden/garden.md`): Home deals with your things, the
//! bar deals with the machine. So the bar carries one square chip per taskbar entry including the
//! parked ones, and at the right a single cluster — Settings, the network, the clock, and an
//! account chip whose popover holds the machine's readings, the volume and brightness controls
//! and the session actions. There is exactly one of each of those on this desktop; Home has no
//! system widgets any more.
//!
//! **Nothing here is a window button.** An entry is a square wearing its application's icon,
//! active or not; it never widens to print the active window's title. A row of labelled window
//! buttons is the Windows taskbar's signature, and it is also this desktop's own redundancy rule
//! broken: the window wears its name on its own tag, so a second copy of it on the bar says
//! nothing new and costs the switcher a fifth of the screen. The name is on the hover row, next
//! to every *other* window of that application, which is the question a person actually has.
//!
//! **There is no Home chip.** A "Home" button at the bottom-left is the one thing on a desktop
//! that reads as a Start menu, and it is a second route to a place the desktop already goes on
//! its own: Home is what is behind everything, `home.reveal` has Super, and the bar is not Home's
//! — it is the machine's.
//!
//! Every chip in that cluster does one named thing. The readings used to have a chip of their own
//! that could not be clicked and a battery percentage is not a place to go, so the readings became
//! the first line of the account popover and their width went to a Settings chip, which is
//! somewhere to go. The clock opens the shell's native calendar popup; the network chip opens the network
//! control; the account chip is labelled with the account, because an unlabelled mark in the
//! corner of the screen is a button nobody dares press.
//!
//! **It is an island, not a bar.** The surface is exactly as wide as its contents
//! ([`island_width`]) and sits at the bottom-left, because a zero-size zone claim anchors a
//! surface at the start of its edge and leaves its extent alone. The machine cluster is at the
//! island's right end rather than the screen's: a strip of chips that runs the whole width of the
//! display is a bar across the bottom of the desktop, which is the shape the person is trying not
//! to have. Everything either side of the island is desktop.
//!
//! ponytail: an opaque plate with square corners, because the surface has no alpha
//! (softbuffer attaches an `Xrgb8888` buffer, so an unpainted pixel is black). Rounded corners
//! and true floating chips need the bridge-owned `Argb8888` wl_shm surface the draw code's own
//! note describes.
//!
//! **The bar reserves nothing.** Its exclusive zone is 0: the compositor keeps the surface
//! bottom-anchored and leaves the output's usable area whole, so a maximized window fills the
//! screen and wears its own controls. That is why the bar has three presences ([`Presence`]):
//!
//! - **Open** — nothing covers the bottom strip, so the bar simply paints over Home. This is the
//!   resting state at a bare desktop, and the switcher is visible without asking for it.
//! - **Hint** — a window (or a fullscreen one) covers the island's strip. The surface shrinks to
//!   a `HINT_WIDTH` by `HINT_HEIGHT` tab in the corner and paints one `^` on the bar's ground:
//!   nothing of the bar is hit-testable under the window, and the person can still see where it
//!   went. A window that only covers the *right* of the screen is not over the island at all
//!   (`app::bar_covered`), so it does not collapse anything.
//! - **Revealed** — the pointer came to *rest* on the hint for `HINT_OPEN_MS`, so the surface
//!   grows back to the bar and stays while the pointer is on it. Rest, not presence: the strip
//!   runs the width of the output, so a pointer merely travelling along the bottom edge used to
//!   open the bar on its way somewhere else. It collapses `HINT_CLOSE_MS` after the pointer
//!   leaves, closing any open row or popover with it: it hides as easily as it appears.
//!
//! One `layout` computes every rectangle and both the painting and the hit-testing read it, so a
//! click can never land on a rectangle the paint did not use. The bar's own surface is the
//! bottom `BAR_HEIGHT` of `area`; anything above it is the hover row, drawn only when the host
//! has grown the surface to make room.
//!
//! Hover behaviour follows cosmic-app-list, reimplemented here: a dwell on an entry with more
//! than one window opens its row, and the row stays while the pointer is on the entry or the
//! row, closing a grace period after it leaves both.

use std::collections::HashMap;

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use templar_window::WindowId;

use crate::surface::design::{self, Elevation};
use crate::surface::field_screen::FieldAction;
use crate::surface::rail::{self, RAIL_GAP, RailSide, RailView};
use crate::taskbar::{Entry, EntryWindow, Rows};

/// The bar's thickness. It reserves nothing: the exclusive zone is 0.
pub const BAR_HEIGHT: i32 = templar_window::geometry::SHELL_BAR_HEIGHT;
/// The hint's thickness under a covering window — thin enough to ignore, thick enough to aim at.
///
// ponytail: one fixed number for every output and every pointer. If it turns out to be hard to
// hit, the knob is this constant (or an edge-sticky pointer region in the compositor, which is a
// protocol change, not a shell one).
pub const HINT_HEIGHT: i32 = 6;

/// How wide the hint's tab is. The strip used to run the whole width of the output — a six-pixel
/// line across the bottom of the screen, and a target a pointer could not avoid. It is now a tab
/// at the bottom-left corner, under the island it belongs to.
pub const HINT_WIDTH: i32 = 72;
/// Dwell on the hint before the bar comes back out. Short: the hint is aimed at deliberately,
/// unlike a taskbar chip the pointer only sweeps across on its way somewhere else.
pub const HINT_OPEN_MS: u64 = 350;

/// How far the pointer may drift and still be resting on the hint, in logical pixels.
///
/// The strip is six pixels tall and the whole width of the output, so a pointer travelling *along*
/// the bottom edge — on its way to a maximized window's own controls, or thrown at the corner —
/// used to satisfy the dwell simply by being low. Requiring it to come to rest is what separates
/// "I want the switcher" from "I passed through".
const HINT_REST_SLOP: i32 = 4;
/// Grace after the pointer leaves the revealed bar before it collapses to the hint. The same
/// number as the open dwell, on purpose: "it should hide as easily as it appears".
pub const HINT_CLOSE_MS: u64 = 120;
/// The row's height above the bar, plus the gap between it and the chips.
///
/// The row is a handful of separate cards side by side, each a window's picture with its title
/// under it, so every window in it can be read at once. It used to be a *fan* — each card offset
/// a sliver from the one beneath it — which kept it narrow but stacked the windows over each
/// other, and a covered card is a window nobody can identify. Above
/// [`templar_window::task::FAN_MAX`] on screen it hands over to Taskfield.
pub const FAN_BAND: i32 = 116;
/// One card of the row.
const FAN_CARD_W: i32 = 152;
/// The gap between two cards of the row: wide enough that each reads as its own card.
const FAN_GAP: i32 = 8;
/// The title line under a row card's picture.
const FAN_TITLE: i32 = 16;
/// A chip's height. Taller than the compositor's tag (`TAG_HEIGHT`, 32) so the application icon
/// can be read at a glance: the icon used to be 26 pixels, squeezed by a 32-pixel chip, and is
/// 30% larger now.
pub const CHIP_H: i32 = 40;
pub const CHIP_GAP: i32 = 8;
/// The margin at each end of the strip, and what centres a chip in the bar's own height.
pub const CHIP_PAD: i32 = 8;
const CELL: i32 = 48;
/// How far the second row of activity lanes (`shell.rails.lanes` = `rows`) sits above the first:
/// one chip and the gap between chips.
pub const ROW_STEP: i32 = CHIP_H + CHIP_GAP;
/// The application icon on a chip and on a row card without a capture yet: 26 × 1.3.
const ICON: i32 = 34;
/// Dwell before a hovered chip names itself. Shorter than the row's, because a name is a much
/// smaller thing to be given than a strip of window pictures, and a chip that is only an icon has
/// to be askable.
const TOOLTIP_MS: u64 = 250;
/// The tooltip's height, the padding inside it, and its gap above the chip.
const TOOLTIP_HEIGHT: i32 = 22;
const TOOLTIP_PAD: i32 = 6;
const TOOLTIP_GAP: i32 = 6;

/// Dwell before hovering an entry opens its fan: long enough that sweeping the pointer across
/// the bar opens nothing.
pub const HOVER_OPEN_MS: u64 = 400;
/// Grace after leaving both the entry and its fan before the fan closes.
pub const HOVER_CLOSE_MS: u64 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarAction {
    Activate(WindowId),
    /// A declared window action (`window.close`, `window.park`) on this window.
    Window {
        id: &'static str,
        window: WindowId,
    },
    /// A field action with no window of its own (`network.open-control`), spelled as the field
    /// spells it so the action manifest's pointer-route check sees this bar as a route.
    Field(FieldAction),
    /// More windows than a fan can hold: hand the question to the compositor's overview, filtered
    /// to this unit. A linear strip is intrinsically bad at thirty windows, and this is the chip
    /// admitting it rather than growing.
    Taskfield(templar_window::task::TaskKey),
    /// More chips than the island can hold at full width: open the overview of everything.
    Overview,
}

/// How much of the bar is on screen: the whole of it, a hint of it, or the whole of it because
/// the pointer asked. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Nothing covers the strip: the bar paints over Home at its full height.
    Open,
    /// A window covers the strip: a `HINT_HEIGHT` chevron and nothing else.
    Hint,
    /// A window covers the strip and the pointer dwelled on the hint: the bar, for as long as
    /// the pointer stays on it.
    Revealed,
}

/// What the bar draws this frame.
#[derive(Debug)]
pub struct BarData<'a> {
    pub entries: &'a [Entry],
    /// Application icons by canonical id; absent keys draw an initial.
    pub icons: &'a HashMap<String, Bitmap>,
    /// The activated window: what a press on the bar's own ground gives the keyboard back to.
    pub activated: Option<WindowId>,
    /// Whether a window fills the screen, so the bar has to collapse to its hint rather than
    /// paint over it (`app::bar_covered`).
    pub covered: bool,
    /// Whether the compositor says the pointer is resting on that output's bottom edge
    /// (`templar_shell_manager_v2.bar_peeked`). One more way into the same reveal the hint's own
    /// dwell opens — and the only one that works under a window filling the screen, where the
    /// pointer never reaches this surface to be seen arriving. It comes *in* to `revealed` rather
    /// than round the side of `covered`, so the way out is the one grace period either way: a
    /// peek ended by a one-pixel overshoot would otherwise collapse the island instantly, while
    /// the same island reached from the hint takes `HINT_CLOSE_MS` to go.
    pub peeked: bool,
    /// Whether a "go home" reveal is in effect, so the Home chip can show that it is.
    pub home_revealed: bool,
    pub now_ms: u64,
    /// Draw as a gesture rail rather than as the island (`surface::rail`). `None` is the island
    /// and everything the module documentation says about presence; `Some` skips all of that —
    /// the host opened the rail on a gesture and is the one that dismisses it.
    pub rail: Option<RailView>,
    /// `Some` when the entries are drawn as two rows (`crate::taskbar::rows_of`): the first
    /// `active` entries on the bar's own row, the rest on a second row above it. The host
    /// computes it once and passes the same answer to the size functions, so the surface is
    /// asked for exactly the height this layout uses.
    pub rows: Option<Rows>,
}

#[derive(Debug, Default)]
pub struct BarScreen {
    /// The entry under the pointer and when it arrived there.
    hover: Option<(usize, u64)>,
    /// The entry whose fan is open.
    pub open_fan: Option<usize>,
    /// When the pointer left both the open entry and its fan.
    left_at: Option<u64>,
    /// While covered: whether the pointer has revealed the bar out from under the window.
    revealed: bool,
    /// While covered and not revealed: when the pointer came to rest on the hint, and where.
    hint_since: Option<(u64, i32, i32)>,
    /// While revealed: when the pointer left the surface entirely.
    away_since: Option<u64>,
    /// The shapes this frame considers solid, as `(rect, corner radius)` in the surface's own
    /// logical coordinates: the island's plate, and the row band and popover frame when they are
    /// open. Everything else on the surface is desktop.
    ///
    /// The host turns these into the alpha channel of the buffer it attaches
    /// (`surface::window::HostedWindow::present_masked`), because pixelkit's own buffer holds
    /// `0x00RRGGBB` and has nowhere to put coverage. Recorded here rather than recomputed there
    /// so that the shape the person sees and the shape the compositor blends can never disagree.
    opaque: Vec<(Rect, i32)>,
}

struct Layout {
    bar: Rect,
    cells: Vec<Rect>,
    /// Present when more entries exist than fit at full chip width. Clicking it opens Taskfield
    /// over everything, rather than clipping later chips to zero width.
    overflow: Option<Rect>,
    /// How many entries were not given a chip (the overflow chip's count).
    overflow_count: usize,
    /// The open row's cards, in [`fan_of`]'s order: on-screen windows most recently activated
    /// first, then parked ones, left to right. They never overlap, so a point is in at most one.
    fan: Vec<Rect>,
    /// In rail mode, the revealed columns `[x0, x1)` in physical pixels: only a cell whose centre
    /// is inside them answers the pointer. `None` for the island, where every cell does.
    revealed: Option<(i32, i32)>,
}

impl Layout {
    /// Whether `cell` is on screen enough to hover and click.
    fn live(&self, cell: Rect) -> bool {
        self.revealed
            .is_none_or(|interval| rail::centre_revealed(cell, interval))
    }
}

/// How many full-width chips fit in `bar_width`, and whether the rest need an overflow chip.
#[must_use]
fn chip_slots(bar_width: i32, entries: usize, cell: i32, gap: i32, pad: i32) -> (usize, bool) {
    let inner = (bar_width - 2 * pad).max(0);
    let stride = cell + gap;
    let max_full = if stride <= 0 {
        0
    } else {
        usize::try_from(((inner + gap) / stride).max(0)).unwrap_or(0)
    };
    if entries <= max_full {
        (entries, false)
    } else {
        (max_full.saturating_sub(1), true)
    }
}

/// The surface size the host should ask for, in logical pixels.
///
/// The output's width always, and then the height the presence asks for: the hint's thickness
/// while a window covers the strip, otherwise the bar plus whatever band an open hover row or an
/// open account popover needs above it. `full_width` is the output's own logical width, which the
/// shell already knows from its Home surface; the compositor never configures this surface's
/// width, so the answer has to come from here.
///
/// The exclusive zone is 0 whatever this returns, so resizing this surface reflows nothing — the
/// compositor only keeps it anchored to the bottom edge
/// (`interfaces/wayland/templar-shell-v2.xml`, `set_exclusive_zone`).
pub fn wanted_size(
    presence: Presence,
    fan_cards: Option<usize>,
    tooltip: bool,
    entries: usize,
    rows: Option<Rows>,
    full_width: i32,
) -> (i32, i32) {
    if presence == Presence::Hint {
        // Hint-sized: the row is closed with the collapse, so there is never a band to make room
        // for here, and the tab is only as wide as a chevron needs.
        return (HINT_WIDTH.min(widest(full_width)), HINT_HEIGHT);
    }
    // An open row is wider than the island it hangs off — an island of one chip is 72 pixels and
    // a window's picture is 152. The surface grows to hold it and shrinks again when it closes;
    // the zone is 0 throughout, so neither move reflows anything. This is the one thing that ever
    // makes the surface wider than its own chips, and it is transient by construction.
    let fan = fan_cards.map_or(0, |cards| {
        2 * CHIP_PAD + row_span(cards, FAN_CARD_W, FAN_GAP)
    });
    let band = band_above(fan_cards, tooltip);
    // Two rows are only as wide as the longer one.
    let across = rows.map_or(entries, Rows::widest);
    (
        island_width(across).max(fan).min(widest(full_width)),
        BAR_HEIGHT + extra_row(rows) + band,
    )
}

/// The height a second row adds: nothing for one row.
fn extra_row(rows: Option<Rows>) -> i32 {
    if rows.is_some() { ROW_STEP } else { 0 }
}

/// How wide `cards` cards of width `card` are, side by side with `gap` between them.
fn row_span(cards: usize, card: i32, gap: i32) -> i32 {
    let cards = i32::try_from(cards).unwrap_or(1).max(1);
    cards * card + (cards - 1) * gap
}

/// How many chips one row of the island can hold before it would need an overflow chip: what
/// [`crate::taskbar::rows_of`] is asked to fit each lane into. From the same `widest` the island
/// itself is clamped to.
#[must_use]
pub fn island_row_capacity(full_width: i32) -> usize {
    row_capacity(widest(full_width), CHIP_PAD)
}

/// How many chips one row of a rail can hold, the room `rail_length` and `rail_cells` share.
#[must_use]
pub fn rail_row_capacity(full_width: i32) -> usize {
    row_capacity(rail_room(full_width), 0)
}

fn row_capacity(width: i32, pad: i32) -> usize {
    let stride = CELL + CHIP_GAP;
    usize::try_from(((width - 2 * pad).max(0) + CHIP_GAP) / stride).unwrap_or(0)
}

/// How much room above the bar an open fan or a chip's name needs.
fn band_above(fan_cards: Option<usize>, tooltip: bool) -> i32 {
    if fan_cards.is_some() {
        FAN_BAND
    } else if tooltip {
        TOOLTIP_HEIGHT + TOOLTIP_GAP
    } else {
        0
    }
}

/// The room one side of a rail has for chips, origin to the corner, less the gap at the origin.
/// Both corners stay clear for the same reason [`widest`] gives: they are the compositor's.
fn rail_room(full_width: i32) -> i32 {
    (full_width / 2 - CORNER_CLEARANCE - RAIL_GAP).max(0)
}

/// How many chips a rail of `entries` shows, the overflow chip included.
fn rail_chips(entries: usize, room: i32, cell: i32, gap: i32) -> usize {
    let (shown, overflow) = chip_slots(room, entries, cell, gap, 0);
    shown + usize::from(overflow)
}

/// How long the apps rail is, in logical pixels: from the origin (`full_width / 2`) to the far
/// end of the last chip it shows, plus a margin. What [`rail::RailReveal`] clamps to.
#[must_use]
pub fn rail_length(entries: usize, full_width: i32) -> i32 {
    let room = rail_room(full_width);
    let chips = i32::try_from(rail_chips(entries, room, CELL, CHIP_GAP)).unwrap_or(0);
    if chips == 0 {
        return 0;
    }
    (RAIL_GAP + chips * CELL + (chips - 1) * CHIP_GAP + CHIP_PAD).min(full_width / 2)
}

/// How much of the apps rail is shown the moment it opens: the whole of the first chip, so
/// something is under the pointer's path at once.
#[must_use]
pub fn rail_initial() -> i32 {
    RAIL_GAP + CELL
}

/// The surface size the host asks for while the apps rail is open: the output's whole width,
/// because the rail runs from its centre to either edge and the mask makes the rest desktop, and
/// the bar plus whatever band an open fan or name needs.
#[must_use]
pub fn rail_wanted_size(
    fan_cards: Option<usize>,
    tooltip: bool,
    rows: Option<Rows>,
    full_width: i32,
) -> (i32, i32) {
    (
        full_width.max(1),
        BAR_HEIGHT + extra_row(rows) + band_above(fan_cards, tooltip),
    )
}

/// The widest an island may be on an output of `full_width`: everything but the two bottom
/// corners.
///
/// The corners are not the bar's. The compositor's bottom-left corner square is the pointer route
/// to Home, and it is suppressed while a shell overlay is under the pointer — so an island allowed
/// to grow to the full width of the output would sit on that square and silently stop Home being
/// reachable by pointer at all, at somewhere around fifteen open applications on a 1280-pixel
/// screen. A centred island that runs out of room crowds its chips instead, which is visible.
fn widest(full_width: i32) -> i32 {
    // Twice the cluster's width, not once: the island is centred, so it reaches the cluster's left
    // edge when it is `full_width - 2 * cluster_width` across. Without this an island of five
    // applications on a 1366-pixel screen sits under the clock, and which of the two is on top is
    // whichever mapped last. Reserved whether or not the cluster is there — it costs a wide island
    // some room in fullscreen, and it costs a mistake nothing.
    (full_width - 2 * crate::surface::cluster_screen::cluster_width() - 2 * CORNER_CLEARANCE).max(1)
}

/// How much of each end of the bottom edge the island leaves alone, in logical pixels. The
/// compositor's own `HOME_CORNER`, which is not a number this crate can import — a mismatch costs
/// clearance, never correctness: too large and the island is narrower than it could be, too small
/// and the corner stops answering.
const CORNER_CLEARANCE: i32 = 8;

/// How wide the island is: its two margins and one square per open application. Nothing else —
/// the machine's own chips are the cluster's, on its own surface at the right-hand end of the
/// screen.
///
/// The surface is this wide and no wider. It used to be the output's whole width, which made a
/// strip of chips read as a bar across the bottom of the screen — the thing the person is trying
/// not to have. The zone claim anchors a bottom-edge surface at the start of its edge and leaves
/// its extent alone (`shell_surface_placement`), so a narrower surface simply sits at the
/// bottom-left and the desktop is visible either side of it.
#[must_use]
pub fn island_width(entries: usize) -> i32 {
    let entries = i32::try_from(entries).unwrap_or(i32::MAX);
    2 * CHIP_PAD + entries * (CELL + CHIP_GAP)
}

/// The letter a chip wears when the application has no icon.
///
/// The application's own name first, and the window title only as a fallback: a one-window entry
/// is labelled with its title, and a terminal's title is a path — so taking the first character
/// of it put a chip reading "/" on the bar. Only a letter or a digit is ever an initial;
/// punctuation is skipped wherever it comes from. A group is the exception: its `app_id` is only
/// the member whose icon it borrows, and the group's own name is what its letter stands for.
fn chip_initial(entry: &Entry) -> String {
    let name = entry
        .app_id
        .as_deref()
        .filter(|_| !matches!(entry.unit.key, templar_window::task::TaskKey::Group(_)))
        .and_then(|id| id.parse::<templar_types::AppId>().ok())
        .map(|id| crate::field_model::short_name(&id));
    name.as_deref()
        .and_then(first_letter)
        .or_else(|| first_letter(&entry.label))
        .map_or_else(
            || "?".to_owned(),
            |letter| letter.to_uppercase().to_string(),
        )
}

fn first_letter(text: &str) -> Option<char> {
    text.chars().find(|letter| letter.is_alphanumeric())
}

/// The windows a chip's row shows: the unit's on-screen windows, most recently activated first,
/// then its parked ones in the same order, capped at [`templar_window::task::FAN_MAX`].
///
/// Parked windows are in it. A terminal put away by the drop-down chord is still one of that
/// terminal's windows, and a row that left it out left the chip with one window to offer and no
/// row at all; its card wears the parked mark and choosing it brings the window back.
fn fan_of(entry: &Entry) -> Vec<&EntryWindow> {
    let on_screen = entry.unit.visible_windows();
    let parked = entry.unit.shelved.iter().copied();
    on_screen
        .chain(parked)
        .filter_map(|id| entry.windows.iter().find(|window| window.id == id))
        .take(templar_window::task::FAN_MAX)
        .collect()
}

/// Whether hovering this chip opens a row at all — two or more windows, no more than
/// [`templar_window::task::FAN_MAX`] of them on screen.
fn fannable(entry: &Entry) -> bool {
    templar_window::task::chip_action(&entry.unit) == templar_window::task::ChipAction::Fan
}

/// One chip's ground: the compositor's tag, rounded the way every other small shell object is.
pub fn chip(ui: &mut Ui<'_>, rect: Rect, fill: u32) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    ui.painter
        .fill_rounded_rect(rect, ui.px(design::CHIP_RADIUS), fill);
}

impl BarScreen {
    /// Whether the host should make room for a fan above the bar.
    pub fn fan_open(&self) -> bool {
        self.open_fan.is_some()
    }

    /// Whether a hovered chip is naming itself, so the host makes room above the island for it.
    /// A tooltip needs less room than a row and is not one: a chip's name is a line of text.
    #[must_use]
    pub fn tooltip_open(&self, now_ms: u64) -> bool {
        self.open_fan.is_none()
            && self
                .hover
                .is_some_and(|(_, since)| now_ms.saturating_sub(since) >= TOOLTIP_MS)
    }

    /// How many cards the open fan holds, for the surface size the host asks for. `None` when no
    /// fan is open.
    #[must_use]
    pub fn fan_cards(&self, entries: &[Entry]) -> Option<usize> {
        let index = self.open_fan?;
        Some(fan_of(entries.get(index)?).len())
    }

    /// How much of the bar is on screen, given what the host says covers the strip. The host
    /// reads this after `draw` to size the surface, and it is what `draw` itself decided.
    pub fn presence(&self, covered: bool) -> Presence {
        match (covered, self.revealed) {
            (false, _) => Presence::Open,
            (true, false) => Presence::Hint,
            (true, true) => Presence::Revealed,
        }
    }

    /// The next moment the bar changes on its own — a dwell that opens a hover row or reveals the
    /// bar out from under a window, or a grace period that closes either — so the host can wake
    /// then and nothing else.
    pub fn next_deadline_ms(&self, entries: &[Entry], covered: bool, now_ms: u64) -> Option<u64> {
        let open = self.hover.and_then(|(index, since)| {
            (self.open_fan != Some(index) && entries.get(index).is_some_and(fannable))
                .then_some(since + HOVER_OPEN_MS)
        });
        let close = self.left_at.map(|left| left + HOVER_CLOSE_MS);
        // Only while it is still coming: a deadline already in the past is a wake-up that
        // arrives, changes nothing and asks for itself again.
        let name = self
            .hover
            .filter(|_| self.open_fan.is_none() && !self.tooltip_open(now_ms))
            .map(|(_, since)| since + TOOLTIP_MS);
        let reveal = self
            .hint_since
            .filter(|_| covered && !self.revealed)
            .map(|(since, _, _)| since + HINT_OPEN_MS);
        let collapse = self
            .away_since
            .filter(|_| covered && self.revealed)
            .map(|since| since + HINT_CLOSE_MS);
        [open, close, name, reveal, collapse]
            .into_iter()
            .flatten()
            .min()
    }

    /// The shapes this frame painted solid, for the host's alpha mask. Empty before the first
    /// draw, which the host reads as "no coverage yet" and falls back to an opaque present.
    #[must_use]
    pub fn opaque_shapes(&self) -> &[(Rect, i32)] {
        &self.opaque
    }

    /// Back to the hint: the fan and the popover go with it, because a menu hanging off a bar
    /// that is no longer there is a menu over somebody's window.
    fn collapse(&mut self) {
        self.revealed = false;
        self.hint_since = None;
        self.away_since = None;
        self.hover = None;
        self.open_fan = None;
        self.left_at = None;
    }

    /// Fold `covered` and the pointer's dwells into this frame's presence.
    ///
    /// The pointer is "on the bar" when it is anywhere on the surface — the band above the strip
    /// included, or the popover would close under the pointer that opened it.
    fn update_presence(&mut self, ui: &Ui<'_>, area: Rect, data: &BarData<'_>) -> Presence {
        if !data.covered {
            // Nothing covers the strip. Forget the reveal, so the next window that does covers a
            // bar that starts from its hint rather than one already open.
            self.revealed = false;
            self.hint_since = None;
            self.away_since = None;
            return Presence::Open;
        }
        // The compositor's own dwell, arriving as an event. It opens the bar exactly as the
        // hint's dwell does — same flag, same way out — rather than as a state of its own.
        if data.peeked {
            self.revealed = true;
            self.hint_since = None;
            self.away_since = None;
            return Presence::Revealed;
        }
        let inside = ui.input.cursor().is_some_and(|(x, y)| area.contains(x, y));
        if self.revealed {
            if inside {
                self.away_since = None;
            } else {
                let away = *self.away_since.get_or_insert(data.now_ms);
                if data.now_ms.saturating_sub(away) >= HINT_CLOSE_MS {
                    self.collapse();
                    return Presence::Hint;
                }
            }
            return Presence::Revealed;
        }
        match ui.input.cursor().filter(|(x, y)| area.contains(*x, *y)) {
            Some((x, y)) => {
                // The dwell restarts wherever the pointer moves to. Resting is the condition, not
                // merely being low: a pointer crossing the bottom edge on its way somewhere else
                // never holds still long enough to ask for the bar.
                let (since, at_x, at_y) = *self.hint_since.get_or_insert((data.now_ms, x, y));
                if (x - at_x).abs() > HINT_REST_SLOP || (y - at_y).abs() > HINT_REST_SLOP {
                    self.hint_since = Some((data.now_ms, x, y));
                } else if data.now_ms.saturating_sub(since) >= HINT_OPEN_MS {
                    self.revealed = true;
                    self.hint_since = None;
                    self.away_since = None;
                    return Presence::Revealed;
                }
            }
            None => self.hint_since = None,
        }
        Presence::Hint
    }

    /// The hint: the bar's ground and one `^`, on a surface too thin to hold anything else.
    ///
    /// Every pixel is written, like every other state of this surface — it has no alpha, so a
    /// pixel this does not paint is a black line across the bottom of the screen. The chevron is
    /// a stair of small squares because the painter has no line primitive and a stair of three is
    /// legible at six logical pixels; a click anywhere on the strip reveals the bar at once,
    /// which is the only thing the hint can be asked to do.
    fn draw_hint(&mut self, ui: &mut Ui<'_>, area: Rect) {
        ui.painter.fill_rect(area, design::ground());
        self.opaque.clear();
        self.opaque.push((area, 0));
        let unit = ui.px(2).max(1);
        let centre = area.x + area.w / 2;
        // The chevron reads against the *ground*, not against a chip: `TAG_INK` is the ink of a
        // dark chip and would be white on the light theme's near-white bar. `text()` is the one
        // colour that is legible on the ground in both themes, which is the whole job here.
        let ink = if ui.input.cursor().is_some_and(|(x, y)| area.contains(x, y)) {
            design::accent()
        } else {
            design::text()
        };
        for step in 0..3 {
            let y = area.bottom() - (step + 1) * unit;
            let reach = (2 - step) * unit;
            ui.painter
                .fill_rect(Rect::new(centre - reach - unit, y, unit, unit), ink);
            ui.painter
                .fill_rect(Rect::new(centre + reach, y, unit, unit), ink);
        }
        if ui.input.take_click(area) {
            self.revealed = true;
            self.hint_since = None;
            self.away_since = None;
        }
    }

    fn layout(ui: &Ui<'_>, area: Rect, data: &BarData<'_>, open_fan: Option<usize>) -> Layout {
        let row_height = ui.px(BAR_HEIGHT).min(area.h);
        let row = Rect::new(area.x, area.bottom() - row_height, area.w, row_height);
        let chip_h = ui.px(CHIP_H).min(row.h);
        let chip_y = row.y + (row.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);
        let cell_w = ui.px(CELL);
        // With two rows the bar is the pair of them: what the fan and the ground are measured
        // from is the top of the upper one, and the fan has that much less room above it.
        let step = ui.px(extra_row(data.rows));
        let bar_height = (row_height + step).min(area.h);

        let (bar, cells, overflow, overflow_count, revealed) = if let Some(view) = data.rail {
            Self::rail_cells(ui, area, row, view, data, chip_y, chip_h)
        } else if let Some(rows) = data.rows {
            // Two lanes: active-lane chips on the bar's own row, the rest one step above. Each row is
            // centred in the same strip, whose width is the longer row's, so a chip does not
            // slide sideways when the other row grows or the surface widens for a fan.
            let strip = ui.px(island_width(rows.widest())).min(row.w);
            let origin = row.x + ((row.w - strip) / 2).max(0);
            let lay = |count: usize, y: i32| -> Vec<Rect> {
                let count = i32::try_from(count).unwrap_or(0);
                let span = count * cell_w + (count - 1).max(0) * gap;
                let start = origin + ((strip - span) / 2).max(0);
                (0..count)
                    .map(|slot| Rect::new(start + slot * (cell_w + gap), y, cell_w, chip_h))
                    .collect()
            };
            let mut cells = lay(rows.active, chip_y);
            cells.extend(lay(rows.latent, chip_y - step));
            let bar = Rect::new(row.x, row.y - step, row.w, row.h + step);
            (bar, cells, None, 0, None)
        } else {
            // The chips are centred *inside* the surface, not laid from its left edge. That
            // matters because an open row makes the surface wider than the island, and the
            // surface is centred on the output — so laying the chips from its left edge slid
            // every one of them sideways the moment a row opened, out from under the pointer
            // that opened it. The row would then close, the surface shrink, the chip come back,
            // and the whole thing cycle. Centring the strip means a change of surface width moves
            // nothing the pointer is on.
            //
            // Each chip is still clipped to whatever room is left rather than dropped: a
            // half-width chip shows an icon. The machine's own chips are not here at all — they
            // are the cluster's, on its own surface at the right-hand end of the screen.
            let bar = row;
            let strip = ui.px(island_width(data.entries.len())).min(bar.w);
            let origin = bar.x + ((bar.w - strip) / 2).max(0);
            let limit = origin + strip - ui.px(CHIP_PAD);
            let mut x = origin + ui.px(CHIP_PAD);
            let mut chip = |width: i32| {
                let rect = Rect::new(x, chip_y, width.min((limit - x).max(0)), chip_h);
                x += width + gap;
                rect
            };
            // Every entry is one square, active or not. A wide button carrying the active
            // window's title is the Windows taskbar's signature, and it is also the redundancy
            // this desktop spent a campaign removing: the window itself wears its name on its own
            // tag, so a second copy of it down here says nothing new and costs 220 pixels of the
            // switcher's room.
            //
            // When the surface is narrower than the chips, later cells used to clip to width 0
            // with no way to reach them. An overflow chip takes the last slot and opens the
            // overview.
            let (shown, has_overflow) =
                chip_slots(bar.w, data.entries.len(), cell_w, gap, ui.px(CHIP_PAD));
            let cells: Vec<Rect> = data
                .entries
                .iter()
                .take(shown)
                .map(|_| chip(cell_w))
                .collect();
            let overflow = has_overflow.then(|| chip(cell_w));
            let overflow_count = data.entries.len().saturating_sub(shown);
            (bar, cells, overflow, overflow_count, None)
        };

        let fan =
            match open_fan.and_then(|index| Some((cells.get(index)?, data.entries.get(index)?))) {
                Some((cell, entry)) if area.h > bar_height => {
                    let card_h = (area.h - bar_height - ui.px(8)).min(ui.px(FAN_BAND - 8));
                    let count = i32::try_from(fan_of(entry).len()).unwrap_or(1).max(1);
                    let gap = ui.px(FAN_GAP);
                    // Side by side and never over each other: when the surface cannot hold every
                    // card at full width (a narrow output), the cards narrow rather than overlap.
                    let card_w = ui
                        .px(FAN_CARD_W)
                        .min((area.w - (count - 1) * gap) / count)
                        .max(1);
                    let span = count * card_w + (count - 1) * gap;
                    // The row starts over its chip and slides left only as far as it has to in
                    // order to stay on the surface, rather than stretching it.
                    let start = cell.x.min(area.right() - span).max(area.x);
                    let y = bar.y - ui.px(4) - card_h;
                    // The most recent first, at the left, in reading order.
                    (0..count)
                        .map(|i| Rect::new(start + i * (card_w + gap), y, card_w, card_h))
                        .collect()
                }
                _ => Vec::new(),
            };

        Layout {
            bar,
            cells,
            overflow,
            overflow_count,
            fan,
            revealed,
        }
    }

    /// The rail's chips: laid once from the origin outward toward the rail's side, entry 0
    /// nearest the origin, so the first thing the pointer meets is the first thing in the list.
    ///
    /// Nothing here reads `view.revealed` except the returned interval: the chips are where they
    /// are for the whole opening and the reveal only moves a clip over them. A chip that moved
    /// with the pointer would be a target running away from the hand aiming at it.
    #[allow(clippy::type_complexity)]
    fn rail_cells(
        ui: &Ui<'_>,
        area: Rect,
        row: Rect,
        view: RailView,
        data: &BarData<'_>,
        chip_y: i32,
        chip_h: i32,
    ) -> (Rect, Vec<Rect>, Option<Rect>, usize, Option<(i32, i32)>) {
        let entries = data.entries.len();
        let step = ui.px(extra_row(data.rows));
        let cell_w = ui.px(CELL);
        let gap = ui.px(CHIP_GAP);
        let origin = area.x + ui.px(view.origin);
        let lead = ui.px(RAIL_GAP);
        let clearance = ui.px(CORNER_CLEARANCE);
        // The shorter of the two halves, so both rails of one output hold the same count and
        // agree with `rail_length`, which only knows the output's width.
        let room = (origin - lead - (area.x + clearance))
            .min(area.right() - clearance - (origin + lead))
            .max(0);
        // Two rows never overflow — `rows_of` only asks for them when each lane fits — so the
        // overflow chip and the prefix that `shown` implies belong to the one-row layout alone.
        let (shown, has_overflow) = if data.rows.is_some() {
            (entries, false)
        } else {
            chip_slots(room, entries, cell_w, gap, 0)
        };
        let place = |slot: usize, y: i32| {
            let along = i32::try_from(slot).unwrap_or(0) * (cell_w + gap);
            let x = match view.side {
                RailSide::Right => origin + lead + along,
                RailSide::Left => origin - lead - along - cell_w,
            };
            Rect::new(x, y, cell_w, chip_h)
        };
        let cells: Vec<Rect> = match data.rows {
            Some(rows) => (0..shown)
                .map(|index| {
                    if index < rows.active {
                        place(index, chip_y)
                    } else {
                        place(index - rows.active, chip_y - step)
                    }
                })
                .collect(),
            None => (0..shown).map(|slot| place(slot, chip_y)).collect(),
        };
        let overflow = has_overflow.then(|| place(shown, chip_y));
        // The plate: the origin to the far end of the last chip and its margin. Nothing is
        // painted on it; it is what a press between chips lands on.
        let far = cells
            .iter()
            .chain(&overflow)
            .fold(origin, |far, cell| match view.side {
                RailSide::Right => far.max(cell.right() + ui.px(CHIP_PAD)),
                RailSide::Left => far.min(cell.x - ui.px(CHIP_PAD)),
            });
        let bar = Rect::new(
            origin.min(far),
            row.y - step,
            (far - origin).abs(),
            row.h + step,
        );
        let revealed = view.physical_interval(area.x, |logical| ui.px(logical));
        (
            bar,
            cells,
            overflow,
            entries.saturating_sub(shown),
            Some(revealed),
        )
    }

    fn update_hover(&mut self, ui: &Ui<'_>, layout: &Layout, data: &BarData<'_>) {
        let cursor = ui.input.cursor();
        // A cell the rail has not revealed yet is not there to the pointer either: a dwell on a
        // chip nobody can see would open a fan out of nowhere.
        let over_cell = cursor.and_then(|(x, y)| {
            layout
                .cells
                .iter()
                .position(|c| c.contains(x, y) && layout.live(*c))
        });
        let over_fan = cursor.is_some_and(|(x, y)| layout.fan.iter().any(|r| r.contains(x, y)));
        match over_cell {
            Some(index) => {
                if self.hover.is_none_or(|(hovered, _)| hovered != index) {
                    self.hover = Some((index, data.now_ms));
                }
            }
            None => self.hover = None,
        }
        if let Some((index, since)) = self.hover
            && self.open_fan != Some(index)
            && data.now_ms.saturating_sub(since) >= HOVER_OPEN_MS
            && data.entries.get(index).is_some_and(fannable)
        {
            self.open_fan = Some(index);
            self.left_at = None;
        }
        if let Some(open) = self.open_fan {
            if over_cell == Some(open) || over_fan {
                self.left_at = None;
            } else {
                let left = *self.left_at.get_or_insert(data.now_ms);
                if data.now_ms.saturating_sub(left) >= HOVER_CLOSE_MS {
                    self.open_fan = None;
                    self.left_at = None;
                }
            }
        }
    }

    /// One row card's picture, above its title line: the window's application icon, centred.
    /// Window-pixel previews are Home-only — the bar asks for no capture and paints none.
    fn draw_card_picture(
        ui: &mut Ui<'_>,
        rect: Rect,
        entry: &Entry,
        window: &EntryWindow,
        data: &BarData<'_>,
    ) {
        let pad = ui.px(4);
        let title_h = ui.px(FAN_TITLE);
        let well = Rect::new(
            rect.x + pad,
            rect.y + pad,
            rect.w - 2 * pad,
            rect.h - 2 * pad - title_h,
        );
        if well.w <= 0 || well.h <= 0 {
            return;
        }
        ui.painter.fill_rounded_rect_aa(
            ui.kernel,
            well,
            ui.px(design::WELL_RADIUS) as f32,
            design::well_fill(),
            255,
        );
        ui.painter
            .stroke_rect(well, ui.scale.hairline(), design::edge());
        let side = ui.px(ICON).min(well.w).min(well.h).max(4);
        let icon = Rect::new(
            well.x + (well.w - side) / 2,
            well.y + (well.h - side) / 2,
            side,
            side,
        );
        // The window's own application first: in a group the cards are different applications.
        let bitmap = window
            .app_id
            .as_ref()
            .or(entry.app_id.as_ref())
            .and_then(|id| data.icons.get(id));
        if let Some(bitmap) = bitmap {
            ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
        } else if let Some(glyph) = window
            .app_id
            .as_deref()
            .or(entry.app_id.as_deref())
            .and_then(super::glyphs::glyph_for_app)
        {
            super::glyphs::draw(&mut ui.painter, ui.kernel, icon, glyph, design::TAG_INK);
        } else {
            ui.label(
                icon,
                &chip_initial(entry),
                design::text_faint(),
                Align::Centre,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &BarData<'_>) -> Vec<BarAction> {
        let mut actions = Vec::new();
        // Presence first: while a window covers the strip the surface is a few pixels tall, and
        // nothing of the bar exists — no rectangle to paint, and none to hit-test either, so a
        // click at the bottom of a covering window can never activate a chip that is not there.
        //
        // A rail has no presence at all: the host opened it on a gesture, and the host dismisses
        // it. No hint, no covered strip, no collapse on the pointer leaving the surface.
        if data.rail.is_none() && self.update_presence(ui, area, data) == Presence::Hint {
            self.draw_hint(ui, area);
            return actions;
        }
        let layout = Self::layout(ui, area, data, self.open_fan);
        self.update_hover(ui, &layout, data);
        let layout = Self::layout(ui, area, data, self.open_fan);

        // Every pixel of the surface, first, and the shapes that are actually solid recorded
        // beside it. The colour buffer has no alpha to give (pixelkit holds `0x00RRGGBB`), so the
        // host builds the coverage from `self.opaque` and hands the compositor an `Argb8888`
        // buffer; a frame that fills the whole surface with the ground and claims only the island
        // is solid is a frame the compositor blends into a floating island. When the ARGB present
        // is not available the same buffer is attached opaque, and the ground fill is what keeps
        // that honest: the chips then read as sitting on Home, which is what is behind them.
        ui.painter.fill_rect(area, design::ground());
        // The coverage is the chips themselves, not a plate behind them: "it shouldnt be a bar
        // but a collection of icons floating". Everything between and around them is desktop, so
        // the ground fill above exists only for the opaque fallback, where it is what makes the
        // chips read as sitting on Home rather than on a black block.
        let radius = ui.px(design::CHIP_RADIUS);
        self.opaque.clear();
        match layout.revealed {
            // A rail's row is only as solid as it is revealed: the chips are all laid out, and
            // the coverage is the curtain drawn over them. The overflow chip is part of the row.
            Some(interval) => self.opaque.extend(rail::clipped_shapes(
                layout
                    .cells
                    .iter()
                    .chain(&layout.overflow)
                    .map(|cell| (*cell, radius)),
                interval,
            )),
            None => self
                .opaque
                .extend(layout.cells.iter().map(|cell| (*cell, radius))),
        }

        // The row next: it floats over whatever is above the bar and claims its clicks before
        // the bar's own cells can.
        if let Some(index) = self.open_fan
            && let Some(entry) = data.entries.get(index)
        {
            let windows = fan_of(entry);
            let hovered = ui
                .input
                .cursor()
                .and_then(|(x, y)| layout.fan.iter().position(|rect| rect.contains(x, y)));
            for (position, (rect, window)) in layout.fan.iter().zip(&windows).enumerate() {
                // `design::shell` again: the tile's own radius, or its corners keep the shadow.
                self.opaque.push((*rect, ui.px(design::TILE_RADIUS)));
                let lifted = hovered == Some(position);
                design::shell(
                    ui,
                    *rect,
                    if lifted {
                        design::accent_soft()
                    } else {
                        design::shell_fill()
                    },
                    design::edge_strong(),
                    Elevation::Lifted,
                );
                Self::draw_card_picture(ui, *rect, entry, window, data);
                // Every card is named: nothing covers any of them, and the title is what tells
                // two windows of one application apart when their icons match.
                let inset = ui.px(6);
                let title = Rect::new(
                    rect.x + inset,
                    rect.bottom() - ui.px(FAN_TITLE + 4),
                    (rect.w - 2 * inset).max(0),
                    ui.px(FAN_TITLE),
                );
                ui.label_styled(
                    title,
                    &window.title,
                    design::state_style(ui),
                    if entry.is_parked(window.id) {
                        design::text_faint()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                super::window_visual::marks(
                    ui,
                    *rect,
                    entry.is_parked(window.id),
                    data.activated == Some(window.id),
                    window.verified,
                );
            }
            for (position, rect) in layout.fan.iter().enumerate() {
                if ui.input.take_click(*rect)
                    && let Some(window) = windows.get(position)
                {
                    // A parked card brings its window back exactly as a chip of one parked
                    // window does: unpark, then activate.
                    if entry.is_parked(window.id) {
                        actions.push(BarAction::Field(FieldAction::Restore(window.id)));
                    }
                    actions.push(BarAction::Activate(window.id));
                    self.open_fan = None;
                    self.left_at = None;
                    break;
                }
            }
        }

        // The taskbar: one chip per entry, in the same colours the compositor gives that window's
        // own name tag, so the two read as the same object.
        for (index, (cell, entry)) in layout.cells.iter().zip(data.entries).enumerate() {
            let live = layout.live(*cell);
            let hovered = (live && ui.input.hovering(*cell)) || self.open_fan == Some(index);
            let fill = match (entry.active, hovered) {
                (true, _) => design::accent_soft(),
                (false, true) => design::accent(),
                (false, false) => design::TAG_FILL,
            };
            chip(ui, *cell, fill);
            // Active is an underline rather than a box around the whole chip: the box read as a
            // second chip, and this is the same two pixels the window's own tag wears.
            if entry.active {
                let thick = ui.px(2).max(1);
                ui.painter.fill_rect(
                    Rect::new(
                        cell.x + ui.px(6),
                        cell.bottom() - thick,
                        (cell.w - ui.px(12)).max(1),
                        thick,
                    ),
                    design::accent(),
                );
            }
            // Centred in the cell: the chip is the picture, with the count badge on its corner.
            let icon_side = ui.px(ICON).min(cell.h - ui.px(6)).min(cell.w);
            let icon = Rect::new(
                cell.x + (cell.w - icon_side) / 2,
                cell.y + (cell.h - icon_side) / 2,
                icon_side,
                icon_side,
            );
            if matches!(entry.unit.key, templar_window::task::TaskKey::Group(_)) {
                let count = entry.windows.len().min(3);
                let side = (icon_side * 3 / 4).max(1);
                let step = (icon_side - side) / count.saturating_sub(1).max(1) as i32;
                for (index, member) in entry.windows.iter().take(count).enumerate().rev() {
                    let member_icon = Rect::new(
                        icon.x + index as i32 * step,
                        icon.y + index as i32 * step,
                        side,
                        side,
                    );
                    super::window_visual::icon(
                        ui,
                        member_icon,
                        member.app_id.as_deref(),
                        data.icons,
                    );
                }
            } else {
                super::window_visual::icon(ui, icon, entry.app_id.as_deref(), data.icons);
            }
            super::window_visual::marks(
                ui,
                *cell,
                entry.all_parked(),
                entry.active,
                entry.windows.iter().all(|window| window.verified),
            );
            // How many windows are behind this chip, groups included. One chip per unit is
            // deliberate (@templaros.decision.one-persistent-bottom-bar-tags-not-bands) and the
            // fan above the bar is how the rest are reached — but a chip that says nothing about
            // the second window is a window the person can only find by moving the one on top of
            // it. The count is what is *on screen*: a parked window is not behind this chip in
            // any sense a person can point at, and the dimming is what says some are.
            if entry.visible() > 1 {
                let side = ui.px(12).min(cell.h);
                let badge = Rect::new(icon.right() - side / 2, icon.y, side.min(cell.w), side);
                // Muted while some of the unit is parked: a chip reading 2 with three windows
                // behind it is telling the truth about the screen, and the colour says so.
                let some_parked = !entry.unit.shelved.is_empty();
                ui.painter.fill_rounded_rect(
                    badge,
                    ui.px(design::CHIP_RADIUS),
                    if some_parked {
                        design::edge_strong()
                    } else {
                        design::accent()
                    },
                );
                ui.label(
                    badge,
                    &entry.visible().to_string(),
                    design::TAG_INK,
                    Align::Centre,
                );
            }
            if let Some(accent) = entry.accent() {
                // A grouped entry keeps its group's stripe, the same one the window's own band
                // wears — at a chip's scale it runs down the left edge rather than the bottom.
                ui.painter.fill_rect(
                    Rect::new(
                        cell.x,
                        cell.y + ui.px(4),
                        ui.px(3),
                        (cell.h - ui.px(8)).max(1),
                    ),
                    accent,
                );
            }
            if live && ui.input.take_click(*cell) {
                match templar_window::task::chip_action(&entry.unit) {
                    // Nothing to choose between. A chip whose windows are all parked is this case
                    // too: the click brings the most recent of them back, which is the only thing
                    // "go to it" can mean when none of it is on screen.
                    templar_window::task::ChipAction::Direct => {
                        if let Some(window) = entry.most_recent() {
                            if entry.is_parked(window) {
                                actions.push(BarAction::Field(FieldAction::Restore(window)));
                            }
                            actions.push(BarAction::Activate(window));
                        }
                    }
                    templar_window::task::ChipAction::Fan => {
                        self.open_fan = if self.open_fan == Some(index) {
                            None
                        } else {
                            Some(index)
                        };
                        self.left_at = None;
                    }
                    // More than a fan can hold. The chip does not grow a menu: the compositor's
                    // overview shows them at the size they actually are.
                    templar_window::task::ChipAction::Taskfield => {
                        actions.push(BarAction::Taskfield(entry.unit.key.clone()));
                        self.open_fan = None;
                        self.left_at = None;
                    }
                }
            }
        }

        if let Some(cell) = layout.overflow {
            chip(ui, cell, design::TAG_FILL);
            ui.label(
                cell,
                &format!("+{}", layout.overflow_count),
                design::TAG_INK,
                Align::Centre,
            );
            if layout.live(cell) && ui.input.take_click(cell) {
                actions.push(BarAction::Overview);
                self.open_fan = None;
                self.left_at = None;
            }
        }

        // The name, on hover, for a chip that is otherwise only a picture. An icon is what the
        // user asked for — "rather than text icons, it should use the program icon of the opened
        // program, in a row. text hovertooltips with the name" — and a picture with no name is
        // exactly the mystery-meat button this desktop refuses everywhere else. The tooltip is the
        // answer to both: nothing is written until the pointer asks, and then it is written in
        // words. A chip whose row is open needs none, because the row is already saying it.
        if let Some((index, since)) = self.hover
            && self.open_fan.is_none()
            && data.now_ms.saturating_sub(since) >= TOOLTIP_MS
            && let Some((cell, entry)) = layout.cells.get(index).zip(data.entries.get(index))
        {
            // A chip with more windows than a fan can hold says how many, and says what clicking
            // it will do — the one chip on the bar whose click goes somewhere else entirely.
            let named = if templar_window::task::chip_action(&entry.unit)
                == templar_window::task::ChipAction::Taskfield
            {
                format!(
                    "{} \u{b7} {} windows \u{2014} show all",
                    entry.label,
                    entry.visible()
                )
            } else {
                entry.label.clone()
            };
            let text = named.as_str();
            let style = design::state_style(ui);
            let width = ui.text.measure(text, style) + ui.px(2 * TOOLTIP_PAD);
            let height = ui.px(TOOLTIP_HEIGHT);
            let frame = Rect::new(
                (cell.x + (cell.w - width) / 2).clamp(area.x, (area.right() - width).max(area.x)),
                cell.y - ui.px(TOOLTIP_GAP) - height,
                width,
                height,
            );
            if frame.y >= area.y {
                // `design::shell` fills at TILE_RADIUS; recording the smaller one would leave
                // the coverage solid where the paint has already curved away.
                self.opaque.push((frame, ui.px(design::TILE_RADIUS)));
                design::shell(
                    ui,
                    frame,
                    design::shell_fill(),
                    design::edge_strong(),
                    Elevation::Lifted,
                );
                ui.label_styled(
                    frame.inset(ui.px(TOOLTIP_PAD)),
                    text,
                    design::state_style(ui),
                    design::text(),
                    Align::Centre,
                );
            }
        }

        // A press on the island's own ground: the compositor gave this surface the keyboard for
        // it, and the activated window is the one that should have it. Reachable only where the
        // input region is, which is the chips — so in practice this is a press that landed on a
        // chip's rounded corner.
        //
        // On a rail the ground is the plate under the chips, and only as much of it as is shown.
        let ground = match layout.revealed {
            Some(interval) => rail::clip_to(layout.bar, interval),
            None => Some(layout.bar),
        };
        if let Some(ground) = ground
            && ui.input.take_click(ground)
            && let Some(window) = data.activated
        {
            actions.push(BarAction::Activate(window));
        }
        actions
    }

    /// Close the fan, the tooltip and the hover — everything the pointer opened. The host calls
    /// it when a rail closes, so the next opening starts from bare chips rather than from a fan
    /// left hanging off the last one.
    pub fn reset(&mut self) {
        self.collapse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taskbar;
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, MouseButton, Scale};
    use templar_window::{Layer, Rect as WRect, ShellToplevel};

    /// The output's logical width, which is what the host passes as `full_width`. A real screen,
    /// because the island's room is now `full_width` less twice the cluster's width — a centred
    /// island reaches a right-anchored cluster from both sides at once — and a toy width leaves it
    /// no room to be tested in.
    const W: i32 = 1920;

    /// The tallest the island ever gets: the bar, a second row and an open fan. The harness
    /// buffer is this tall so every state fits in one buffer, and the island is always drawn at
    /// the bottom of it, exactly as the host's surface is.
    fn tallest() -> i32 {
        BAR_HEIGHT + ROW_STEP + FAN_BAND
    }

    fn window(id: u64, app: &str, title: &str, activated: bool, maximized: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            claimed_app_id: None,
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            shelved: false,
            maximized,
            geometry: WRect::new(0, 0, 100, 100),
            layer: Layer::Normal,
            group: None,
            tab_count: None,
        }
    }

    struct Harness {
        screen: BarScreen,
        toplevels: Vec<ShellToplevel>,
        icons: HashMap<String, Bitmap>,
        text: pixelkit_text::TextCache,
        kernel: RasterKernel,
        buffer: WindowBuffer,
        input: Input,
        /// What the host would say about the bottom strip this frame (`app::bar_covered`).
        covered: bool,
        peeked: bool,
        now: u64,
        width: i32,
        height: i32,
        /// Draw as a rail rather than the island.
        rail: Option<RailView>,
        /// The activity lane mode. `Off` unless a test is about lanes, so the order every other
        /// test reads is the plain one.
        lanes: taskbar::LaneMode,
    }

    impl Harness {
        /// The application ids on the active lane: the fixtures' terminal.
        fn terminals() -> std::collections::HashSet<String> {
            [
                "org.example.Terminal".to_owned(),
                "org.example.Shell".to_owned(),
            ]
            .into()
        }

        /// The entries and two-row shape exactly as `app.rs` computes them for this frame.
        fn shape(&self) -> (Vec<Entry>, Option<Rows>) {
            let entries = taskbar::entries_in(&self.toplevels, self.lanes, &Self::terminals());
            let capacity = if self.rail.is_some() {
                rail_row_capacity(W)
            } else {
                island_row_capacity(W)
            };
            let rows = taskbar::rows_of(&entries, self.lanes, capacity);
            (entries, rows)
        }

        fn new(toplevels: Vec<ShellToplevel>) -> Self {
            Self {
                screen: BarScreen::default(),
                toplevels,
                icons: HashMap::new(),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(W as u32, tallest() as u32),
                input: Input::new(),
                covered: false,
                peeked: false,
                now: 0,
                width: W,
                height: BAR_HEIGHT,
                rail: None,
                lanes: taskbar::LaneMode::Off,
            }
        }

        fn activated(&self) -> Option<WindowId> {
            self.toplevels
                .iter()
                .find(|t| t.activated)
                .map(|t| t.window_id)
        }

        /// The surface the host would give the bar this frame — width included, exactly as
        /// `app.rs` computes it, so a test never clicks somewhere the real surface does not have.
        fn wanted(&self) -> (i32, i32) {
            let (entries, rows) = self.shape();
            let fan = self.screen.fan_cards(&entries);
            let tooltip = self.screen.tooltip_open(self.now);
            if self.rail.is_some() {
                return rail_wanted_size(fan, tooltip, rows, W);
            }
            wanted_size(
                self.screen.presence(self.covered),
                fan,
                tooltip,
                entries.len(),
                rows,
                W,
            )
        }

        /// Move the pointer to a point on the surface and draw, the way the host does on every
        /// `CursorMoved`.
        fn point_at(&mut self, x: i32, y: i32) -> Vec<BarAction> {
            self.input.cursor_moved(x as f32, y as f32);
            self.frame()
        }

        /// How wide the island is with this harness's windows on it.
        fn island(&self) -> i32 {
            let (entries, rows) = self.shape();
            island_width(rows.map_or(entries.len(), Rows::widest))
        }

        /// The middle of the hint tab's bottom edge — where the hint is, and where a pointer
        /// reaching for it lands. The tab is at the bottom-left corner now, not across the
        /// screen, so the middle of the *output* is no longer on it.
        fn bottom_middle() -> (i32, i32) {
            (HINT_WIDTH / 2, tallest() - 1)
        }

        fn frame(&mut self) -> Vec<BarAction> {
            let (entries, rows) = self.shape();
            let activated = self.activated();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                activated,
                covered: self.covered,
                peeked: self.peeked,
                home_revealed: false,
                now_ms: self.now,
                rail: self.rail,
                rows,
            };
            let (width, height) = (self.width, self.height);
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let top = tallest() - height;
            let actions = self
                .screen
                .draw(&mut ui, Rect::new(0, top, width, height), &data);
            drop(ui);
            self.input.end_frame();
            // The host resizes the surface the moment the answer changes; so does the harness,
            // so the next frame's rectangles are the ones a real click would land on.
            let (width, height) = self.wanted();
            self.width = width;
            self.height = height;
            actions
        }

        fn click(&mut self, x: i32, y: i32) -> Vec<BarAction> {
            self.input.cursor_moved(x as f32, y as f32);
            self.input.mouse(MouseButton::Left, true);
            self.input.mouse(MouseButton::Left, false);
            self.frame()
        }

        /// The rectangles this frame would use, for a test that needs to click one of them.
        fn rects(&mut self) -> Layout {
            let (entries, rows) = self.shape();
            let activated = self.activated();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                activated,
                covered: self.covered,
                peeked: self.peeked,
                home_revealed: false,
                now_ms: self.now,
                rail: self.rail,
                rows,
            };
            let ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let top = tallest() - self.height;
            BarScreen::layout(
                &ui,
                Rect::new(0, top, self.width, self.height),
                &data,
                self.screen.open_fan,
            )
        }

        fn cells(&mut self) -> Vec<Rect> {
            self.rects().cells
        }

        /// The middle of a rectangle, which is where a click belongs.
        fn centre(rect: Rect) -> (i32, i32) {
            (rect.x + rect.w / 2, rect.y + rect.h / 2)
        }
    }

    /// The bar surface has no alpha: softbuffer attaches an `Xrgb8888` shm buffer, so a pixel
    /// this paint does not write shows as black rather than as the desktop. Painting only the
    /// chips left a black block in the corner of the screen with the chips on it — the mess the
    /// user reported. Every pixel of the surface, in every state, must be written every frame.
    #[test]
    fn every_pixel_of_the_surface_is_painted() {
        const SENTINEL: u32 = 0x00ff_00ff;
        for (toplevels, covered) in [
            (Vec::new(), false),
            (
                vec![window(1, "org.example.Editor", "notes", true, true)],
                false,
            ),
            // The hint is the same surface, six pixels tall, and the same rule applies to it:
            // an unpainted row here is a black line across the bottom of the screen.
            (
                vec![window(1, "org.example.Editor", "notes", true, true)],
                true,
            ),
        ] {
            let mut h = Harness::new(toplevels);
            h.covered = covered;
            h.frame();
            let (width, height) = (h.width, h.height);
            h.buffer.pixels.fill(SENTINEL);
            h.frame();
            let stride = h.buffer.width as usize;
            let top = tallest() - height;
            for y in top..top + height {
                for x in 0..width {
                    assert_ne!(
                        h.buffer.pixels[y as usize * stride + x as usize],
                        SENTINEL,
                        "the bar left {x},{y} unpainted"
                    );
                }
            }
        }
    }

    #[test]
    fn a_one_window_entry_activates_on_click_and_a_two_window_entry_opens_its_fan() {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", false, false),
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.frame();
        let cells = h.cells();
        let editor = cells[0];
        let actions = h.click(editor.x + editor.w / 2, editor.y + editor.h / 2);
        assert_eq!(actions, vec![BarAction::Activate(WindowId(1))]);
        let files = cells[1];
        let actions = h.click(files.x + files.w / 2, files.y + files.h / 2);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(h.screen.open_fan, Some(1));
        // The host has already made room (the harness resizes exactly where `app.rs` does); the
        // top card is the most recent window, and clicking it activates it and closes the fan.
        assert_eq!(h.wanted().1, BAR_HEIGHT + FAN_BAND);
        h.frame();
        let top = h.rects().fan[0];
        let actions = h.click(top.x + top.w / 2, top.y + top.h / 2);
        assert_eq!(actions, vec![BarAction::Activate(WindowId(2))]);
        assert_eq!(h.screen.open_fan, None);
    }

    /// A terminal put away by the drop-down chord is parked, not gone. With one terminal on
    /// screen and two parked the chip used to count one window, act directly, and never open a
    /// row — the parked ones were unreachable from the bar. Now the row holds all three, on-screen
    /// first, and a parked card brings its window back.
    #[test]
    fn parked_windows_get_a_row_and_a_parked_card_restores_its_window() {
        let mut parked = window(3, "org.example.Terminal", "~/notes", false, false);
        parked.shelved = true;
        let mut also_parked = window(4, "org.example.Terminal", "~/src", false, false);
        also_parked.shelved = true;
        let mut h = Harness::new(vec![
            window(2, "org.example.Terminal", "~", false, false),
            parked,
            also_parked,
        ]);
        open_fan_of(&mut h, 0);
        let fan = h.rects().fan;
        assert_eq!(fan.len(), 3, "the parked windows are in the row");
        let (x, y) = Harness::centre(fan[0]);
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Activate(WindowId(2))],
            "the on-screen window comes first and is simply activated"
        );
        open_fan_of(&mut h, 0);
        let (x, y) = Harness::centre(h.rects().fan[1]);
        assert_eq!(
            h.click(x, y),
            vec![
                BarAction::Field(FieldAction::Restore(WindowId(3))),
                BarAction::Activate(WindowId(3)),
            ]
        );
    }

    #[test]
    fn hovering_a_multi_window_entry_opens_its_fan_after_the_dwell_and_leaving_closes_it() {
        let mut h = Harness::new(vec![
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.frame();
        let cell = h.cells()[0];
        h.input
            .cursor_moved((cell.x + 10) as f32, (cell.y + 10) as f32);
        h.frame();
        assert_eq!(h.screen.open_fan, None);
        let entries = taskbar::entries(&h.toplevels);
        // The name comes first: a chip says what it is a quarter of a second before it offers
        // its windows.
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false, h.now),
            Some(TOOLTIP_MS)
        );
        h.now = TOOLTIP_MS;
        h.frame();
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false, h.now),
            Some(HOVER_OPEN_MS),
            "and the fan's dwell is what is left to wait for"
        );
        h.now = HOVER_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.open_fan, Some(0));
        // Off the bar entirely: a grace period, then closed.
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.open_fan, Some(0));
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false, h.now),
            Some(HOVER_OPEN_MS + HOVER_CLOSE_MS)
        );
        h.now += HOVER_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.open_fan, None);
    }

    /// Open the fan of an entry the way a person does: rest on its chip until the dwell elapses.
    /// The host has resized by the time this returns, so the fan's rectangles are the ones a real
    /// click would land on. Re-aims at the chip after every frame.
    ///
    /// The re-aim is not modelling anything: the harness lays its chips from the buffer's own
    /// origin, so a chip's coordinates do not move when the surface resizes. On hardware they do,
    /// in both axes — the compositor re-sends the pointer's motion when the surface under it
    /// resizes (`space_sync.rs`), and centring the chip strip (`layout`) is what holds them still
    /// in the width. Neither is visible from here, which is why both are named in the runbook.
    fn open_fan_of(h: &mut Harness, index: usize) {
        for now in [0, 0, HOVER_OPEN_MS, HOVER_OPEN_MS] {
            h.now = now;
            let cell = h.cells()[index];
            h.input
                .cursor_moved((cell.x + 10) as f32, (cell.y + 10) as f32);
            h.frame();
        }
        assert_eq!(h.screen.open_fan, Some(index));
    }

    /// A fan card is the window, not its name — and when the capture has not arrived it is still
    /// Window-pixel previews are Home-only: both cards of one application wear the same
    /// centred icon, so their faces match — no per-window pixels — and neither is an empty box.
    #[test]
    fn a_fan_card_shows_its_window_icon_and_never_window_pixels() {
        let mut h = Harness::new(vec![
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        open_fan_of(&mut h, 0);
        let fan = h.rects().fan;
        assert_eq!(fan.len(), 2);
        let stride = h.buffer.width as usize;
        // The cards sit side by side, so each one's whole face is its own.
        let at = |rect: Rect, from_left: i32| {
            let (x, y) = (rect.x + from_left, rect.y + 4 + (rect.h - 8) / 2);
            h.buffer.pixels[y as usize * stride + x as usize] & 0x00ff_ffff
        };
        assert_eq!(
            at(fan[0], fan[0].w / 2),
            at(fan[1], fan[1].w / 2),
            "same application, same icon: no per-window pixels on a fan card"
        );
        assert_ne!(at(fan[0], fan[0].w / 2), 0, "an empty box is not a card");
    }

    /// The island is only as wide as its chips and the fan shares that width, so a fan that
    /// reached past it would be cards hanging off the end of the surface.
    #[test]
    fn the_open_fan_fits_inside_the_island() {
        for titles in [vec!["Downloads", "Pictures"], vec!["a", "b", "c", "d"]] {
            let toplevels = titles
                .iter()
                .enumerate()
                .map(|(index, title)| {
                    window(index as u64 + 2, "org.templaros.Files", title, false, false)
                })
                .collect();
            let mut h = Harness::new(toplevels);
            open_fan_of(&mut h, 0);
            let surface = h.wanted().0;
            let fan = h.rects().fan;
            assert_eq!(fan.len(), titles.len());
            for rect in &fan {
                assert!(
                    rect.x >= 0 && rect.right() <= surface,
                    "{rect:?} leaves the surface ({surface} wide) with {} windows",
                    titles.len()
                );
            }
            // Every card can be seen whole: the row used to stack them over each other.
            for pair in fan.windows(2) {
                assert!(
                    pair[0].right() < pair[1].x,
                    "{:?} and {:?} overlap or touch",
                    pair[0],
                    pair[1]
                );
            }
            assert!(
                surface >= h.island(),
                "an open fan widens the surface rather than squeezing its own cards"
            );
        }
    }

    /// The island reserves nothing, so a maximized window fills the output and wears its own tags
    /// and controls: the copy of them that used to sit at the right-hand end of the bar is gone,
    /// and so is the machine cluster — the island is open windows and nothing else.
    #[test]
    fn a_maximized_window_wears_its_own_controls_and_the_bar_carries_none() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, true)]);
        h.frame();
        assert_eq!(
            h.width,
            h.island(),
            "the island is its contents wide, not the output's"
        );
        let layout = h.rects();
        assert_eq!(
            layout.cells.len(),
            1,
            "one window, one chip, and no cluster"
        );
        // The ground beside the one chip is the island's own, and a press on it hands the keyboard
        // back to the activated window rather than closing or unmaximizing it.
        let ground = i32::midpoint(layout.cells[0].right(), layout.bar.right());
        assert_eq!(
            h.click(ground, layout.bar.y + layout.bar.h / 2),
            vec![BarAction::Activate(WindowId(1))]
        );
    }

    /// `TEMPLAR_BAR_BASELINE_DUMP=<path>` writes the bar with its row open as a PNG for review, the
    /// way `offscreen`'s Home baseline does; without it this only checks the frame draws.
    #[test]
    fn the_bar_renders_with_its_fan_open() {
        let mut active = window(1, "org.example.Editor", "notes — draft.md", true, true);
        active.group = None;
        let mut term = window(4, "org.example.Term", "sh", false, false);
        term.group = Some(templar_window::GroupInfo {
            group_id: 9,
            name: "Research".to_owned(),
            accent: 0x00b0_5a2c,
        });
        let mut h = Harness::new(vec![
            active,
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
            term,
        ]);
        h.screen.open_fan = Some(1);
        // One frame to let the harness take the size the open fan asks for, then the render.
        h.frame();
        h.frame();
        if let Ok(path) = std::env::var("TEMPLAR_BAR_BASELINE_DUMP") {
            crate::surface::offscreen::write_png(std::path::Path::new(&path), &h.buffer)
                .expect("dumping the bar render");
        }
        assert!(h.buffer.pixels.iter().any(|&p| p != 0));
    }

    /// The island is as wide as its contents and no wider — the user's words, "it shouldn't be
    /// stretching the full length of the screen". Only the band above it changes with a row or
    /// the popover, and the exclusive zone is 0 through all of it, so nothing here reflows a
    /// window.
    #[test]
    fn the_island_is_its_contents_wide_and_grows_upward_for_a_fan_or_the_popover() {
        let open = Presence::Open;
        let empty = island_width(0);
        assert!(
            empty < W / 2,
            "an empty island is not a bar: {empty} of {W}"
        );
        assert_eq!(island_width(3) - empty, 3 * (CELL + CHIP_GAP));

        // A strip too narrow for every chip keeps an overflow chip rather than clipping later
        // ones to zero. One slot is the overflow; the rest that fit stay full-width.
        let pad = CHIP_PAD;
        let (shown, overflow) = chip_slots(2 * pad + CELL, 10, CELL, CHIP_GAP, pad);
        assert!(overflow);
        assert_eq!(shown, 0, "one cell of room is the overflow chip");
        let (shown, overflow) =
            chip_slots(2 * pad + 3 * (CELL + CHIP_GAP), 10, CELL, CHIP_GAP, pad);
        assert!(overflow);
        assert_eq!(shown, 2);
        let (shown, overflow) =
            chip_slots(2 * pad + 10 * (CELL + CHIP_GAP), 10, CELL, CHIP_GAP, pad);
        assert!(!overflow);
        assert_eq!(shown, 10);

        assert_eq!(
            wanted_size(open, None, false, 0, None, W),
            (empty, BAR_HEIGHT)
        );
        // A row is wider than the island it hangs off, so it is the row that sets the width.
        let one_card = wanted_size(open, Some(1), false, 0, None, W);
        assert_eq!(one_card.1, BAR_HEIGHT + FAN_BAND);
        assert!(one_card.0 > empty, "an open row widens the surface");
        // Each further card costs a whole card and a gap: the cards sit side by side.
        assert_eq!(
            wanted_size(open, Some(4), false, 0, None, W).0 - one_card.0,
            3 * (FAN_CARD_W + FAN_GAP)
        );
        // An output narrower than the island clamps to the output *less its two corners*, which
        // belong to the compositor's hot corner rather than to the bar; a width of nothing is
        // still a surface.
        // An output with no room for both gives the island what is left and never a negative
        // width; a width of nothing is still a surface.
        assert_eq!(
            wanted_size(open, None, false, 20, None, 200),
            (1, BAR_HEIGHT)
        );
        assert_eq!(wanted_size(open, None, false, 0, None, 0), (1, BAR_HEIGHT));
        // However many applications are open, the island never reaches either corner.
        assert!(
            wanted_size(open, None, false, 200, None, 1280).0 <= 1280 - 2 * CORNER_CLEARANCE,
            "an island that spans the output would sit on the corner and silence it"
        );
        // Revealed is the island, in full; the hint is a tab, band or no band.
        assert_eq!(
            wanted_size(Presence::Revealed, Some(1), false, 0, None, W),
            one_card,
            "revealed is the island in full, fan and all"
        );
        assert_eq!(
            wanted_size(Presence::Hint, Some(1), false, 0, None, W),
            (HINT_WIDTH, HINT_HEIGHT)
        );
        const { assert!(HINT_HEIGHT < BAR_HEIGHT) };
    }

    /// A window over the island's strip collapses it to its corner tab; a pointer resting on the
    /// tab brings it back.
    #[test]
    fn a_covering_window_collapses_the_bar_to_a_hint_and_a_dwell_reveals_it() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.covered = true;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);
        assert_eq!(h.wanted(), (HINT_WIDTH, HINT_HEIGHT));

        let (x, y) = Harness::bottom_middle();
        h.point_at(x, y);
        assert_eq!(
            h.screen.presence(true),
            Presence::Hint,
            "the dwell has not elapsed"
        );
        let entries = taskbar::entries(&h.toplevels);
        assert_eq!(
            h.screen.next_deadline_ms(&entries, true, h.now),
            Some(HINT_OPEN_MS),
            "the host wakes when the dwell is up"
        );
        h.now = HINT_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT));
    }

    /// The coverage the host turns into the surface's alpha channel. It is the whole of what
    /// makes the island float, and it is not visible in a colour render, so it is asserted here:
    /// solid exactly where the island is, and nothing outside it.
    #[test]
    fn the_coverage_is_the_island_and_nothing_around_it() {
        let mut h = Harness::new(vec![
            window(1, "org.templaros.Files", "Downloads", true, false),
            window(2, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.frame();

        let shapes = h.screen.opaque_shapes().to_vec();
        // One entry, one chip, and no plate behind it: the island is its chips and nothing else.
        assert_eq!(shapes.len(), h.cells().len(), "{shapes:?}");
        assert!(shapes.iter().all(|(_, radius)| *radius > 0), "all rounded");
        let widest = shapes.iter().map(|(rect, _)| rect.w).max().unwrap();
        assert!(
            widest < h.island(),
            "no shape spans the island: it is chips, not a bar ({widest} of {})",
            h.island()
        );
        let plate = shapes[0].0;
        assert_eq!(plate.h, CHIP_H, "a chip's height, not the bar's");

        // Open a fan: its cards are solid too, and they are the only things above the chips
        // that are — the band they grow into is otherwise desktop.
        open_fan_of(&mut h, 0);
        let shapes = h.screen.opaque_shapes().to_vec();
        assert!(shapes.len() > h.cells().len(), "{shapes:?}");
        let card = shapes.last().expect("a fan card is recorded last").0;
        assert!(
            card.bottom() <= plate.y,
            "a fan card sits above the chips, not on them"
        );

        // The hint covers its whole tab and nothing else: a six-pixel surface has no room for a
        // shape, and an uncovered pixel there is a hole in the corner of the screen.
        h.covered = true;
        h.frame();
        h.point_at(HINT_WIDTH / 2, tallest() - 1);
        let shapes = h.screen.opaque_shapes().to_vec();
        assert_eq!(shapes.len(), 1, "{shapes:?}");
        assert_eq!(shapes[0].0.h, HINT_HEIGHT);
        assert_eq!(shapes[0].1, 0, "the tab is not rounded");
    }

    /// The chips do not move when the surface does. An open row makes the surface wider than the
    /// island, and the surface is centred on the output — so laying the chips from its left edge
    /// slid every one of them out from under the pointer that opened the row, which then closed,
    /// which shrank the surface, which brought the chip back: a cycle rather than a hover.
    #[test]
    fn opening_a_row_leaves_every_chip_where_it_was() {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.frame();
        // Where each chip is *on the output*: the surface is centred there, so a chip's screen
        // position is the surface's own offset plus its position inside it. That is the number a
        // pointer cares about, and the one that has to hold still.
        let on_screen = |h: &mut Harness| -> Vec<i32> {
            let offset = (W - h.wanted().0) / 2;
            h.cells().iter().map(|cell| offset + cell.x).collect()
        };
        let closed = on_screen(&mut h);
        let narrow = h.wanted().0;

        open_fan_of(&mut h, 1);
        assert!(
            h.wanted().0 > narrow,
            "the row is what makes the surface wider"
        );
        assert_eq!(
            on_screen(&mut h),
            closed,
            "a wider surface moved the chips out from under the pointer"
        );
    }

    /// A chip is a picture, so it has to be askable: resting on one names it, and the name is
    /// drawn on the surface rather than written into the chip, so a chip stays an icon.
    #[test]
    fn resting_on_a_chip_names_the_application() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.frame();
        let (x, y) = Harness::centre(h.cells()[0]);
        h.point_at(x, y);
        let before = h.screen.opaque_shapes().len();
        assert_eq!(before, h.cells().len(), "nothing named yet");

        h.now = TOOLTIP_MS;
        // Twice: the first frame is what makes the host grow the surface, and a name hung above
        // the chips has nowhere to be until it has — the same beat the hover row takes.
        h.frame();
        h.frame();
        let shapes = h.screen.opaque_shapes().to_vec();
        assert_eq!(shapes.len(), before + 1, "the tooltip is one more shape");
        let tip = shapes.last().expect("the tooltip is recorded last").0;
        assert!(tip.bottom() <= h.cells()[0].y, "it sits above the chip");
        assert_eq!(tip.h, TOOLTIP_HEIGHT, "and it is a line of text tall");

        // Away again, and the surface is chips only.
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.opaque_shapes().len(), before);
    }

    /// The user's words: the bar "popups up at inopportune times". The strip is the whole width
    /// of the output, so a pointer travelling along the bottom edge — to a maximized window's own
    /// controls, or thrown at a corner — satisfied a dwell that only asked it to be low. Resting
    /// is the condition: every move restarts the clock.
    #[test]
    fn a_pointer_travelling_along_the_bottom_edge_never_reveals_the_bar() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.covered = true;
        h.frame();
        let (_, y) = Harness::bottom_middle();
        // Four times the dwell, spent crossing the tab rather than resting on it.
        for step in 0..8_i32 {
            h.now = u64::try_from(step).unwrap() * HINT_OPEN_MS / 2;
            h.point_at(4 + step * (HINT_REST_SLOP + 1), y);
            assert_eq!(
                h.screen.presence(true),
                Presence::Hint,
                "a pointer still moving has not asked for the bar"
            );
        }
        // It stops. The dwell now runs from where it stopped, not from when it arrived.
        let resting = h.now;
        let x = 4 + 7 * (HINT_REST_SLOP + 1);
        h.now = resting + HINT_OPEN_MS - 1;
        h.point_at(x, y);
        assert_eq!(h.screen.presence(true), Presence::Hint);
        h.now = resting + HINT_OPEN_MS;
        h.point_at(x, y);
        assert_eq!(h.screen.presence(true), Presence::Revealed);
    }

    /// "it should hide as easily as it appears, if moused away from" — and the row and the
    /// popover go with it, because a menu hanging off a bar that is gone is a menu over a window.
    #[test]
    fn leaving_the_revealed_bar_collapses_it_and_closes_the_fan_and_the_popover() {
        let mut h = Harness::new(vec![
            window(2, "org.templaros.Files", "Downloads", false, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.covered = true;
        h.frame();
        let (x, y) = Harness::bottom_middle();
        h.point_at(x, y);
        h.now = HINT_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        h.screen.open_fan = Some(0);
        h.frame();

        h.input.cursor_left();
        h.frame();
        assert_eq!(
            h.screen.presence(true),
            Presence::Revealed,
            "the grace period has not elapsed"
        );
        let entries = taskbar::entries(&h.toplevels);
        assert_eq!(
            h.screen.next_deadline_ms(&entries, true, h.now),
            Some(HINT_OPEN_MS + HINT_CLOSE_MS)
        );
        h.now += HINT_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);
        assert_eq!(h.wanted(), (HINT_WIDTH, HINT_HEIGHT));
        assert_eq!(h.screen.open_fan, None, "the fan closed with it");
    }

    /// Nothing covers the strip: the bar is simply there, whatever the pointer does and however
    /// long it rests. And a reveal is forgotten when the covering window goes, so the next one
    /// covers a bar that starts from its hint again.
    #[test]
    fn an_uncovered_bar_is_open_whatever_the_pointer_does() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.frame();
        let (x, y) = Harness::bottom_middle();
        h.point_at(x, y);
        h.now = HINT_OPEN_MS * 10;
        h.frame();
        assert_eq!(h.screen.presence(false), Presence::Open);
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT));

        // Covered, revealed, then uncovered: the reveal does not survive the window leaving.
        h.covered = true;
        h.frame();
        h.now += HINT_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        h.covered = false;
        h.frame();
        assert_eq!(h.screen.presence(false), Presence::Open);
        h.covered = true;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);
    }

    /// The compositor's own dwell arrives as `bar_peeked` and opens the bar the same way the
    /// hint's dwell does — and, crucially, leaves the same way: through the grace period, not by
    /// snapping shut the instant the peek ends.
    ///
    /// Under a window filling the screen this is the only reveal that can happen at all. The
    /// pointer never reaches this surface to be seen arriving, and the hint tab it would have to
    /// arrive on is 72×6.
    #[test]
    fn a_peek_opens_the_bar_and_leaves_it_through_the_same_grace_period() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.covered = true;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);

        // No dwell of the shell's own, no pointer on the surface: the event alone opens it.
        h.peeked = true;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT));

        // The peek ends with the pointer off the surface. The bar does not vanish under it: the
        // same grace the hint's reveal has runs first.
        h.peeked = false;
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        h.now += HINT_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);
    }

    /// A fan is at most four cards. Above that a strip stops being a way to choose — the fifth
    /// window is a sliver of a sliver — so the chip stops offering one and hands the question to
    /// the compositor's overview instead.
    #[test]
    fn fan_caps_at_four() {
        use templar_window::task::{ChipAction, FAN_MAX, chip_action};

        for count in 2..=6u64 {
            let toplevels: Vec<ShellToplevel> = (0..count)
                .map(|i| window(i + 1, "org.templaros.Files", "a window", false, false))
                .collect();
            let mut h = Harness::new(toplevels);
            h.frame();
            let entries = taskbar::entries(&h.toplevels);
            let over_four = count as usize > FAN_MAX;
            assert_eq!(
                chip_action(&entries[0].unit) == ChipAction::Taskfield,
                over_four,
                "{count} windows"
            );

            let cell = h.cells()[0];
            let (x, y) = Harness::centre(cell);
            let actions = h.click(x, y);
            if over_four {
                // No fan at all: the click is the handover, and the surface never grew.
                assert_eq!(
                    actions,
                    vec![BarAction::Taskfield(entries[0].unit.key.clone())],
                    "{count} windows"
                );
                assert_eq!(h.screen.open_fan, None);
                assert_eq!(h.screen.fan_cards(&entries), None);
                assert_eq!(
                    h.wanted().1,
                    BAR_HEIGHT,
                    "no band for a fan that never opened"
                );
            } else {
                assert!(actions.is_empty(), "{count} windows: {actions:?}");
                assert_eq!(h.screen.open_fan, Some(0));
                assert_eq!(h.screen.fan_cards(&entries), Some(count as usize));
                h.frame();
                assert_eq!(h.rects().fan.len(), count as usize);
            }
        }
    }

    /// The chips are spatial memory. Using a window changes what is *inside* a chip — the fan's
    /// order, the badge, the underline — and must never change where the chips are: a strip that
    /// resorts itself on every click is a strip nobody can aim at.
    #[test]
    fn chip_order_stable_under_activation() {
        let mut h = Harness::new(vec![
            window(1, "org.templaros.Files", "Downloads", false, false),
            window(2, "org.example.Editor", "notes", true, false),
            window(3, "org.templaros.Files", "Pictures", false, false),
        ]);
        h.frame();
        let before: Vec<String> = taskbar::entries(&h.toplevels)
            .iter()
            .map(crate::taskbar::Entry::key)
            .collect();
        let cells = h.cells();

        // The editor loses activation to the second Files window, which also becomes the most
        // recent thing on the machine.
        h.toplevels[1].activated = false;
        h.toplevels[2].activated = true;
        h.toplevels[2].mru_rank = 0;
        h.frame();
        let entries = taskbar::entries(&h.toplevels);
        assert_eq!(
            entries
                .iter()
                .map(crate::taskbar::Entry::key)
                .collect::<Vec<_>>(),
            before
        );
        assert_eq!(h.cells(), cells, "a chip moved");
        // Inside the chip, though, recency moved: the fan offers the newest window first.
        assert_eq!(
            fan_of(&entries[0]).first().map(|window| window.id),
            Some(WindowId(3))
        );
    }

    /// A click on the hint is a person asking for the bar now, not in a hundred milliseconds.
    /// And it is the only thing a click on the hint can do: a chip's rectangle does not exist
    /// while the surface is six pixels tall, so nothing under the window can be activated by
    /// aiming at where a chip would have been.
    #[test]
    fn a_click_on_the_hint_reveals_at_once_and_activates_nothing() {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.templaros.Files", "Downloads", false, false),
        ]);
        h.frame();
        // Where the first chip is while the island is open — and where the pointer would aim if
        // it tried to click it through a covering window. The tab is under the island's left end,
        // so that aim lands on the hint rather than on nothing.
        let chip_x = Harness::centre(h.cells()[0]).0;
        h.covered = true;
        h.frame();
        assert_eq!(h.wanted(), (HINT_WIDTH, HINT_HEIGHT));
        let actions = h.click(chip_x, tallest() - 1);
        assert!(actions.is_empty(), "a hint has no chips: {actions:?}");
        assert_eq!(
            h.screen.presence(true),
            Presence::Revealed,
            "it reveals now"
        );
    }

    /// Three applications, one window each, on a rail of this side revealed this far.
    fn rail_harness(side: RailSide, revealed: i32) -> Harness {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.example.Terminal", "~", false, false),
            window(3, "org.example.Browser", "news", false, false),
        ]);
        h.rail = Some(RailView {
            origin: W / 2,
            side,
            revealed,
        });
        h.width = W;
        h
    }

    fn set_revealed(h: &mut Harness, revealed: i32) {
        if let Some(view) = h.rail.as_mut() {
            view.revealed = revealed;
        }
    }

    #[test]
    fn rail_chips_stay_put_while_the_reveal_grows() {
        for side in [RailSide::Left, RailSide::Right] {
            let mut h = rail_harness(side, 4);
            let early = h.cells();
            set_revealed(&mut h, rail_length(3, W));
            assert_eq!(h.cells(), early, "{side:?}");
            assert_eq!(early.len(), 3);
        }
    }

    #[test]
    fn rail_first_chip_sits_just_past_the_origin_on_its_side() {
        let origin = W / 2;
        let right = rail_harness(RailSide::Right, 0).cells();
        assert_eq!(right[0].x, origin + RAIL_GAP);
        assert!(right.windows(2).all(|pair| pair[1].x > pair[0].right()));
        let left = rail_harness(RailSide::Left, 0).cells();
        assert_eq!(left[0].right(), origin - RAIL_GAP);
        assert!(left.windows(2).all(|pair| pair[1].right() < pair[0].x));
    }

    #[test]
    fn rail_coverage_follows_the_reveal_and_ends_at_the_last_chip() {
        for side in [RailSide::Left, RailSide::Right] {
            let length = rail_length(3, W);
            let mut h = rail_harness(side, 0);
            let cells = h.cells();
            let far = cells.last().unwrap();
            // The length is exactly the last chip's far edge and a margin, from the origin.
            let reach = match side {
                RailSide::Right => far.right() + CHIP_PAD - W / 2,
                RailSide::Left => W / 2 - (far.x - CHIP_PAD),
            };
            assert_eq!(reach, length, "{side:?}");
            let mut last = 0;
            for revealed in (0..=length + 40).step_by(10) {
                set_revealed(&mut h, revealed);
                h.frame();
                let shapes = h.screen.opaque_shapes().to_vec();
                let covered: i32 = shapes.iter().map(|(rect, _)| rect.w).sum();
                assert!(covered >= last, "{side:?} shrank at {revealed}");
                last = covered;
                let (x0, x1) = RailView {
                    origin: W / 2,
                    side,
                    revealed,
                }
                .interval();
                for (rect, _) in &shapes {
                    assert!(
                        rect.x >= x0 && rect.right() <= x1,
                        "{rect:?} outside {x0}..{x1}"
                    );
                    assert!(
                        cells
                            .iter()
                            .any(|cell| cell.x <= rect.x && rect.right() <= cell.right())
                    );
                }
            }
            assert_eq!(
                last,
                cells.iter().map(|cell| cell.w).sum::<i32>(),
                "{side:?}"
            );
        }
    }

    #[test]
    fn rail_click_on_an_unrevealed_chip_does_nothing_until_it_is_revealed() {
        let mut h = rail_harness(RailSide::Right, rail_initial());
        let third = h.cells()[2];
        let target = taskbar::entries(&h.toplevels)[2].most_recent().unwrap();
        let (x, y) = Harness::centre(third);
        let actions = h.click(x, y);
        assert!(actions.is_empty(), "unrevealed chip answered: {actions:?}");
        set_revealed(&mut h, rail_length(3, W));
        let actions = h.click(x, y);
        assert_eq!(actions, vec![BarAction::Activate(target)]);
    }

    #[test]
    fn rail_ignores_covered_and_never_collapses_itself() {
        let mut h = rail_harness(RailSide::Left, rail_length(3, W));
        h.covered = true;
        h.frame();
        assert_eq!(h.screen.opaque_shapes().len(), 3, "no hint on a rail");
        h.input.cursor_left();
        h.now += 10 * HINT_CLOSE_MS;
        h.frame();
        assert_eq!(
            h.screen.opaque_shapes().len(),
            3,
            "the host dismisses a rail"
        );
        assert_eq!(h.wanted(), (W, BAR_HEIGHT));
    }

    /// Four applications, the second and fourth of them terminals, under `mode`.
    fn lane_harness(mode: taskbar::LaneMode) -> Harness {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.example.Terminal", "build", false, false),
            window(3, "org.example.Browser", "news", false, false),
            window(4, "org.example.Shell", "sh", false, false),
        ]);
        h.lanes = mode;
        // Let the surface settle at the size the lanes ask for, as the host does.
        h.frame();
        h
    }

    fn keys(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(Entry::key).collect()
    }

    #[test]
    fn contiguous_lanes_keep_terminal_chips_together_in_one_row() {
        let mut h = lane_harness(taskbar::LaneMode::Contiguous);
        let (entries, rows) = h.shape();
        assert_eq!(rows, None);
        assert_eq!(
            keys(&entries),
            vec![
                "org.example.Terminal",
                "org.example.Shell",
                "org.example.Editor",
                "org.example.Browser"
            ]
        );
        let cells = h.cells();
        assert!(
            cells.iter().all(|cell| cell.y == cells[0].y),
            "one row: {cells:?}"
        );
        assert_eq!(h.wanted().1, BAR_HEIGHT);
    }

    #[test]
    fn row_lanes_put_terminals_on_the_bars_own_row_and_the_rest_above() {
        let mut h = lane_harness(taskbar::LaneMode::Rows);
        let (_, rows) = h.shape();
        assert_eq!(
            rows,
            Some(Rows {
                active: 2,
                latent: 2
            })
        );
        assert_eq!(
            h.wanted().1,
            BAR_HEIGHT + ROW_STEP,
            "the surface has room for both rows"
        );
        let cells = h.cells();
        assert_eq!(cells.len(), 4);
        assert_eq!(cells[0].y, cells[1].y);
        assert_eq!(cells[2].y, cells[3].y);
        assert_eq!(
            cells[0].y - cells[2].y,
            ROW_STEP,
            "latent row one step above"
        );
        let layout = h.rects();
        assert_eq!(layout.bar.y, cells[2].y - (BAR_HEIGHT - CHIP_H) / 2);
        // Every chip is inside the surface the host asked for.
        let top = tallest() - h.wanted().1;
        assert!(cells.iter().all(|cell| cell.y >= top));
    }

    #[test]
    fn a_click_on_an_upper_row_chip_activates_that_chips_window() {
        let mut h = lane_harness(taskbar::LaneMode::Rows);
        let (entries, _) = h.shape();
        let cells = h.cells();
        // Index 3 is the last latent chip: Browser, on the upper row.
        let target = entries[3].most_recent().unwrap();
        assert_eq!(entries[3].key(), "org.example.Browser");
        let centre = cells[3];
        let actions = h.click(centre.x + centre.w / 2, centre.y + centre.h / 2);
        assert_eq!(actions, vec![BarAction::Activate(target)]);
    }

    #[test]
    fn row_lanes_with_only_one_lane_populated_are_one_row() {
        let mut h = lane_harness(taskbar::LaneMode::Rows);
        for top in &mut h.toplevels {
            top.canonical_app_id = Some("org.example.Editor".parse().unwrap());
        }
        let (_, rows) = h.shape();
        assert_eq!(rows, None);
        h.frame();
        assert_eq!(h.wanted().1, BAR_HEIGHT);
        let cells = h.cells();
        assert!(cells.iter().all(|cell| cell.y == cells[0].y));
    }

    #[test]
    fn rail_rows_lay_out_from_the_origin_and_never_move_with_the_reveal() {
        for side in [RailSide::Left, RailSide::Right] {
            let mut h = lane_harness(taskbar::LaneMode::Rows);
            h.rail = Some(RailView {
                origin: W / 2,
                side,
                revealed: 4,
            });
            h.width = W;
            h.frame();
            let early = h.cells();
            assert_eq!(early.len(), 4);
            assert_eq!(early[0].y - early[2].y, ROW_STEP, "{side:?}");
            // Both rows start at the same distance from the origin.
            let along = |cell: &Rect| match side {
                RailSide::Right => cell.x - W / 2,
                RailSide::Left => W / 2 - cell.right(),
            };
            assert_eq!(along(&early[0]), along(&early[2]), "{side:?}");
            assert_eq!(along(&early[0]), RAIL_GAP, "{side:?}");
            set_revealed(&mut h, rail_length(2, W));
            h.frame();
            assert_eq!(h.cells(), early, "{side:?}");
        }
    }

    #[test]
    fn a_lane_too_long_for_its_row_falls_back_to_one_row_with_an_overflow_chip() {
        let capacity = island_row_capacity(W);
        let mut tops: Vec<ShellToplevel> = (1..=u64::try_from(capacity).unwrap() + 3)
            .map(|id| window(id, &format!("org.example.App{id}"), "x", false, false))
            .collect();
        // A single terminal chip, and a latent lane longer than the row holds.
        tops[0].canonical_app_id = Some("org.example.Terminal".parse().unwrap());
        let mut h = Harness::new(tops);
        h.lanes = taskbar::LaneMode::Rows;
        let (entries, rows) = h.shape();
        assert_eq!(rows, None, "the latent lane does not fit a row");
        h.frame();
        assert_eq!(h.wanted().1, BAR_HEIGHT);
        let layout = h.rects();
        assert!(
            layout.overflow.is_some(),
            "the one-row layout carries the overflow chip"
        );
        assert_eq!(layout.cells.len() + layout.overflow_count, entries.len());
    }
}

#[cfg(test)]
mod initial_tests {
    use super::{chip_initial, first_letter};
    use crate::taskbar::{Entry, EntryWindow};
    use templar_window::WindowId;

    fn entry(app_id: Option<&str>, label: &str) -> Entry {
        Entry {
            unit: templar_window::task::TaskUnit {
                key: templar_window::task::TaskKey::Unknown(WindowId(1)),
                label: label.to_owned(),
                accent: None,
                windows: vec![WindowId(1)],
                shelved: Vec::new(),
            },
            label: label.to_owned(),
            app_id: app_id.map(ToOwned::to_owned),
            windows: vec![EntryWindow {
                id: WindowId(1),
                title: label.to_owned(),
                app_id: app_id.map(ToOwned::to_owned),
                verified: true,
            }],
            active: false,
            lane: crate::taskbar::Lane::Latent,
        }
    }

    /// A terminal's title is a path, and a path starts with a slash. The chip must never wear one.
    #[test]
    fn a_chip_takes_its_letter_from_the_application_and_never_from_punctuation() {
        assert_eq!(
            chip_initial(&entry(
                Some("desktop-import.kitty.desktop"),
                "/mnt/Samsung980_1TB/Rust-projects"
            )),
            "K"
        );
        // No launcher identity at all: the title is all there is, so skip to its first letter.
        assert_eq!(
            chip_initial(&entry(None, "/mnt/Samsung980_1TB/Rust-projects")),
            "M"
        );
        assert_eq!(chip_initial(&entry(None, "~/notes")), "N");
        assert_eq!(
            chip_initial(&entry(Some("org.templaros.Settings"), "…")),
            "S"
        );
        // Nothing usable anywhere still produces a mark rather than an empty plate.
        assert_eq!(chip_initial(&entry(None, "///")), "?");
        assert_eq!(first_letter("/// 7up"), Some('7'));
    }

    /// A group borrows a member's icon, never its member's letter: without a picture, the
    /// chip's initial is the group's own name.
    #[test]
    fn a_group_chip_without_a_picture_takes_its_letter_from_the_group() {
        let mut group = entry(Some("desktop-import.kitty.desktop"), "Writing");
        group.unit.key = templar_window::task::TaskKey::Group(templar_window::GroupId(7));
        assert_eq!(chip_initial(&group), "W");
    }
}
