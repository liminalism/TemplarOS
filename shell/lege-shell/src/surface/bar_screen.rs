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
use crate::taskbar::Entry;

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
pub const ROW_BAND: i32 = 40;
const ROW_ITEM_WIDTH: i32 = 220;
/// A chip's height, and the compositor's tag height: the same object on the bar as on the window.
const CHIP_H: i32 = lege_window::geometry::TAG_HEIGHT;
const CHIP_GAP: i32 = 8;
/// The margin at each end of the strip, and what centres a chip in the bar's own height.
const CHIP_PAD: i32 = 8;
const CELL: i32 = 48;
const ICON: i32 = 32;
/// The Settings chip — the whole of Settings, not one corner of it. It took the readings chip's
/// place in the cluster.
const SETTINGS_WIDTH: i32 = 112;
const NETWORK_WIDTH: i32 = 96;
const CLOCK_WIDTH: i32 = 160;
/// The account chip: the account's name, its mark, and the popover behind it.
const ACCOUNT_WIDTH: i32 = 140;
/// The account popover's width and one row's height, and the padding inside its frame.
const POPOVER_WIDTH: i32 = 232;
const POPOVER_ROW: i32 = 28;
const POPOVER_PAD: i32 = 8;
/// The gap between the popover's frame and the bar it grows out of.
const POPOVER_GAP: i32 = 6;

/// The account popover's rows, in order.
///
/// Written out as literal `RunAction` constructions rather than as a table of bare ids on
/// purpose: that spelling is what `xtask check-actions` reads as a pointer route, and this popover
/// is now the only pointer route to the session and machine actions — Home no longer carries the
/// Session or Settings widget.
///
/// `settings.open` is not here any more: it has its own chip in the cluster, and an action offered
/// in two places is one a person has to look for twice.
const ACCOUNT_ROWS: [FieldAction; 10] = [
    FieldAction::RunAction("volume.decrease"),
    FieldAction::RunAction("volume.mute"),
    FieldAction::RunAction("volume.increase"),
    FieldAction::RunAction("brightness.decrease"),
    FieldAction::RunAction("brightness.increase"),
    FieldAction::RunAction("session.lock"),
    FieldAction::RunAction("session.suspend"),
    FieldAction::RunAction("session.logout"),
    FieldAction::RunAction("session.reboot"),
    FieldAction::RunAction("session.poweroff"),
];

/// The action id a popover row performs. The table above is built entirely of `RunAction`s.
fn row_id(row: &FieldAction) -> &'static str {
    match row {
        FieldAction::RunAction(id) => id,
        _ => "",
    }
}

/// How far above the bar the account popover reaches, gap included. The readings line is the
/// first row and costs a row's height like any other.
fn popover_band() -> i32 {
    POPOVER_GAP + 2 * POPOVER_PAD + (ACCOUNT_ROWS.len() as i32 + 1) * POPOVER_ROW
}
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
    pub clock_label: &'a str,
    /// Whether the network control's role has a provider that can be launched.
    pub network_control_available: bool,
    /// Whether an installed application declares `Categories=Calendar`, so the clock has
    /// somewhere to open into.
    pub calendar_available: bool,
    /// Who is signed in: what the account chip says next to its mark.
    pub account_label: &'a str,
    /// The activated window: what a press on the bar's own ground gives the keyboard back to.
    pub activated: Option<WindowId>,
    /// Whether something covers the bottom `BAR_HEIGHT` strip of this output, so the bar has to
    /// collapse to its hint rather than paint over a window. The host decides it
    /// (`app::bar_covered`): unshelved toplevels, `home_revealed`, and the output's rect.
    pub covered: bool,
    /// Whether a "go home" reveal is in effect, so the Home chip can show that it is.
    pub home_revealed: bool,
    /// Charge, volume and backlight, as the services last reported them.
    pub machine: &'a crate::data::MachineReadings,
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
    /// Whether the account popover is open.
    pub account_open: bool,
    /// While covered: whether the pointer has revealed the bar out from under the window.
    revealed: bool,
    /// While covered and not revealed: when the pointer came to rest on the hint, and where.
    hint_since: Option<(u64, i32, i32)>,
    /// While revealed: when the pointer left the surface entirely.
    away_since: Option<u64>,
    /// The popover row armed by a first click and waiting for its second.
    ///
    /// `ActionDecl::confirm` is set on the three actions that end the session, and a bar chip is
    /// exactly the sort of thing a sleeve catches: without this, brushing the popover would shut
    /// the machine down.
    confirming: Option<&'static str>,
}

struct Layout {
    bar: Rect,
    cells: Vec<Rect>,
    /// The machine cluster, right to left: account, clock, network, Settings.
    settings: Rect,
    network: Rect,
    clock: Rect,
    account: Rect,
    /// The open row's items, one rect per window of the open entry.
    row: Vec<Rect>,
    /// The account popover's frame and its rows, when it is open and the host has made room.
    popover: Option<(Rect, Vec<Rect>)>,
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
    row_open: bool,
    account_open: bool,
    entries: usize,
    full_width: i32,
) -> (i32, i32) {
    if presence == Presence::Hint {
        // Hint-sized: the row and the popover are closed with the collapse, so there is never a
        // band to make room for here, and the tab is only as wide as a chevron needs.
        return (HINT_WIDTH.min(full_width.max(1)), HINT_HEIGHT);
    }
    let band = match (row_open, account_open) {
        (_, true) => popover_band(),
        (true, false) => ROW_BAND,
        (false, false) => 0,
    };
    (
        island_width(entries).min(full_width.max(1)),
        BAR_HEIGHT + band,
    )
}

/// How wide the island is: its two margins, one square per entry, the gap that separates the
/// switcher from the machine, and the cluster.
///
/// The surface is this wide and no wider. It used to be the output's whole width, which made a
/// strip of chips read as a bar across the bottom of the screen — the thing the person is trying
/// not to have. The zone claim anchors a bottom-edge surface at the start of its edge and leaves
/// its extent alone (`shell_surface_placement`), so a narrower surface simply sits at the
/// bottom-left and the desktop is visible either side of it.
#[must_use]
pub fn island_width(entries: usize) -> i32 {
    let entries = i32::try_from(entries).unwrap_or(i32::MAX);
    let switcher = entries * (CELL + CHIP_GAP);
    let cluster = SETTINGS_WIDTH + NETWORK_WIDTH + CLOCK_WIDTH + ACCOUNT_WIDTH + 3 * CHIP_GAP;
    2 * CHIP_PAD + switcher + cluster
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

/// The popover's first line: charge, volume and backlight, as the services last reported them.
/// A reading with no hardware behind it is left out rather than shown as a dash — a machine with
/// no backlight has nothing to say about brightness.
fn readings_line(machine: &crate::data::MachineReadings) -> String {
    let mut parts = vec![machine.chip_label()];
    if let Some(audio) = machine.audio.as_ref().filter(|audio| audio.present) {
        parts.push(format!("Volume {}%", audio.volume_percent));
    }
    if let Some(backlight) = machine.backlight.as_ref().filter(|light| light.present) {
        parts.push(format!("Brightness {}%", backlight.percent));
    }
    parts.join(" \u{b7} ")
}

/// One chip's ground: the compositor's tag, rounded the way every other small shell object is.
fn chip(ui: &mut Ui<'_>, rect: Rect, fill: u32) {
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
    pub fn next_deadline_ms(&self, entries: &[Entry], covered: bool) -> Option<u64> {
        let open = self.hover.and_then(|(index, since)| {
            (self.open_row != Some(index)
                && entries
                    .get(index)
                    .is_some_and(|entry| entry.windows.len() > 1))
            .then_some(since + HOVER_OPEN_MS)
        });
        let close = self.left_at.map(|left| left + HOVER_CLOSE_MS);
        let reveal = self
            .hint_since
            .filter(|_| covered && !self.revealed)
            .map(|(since, _, _)| since + HINT_OPEN_MS);
        let collapse = self
            .away_since
            .filter(|_| covered && self.revealed)
            .map(|since| since + HINT_CLOSE_MS);
        [open, close, reveal, collapse].into_iter().flatten().min()
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
        self.account_open = false;
        self.confirming = None;
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

    fn layout(
        ui: &Ui<'_>,
        area: Rect,
        data: &BarData<'_>,
        open_row: Option<usize>,
        account_open: bool,
    ) -> Layout {
        let bar_height = ui.px(BAR_HEIGHT).min(area.h);
        let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
        let chip_h = ui.px(CHIP_H).min(bar.h);
        let chip_y = bar.y + (bar.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);

        // The right-hand end first, because everything there is anchored to it: the machine
        // cluster, right to left. The taskbar then fills whatever is left, so a screenful of
        // windows crowds itself rather than sliding under the clock.
        //
        // The maximized window's controls used to own the very end of this bar. They are gone:
        // with no exclusive zone a maximized window fills the output and wears its own tags and
        // controls, so a second copy of them down here was the same buttons twice.
        let mut right = bar.right() - ui.px(CHIP_PAD);
        let mut cluster = |width: i32| {
            let rect = Rect::new(right - width, chip_y, width, chip_h);
            right = rect.x - gap;
            rect
        };
        let account = cluster(ui.px(ACCOUNT_WIDTH));
        let clock = cluster(ui.px(CLOCK_WIDTH));
        let network = cluster(ui.px(NETWORK_WIDTH));
        let settings = cluster(ui.px(SETTINGS_WIDTH));
        let limit = right;

        // Then left to right from the bar's own left edge. Each chip is clipped to whatever room
        // is left rather than being dropped: a half-width chip still shows an icon.
        let mut x = bar.x + ui.px(CHIP_PAD);
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
                    let item_w = ui.px(ROW_ITEM_WIDTH).min(area.w / count).max(ui.px(CELL));
                    let start = cell.x.min(area.right() - item_w * count).max(area.x);
                    (0..count)
                        .map(|i| {
                            Rect::new(
                                start + i * item_w,
                                bar.y - ui.px(4) - item_h,
                                item_w,
                                item_h,
                            )
                        })
                        .collect()
                }
                _ => Vec::new(),
            };

        // The popover hangs off the account chip's right edge and grows upward into the band the
        // host made for it — the same trick the hover row uses, and for the same reason: the zone
        // is a floor, so growing over the windows above moves none of them.
        let popover = (account_open && area.h > bar_height).then(|| {
            let width = ui.px(POPOVER_WIDTH).min(area.w);
            let row_h = ui.px(POPOVER_ROW);
            let pad = ui.px(POPOVER_PAD);
            let height =
                (2 * pad + (ACCOUNT_ROWS.len() as i32 + 1) * row_h).min(area.h - bar_height);
            let frame = Rect::new(
                (account.right() - width).max(area.x),
                bar.y - ui.px(POPOVER_GAP) - height,
                width,
                height,
            );
            let rows = (0..=ACCOUNT_ROWS.len() as i32)
                .map(|i| {
                    Rect::new(
                        frame.x + pad,
                        frame.y + pad + i * row_h,
                        (frame.w - 2 * pad).max(0),
                        row_h,
                    )
                })
                .collect();
            (frame, rows)
        });

        Layout {
            bar,
            cells,
            settings,
            network,
            clock,
            account,
            row,
            popover,
        }
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
        let layout = Self::layout(ui, area, data, self.open_row, self.account_open);
        self.update_hover(ui, &layout, data);
        let layout = Self::layout(ui, area, data, self.open_row, self.account_open);

        // Every pixel of the surface, first. This surface has no alpha — softbuffer attaches an
        // `Xrgb8888` shm buffer — so anything a frame does not write shows as black, and a strip
        // that painted only its chips was a black block in the corner of the screen rather than
        // chips floating on the desktop. Home's own ground is the honest fill: the chips then
        // read as sitting on Home, which is what is actually behind them.
        //
        // ponytail: an opaque ground, not transparency. The upgrade is a bridge-owned
        // `Argb8888` wl_shm surface painted by pixelkit, the way `session_lock` already does it,
        // instead of a winit window over softbuffer.
        ui.painter.fill_rect(area, design::ground());

        // The account popover first, for the same reason the row is early: it floats over what is
        // above the bar and must claim its clicks before anything underneath can.
        if let Some((frame, rows)) = &layout.popover {
            design::shell(
                ui,
                *frame,
                design::shell_fill(),
                design::edge_strong(),
                Elevation::Lifted,
            );
            // The readings first, as a line rather than a row: what the machine is doing belongs
            // beside the controls that change it, and it is the one line here that is not a
            // button. It replaces the chip the cluster used to spend 112 pixels on.
            if let Some(first) = rows.first() {
                ui.label_styled(
                    first.inset(ui.px(6)),
                    &readings_line(data.machine),
                    design::state_style(ui),
                    design::text_faint(),
                    Align::Left,
                );
            }
            for (rect, row) in rows.iter().skip(1).zip(ACCOUNT_ROWS) {
                let id = row_id(&row);
                let Some(declaration) = lege_types::action::action(id) else {
                    continue;
                };
                let refused = data.machine.refuses(id);
                let armed = self.confirming == Some(id);
                let hovered = !refused && ui.input.hovering(*rect);
                if armed || hovered {
                    ui.painter.fill_rounded_rect(
                        *rect,
                        ui.px(design::CHIP_RADIUS),
                        if armed {
                            design::danger()
                        } else {
                            design::accent_soft()
                        },
                    );
                }
                // A row that ends the session says so twice: an ellipsis before it is armed,
                // and then in words. "Click again to log out" is a sentence; "Log out — again to
                // confirm" is a state machine talking to itself.
                let label = if armed {
                    format!("Click again to {}", declaration.label.to_lowercase())
                } else if declaration.confirm {
                    format!("{}\u{2026}", declaration.label)
                } else {
                    declaration.label.to_owned()
                };
                ui.label_styled(
                    rect.inset(ui.px(6)),
                    &label,
                    design::state_style(ui),
                    if refused {
                        design::text_faint()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                if !refused && ui.input.take_click(*rect) {
                    // Two steps for the three actions that end the session, one for everything
                    // else. A different row cancels whatever was armed.
                    if declaration.confirm && !armed {
                        self.confirming = Some(id);
                    } else {
                        self.confirming = None;
                        self.account_open = false;
                        actions.push(BarAction::Field(FieldAction::RunAction(id)));
                    }
                }
            }
            // Anything else inside the frame is the popover's own ground, not the window behind.
            let _ = ui.input.take_click(*frame);
        }

        // The row next: it floats over whatever is above the bar and claims its clicks before
        // the bar's own cells can.
        if let Some(index) = self.open_row
            && let Some(entry) = data.entries.get(index)
        {
            for (rect, (window, title)) in layout.row.iter().zip(&entry.windows) {
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
                ui.label_styled(
                    rect.inset(ui.px(8)),
                    title,
                    design::state_style(ui),
                    if entry.is_parked(*window) {
                        design::text_faint()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                if ui.input.take_click(*rect) {
                    if entry.is_parked(*window) {
                        actions.push(BarAction::Field(FieldAction::Restore(*window)));
                    }
                    actions.push(BarAction::Activate(*window));
                    self.open_row = None;
                    self.left_at = None;
                }
            }
        }

        // The machine cluster, left to right: Settings, the network, the clock, the account.
        // The label comes from the action manifest for the same reason the network's does.
        let settings_label = lege_types::action::action("settings.open")
            .map_or("Settings", |declaration| declaration.label);
        let settings_hovered = ui.input.hovering(layout.settings);
        chip(
            ui,
            layout.settings,
            if settings_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.settings.inset(ui.px(6)),
            &format!("\u{2699} {settings_label}"),
            design::state_style(ui),
            design::TAG_INK,
            Align::Centre,
        );
        if ui.input.take_click(layout.settings) {
            actions.push(BarAction::Field(FieldAction::RunAction("settings.open")));
        }

        // The label comes from the action manifest, so the bar and the cheat sheet
        // cannot call the same capability two different things.
        let network_label = lege_types::action::action("network.open-control")
            .map_or("Network", |declaration| declaration.label);
        let network_hovered = data.network_control_available && ui.input.hovering(layout.network);
        chip(
            ui,
            layout.network,
            if network_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.network.inset(ui.px(6)),
            network_label,
            design::state_style(ui),
            if data.network_control_available {
                design::TAG_INK
            } else {
                design::text_faint()
            },
            Align::Centre,
        );
        if data.network_control_available && ui.input.take_click(layout.network) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "network.open-control",
            )));
        }

        // The clock opens the installed calendar. Dimmed when none is installed, the way the
        // network chip is: a chip that looks live and does nothing is worse than one that says so.
        let clock_hovered = data.calendar_available && ui.input.hovering(layout.clock);
        chip(
            ui,
            layout.clock,
            if clock_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.clock.inset(ui.px(6)),
            data.clock_label,
            design::state_style(ui),
            if data.calendar_available {
                design::TAG_INK
            } else {
                design::text_faint()
            },
            Align::Centre,
        );
        if data.calendar_available && ui.input.take_click(layout.clock) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "clock.open-calendar",
            )));
        }

        // The account chip: Settings, the machine's controls and the session actions, in one
        // place, because there is one of each of them on this desktop.
        let account_hovered = ui.input.hovering(layout.account);
        chip(
            ui,
            layout.account,
            if self.account_open || account_hovered {
                design::accent()
            } else {
                design::TAG_FILL
            },
        );
        ui.label_styled(
            layout.account.inset(ui.px(6)),
            &format!("\u{25c9} {}", data.account_label),
            design::state_style(ui),
            design::TAG_INK,
            Align::Centre,
        );
        if ui.input.take_click(layout.account) {
            self.account_open = !self.account_open;
            self.confirming = None;
            if self.account_open {
                self.open_row = None;
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

        // A press on empty bar ground: the compositor gave this surface the keyboard for it, and
        // the activated window is the one that should have it. A press anywhere else while the
        // popover is open dismisses it, the way any menu behaves.
        if ui.input.take_click(layout.bar) {
            self.account_open = false;
            self.confirming = None;
            if let Some(window) = data.activated {
                actions.push(BarAction::Activate(window));
            }
        } else if self.account_open && ui.input.take_click(area) {
            self.account_open = false;
            self.confirming = None;
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

    /// The output's logical width, which is what the host passes as `full_width`.
    const W: i32 = 1200;

    /// The tallest the surface ever gets: the bar plus the account popover's band. The harness
    /// buffer is this tall so every state fits in one buffer, and the bar is always drawn at the
    /// bottom of it, exactly as the host's surface is.
    fn tallest() -> i32 {
        BAR_HEIGHT + popover_band()
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
        text: pixelkit_text::TextCache,
        kernel: RasterKernel,
        buffer: WindowBuffer,
        input: Input,
        machine: crate::data::MachineReadings,
        calendar_available: bool,
        /// What the host would say about the bottom strip this frame (`app::bar_covered`).
        covered: bool,
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
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(W as u32, tallest() as u32),
                input: Input::new(),
                machine: crate::data::MachineReadings {
                    audio: Some(lege_client::status::AudioState {
                        present: true,
                        volume_percent: 45,
                        muted: false,
                    }),
                    backlight: Some(lege_client::status::BacklightState {
                        present: true,
                        percent: 60,
                        ..Default::default()
                    }),
                    battery: None,
                },
                calendar_available: true,
                covered: false,
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
                self.screen.open_row.is_some(),
                self.screen.account_open,
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
            let machine = self.machine.clone();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                clock_label: "MON 7 SEP 12:00",
                network_control_available: true,
                calendar_available: self.calendar_available,
                account_label: "dk",
                activated,
                covered: self.covered,
                home_revealed: false,
                machine: &machine,
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
            let machine = self.machine.clone();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                clock_label: "",
                network_control_available: true,
                calendar_available: self.calendar_available,
                account_label: "dk",
                activated,
                covered: self.covered,
                home_revealed: false,
                machine: &machine,
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
                self.screen.account_open,
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
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT + ROW_BAND));
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
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false),
            Some(HOVER_OPEN_MS)
        );
        h.now = HOVER_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        // Off the bar entirely: a grace period, then closed.
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        assert_eq!(
            h.screen.next_deadline_ms(&entries, false),
            Some(HOVER_OPEN_MS + HOVER_CLOSE_MS)
        );
        h.now += HOVER_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.open_row, None);
    }

    /// The bar reserves nothing, so a maximized window fills the output and wears its own tags
    /// and controls: the copy of them that used to sit at the right-hand end of the bar is gone,
    /// and the account chip owns the end of the strip whether a window is maximized or not.
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
        assert!(
            layout.account.right() >= layout.bar.right() - CHIP_PAD - 1,
            "the account chip is the right-hand end of the bar"
        );
        // Everything to the right of the account chip is bar ground, and a press on it hands the
        // keyboard back to the activated window rather than closing or unmaximizing it.
        let ground = i32::midpoint(layout.account.right(), layout.bar.right());
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

        assert_eq!(wanted_size(open, false, false, 0, W), (empty, BAR_HEIGHT));
        assert_eq!(
            wanted_size(open, true, false, 0, W),
            (empty, BAR_HEIGHT + ROW_BAND)
        );
        assert_eq!(
            wanted_size(open, false, true, 0, W),
            (empty, BAR_HEIGHT + popover_band())
        );
        // The popover is the taller of the two, so it wins when both are somehow asked for.
        assert_eq!(
            wanted_size(open, true, true, 0, W),
            (empty, BAR_HEIGHT + popover_band())
        );
        assert!(popover_band() > ROW_BAND);
        // An output narrower than the island clamps to the output, and a width of nothing is
        // still a surface.
        assert_eq!(wanted_size(open, false, false, 0, 200), (200, BAR_HEIGHT));
        assert_eq!(wanted_size(open, false, false, 0, 0), (1, BAR_HEIGHT));
        // Revealed is the island, in full; the hint is a tab, band or no band.
        assert_eq!(
            wanted_size(Presence::Revealed, true, false, 0, W),
            (empty, BAR_HEIGHT + ROW_BAND)
        );
        assert_eq!(
            wanted_size(Presence::Hint, true, true, 0, W),
            (HINT_WIDTH, HINT_HEIGHT)
        );
        const { assert!(HINT_HEIGHT < BAR_HEIGHT) };
        const { assert!(HINT_WIDTH < CLOCK_WIDTH) };
    }

    #[test]
    fn the_network_indicator_runs_its_declared_action() {
        let mut h = Harness::new(Vec::new());
        h.frame();
        let (x, y) = Harness::centre(h.rects().network);
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction(
                "network.open-control"
            ))]
        );
    }

    /// A parked window is still on the bar, and clicking it brings it back before activating it —
    /// `Activate` alone would focus a window nobody can see.
    #[test]
    fn clicking_a_parked_entry_restores_it_and_then_activates_it() {
        let mut parked = window(1, "org.example.Editor", "notes", false, false);
        parked.shelved = true;
        let mut h = Harness::new(vec![parked]);
        h.frame();
        let (x, y) = Harness::centre(h.cells()[0]);
        assert_eq!(
            h.click(x, y),
            vec![
                BarAction::Field(FieldAction::Restore(WindowId(1))),
                BarAction::Activate(WindowId(1)),
            ]
        );
    }

    /// The account popover is the one place the machine's actions live now. Every row is a
    /// declared action; the session-ending ones take two clicks.
    #[test]
    fn the_account_popover_runs_a_control_and_confirms_before_it_ends_the_session() {
        let mut h = Harness::new(Vec::new());
        h.frame();
        let (x, y) = Harness::centre(h.rects().account);
        assert!(h.click(x, y).is_empty(), "opening it performs nothing");
        assert!(h.screen.account_open);
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT + popover_band()));
        h.frame();

        // `LEGE_BAR_POPOVER_DUMP=<path>` writes the open popover as a PNG for review, the way
        // the row's own render test does.
        if let Ok(path) = std::env::var("LEGE_BAR_POPOVER_DUMP") {
            crate::surface::offscreen::write_png(std::path::Path::new(&path), &h.buffer)
                .expect("dumping the popover render");
        }
        let rows = h.rects().popover.expect("the popover has room").1;

        // A confirmed action arms on the first click and fires on the second.
        let (x, y) = Harness::centre(popover_row(&rows, "session.poweroff"));
        assert!(h.click(x, y).is_empty(), "the first click only arms it");
        assert!(h.screen.account_open, "and leaves the popover open");
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction("session.poweroff"))]
        );
        assert!(!h.screen.account_open, "performing a row closes it");

        // An unconfirmed one fires at once.
        let (x, y) = Harness::centre(h.rects().account);
        h.click(x, y);
        h.frame();
        let rows = h.rects().popover.expect("the popover has room").1;
        let (x, y) = Harness::centre(popover_row(&rows, "volume.mute"));
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction("volume.mute"))]
        );
    }

    /// Where one popover row is. The first rectangle is the readings line, which is not a row, so
    /// every test that clicks a row has to skip it — once, here.
    fn popover_row(rows: &[Rect], id: &str) -> Rect {
        let index = ACCOUNT_ROWS
            .iter()
            .position(|row| row_id(row) == id)
            .unwrap_or_else(|| panic!("{id} is not a popover row"));
        rows[index + 1]
    }

    /// A machine with no backlight cannot change its brightness: the row is drawn, and inert.
    #[test]
    fn a_refused_control_takes_no_click() {
        let mut h = Harness::new(Vec::new());
        h.machine.backlight = None;
        h.screen.account_open = true;
        h.frame();
        h.frame();
        let rows = h.rects().popover.expect("the popover has room").1;
        let (x, y) = Harness::centre(popover_row(&rows, "brightness.decrease"));
        assert!(h.click(x, y).is_empty(), "a refused row does nothing");
        assert!(h.screen.account_open, "and does not dismiss the popover");
    }

    /// The user's report was "the bottom right buttons aren't clickable". Every chip in the
    /// cluster now performs one named action, and this is the test that says so — one click each,
    /// at the rectangle the paint actually used.
    #[test]
    fn every_chip_in_the_machine_cluster_performs_its_named_action() {
        let mut h = Harness::new(Vec::new());
        h.frame();
        let layout = h.rects();

        let (x, y) = Harness::centre(layout.settings);
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction("settings.open"))],
            "the Settings chip opens Settings"
        );
        let (x, y) = Harness::centre(layout.clock);
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction(
                "clock.open-calendar"
            ))],
            "the clock opens the calendar"
        );
        let (x, y) = Harness::centre(layout.network);
        assert_eq!(
            h.click(x, y),
            vec![BarAction::Field(FieldAction::RunAction(
                "network.open-control"
            ))],
            "the network chip opens the network control"
        );
        let (x, y) = Harness::centre(layout.account);
        assert!(h.click(x, y).is_empty(), "the account chip only opens up");
        assert!(h.screen.account_open, "and it does open");
    }

    /// A chip with nothing to open is drawn inert rather than lying: no calendar installed means
    /// the clock takes no click, and the same rule the network chip has always followed.
    #[test]
    fn a_clock_with_no_calendar_installed_takes_no_click() {
        let mut h = Harness::new(Vec::new());
        h.calendar_available = false;
        h.frame();
        let (x, y) = Harness::centre(h.rects().clock);
        assert!(h.click(x, y).is_empty(), "a dimmed clock does nothing");
    }

    /// The cluster runs right to left from the end of the bar and never overlaps itself. It is
    /// the last thing on the strip now that the maximized window's controls have gone.
    #[test]
    fn the_cluster_runs_to_the_end_of_the_bar_without_overlapping_itself() {
        let mut h = Harness::new(vec![window(1, "org.legeos.Files", "Files", true, true)]);
        h.frame();
        let layout = h.rects();
        let cluster = [
            layout.account,
            layout.clock,
            layout.network,
            layout.settings,
        ];
        assert!(
            cluster[0].right() <= layout.bar.right(),
            "the cluster stays on the bar"
        );
        for pair in cluster.windows(2) {
            assert!(
                pair[1].right() <= pair[0].x,
                "the cluster runs right to left without overlapping itself"
            );
        }
        assert!(
            cluster.iter().all(|rect| rect.x >= layout.bar.x),
            "and stays on the bar"
        );
    }

    /// The user's words: "the switcher should be visible, if it can paint over garden/home
    /// surface. if it can't, it is mouseover, and there can be a hint at the very bottom, like a
    /// graphical ^." Covered means hint-sized; a dwell on the hint brings the bar back.
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
            h.screen.next_deadline_ms(&entries, true),
            Some(HINT_OPEN_MS),
            "the host wakes when the dwell is up"
        );
        h.now = HINT_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Revealed);
        assert_eq!(h.wanted(), (h.island(), BAR_HEIGHT));
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
        h.screen.account_open = true;
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
            h.screen.next_deadline_ms(&entries, true),
            Some(HINT_OPEN_MS + HINT_CLOSE_MS)
        );
        h.now += HINT_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.presence(true), Presence::Hint);
        assert_eq!(h.wanted(), (HINT_WIDTH, HINT_HEIGHT));
        assert_eq!(h.screen.open_row, None, "the row closed with it");
        assert!(!h.screen.account_open, "and so did the popover");
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

    /// Every row of the popover names a declared action, so no chip can promise something the
    /// manifest has never heard of.
    #[test]
    fn every_popover_row_is_a_declared_action() {
        for row in &ACCOUNT_ROWS {
            let id = row_id(row);
            assert!(
                lege_types::action::action(id).is_some(),
                "{id} is not declared"
            );
        }
    }
}

#[cfg(test)]
mod initial_tests {
    use super::{chip_initial, first_letter};
    use crate::taskbar::Entry;
    use lege_window::WindowId;

    fn entry(app_id: Option<&str>, label: &str) -> Entry {
        Entry {
            key: label.to_owned(),
            label: label.to_owned(),
            accent: None,
            app_id: app_id.map(ToOwned::to_owned),
            windows: vec![(WindowId(1), label.to_owned())],
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
