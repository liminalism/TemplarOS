//! The machine cluster: Settings, the network, the clock, and the account — at the bottom-right
//! corner of the screen, on a surface of its own.
//!
//! It used to be the right-hand end of the bar. That became wrong twice over when the bar turned
//! into a centred island of open windows: what the machine is doing is not what the switcher is
//! for, and centring it put a clock and a login menu in the middle of the screen. So it is its own
//! surface, anchored to the *end* of the bottom edge (`templar-shell-v1.xml`'s `set_edge_alignment`),
//! and the island is open windows and nothing else.
//!
//! **It is absent in fullscreen**, entirely — no hint, no peek, no surface. A fullscreen window is
//! one thing being looked at and a battery percentage is not part of it. The switcher still comes
//! back on a dwell, because moving between windows is the one thing a person still needs there.
//!
//! Like the island it reserves nothing, and its transparent parts are desktop: the coverage it
//! records ([`ClusterScreen::opaque_shapes`]) is the chips themselves, and the host turns that
//! into both the surface's alpha and its input region.
//!
//! ## The two levels
//!
//! Volume and brightness are the two things on this machine a person changes by *feel* rather than
//! by number, and until now the popover offered them as five rows that each moved a level five
//! points and told you nothing: to go from loud to quiet was six clicks, each one a round trip,
//! with the reading a line away and a step behind. They are sliders now — one press anywhere on a
//! track goes there, a drag follows the pointer.
//!
//! The volume slider's scale is **decibels**, not percent. `Audio1`'s percentage is WirePlumber's
//! own value, which is the cubic scale — the linear gain is `(p/100)³`, so `dB = 60·log10(p/100)`
//! — and a track laid out linearly in that percentage spends half its length on the top 6 dB,
//! where nothing much happens, and a tenth on the bottom 20 dB, where everything does. Laying the
//! track out in decibels instead puts the same amount of travel behind the same change in loudness
//! everywhere along it. [`MIN_DB`] is the quietest position; below [`MUTE_ZONE`] the bottom of the
//! track is mute rather than an even quieter number, because "off" is a state and not a level.
//!
//! The brightness slider is linear in the backlight's own percentage — that scale is already about
//! perceptual — and starts at the floor `Backlight1` enforces rather than at zero, so none of its
//! length is spent on a screen that cannot be read.

use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use crate::surface::bar_screen::{BAR_HEIGHT, BarAction, CHIP_GAP, CHIP_H, CHIP_PAD, chip};
use crate::surface::design::{self, Elevation};
use crate::surface::field_screen::FieldAction;
use crate::surface::rail::{self, RAIL_GAP, RailSide, RailView};

/// The Settings chip — the whole of Settings, not one corner of it. It took the readings chip's
/// place in the cluster.
const SETTINGS_WIDTH: i32 = 112;
const NETWORK_WIDTH: i32 = 96;
const CLOCK_WIDTH: i32 = 160;
/// The account chip: the account's name, its mark, and the popover behind it.
const ACCOUNT_WIDTH: i32 = 140;
/// The account popover's width and one row's height, and the padding inside its frame.
///
/// Wider than a menu needs to be, because two of its rows are tracks and a track's width is its
/// resolution: at this width a decibel is about three pixels, so the quiet end of the volume
/// slider is aimable rather than a rounding error.
const POPOVER_WIDTH: i32 = 300;
const POPOVER_ROW: i32 = 28;
const POPOVER_PAD: i32 = 8;
/// A slider's row: taller than a menu row, because the whole row is its grab target.
const SLIDER_ROW: i32 = 30;
/// The track itself, the name at its left, and the reading at its right.
const TRACK_H: i32 = 8;
const SLIDER_LABEL: i32 = 66;
const SLIDER_READING: i32 = 40;
/// The rule between the machine's levels and the session's verbs.
const DIVIDER_ROW: i32 = 9;
/// Grace after the pointer leaves the cluster before an open popover closes. The same number the
/// island's hover row uses, and for the same reason: long enough to cross a gap, short enough that
/// a menu never outstays the pointer.
pub const POPOVER_CLOSE_MS: u64 = 200;

/// The gap between the popover's frame and the bar it grows out of.
const POPOVER_GAP: i32 = 6;

/// The quietest position on the volume track, in decibels of gain.
///
/// Sixty decibels below unity is about the range a pair of speakers is useful over: quiet enough
/// that the bottom of the track is genuinely quiet, loud enough that the step above mute is not
/// silence with extra steps.
const MIN_DB: f64 = -60.0;

/// How much of the bottom of the volume track means *mute* rather than *very quiet*.
///
/// A slider that can only approach silence asymptotically has no "off", and off is the thing a
/// person reaches for fastest. Three per cent of the track is a few pixels: enough to hit on
/// purpose, too little to hit by accident on the way to the quietest audible setting.
const MUTE_ZONE: f64 = 0.03;

/// The lowest brightness the slider offers, matching the floor `Backlight1` enforces
/// (`services/templar-sessiond/src/apis/backlight.rs`'s `MINIMUM_PERCENT`).
///
/// Duplicated rather than shared because the shell does not depend on the service, and the copy is
/// only about where the *track* starts: the service clamps regardless, so the worst a stale copy
/// can do is waste a few pixels of travel or stop a little above the floor.
const BRIGHTNESS_FLOOR: u32 = 10;

/// While a slider is being dragged, how often what it says actually reaches the service.
///
/// Every `Set` is a round trip — one `wpctl` process for volume, one logind call for brightness —
/// and a drag produces a position per frame. The pointer's own value is what the track draws, so
/// throttling costs nothing visible; the release always sends, so where a drag ends is never
/// approximate.
const SET_THROTTLE_MS: u64 = 100;

/// The account popover's session rows, in order.
///
/// Written out as literal `RunAction` constructions rather than as a table of bare ids on
/// purpose: that spelling is what `xtask check-actions` reads as a pointer route, and this popover
/// is the only pointer route to the session actions — Home no longer carries the Session widget.
///
/// The five level rows that used to sit above these are the two sliders now. The actions they
/// named have not gone anywhere and neither have their pointer routes: `volume.mute` is the volume
/// slider's own label, and the four step actions are a wheel over a track.
///
/// `settings.open` is not here either: it has its own chip in the cluster, and an action offered
/// in two places is one a person has to look for twice.
const SESSION_ROWS: [FieldAction; 5] = [
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

/// Which of the two levels a press landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slider {
    Volume,
    Brightness,
}

/// A slider the pointer has hold of.
#[derive(Clone, Copy, Debug)]
struct Drag {
    slider: Slider,
    /// Where along the track the pointer is, `0..=1`. The drawn position: a drag has to follow the
    /// pointer at the frame rate, and what the service last reported is a round trip behind.
    at: f64,
    /// The mute state this drag has already sent, if any. A drag that crosses in or out of the
    /// mute zone says so once per crossing, not once per throttle tick — ten `SetMuted`s a second
    /// is ten processes saying the same thing.
    sent_mute: Option<bool>,
}

/// How far above the bar the account popover reaches, gap included. The readings line is the
/// first row and costs a row's height like any other.
pub fn popover_band() -> i32 {
    POPOVER_GAP + 2 * POPOVER_PAD + popover_height()
}

/// The popover's own height: the readings line, the two sliders, the rule, and the session rows.
fn popover_height() -> i32 {
    POPOVER_ROW + 2 * SLIDER_ROW + DIVIDER_ROW + SESSION_ROWS.len() as i32 * POPOVER_ROW
}

/// The volume percentage a position on the track means.
///
/// The track is linear in decibels — `dB = MIN_DB · (1 − t)` — and `Audio1`'s percentage is the
/// cubic scale WirePlumber reports, in which the linear gain is `(p/100)³`. So `p = 100 · 10^(dB/60)`.
/// The bottom of the track is [`MIN_DB`], not silence: mute is [`MUTE_ZONE`]'s job.
///
// ponytail: the wire carries an integer percentage, which is ~12 distinct positions across the
// bottom 20 dB. Add an `Audio1.SetVolumeDb` if that ever feels chunky in the hand.
fn volume_percent_for(t: f64) -> u32 {
    let db = MIN_DB * (1.0 - t.clamp(0.0, 1.0));
    (100.0 * 10f64.powf(db / 60.0)).round().clamp(0.0, 100.0) as u32
}

/// Where a volume percentage sits on the track — the inverse of [`volume_percent_for`].
///
/// Clamped at both ends: a sink something else has boosted past 100% reads back above 100 and
/// draws full rather than off the end, and anything at or below [`MIN_DB`] draws at the bottom.
fn volume_track_for(percent: u32) -> f64 {
    if percent == 0 {
        return 0.0;
    }
    let db = 60.0 * (f64::from(percent) / 100.0).log10();
    (1.0 - db / MIN_DB).clamp(0.0, 1.0)
}

/// The brightness percentage a position on the track means: linear from the floor to full.
fn brightness_percent_for(t: f64) -> u32 {
    let span = f64::from(100 - BRIGHTNESS_FLOOR);
    (f64::from(BRIGHTNESS_FLOOR) + span * t.clamp(0.0, 1.0)).round() as u32
}

/// Where a brightness percentage sits on the track — the inverse of [`brightness_percent_for`].
fn brightness_track_for(percent: u32) -> f64 {
    let span = f64::from(100 - BRIGHTNESS_FLOOR);
    (f64::from(percent.max(BRIGHTNESS_FLOOR)) - f64::from(BRIGHTNESS_FLOOR)) / span
}

/// Where along a track a pointer at `x` is asking for.
fn fraction_at(track: Rect, x: i32) -> f64 {
    if track.w <= 0 {
        return 0.0;
    }
    (f64::from(x - track.x) / f64::from(track.w)).clamp(0.0, 1.0)
}

/// A slider row's three parts: the name, the track, and the reading.
///
/// The track is drawn `TRACK_H` thin and grabbed at the row's full height — a line eight pixels
/// tall is a pretty control and a poor target, and the row has nothing else in it to hit.
fn slider_parts(ui: &Ui<'_>, row: Rect) -> (Rect, Rect, Rect, Rect) {
    let gap = ui.px(6);
    let label = Rect::new(row.x, row.y, ui.px(SLIDER_LABEL), row.h);
    let reading_w = ui.px(SLIDER_READING);
    let reading = Rect::new(row.right() - reading_w, row.y, reading_w, row.h);
    let left = label.right() + gap;
    let width = (reading.x - gap - left).max(1);
    let track_h = ui.px(TRACK_H);
    let track = Rect::new(left, row.y + (row.h - track_h) / 2, width, track_h);
    let grab = Rect::new(left, row.y, width, row.h);
    (label, track, grab, reading)
}

/// What the cluster draws this frame.
#[derive(Debug)]
pub struct ClusterData<'a> {
    pub clock_label: &'a str,
    /// Whether the network control's role has a provider that can be launched.
    pub network_control_available: bool,
    /// Whether an installed application declares `Categories=Calendar`, so the clock has
    /// somewhere to open into.
    pub calendar_available: bool,
    /// Who is signed in: what the account chip says next to its mark.
    pub account_label: &'a str,
    /// Charge, volume and backlight, as the services last reported them.
    pub machine: &'a crate::data::MachineReadings,
    pub now_ms: u64,
    /// Draw as a gesture rail rather than at the corner (`surface::rail`). `None` is the corner
    /// cluster, unchanged.
    pub rail: Option<RailView>,
}

/// Where each chip and the popover are, in the surface's own coordinates.
#[derive(Debug)]
struct Layout {
    settings: Rect,
    network: Rect,
    clock: Rect,
    account: Rect,
    /// The account popover, when it is open and the host has made room.
    popover: Option<Popover>,
    /// In rail mode, the revealed columns `[x0, x1)` in physical pixels. `None` at the corner.
    revealed: Option<(i32, i32)>,
}

impl Layout {
    /// Whether `chip` is on screen enough to hover and click: always at the corner, and on a rail
    /// once the reveal has passed its centre.
    fn live(&self, chip: Rect) -> bool {
        self.revealed
            .is_none_or(|interval| rail::centre_revealed(chip, interval))
    }
}

/// The account popover's parts.
#[derive(Debug)]
struct Popover {
    frame: Rect,
    readings: Rect,
    volume: Rect,
    brightness: Rect,
    divider: Rect,
    rows: Vec<Rect>,
}

#[derive(Debug, Default)]
pub struct ClusterScreen {
    /// Whether the account popover is open.
    pub account_open: bool,
    /// The popover row armed by a first click and waiting for its second.
    ///
    /// `ActionDecl::confirm` is set on the three actions that end the session, and a chip in the
    /// corner of the screen is exactly the sort of thing a sleeve catches: without this, brushing
    /// the popover would shut the machine down.
    confirming: Option<&'static str>,
    /// The shapes this frame painted solid, for the host's alpha and input regions.
    opaque: Vec<(Rect, i32)>,
    /// When the pointer left the cluster's surface, while the popover is open.
    left_at: Option<u64>,
    /// The slider the pointer has hold of, if any.
    drag: Option<Drag>,
    /// When this drag last told the service where it had got to.
    last_sent_ms: u64,
}

/// How wide the cluster is: its two margins, its four chips, and the gaps between them.
#[must_use]
pub fn cluster_width() -> i32 {
    2 * CHIP_PAD + SETTINGS_WIDTH + NETWORK_WIDTH + CLOCK_WIDTH + ACCOUNT_WIDTH + 3 * CHIP_GAP
}

/// The surface size the host should ask for. The popover grows it upward, exactly as the island's
/// hover row does, and reserves nothing either way.
#[must_use]
pub fn wanted_size(account_open: bool, full_width: i32) -> (i32, i32) {
    let band = if account_open { popover_band() } else { 0 };
    (cluster_width().min(full_width.max(1)), BAR_HEIGHT + band)
}

/// How long the system rail is, in logical pixels: the gap at the origin, the four chips and the
/// gaps between them, and a margin at the far end.
#[must_use]
pub fn rail_length() -> i32 {
    RAIL_GAP + cluster_width() - CHIP_PAD
}

/// How much of the system rail is shown the moment it opens: the whole Settings chip, which is
/// the one nearest the origin.
#[must_use]
pub fn rail_initial() -> i32 {
    RAIL_GAP + SETTINGS_WIDTH
}

/// The surface size the host asks for while the system rail is open: the output's whole width,
/// and the popover's band above the bar when it is open.
#[must_use]
pub fn rail_wanted_size(account_open: bool, full_width: i32) -> (i32, i32) {
    let band = if account_open { popover_band() } else { 0 };
    (full_width.max(1), BAR_HEIGHT + band)
}

impl ClusterScreen {
    /// The shapes this frame painted solid. Empty before the first draw, which the host reads as
    /// "no coverage yet" and answers with an opaque present.
    #[must_use]
    pub fn opaque_shapes(&self) -> &[(Rect, i32)] {
        &self.opaque
    }

    /// Whether the host should make room above the cluster for the account popover.
    #[must_use]
    pub fn account_open(&self) -> bool {
        self.account_open
    }

    /// Close the popover and drop whatever it was doing — an armed session row, a drag, the leave
    /// timer. The host calls it when a rail closes, so the next opening starts closed.
    pub fn reset(&mut self) {
        self.account_open = false;
        self.confirming = None;
        self.left_at = None;
        self.drag = None;
    }

    fn layout(ui: &Ui<'_>, area: Rect, account_open: bool, rail: Option<RailView>) -> Layout {
        let bar_height = ui.px(BAR_HEIGHT).min(area.h);
        let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
        let chip_h = ui.px(CHIP_H).min(bar.h);
        let chip_y = bar.y + (bar.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);

        let (settings, network, clock, account) = if let Some(view) = rail {
            // Outward from the origin: Settings first, because it is the one place the rest of
            // the machine is reached from, then the network, the clock, and the account at the
            // far end — the same order the corner reads in, mirrored on a left rail. Laid once;
            // the reveal is a clip over them, never a move.
            let origin = area.x + ui.px(view.origin);
            let mut at = ui.px(RAIL_GAP);
            let mut place = |width: i32| {
                let x = match view.side {
                    RailSide::Right => origin + at,
                    RailSide::Left => origin - at - width,
                };
                at += width + gap;
                Rect::new(x, chip_y, width, chip_h)
            };
            let settings = place(ui.px(SETTINGS_WIDTH));
            let network = place(ui.px(NETWORK_WIDTH));
            let clock = place(ui.px(CLOCK_WIDTH));
            let account = place(ui.px(ACCOUNT_WIDTH));
            (settings, network, clock, account)
        } else {
            // Right to left, because the right-hand end of the screen is what this is anchored to.
            let mut right = bar.right() - ui.px(CHIP_PAD);
            let mut place = |width: i32| {
                let rect = Rect::new(right - width, chip_y, width, chip_h);
                right = rect.x - gap;
                rect
            };
            let account = place(ui.px(ACCOUNT_WIDTH));
            let clock = place(ui.px(CLOCK_WIDTH));
            let network = place(ui.px(NETWORK_WIDTH));
            let settings = place(ui.px(SETTINGS_WIDTH));
            (settings, network, clock, account)
        };
        let revealed = rail.map(|view| view.physical_interval(area.x, |logical| ui.px(logical)));

        let popover = (account_open && area.h > bar_height).then(|| {
            let width = ui.px(POPOVER_WIDTH).min(area.w);
            let pad = ui.px(POPOVER_PAD);
            let row_h = ui.px(POPOVER_ROW);
            let slider_h = ui.px(SLIDER_ROW);
            let divider_h = ui.px(DIVIDER_ROW);
            // Summed from the scaled parts rather than scaling the sum, so the frame is never a
            // rounded pixel shorter than the rows laid out inside it.
            let content = row_h
                + 2 * slider_h
                + divider_h
                + i32::try_from(SESSION_ROWS.len()).unwrap_or(5) * row_h;
            let height = (2 * pad + content).min(area.h - bar_height);
            // Over the account chip, flush with its outer end — its right edge at the corner and
            // on a right rail, its left edge on a left rail — and never off the surface.
            let x = match rail.map(|view| view.side) {
                Some(RailSide::Left) => account.x,
                _ => account.right() - width,
            };
            let frame = Rect::new(
                x.min(area.right() - width).max(area.x),
                bar.y - ui.px(POPOVER_GAP) - height,
                width,
                height,
            );
            // Stacked from the top in the order they read: what the machine is doing, the two
            // levels that change it, a rule, and the verbs that end the session.
            let inner_x = frame.x + pad;
            let inner_w = (frame.w - 2 * pad).max(0);
            let mut y = frame.y + pad;
            let mut band = |height: i32| {
                let rect = Rect::new(inner_x, y, inner_w, height);
                y += height;
                rect
            };
            let readings = band(row_h);
            let volume = band(slider_h);
            let brightness = band(slider_h);
            let divider = band(divider_h);
            let rows = (0..SESSION_ROWS.len()).map(|_| band(row_h)).collect();
            Popover {
                frame,
                readings,
                volume,
                brightness,
                divider,
                rows,
            }
        });

        Layout {
            settings,
            network,
            clock,
            account,
            popover,
            revealed,
        }
    }

    /// Whether a drag is on this slider right now.
    fn dragging(&self, slider: Slider) -> Option<f64> {
        self.drag
            .filter(|drag| drag.slider == slider)
            .map(|drag| drag.at)
    }

    /// Paint one level as a name, a track and a reading.
    fn paint_slider(
        ui: &mut Ui<'_>,
        row: Rect,
        name: &str,
        at: f64,
        reading: &str,
        live: bool,
        pressable: bool,
    ) {
        let (label, track, _grab, reading_rect) = slider_parts(ui, row);
        // Only a name that does something lights up under the pointer. The volume row's is the
        // mute button; the brightness row's is a word.
        let hovered = live && pressable && ui.input.hovering(label);
        if hovered {
            ui.painter
                .fill_rounded_rect(label, ui.px(design::CHIP_RADIUS), design::accent_soft());
        }
        let ink = if live {
            design::text()
        } else {
            design::text_faint()
        };
        ui.label_styled(label, name, design::state_style(ui), ink, Align::Left);
        ui.bar(
            track,
            at,
            if live {
                design::accent()
            } else {
                design::text_faint()
            },
        );
        // The handle: a person aims at a thing, not at the end of a fill.
        if live && track.w > 0 {
            let size = ui.px(TRACK_H + 6);
            let centre = track.x + (f64::from(track.w) * at.clamp(0.0, 1.0)).round() as i32;
            ui.painter.fill_rounded_rect(
                Rect::new(
                    (centre - size / 2).clamp(track.x, track.right() - size),
                    track.y + (track.h - size) / 2,
                    size,
                    size,
                ),
                size / 2,
                design::strong_ink(),
            );
        }
        ui.label_styled(
            reading_rect,
            reading,
            design::state_style(ui),
            ink,
            Align::Right,
        );
    }

    /// The two levels: paint them where the pointer says they are, and answer what it did.
    fn levels(
        &mut self,
        ui: &mut Ui<'_>,
        popover: &Popover,
        data: &ClusterData<'_>,
    ) -> Vec<BarAction> {
        self.paint_levels(ui, popover, data);
        self.level_input(ui, popover, data)
    }

    /// Draw both tracks at the position the pointer is asking for, falling back to what the
    /// services last reported for whichever one is not being dragged.
    fn paint_levels(&self, ui: &mut Ui<'_>, popover: &Popover, data: &ClusterData<'_>) {
        let audio = data.machine.audio.as_ref().filter(|state| state.present);
        let backlight = data
            .machine
            .backlight
            .as_ref()
            .filter(|state| state.present);

        // --- volume ------------------------------------------------------------------
        let dragged = self.dragging(Slider::Volume);
        let at = dragged.unwrap_or_else(|| {
            audio.map_or(0.0, |state| {
                if state.muted {
                    0.0
                } else {
                    volume_track_for(state.volume_percent)
                }
            })
        });
        // While dragging, the bottom of the track *is* mute — say so before the service has been
        // told, or the label lags the hand by a round trip.
        let muted = match dragged {
            Some(at) => at < MUTE_ZONE,
            None => audio.is_some_and(|state| state.muted),
        };
        let reading = match audio {
            None => "\u{2014}".to_owned(),
            Some(_) if muted => String::new(),
            Some(state) => format!(
                "{}%",
                dragged.map_or(state.volume_percent, volume_percent_for)
            ),
        };
        Self::paint_slider(
            ui,
            popover.volume,
            if muted { "Muted" } else { "Volume" },
            if muted { 0.0 } else { at },
            &reading,
            audio.is_some(),
            true,
        );

        // --- brightness --------------------------------------------------------------
        let dragged = self.dragging(Slider::Brightness);
        let at = dragged
            .unwrap_or_else(|| backlight.map_or(0.0, |state| brightness_track_for(state.percent)));
        let reading = match backlight {
            None => "\u{2014}".to_owned(),
            Some(state) => format!("{}%", dragged.map_or(state.percent, brightness_percent_for)),
        };
        Self::paint_slider(
            ui,
            popover.brightness,
            "Brightness",
            at,
            &reading,
            backlight.is_some(),
            false,
        );
    }

    /// What the pointer did to either track.
    ///
    /// One place for both, because a drag belongs to whichever track it started on and has to
    /// keep following the pointer wherever in the popover it wanders — a slider that only tracks
    /// while the pointer stays inside eight pixels of trough is a slider people fight.
    fn level_input(
        &mut self,
        ui: &mut Ui<'_>,
        popover: &Popover,
        data: &ClusterData<'_>,
    ) -> Vec<BarAction> {
        let mut actions = Vec::new();
        let audio = data.machine.audio.as_ref().filter(|state| state.present);
        let backlight = data
            .machine
            .backlight
            .as_ref()
            .filter(|state| state.present);
        for (slider, row, live) in [
            (Slider::Volume, popover.volume, audio.is_some()),
            (Slider::Brightness, popover.brightness, backlight.is_some()),
        ] {
            let (label, track, grab, _) = slider_parts(ui, row);
            if !live {
                continue;
            }
            // The volume slider's name is its mute button: "off" is a state, and the row already
            // spells it. This is `volume.mute`'s pointer route.
            if slider == Slider::Volume && ui.input.take_click(label) {
                actions.push(BarAction::Field(FieldAction::RunAction("volume.mute")));
                continue;
            }
            // A wheel over a track steps it, which is what keeps the four step actions reachable
            // by pointer now that their rows are gone.
            let (_, dy) = ui.input.take_scroll(grab);
            if dy != 0.0 {
                // Wheel up is a negative delta, and wheel up means more.
                let id = match (slider, dy < 0.0) {
                    (Slider::Volume, true) => "volume.increase",
                    (Slider::Volume, false) => "volume.decrease",
                    (Slider::Brightness, true) => "brightness.increase",
                    (Slider::Brightness, false) => "brightness.decrease",
                };
                actions.push(BarAction::Field(FieldAction::RunAction(id)));
                continue;
            }
            if ui.input.take_click(grab) {
                let at = ui
                    .input
                    .cursor()
                    .map_or(0.0, |(x, _)| fraction_at(track, x));
                self.drag = Some(Drag {
                    slider,
                    at,
                    // Where the sink already is, so a drag that starts and stays in the audible
                    // range never sends an unmute nobody asked for.
                    sent_mute: (slider == Slider::Volume)
                        .then(|| audio.is_some_and(|state| state.muted)),
                });
                // Nothing has been sent for this drag: the press itself is the first thing to go.
                self.last_sent_ms = 0;
            }
        }

        // A drag follows the pointer anywhere on the popover and ends when the button comes up —
        // or when the pointer leaves the surface, which `Input::cursor_left` reports as the button
        // coming up, so a drag that ends off the edge still sends its last word.
        if let Some(mut drag) = self.drag {
            let held = ui.input.pressing(popover.frame);
            if held {
                let row = match drag.slider {
                    Slider::Volume => popover.volume,
                    Slider::Brightness => popover.brightness,
                };
                let (_, track, _, _) = slider_parts(ui, row);
                if let Some((x, _)) = ui.input.cursor() {
                    drag.at = fraction_at(track, x);
                }
            }
            let due = data.now_ms.saturating_sub(self.last_sent_ms) >= SET_THROTTLE_MS;
            if !held || due {
                match drag.slider {
                    Slider::Volume if drag.at < MUTE_ZONE => {
                        if drag.sent_mute != Some(true) {
                            drag.sent_mute = Some(true);
                            actions.push(BarAction::Field(FieldAction::SetMuted(true)));
                        }
                    }
                    Slider::Volume => {
                        if drag.sent_mute != Some(false) {
                            drag.sent_mute = Some(false);
                            actions.push(BarAction::Field(FieldAction::SetMuted(false)));
                        }
                        actions.push(BarAction::Field(FieldAction::SetVolume(
                            volume_percent_for(drag.at),
                        )));
                    }
                    Slider::Brightness => {
                        actions.push(BarAction::Field(FieldAction::SetBrightness(
                            brightness_percent_for(drag.at),
                        )));
                    }
                }
                self.last_sent_ms = data.now_ms;
            }
            self.drag = held.then_some(drag);
        }
        actions
    }

    /// Paint the cluster and answer this frame's clicks.
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &ClusterData<'_>) -> Vec<BarAction> {
        let mut actions = Vec::new();
        let layout = Self::layout(ui, area, self.account_open, data.rail);

        // The ground first, for the opaque fallback; the chips are the coverage, so everything
        // between and around them is desktop. The same bargain the island makes.
        ui.painter.fill_rect(area, design::ground());
        let radius = ui.px(design::CHIP_RADIUS);
        self.opaque.clear();
        let chips = [
            layout.settings,
            layout.network,
            layout.clock,
            layout.account,
        ]
        .map(|shape| (shape, radius));
        match layout.revealed {
            // On a rail the row is as solid as the curtain is drawn; the popover above it is not
            // part of the curtain and stays whole.
            Some(interval) => self.opaque.extend(rail::clipped_shapes(chips, interval)),
            None => self.opaque.extend(chips),
        }

        // The account popover first, for the same reason the row is early: it floats over what is
        // above the bar and must claim its clicks before anything underneath can.
        if let Some(popover) = &layout.popover {
            // `design::shell` fills at TILE_RADIUS, not CHIP_RADIUS. Recording the smaller one
            // leaves the coverage solid where the paint has already curved away — four dark ears
            // on every corner, of the shadow behind it. The whole frame is one shape, which is
            // also what puts the sliders' tracks inside the surface's input region.
            self.opaque
                .push((popover.frame, ui.px(design::TILE_RADIUS)));
            design::shell(
                ui,
                popover.frame,
                design::shell_fill(),
                design::edge_strong(),
                Elevation::Lifted,
            );
            // The charge first, as a line rather than a row: the one reading here with no control
            // beside it, because nothing on this machine sets the battery.
            ui.label_styled(
                popover.readings.inset(ui.px(6)),
                &data.machine.charge_label(),
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            );
            actions.extend(self.levels(ui, popover, data));
            let rule = Rect::new(
                popover.divider.x + ui.px(4),
                popover.divider.y + popover.divider.h / 2,
                (popover.divider.w - ui.px(8)).max(0),
                1,
            );
            ui.painter.fill_rect(rule, design::edge());
            for (rect, row) in popover.rows.iter().zip(SESSION_ROWS) {
                let id = row_id(&row);
                let Some(declaration) = templar_types::action::action(id) else {
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
            let _ = ui.input.take_click(popover.frame);
        } else {
            // No popover, no tracks: a drag cannot outlive the thing it was on.
            self.drag = None;
        }

        // The machine cluster, left to right: Settings, the network, the clock, the account.
        // The label comes from the action manifest for the same reason the network's does.
        let settings_label = templar_types::action::action("settings.open")
            .map_or("Settings", |declaration| declaration.label);
        // On a rail a chip the reveal has not reached is painted (the mask hides it) but not
        // there to the pointer: no hover, no click.
        let settings_live = layout.live(layout.settings);
        let settings_hovered = settings_live && ui.input.hovering(layout.settings);
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
        if settings_live && ui.input.take_click(layout.settings) {
            actions.push(BarAction::Field(FieldAction::RunAction("settings.open")));
        }

        // The label comes from the action manifest, so the bar and the cheat sheet
        // cannot call the same capability two different things.
        let network_label = templar_types::action::action("network.open-control")
            .map_or("Network", |declaration| declaration.label);
        let network_live = data.network_control_available && layout.live(layout.network);
        let network_hovered = network_live && ui.input.hovering(layout.network);
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
        if network_live && ui.input.take_click(layout.network) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "network.open-control",
            )));
        }

        // The clock opens the installed calendar. Dimmed when none is installed, the way the
        // network chip is: a chip that looks live and does nothing is worse than one that says so.
        let clock_live = data.calendar_available && layout.live(layout.clock);
        let clock_hovered = clock_live && ui.input.hovering(layout.clock);
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
        if clock_live && ui.input.take_click(layout.clock) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "clock.open-calendar",
            )));
        }

        // The account chip: the machine's two levels and the session actions, in one place,
        // because there is one of each of them on this desktop.
        let account_live = layout.live(layout.account);
        let account_hovered = account_live && ui.input.hovering(layout.account);
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
        if account_live && ui.input.take_click(layout.account) {
            self.account_open = !self.account_open;
            self.confirming = None;
        }

        // Leaving closes it. A press on the surface's own ground cannot: the input region is the
        // chips and the popover, so the ground is desktop and its clicks go to whatever is behind.
        // That left the popover with no way out but the chip that opened it — a menu that stays up
        // while you use the window underneath. The row's rule instead: the pointer leaves, a grace
        // period passes, it goes.
        if self.account_open {
            if ui.input.cursor().is_some_and(|(x, y)| area.contains(x, y)) {
                self.left_at = None;
            } else {
                let left = *self.left_at.get_or_insert(data.now_ms);
                if data.now_ms.saturating_sub(left) >= POPOVER_CLOSE_MS {
                    self.account_open = false;
                    self.confirming = None;
                    self.left_at = None;
                }
            }
        } else {
            self.left_at = None;
        }
        actions
    }

    /// When the host should wake next: to close a popover the pointer has left, or to send the
    /// next step of a drag whose pointer is holding still.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        let close = self.left_at.map(|left| left + POPOVER_CLOSE_MS);
        let drag = self.drag.map(|_| self.last_sent_ms + SET_THROTTLE_MS);
        match (close, drag) {
            (Some(close), Some(drag)) => Some(close.min(drag)),
            (some, None) | (None, some) => some,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two `f64` positions on a track are the same position.
    fn same(left: f64, right: f64) -> bool {
        (left - right).abs() < 1e-9
    }

    #[test]
    fn the_volume_track_is_laid_out_in_decibels() {
        // `Audio1`'s percentage is WirePlumber's cubic scale — checked against `pactl`, which
        // reports a node `wpctl` calls 0.49 as -18.42 dB, exactly 60·log10(0.49325). So half the
        // track is -30 dB, which is 32%, and not the 50% a linear track would put there.
        assert_eq!(volume_percent_for(0.0), 10);
        assert_eq!(volume_percent_for(0.25), 18);
        assert_eq!(volume_percent_for(0.5), 32);
        assert_eq!(volume_percent_for(0.75), 56);
        assert_eq!(volume_percent_for(1.0), 100);
    }

    #[test]
    fn a_volume_reading_and_its_position_are_inverses() {
        for percent in [10, 18, 32, 56, 100] {
            assert_eq!(
                volume_percent_for(volume_track_for(percent)),
                percent,
                "{percent}%"
            );
        }
        // Quieter than the bottom of the track, and louder than unity, both draw at an end rather
        // than off one: something else can boost a sink past 100%, and this control does not.
        assert!(same(volume_track_for(0), 0.0));
        assert!(same(volume_track_for(5), 0.0));
        assert!(same(volume_track_for(150), 1.0));
    }

    #[test]
    fn the_mute_zone_is_a_position_and_not_a_silent_value() {
        // The bottom of the track is the quietest *audible* setting, so "off" needs somewhere of
        // its own to live: the first sliver, big enough to aim at and too small to cross by
        // accident on the way to very quiet.
        const { assert!(MUTE_ZONE > 0.0 && MUTE_ZONE < 0.1) };
        assert!(
            volume_percent_for(MUTE_ZONE) >= 10,
            "everything the mute zone covers is otherwise audible"
        );
    }

    #[test]
    fn the_brightness_track_never_reaches_a_screen_nobody_can_read() {
        assert_eq!(brightness_percent_for(0.0), BRIGHTNESS_FLOOR);
        assert_eq!(brightness_percent_for(0.5), 55);
        assert_eq!(brightness_percent_for(1.0), 100);
        for percent in [BRIGHTNESS_FLOOR, 25, 55, 80, 100] {
            assert_eq!(
                brightness_percent_for(brightness_track_for(percent)),
                percent,
                "{percent}%"
            );
        }
        // A backlight something else drove below the floor draws at the bottom of the track, not
        // off the near end of it.
        assert!(same(brightness_track_for(1), 0.0));
    }

    #[test]
    fn both_tracks_only_ever_go_up() {
        let mut last = (0, 0);
        for step in 0..=100 {
            let at = f64::from(step) / 100.0;
            let now = (volume_percent_for(at), brightness_percent_for(at));
            assert!(now.0 >= last.0 && now.1 >= last.1, "went backwards at {at}");
            last = now;
        }
        assert_eq!(last, (100, 100));
    }

    #[test]
    fn a_press_lands_where_the_pointer_is() {
        let track = Rect::new(20, 0, 100, 8);
        assert!(same(fraction_at(track, 20), 0.0));
        assert!(same(fraction_at(track, 70), 0.5));
        assert!(same(fraction_at(track, 120), 1.0));
        // Past either end is that end, so a drag that runs off the track stops on it.
        assert!(same(fraction_at(track, -50), 0.0));
        assert!(same(fraction_at(track, 400), 1.0));
        // A track with no width is a track nothing can be asked of.
        assert!(same(fraction_at(Rect::new(0, 0, 0, 8), 5), 0.0));
    }

    #[test]
    fn every_session_row_names_a_declared_action() {
        for row in SESSION_ROWS {
            let id = row_id(&row);
            assert!(
                templar_types::action::action(id).is_some(),
                "{id} is not in the manifest"
            );
        }
    }

    const RAIL_W: i32 = 1920;

    fn rail_tallest() -> i32 {
        BAR_HEIGHT + popover_band()
    }

    /// The cluster drawn as a rail, in a surface the host would size, at the bottom of one buffer
    /// tall enough for the popover — the bar's `Harness`, cut down.
    struct Rail {
        screen: ClusterScreen,
        view: RailView,
        text: pixelkit_text::TextCache,
        kernel: pixelkit_raster::RasterKernel,
        buffer: pixelkit_raster::WindowBuffer,
        input: pixelkit_shell::Input,
        machine: crate::data::MachineReadings,
        height: i32,
    }

    impl Rail {
        fn new(side: RailSide, revealed: i32) -> Self {
            Self {
                screen: ClusterScreen::default(),
                view: RailView {
                    origin: RAIL_W / 2,
                    side,
                    revealed,
                },
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: pixelkit_raster::RasterKernel::new(),
                buffer: pixelkit_raster::WindowBuffer::new(RAIL_W as u32, rail_tallest() as u32),
                input: pixelkit_shell::Input::new(),
                machine: crate::data::MachineReadings::default(),
                height: BAR_HEIGHT,
            }
        }

        fn area(&self) -> Rect {
            Rect::new(0, rail_tallest() - self.height, RAIL_W, self.height)
        }

        fn layout(&mut self) -> Layout {
            let area = self.area();
            let ui = Ui::new(
                pixelkit_raster::Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                pixelkit_shell::Scale::ONE,
                &mut self.kernel,
            );
            ClusterScreen::layout(&ui, area, self.screen.account_open, Some(self.view))
        }

        fn frame(&mut self) -> Vec<BarAction> {
            let area = self.area();
            let data = ClusterData {
                clock_label: "10:00",
                network_control_available: true,
                calendar_available: true,
                account_label: "templar",
                machine: &self.machine,
                now_ms: 0,
                rail: Some(self.view),
            };
            let mut ui = Ui::new(
                pixelkit_raster::Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                pixelkit_shell::Scale::ONE,
                &mut self.kernel,
            );
            let actions = self.screen.draw(&mut ui, area, &data);
            drop(ui);
            self.input.end_frame();
            self.height = rail_wanted_size(self.screen.account_open, RAIL_W).1;
            actions
        }

        fn click(&mut self, rect: Rect) -> Vec<BarAction> {
            let (x, y) = (rect.x + rect.w / 2, rect.y + rect.h / 2);
            self.input.cursor_moved(x as f32, y as f32);
            self.input.mouse(pixelkit_shell::MouseButton::Left, true);
            self.input.mouse(pixelkit_shell::MouseButton::Left, false);
            self.frame()
        }
    }

    #[test]
    fn rail_cluster_runs_outward_from_the_origin_settings_first() {
        let origin = RAIL_W / 2;
        let right = Rail::new(RailSide::Right, 0).layout();
        assert_eq!(right.settings.x, origin + RAIL_GAP);
        assert!(right.network.x > right.settings.right());
        assert!(right.clock.x > right.network.right());
        assert!(right.account.x > right.clock.right());
        assert_eq!(right.account.right() + CHIP_PAD - origin, rail_length());

        let left = Rail::new(RailSide::Left, 0).layout();
        assert_eq!(left.settings.right(), origin - RAIL_GAP);
        assert!(left.network.right() < left.settings.x);
        assert!(left.clock.right() < left.network.x);
        assert!(left.account.right() < left.clock.x);
        assert_eq!(origin - (left.account.x - CHIP_PAD), rail_length());
    }

    #[test]
    fn rail_cluster_chips_stay_put_while_the_reveal_grows() {
        let mut rail = Rail::new(RailSide::Right, rail_initial());
        let early = rail.layout();
        rail.view.revealed = rail_length();
        let late = rail.layout();
        assert_eq!(
            (early.settings, early.network, early.clock, early.account),
            (late.settings, late.network, late.clock, late.account)
        );
    }

    #[test]
    fn rail_cluster_click_on_an_unrevealed_chip_does_nothing_until_revealed() {
        let mut rail = Rail::new(RailSide::Left, rail_initial());
        let clock = rail.layout().clock;
        assert!(rail.click(clock).is_empty());
        rail.view.revealed = rail_length();
        assert_eq!(
            rail.click(clock),
            vec![BarAction::Field(FieldAction::RunAction(
                "clock.open-calendar"
            ))]
        );
    }

    #[test]
    fn rail_cluster_coverage_is_clipped_but_the_popover_is_whole() {
        let mut rail = Rail::new(RailSide::Left, rail_initial());
        rail.frame();
        let (x0, x1) = rail.view.interval();
        let shapes = rail.screen.opaque_shapes().to_vec();
        assert_eq!(shapes.len(), 1, "only Settings is revealed: {shapes:?}");
        assert!(shapes[0].0.x >= x0 && shapes[0].0.right() <= x1);

        rail.view.revealed = rail_length();
        let account = rail.layout().account;
        rail.click(account);
        assert!(rail.screen.account_open(), "the toggle opens the popover");
        rail.frame();
        let popover = rail.layout().popover.expect("room was made").frame;
        assert!(popover.x >= 0 && popover.right() <= RAIL_W, "{popover:?}");
        let account = rail.layout().account;
        assert!(popover.bottom() <= account.y);
        assert!(
            rail.screen
                .opaque_shapes()
                .iter()
                .any(|(rect, _)| *rect == popover)
        );
    }
}
