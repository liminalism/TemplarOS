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
//! somewhere to go. The clock opens the installed calendar; the network chip opens the network
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

use lege_window::WindowId;

use crate::surface::design::{self, Elevation};
use crate::surface::field_screen::FieldAction;
use crate::taskbar::{Entry, EntryWindow};

/// The bar's thickness. It reserves nothing: the exclusive zone is 0.
pub const BAR_HEIGHT: i32 = 48;
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
/// The hover row's height, plus the gap between it and the bar.
///
/// The row used to be a list of names 40 pixels tall. It is a strip of the windows themselves
/// now, so it needs the height a picture needs — and it costs it only while the pointer rests on
/// a chip. The width is what went down: an item is [`ROW_ITEM_WIDTH`] rather than 220, because
/// the surface is a content-width island and the row shares its width, not the screen's.
pub const ROW_BAND: i32 = 104;
/// One row item's pitch. The item itself is [`ROW_GAP`] narrower, so two previews side by side
/// read as two windows rather than one strip — and on the ARGB surface that gutter is desktop.
const ROW_ITEM_WIDTH: i32 = 132;
const ROW_GAP: i32 = 4;
/// The title line under a row item's preview.
const ROW_TITLE: i32 = 16;
/// A chip's height, and the compositor's tag height: the same object on the bar as on the window.
pub const CHIP_H: i32 = lege_window::geometry::TAG_HEIGHT;
pub const CHIP_GAP: i32 = 8;
/// The margin at each end of the strip, and what centres a chip in the bar's own height.
pub const CHIP_PAD: i32 = 8;
const CELL: i32 = 48;
const ICON: i32 = 32;
/// Dwell before a hovered chip names itself. Shorter than the row's, because a name is a much
/// smaller thing to be given than a strip of window pictures, and a chip that is only an icon has
/// to be askable.
const TOOLTIP_MS: u64 = 250;
/// The tooltip's height, the padding inside it, and its gap above the chip.
const TOOLTIP_HEIGHT: i32 = 22;
const TOOLTIP_PAD: i32 = 6;
const TOOLTIP_GAP: i32 = 6;

/// Dwell before hovering an entry opens its row: long enough that sweeping the pointer across
/// the bar opens nothing.
pub const HOVER_OPEN_MS: u64 = 400;
/// Grace after leaving both the entry and its row before the row closes.
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
    /// Window captures by [`Entry`]'s `EntryWindow::preview` key — the same map Home's session
    /// tiles read. A key with nothing behind it is a preview that has not arrived (or previews
    /// turned off), and the row falls back to the application's icon and the title.
    pub previews: &'a HashMap<String, Bitmap>,
    /// The activated window: what a press on the bar's own ground gives the keyboard back to.
    pub activated: Option<WindowId>,
    /// Whether a window fills the screen, so the bar has to collapse to its hint rather than
    /// paint over it (`app::bar_covered`).
    pub covered: bool,
    /// Whether the compositor says the pointer is resting on that output's bottom edge
    /// (`lege_shell_manager_v1.bar_peeked`). One more way into the same reveal the hint's own
    /// dwell opens — and the only one that works under a window filling the screen, where the
    /// pointer never reaches this surface to be seen arriving. It comes *in* to `revealed` rather
    /// than round the side of `covered`, so the way out is the one grace period either way: a
    /// peek ended by a one-pixel overshoot would otherwise collapse the island instantly, while
    /// the same island reached from the hint takes `HINT_CLOSE_MS` to go.
    pub peeked: bool,
    /// Whether a "go home" reveal is in effect, so the Home chip can show that it is.
    pub home_revealed: bool,
    pub now_ms: u64,
}

#[derive(Debug, Default)]
pub struct BarScreen {
    /// The entry under the pointer and when it arrived there.
    hover: Option<(usize, u64)>,
    /// The entry whose row is open.
    pub open_row: Option<usize>,
    /// When the pointer left both the open entry and its row.
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
    /// The open row's items, one rect per window of the open entry.
    row: Vec<Rect>,
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
/// (`interfaces/wayland/lege-shell-v1.xml`, `set_exclusive_zone`).
pub fn wanted_size(
    presence: Presence,
    row_windows: Option<usize>,
    tooltip: bool,
    entries: usize,
    full_width: i32,
) -> (i32, i32) {
    if presence == Presence::Hint {
        // Hint-sized: the row is closed with the collapse, so there is never a band to make room
        // for here, and the tab is only as wide as a chevron needs.
        return (HINT_WIDTH.min(widest(full_width)), HINT_HEIGHT);
    }
    // An open row is wider than the island it hangs off — an island of one chip is 72 pixels and
    // a window's picture is 132. The surface grows to hold it and shrinks again when it closes;
    // the zone is 0 throughout, so neither move reflows anything. This is the one thing that ever
    // makes the surface wider than its own chips, and it is transient by construction.
    let row = row_windows.map_or(0, |windows| {
        2 * CHIP_PAD + i32::try_from(windows).unwrap_or(1).max(1) * (ROW_ITEM_WIDTH + ROW_GAP)
    });
    let band = if row_windows.is_some() {
        ROW_BAND
    } else if tooltip {
        TOOLTIP_HEIGHT + TOOLTIP_GAP
    } else {
        0
    };
    (
        island_width(entries).max(row).min(widest(full_width)),
        BAR_HEIGHT + band,
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
/// punctuation is skipped wherever it comes from.
fn chip_initial(entry: &Entry) -> String {
    let name = entry
        .app_id
        .as_deref()
        .and_then(|id| id.parse::<lege_types::AppId>().ok())
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

/// One chip's ground: the compositor's tag, rounded the way every other small shell object is.
pub fn chip(ui: &mut Ui<'_>, rect: Rect, fill: u32) {
    if rect.w <= 0 || rect.h <= 0 {
        return;
    }
    ui.painter
        .fill_rounded_rect(rect, ui.px(design::CHIP_RADIUS), fill);
}

impl BarScreen {
    /// Whether the host should make room for a row above the bar.
    pub fn row_open(&self) -> bool {
        self.open_row.is_some()
    }

    /// Whether a hovered chip is naming itself, so the host makes room above the island for it.
    /// A tooltip needs less room than a row and is not one: a chip's name is a line of text.
    #[must_use]
    pub fn tooltip_open(&self, now_ms: u64) -> bool {
        self.open_row.is_none()
            && self
                .hover
                .is_some_and(|(_, since)| now_ms.saturating_sub(since) >= TOOLTIP_MS)
    }

    /// How many windows the open row holds, for the surface size the host asks for. `None` when
    /// no row is open.
    #[must_use]
    pub fn row_windows(&self, entries: &[Entry]) -> Option<usize> {
        let index = self.open_row?;
        Some(entries.get(index)?.windows.len())
    }

    /// The windows whose previews the row wants captured: the open row's, and nothing else.
    ///
    /// Home stops capturing the moment a window covers it (`app::previews_covered`), which is
    /// exactly when this row is the thing being looked at — so the row asks on its own behalf.
    /// It is a slice of the open entry rather than a policy: empty means "ask for nothing", which
    /// is every frame in which no row is open.
    #[must_use]
    pub fn preview_wants<'a>(&self, entries: &'a [Entry]) -> &'a [EntryWindow] {
        self.open_row
            .and_then(|index| entries.get(index))
            .map_or(&[], |entry| entry.windows.as_slice())
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
            (self.open_row != Some(index)
                && entries
                    .get(index)
                    .is_some_and(|entry| entry.windows.len() > 1))
            .then_some(since + HOVER_OPEN_MS)
        });
        let close = self.left_at.map(|left| left + HOVER_CLOSE_MS);
        // Only while it is still coming: a deadline already in the past is a wake-up that
        // arrives, changes nothing and asks for itself again.
        let name = self
            .hover
            .filter(|_| self.open_row.is_none() && !self.tooltip_open(now_ms))
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

    /// Back to the hint: the row and the popover go with it, because a menu hanging off a bar
    /// that is no longer there is a menu over somebody's window.
    fn collapse(&mut self) {
        self.revealed = false;
        self.hint_since = None;
        self.away_since = None;
        self.hover = None;
        self.open_row = None;
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

    fn layout(ui: &Ui<'_>, area: Rect, data: &BarData<'_>, open_row: Option<usize>) -> Layout {
        let bar_height = ui.px(BAR_HEIGHT).min(area.h);
        let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
        let chip_h = ui.px(CHIP_H).min(bar.h);
        let chip_y = bar.y + (bar.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);

        // The chips are centred *inside* the surface, not laid from its left edge. That matters
        // because an open row makes the surface wider than the island, and the surface is centred
        // on the output — so laying the chips from its left edge slid every one of them sideways
        // the moment a row opened, out from under the pointer that opened it. The row would then
        // close, the surface shrink, the chip come back, and the whole thing cycle. Centring the
        // strip means a change of surface width moves nothing the pointer is on.
        //
        // Each chip is still clipped to whatever room is left rather than dropped: a half-width
        // chip shows an icon. The machine's own chips are not here at all — they are the
        // cluster's, on its own surface at the right-hand end of the screen.
        let strip = ui.px(island_width(data.entries.len())).min(bar.w);
        let origin = bar.x + ((bar.w - strip) / 2).max(0);
        let limit = origin + strip - ui.px(CHIP_PAD);
        let mut x = origin + ui.px(CHIP_PAD);
        let mut chip = |width: i32| {
            let rect = Rect::new(x, chip_y, width.min((limit - x).max(0)), chip_h);
            x += width + gap;
            rect
        };
        // Every entry is one square, active or not. A wide button carrying the active window's
        // title is the Windows taskbar's signature, and it is also the redundancy this desktop
        // spent a campaign removing: the window itself wears its name on its own tag, so a second
        // copy of it down here says nothing new and costs 220 pixels of the switcher's room.
        let cells: Vec<Rect> = data.entries.iter().map(|_| chip(ui.px(CELL))).collect();

        let row =
            match open_row.and_then(|index| Some((cells.get(index)?, data.entries.get(index)?))) {
                Some((cell, entry)) if area.h > bar_height => {
                    let item_h = (area.h - bar_height - ui.px(8)).min(ui.px(ROW_BAND - 8));
                    let count = i32::try_from(entry.windows.len()).unwrap_or(1).max(1);
                    // The island is only as wide as its chips, so a long row shares that width
                    // rather than stretching the surface back across the screen.
                    let pitch = ui.px(ROW_ITEM_WIDTH).min(area.w / count).max(ui.px(CELL));
                    let start = cell.x.min(area.right() - pitch * count).max(area.x);
                    // The pitch is the item plus its gutter; the item is what is painted and what
                    // is clicked, so the gutter belongs to neither.
                    let item_w = (pitch - ui.px(ROW_GAP)).max(1);
                    (0..count)
                        .map(|i| {
                            Rect::new(start + i * pitch, bar.y - ui.px(4) - item_h, item_w, item_h)
                        })
                        .collect()
                }
                _ => Vec::new(),
            };

        Layout { bar, cells, row }
    }

    fn update_hover(&mut self, ui: &Ui<'_>, layout: &Layout, data: &BarData<'_>) {
        let cursor = ui.input.cursor();
        let over_cell =
            cursor.and_then(|(x, y)| layout.cells.iter().position(|c| c.contains(x, y)));
        let over_row = cursor.is_some_and(|(x, y)| layout.row.iter().any(|r| r.contains(x, y)));
        match over_cell {
            Some(index) => {
                if self.hover.is_none_or(|(hovered, _)| hovered != index) {
                    self.hover = Some((index, data.now_ms));
                }
            }
            None => self.hover = None,
        }
        if let Some((index, since)) = self.hover
            && self.open_row != Some(index)
            && data.now_ms.saturating_sub(since) >= HOVER_OPEN_MS
            && data.entries.get(index).is_some_and(|e| e.windows.len() > 1)
        {
            self.open_row = Some(index);
            self.left_at = None;
        }
        if let Some(open) = self.open_row {
            if over_cell == Some(open) || over_row {
                self.left_at = None;
            } else {
                let left = *self.left_at.get_or_insert(data.now_ms);
                if data.now_ms.saturating_sub(left) >= HOVER_CLOSE_MS {
                    self.open_row = None;
                    self.left_at = None;
                }
            }
        }
    }

    /// One row item's picture: the window itself once a capture has arrived, and until then the
    /// application's icon — which is what the row was before previews, so a preview in flight is
    /// a legible item rather than an empty box.
    fn draw_row_preview(
        ui: &mut Ui<'_>,
        rect: Rect,
        entry: &Entry,
        window: &EntryWindow,
        data: &BarData<'_>,
    ) {
        let pad = ui.px(4);
        let well = Rect::new(
            rect.x + pad,
            rect.y + pad,
            rect.w - 2 * pad,
            rect.h - 2 * pad - ui.px(ROW_TITLE),
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
        if let Some(preview) = data.previews.get(&window.preview) {
            // ponytail: stretched into the well, exactly as Home's own tiles blit theirs. If a
            // squashed thumbnail ever reads wrong, the upgrade is letterboxing to the capture's
            // aspect inside this one rectangle.
            ui.painter
                .blit(preview, well.inset(ui.px(1)), ScaleFilter::Bilinear);
            return;
        }
        let side = ui.px(ICON).min(well.w).min(well.h).max(4);
        let icon = Rect::new(
            well.x + (well.w - side) / 2,
            well.y + (well.h - side) / 2,
            side,
            side,
        );
        if let Some(bitmap) = entry.app_id.as_ref().and_then(|id| data.icons.get(id)) {
            ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
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
        if self.update_presence(ui, area, data) == Presence::Hint {
            self.draw_hint(ui, area);
            return actions;
        }
        let layout = Self::layout(ui, area, data, self.open_row);
        self.update_hover(ui, &layout, data);
        let layout = Self::layout(ui, area, data, self.open_row);

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
        self.opaque
            .extend(layout.cells.iter().map(|cell| (*cell, radius)));

        // The row next: it floats over whatever is above the bar and claims its clicks before
        // the bar's own cells can.
        if let Some(index) = self.open_row
            && let Some(entry) = data.entries.get(index)
        {
            for (rect, window) in layout.row.iter().zip(&entry.windows) {
                // `design::shell` again: the tile's own radius, or its corners keep the shadow.
                self.opaque.push((*rect, ui.px(design::TILE_RADIUS)));
                let parked = entry.is_parked(window.id);
                let hovered = ui.input.hovering(*rect);
                design::shell(
                    ui,
                    *rect,
                    if hovered {
                        design::accent_soft()
                    } else {
                        design::shell_fill()
                    },
                    design::edge_strong(),
                    Elevation::Lifted,
                );
                Self::draw_row_preview(ui, *rect, entry, window, data);
                let title = Rect::new(
                    rect.x,
                    rect.bottom() - ui.px(ROW_TITLE + 4),
                    rect.w,
                    ui.px(ROW_TITLE),
                );
                ui.label_styled(
                    title.inset(ui.px(6)),
                    &window.title,
                    design::state_style(ui),
                    if parked {
                        design::text_faint()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                if ui.input.take_click(*rect) {
                    if parked {
                        actions.push(BarAction::Field(FieldAction::Restore(window.id)));
                    }
                    actions.push(BarAction::Activate(window.id));
                    self.open_row = None;
                    self.left_at = None;
                }
            }
        }

        // The taskbar: one chip per entry, in the same colours the compositor gives that window's
        // own name tag, so the two read as the same object.
        for (index, (cell, entry)) in layout.cells.iter().zip(data.entries).enumerate() {
            let hovered = ui.input.hovering(*cell) || self.open_row == Some(index);
            let (fill, ink) = match (entry.active, hovered) {
                (true, _) => (design::accent_soft(), design::text()),
                (false, true) => (design::accent(), design::TAG_INK),
                (false, false) => (design::TAG_FILL, design::TAG_INK),
            };
            // A parked window is still running and still reachable — it is just not on screen.
            // The bar is the only place it now exists, Home having stopped holding tiles for
            // running windows, so it is dimmed rather than dropped.
            let ink = if entry.all_parked() {
                design::text_faint()
            } else {
                ink
            };
            chip(ui, *cell, fill);
            if entry.active {
                ui.painter
                    .stroke_rect(*cell, ui.px(2).max(1), design::accent());
            }
            let icon_side = ui.px(ICON).min(cell.h - ui.px(6));
            let icon = Rect::new(
                cell.x + ui.px(4),
                cell.y + (cell.h - icon_side) / 2,
                icon_side.min(cell.w),
                icon_side,
            );
            if let Some(bitmap) = entry.app_id.as_ref().and_then(|id| data.icons.get(id)) {
                ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
            } else {
                let plate = entry.accent.unwrap_or(fill);
                ui.painter
                    .fill_rounded_rect(icon, ui.px(design::CHIP_RADIUS), plate);
                let mark = if entry.accent.is_some() {
                    entry.windows.len().to_string()
                } else {
                    chip_initial(entry)
                };
                ui.label(icon, &mark, ink, Align::Centre);
            }
            // How many windows are behind this chip. One entry per application is deliberate
            // (@legeos.decision.one-persistent-bottom-bar-tags-not-bands) and the row above the
            // bar is how the rest are reached — but a chip that says nothing about the second
            // window is a window the person can only find by moving the one on top of it. Groups
            // already carry their count as the icon mark, so this is the loose-entry case.
            if entry.accent.is_none() && entry.windows.len() > 1 {
                let side = ui.px(12).min(cell.h);
                let badge = Rect::new(icon.right() - side / 2, icon.y, side.min(cell.w), side);
                ui.painter
                    .fill_rounded_rect(badge, ui.px(design::CHIP_RADIUS), design::accent());
                ui.label(
                    badge,
                    &entry.windows.len().to_string(),
                    design::TAG_INK,
                    Align::Centre,
                );
            }
            if let Some(accent) = entry.accent {
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
            if ui.input.take_click(*cell) {
                if let Some(window) = entry.sole_window() {
                    if entry.is_parked(window) {
                        actions.push(BarAction::Field(FieldAction::Restore(window)));
                    }
                    actions.push(BarAction::Activate(window));
                } else {
                    self.open_row = if self.open_row == Some(index) {
                        None
                    } else {
                        Some(index)
                    };
                    self.left_at = None;
                }
            }
        }

        // The name, on hover, for a chip that is otherwise only a picture. An icon is what the
        // user asked for — "rather than text icons, it should use the program icon of the opened
        // program, in a row. text hovertooltips with the name" — and a picture with no name is
        // exactly the mystery-meat button this desktop refuses everywhere else. The tooltip is the
        // answer to both: nothing is written until the pointer asks, and then it is written in
        // words. A chip whose row is open needs none, because the row is already saying it.
        if let Some((index, since)) = self.hover
            && self.open_row.is_none()
            && data.now_ms.saturating_sub(since) >= TOOLTIP_MS
            && let Some((cell, entry)) = layout.cells.get(index).zip(data.entries.get(index))
        {
            let text = entry.label.as_str();
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
        if ui.input.take_click(layout.bar)
            && let Some(window) = data.activated
        {
            actions.push(BarAction::Activate(window));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taskbar;
    use lege_window::{Layer, Rect as WRect, ShellToplevel};
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, MouseButton, Scale};

    /// The output's logical width, which is what the host passes as `full_width`. A real screen,
    /// because the island's room is now `full_width` less twice the cluster's width — a centred
    /// island reaches a right-anchored cluster from both sides at once — and a toy width leaves it
    /// no room to be tested in.
    const W: i32 = 1920;

    /// The tallest the island ever gets: the bar plus an open hover row. The harness buffer is
    /// this tall so every state fits in one buffer, and the island is always drawn at the bottom
    /// of it, exactly as the host's surface is.
    fn tallest() -> i32 {
        BAR_HEIGHT + ROW_BAND
    }

    fn window(id: u64, app: &str, title: &str, activated: bool, maximized: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated,
            fullscreen: false,
            shelved: false,
            maximized,
            geometry: WRect::new(0, 0, 100, 100),
            layer: Layer::Normal,
            group: None,
        }
    }

    struct Harness {
        screen: BarScreen,
        toplevels: Vec<ShellToplevel>,
        icons: HashMap<String, Bitmap>,
        previews: HashMap<String, Bitmap>,
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
    }

    impl Harness {
        fn new(toplevels: Vec<ShellToplevel>) -> Self {
            Self {
                screen: BarScreen::default(),
                toplevels,
                icons: HashMap::new(),
                previews: HashMap::new(),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(W as u32, tallest() as u32),
                input: Input::new(),
                covered: false,
                peeked: false,
                now: 0,
                width: W,
                height: BAR_HEIGHT,
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
            wanted_size(
                self.screen.presence(self.covered),
                self.screen.row_windows(&taskbar::entries(&self.toplevels)),
                self.screen.tooltip_open(self.now),
                taskbar::entries(&self.toplevels).len(),
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
            island_width(taskbar::entries(&self.toplevels).len())
        }

        /// The middle of the hint tab's bottom edge — where the hint is, and where a pointer
        /// reaching for it lands. The tab is at the bottom-left corner now, not across the
        /// screen, so the middle of the *output* is no longer on it.
        fn bottom_middle() -> (i32, i32) {
            (HINT_WIDTH / 2, tallest() - 1)
        }

        fn frame(&mut self) -> Vec<BarAction> {
            let entries = taskbar::entries(&self.toplevels);
            let activated = self.activated();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                previews: &self.previews,
                activated,
                covered: self.covered,
                peeked: self.peeked,
                home_revealed: false,
                now_ms: self.now,
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
            let entries = taskbar::entries(&self.toplevels);
            let activated = self.activated();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                previews: &self.previews,
                activated,
                covered: self.covered,
                peeked: self.peeked,
                home_revealed: false,
                now_ms: self.now,
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
                self.screen.open_row,
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
    fn a_one_window_entry_activates_on_click_and_a_two_window_entry_opens_its_row() {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", false, false),
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
        ]);
        h.frame();
        let cells = h.cells();
        let editor = cells[0];
        let actions = h.click(editor.x + editor.w / 2, editor.y + editor.h / 2);
        assert_eq!(actions, vec![BarAction::Activate(WindowId(1))]);
        let files = cells[1];
        let actions = h.click(files.x + files.w / 2, files.y + files.h / 2);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(h.screen.open_row, Some(1));
        // The host has already made room (the harness resizes exactly where `app.rs` does); the
        // row's first item activates its window and closes the row.
        assert_eq!(h.wanted().1, BAR_HEIGHT + ROW_BAND);
        h.frame();
        let row_y = h.rects().row[0].y + 8;
        let actions = h.click(files.x + 20, row_y);
        assert_eq!(actions, vec![BarAction::Activate(WindowId(2))]);
        assert_eq!(h.screen.open_row, None);
    }

    #[test]
    fn hovering_a_multi_window_entry_opens_its_row_after_the_dwell_and_leaving_closes_it() {
        let mut h = Harness::new(vec![
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
        ]);
        h.frame();
        let cell = h.cells()[0];
        h.input
            .cursor_moved((cell.x + 10) as f32, (cell.y + 10) as f32);
        h.frame();
        assert_eq!(h.screen.open_row, None);
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
            "and the row's dwell is what is left to wait for"
        );
        h.now = HOVER_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        // Off the bar entirely: a grace period, then closed.
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false, h.now),
            Some(HOVER_OPEN_MS + HOVER_CLOSE_MS)
        );
        h.now += HOVER_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.open_row, None);
    }

    /// Open the row of the first entry the way a person does: rest on its chip until the dwell
    /// elapses. The host has resized by the time this returns, so the row's rectangles are the
    /// ones a real click would land on.
    /// Hover a chip until its row opens, re-aiming at it after every frame.
    ///
    /// The re-aim is not modelling anything: the harness lays its chips from the buffer's own
    /// origin, so a chip's coordinates do not move when the surface resizes. On hardware they do,
    /// in both axes — the host answers with `follow_bottom_anchored_resize` for the height, and
    /// centring the chip strip (`layout`) is what holds them still in the width. Neither is
    /// visible from here, which is why both are named in the runbook.
    fn open_row_of(h: &mut Harness, index: usize) {
        for now in [0, 0, HOVER_OPEN_MS, HOVER_OPEN_MS] {
            h.now = now;
            let cell = h.cells()[index];
            h.input
                .cursor_moved((cell.x + 10) as f32, (cell.y + 10) as f32);
            h.frame();
        }
        assert_eq!(h.screen.open_row, Some(index));
    }

    /// A row item is the window, not its name — and when the capture has not arrived it is still
    /// legible, because that is what the row was before previews. Both halves in one frame: the
    /// first window has a preview and the second has not.
    #[test]
    fn a_row_item_shows_its_window_preview_and_falls_back_without_one() {
        const CAPTURE: u32 = 0x0012_3456;
        let mut h = Harness::new(vec![
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
        ]);
        h.previews.insert(
            "window:2".to_owned(),
            Bitmap {
                width: 8,
                height: 8,
                // Opaque, like every capture `previews::ShmBuffer::read` produces: a bitmap with
                // no alpha blits to nothing.
                pixels: vec![0xff00_0000 | CAPTURE; 64],
            },
        );
        open_row_of(&mut h, 0);
        let row = h.rects().row;
        assert_eq!(row.len(), 2);
        let stride = h.buffer.width as usize;
        // The middle of each item's well: above the title line, inside the preview.
        let well_centre = |rect: Rect| {
            let (x, y) = (
                rect.x + rect.w / 2,
                rect.y + 4 + (rect.h - 8 - ROW_TITLE) / 2,
            );
            h.buffer.pixels[y as usize * stride + x as usize] & 0x00ff_ffff
        };
        assert_eq!(
            well_centre(row[0]),
            CAPTURE,
            "the window with a capture shows it"
        );
        assert_ne!(
            well_centre(row[1]),
            CAPTURE,
            "the window without one shows its own fallback, not the other window"
        );
        // And the fallback is not the raw surface either: the well is painted.
        assert_ne!(well_centre(row[1]), 0, "an empty box is not an item");
    }

    /// The island is only as wide as its chips and the row shares that width, so a row that
    /// reached past it would be a strip of previews hanging off the end of the surface.
    #[test]
    fn the_open_row_fits_inside_the_island() {
        for titles in [vec!["Downloads", "Pictures"], vec!["a", "b", "c", "d", "e"]] {
            let toplevels = titles
                .iter()
                .enumerate()
                .map(|(index, title)| {
                    window(index as u64 + 2, "org.legeos.Files", title, false, false)
                })
                .collect();
            let mut h = Harness::new(toplevels);
            open_row_of(&mut h, 0);
            let surface = h.wanted().0;
            let row = h.rects().row;
            assert_eq!(row.len(), titles.len());
            for rect in &row {
                assert!(
                    rect.x >= 0 && rect.right() <= surface,
                    "{rect:?} leaves the surface ({surface} wide) with {} windows",
                    titles.len()
                );
            }
            assert!(
                surface >= h.island(),
                "an open row widens the surface rather than squeezing its own tiles"
            );
        }
    }

    /// The row is the only thing that asks for a capture while a window covers Home, so it must
    /// ask for exactly the windows it is showing and stop the moment it closes.
    #[test]
    fn previews_are_asked_for_only_while_a_row_is_open() {
        let mut h = Harness::new(vec![
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
            window(4, "org.example.Editor", "notes", false, false),
        ]);
        h.frame();
        let entries = taskbar::entries(&h.toplevels);
        assert!(
            h.screen.preview_wants(&entries).is_empty(),
            "a closed row asks for nothing"
        );
        // The Files entry is second: applications sort by key, and `org.example.Editor` is the
        // one-window entry that opens no row at all.
        open_row_of(&mut h, 1);
        let wants: Vec<WindowId> = h
            .screen
            .preview_wants(&entries)
            .iter()
            .map(|window| window.id)
            .collect();
        assert_eq!(
            wants,
            vec![WindowId(2), WindowId(3)],
            "the open row's windows, and not the editor that is not in it"
        );
        h.input.cursor_left();
        h.frame();
        h.now += HOVER_CLOSE_MS;
        h.frame();
        assert!(
            h.screen.preview_wants(&entries).is_empty(),
            "the row closed, so nothing is being looked at"
        );
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

    /// `LEGE_BAR_BASELINE_DUMP=<path>` writes the bar with its row open as a PNG for review, the
    /// way `offscreen`'s Home baseline does; without it this only checks the frame draws.
    #[test]
    fn the_bar_renders_with_its_row_open() {
        let mut active = window(1, "org.example.Editor", "notes — draft.md", true, true);
        active.group = None;
        let mut term = window(4, "org.example.Term", "sh", false, false);
        term.group = Some(lege_window::GroupInfo {
            group_id: 9,
            name: "Research".to_owned(),
            accent: 0x00b0_5a2c,
        });
        let mut h = Harness::new(vec![
            active,
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
            term,
        ]);
        h.screen.open_row = Some(2);
        // One frame to let the harness take the size the open row asks for, then the render.
        h.frame();
        h.frame();
        if let Ok(path) = std::env::var("LEGE_BAR_BASELINE_DUMP") {
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
    fn the_island_is_its_contents_wide_and_grows_upward_for_a_row_or_the_popover() {
        let open = Presence::Open;
        let empty = island_width(0);
        assert!(
            empty < W / 2,
            "an empty island is not a bar: {empty} of {W}"
        );
        assert_eq!(island_width(3) - empty, 3 * (CELL + CHIP_GAP));

        assert_eq!(wanted_size(open, None, false, 0, W), (empty, BAR_HEIGHT));
        // A row is wider than the island it hangs off, so it is the row that sets the width.
        let one_row = wanted_size(open, Some(1), false, 0, W);
        assert_eq!(one_row.1, BAR_HEIGHT + ROW_BAND);
        assert!(one_row.0 > empty, "an open row widens the surface");
        assert!(
            wanted_size(open, Some(3), false, 0, W).0 > one_row.0,
            "and a longer row widens it further"
        );
        // An output narrower than the island clamps to the output *less its two corners*, which
        // belong to the compositor's hot corner rather than to the bar; a width of nothing is
        // still a surface.
        // An output with no room for both gives the island what is left and never a negative
        // width; a width of nothing is still a surface.
        assert_eq!(wanted_size(open, None, false, 20, 200), (1, BAR_HEIGHT));
        assert_eq!(wanted_size(open, None, false, 0, 0), (1, BAR_HEIGHT));
        // However many applications are open, the island never reaches either corner.
        assert!(
            wanted_size(open, None, false, 200, 1280).0 <= 1280 - 2 * CORNER_CLEARANCE,
            "an island that spans the output would sit on the corner and silence it"
        );
        // Revealed is the island, in full; the hint is a tab, band or no band.
        assert_eq!(
            wanted_size(Presence::Revealed, Some(1), false, 0, W),
            one_row,
            "revealed is the island in full, row and all"
        );
        assert_eq!(
            wanted_size(Presence::Hint, Some(1), false, 0, W),
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
            window(1, "org.legeos.Files", "Downloads", true, false),
            window(2, "org.legeos.Files", "Pictures", false, false),
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

        // Open a hover row: its tiles are solid too, and they are the only things above the
        // chips that are — the band they grow into is otherwise desktop.
        open_row_of(&mut h, 0);
        let shapes = h.screen.opaque_shapes().to_vec();
        assert!(shapes.len() > h.cells().len(), "{shapes:?}");
        let tile = shapes.last().expect("a row tile is recorded last").0;
        assert!(
            tile.bottom() <= plate.y,
            "a row tile sits above the chips, not on them"
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
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
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

        open_row_of(&mut h, 1);
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
    fn leaving_the_revealed_bar_collapses_it_and_closes_the_row_and_the_popover() {
        let mut h = Harness::new(vec![
            window(2, "org.legeos.Files", "Downloads", false, false),
            window(3, "org.legeos.Files", "Pictures", false, false),
        ]);
        h.covered = true;
        h.frame();
        let (x, y) = Harness::bottom_middle();
        h.point_at(x, y);
        h.now = HINT_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        h.screen.open_row = Some(0);
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
        assert_eq!(h.screen.open_row, None, "the row closed with it");
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

    /// A click on the hint is a person asking for the bar now, not in a hundred milliseconds.
    /// And it is the only thing a click on the hint can do: a chip's rectangle does not exist
    /// while the surface is six pixels tall, so nothing under the window can be activated by
    /// aiming at where a chip would have been.
    #[test]
    fn a_click_on_the_hint_reveals_at_once_and_activates_nothing() {
        let mut h = Harness::new(vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.legeos.Files", "Downloads", false, false),
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
}

#[cfg(test)]
mod initial_tests {
    use super::{chip_initial, first_letter};
    use crate::taskbar::{Entry, EntryWindow};
    use lege_window::WindowId;

    fn entry(app_id: Option<&str>, label: &str) -> Entry {
        Entry {
            key: label.to_owned(),
            label: label.to_owned(),
            accent: None,
            app_id: app_id.map(ToOwned::to_owned),
            windows: vec![EntryWindow {
                id: WindowId(1),
                title: label.to_owned(),
                preview: "window:1".to_owned(),
            }],
            parked: Vec::new(),
            active: false,
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
        assert_eq!(chip_initial(&entry(Some("org.legeos.Settings"), "…")), "S");
        // Nothing usable anywhere still produces a mark rather than an empty plate.
        assert_eq!(chip_initial(&entry(None, "///")), "?");
        assert_eq!(first_letter("/// 7up"), Some('7'));
    }
}
