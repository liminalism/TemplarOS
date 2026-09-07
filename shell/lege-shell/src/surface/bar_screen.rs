//! The bottom bar: a strip of chips at the left — network, clock, one per taskbar entry — which
//! becomes a full-width row only when there is something that needs the width.
//!
//! There is no bar when there is nothing to put on one. A person with no windows open should see
//! the clock and the network, not a band of empty furniture across the bottom of their screen, so
//! the resting state is chips on the compositor's own tag colours and nothing else: the surface
//! is exactly as wide as they are, and the desktop shows through everywhere else. Two things
//! still want the full width — a maximized window's controls, which belong at the right-hand end
//! of the screen rather than beside the clock, and an entry's hover row, which needs somewhere to
//! open. [`wanted_size`] is the one answer to which of those is true, and the host sizes the
//! surface to it.
//!
//! One `layout` computes every rectangle and both the painting and the hit-testing read it, so a
//! click can never land on a rectangle the paint did not use. The bar's own surface is the
//! bottom `BAR_HEIGHT` of `area`; anything above it is the hover row, drawn only when the host
//! has grown the surface to make room (the exclusive zone stays `BAR_HEIGHT` across the whole
//! bottom edge whatever the surface's own width is, so neither growing the row nor shrinking to
//! the chips reflows anything).
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

/// The bar's thickness, and the exclusive zone it reserves.
pub const BAR_HEIGHT: i32 = 48;
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
const ACTIVE_WIDTH: i32 = 220;
const NETWORK_WIDTH: i32 = 96;
const CLOCK_WIDTH: i32 = 160;
const CONTROL: i32 = 40;
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
    SetMaximized(WindowId, bool),
}

/// What the bar draws this frame.
#[derive(Debug)]
pub struct BarData<'a> {
    pub entries: &'a [Entry],
    /// Application icons by canonical id; absent keys draw an initial.
    pub icons: &'a HashMap<String, Bitmap>,
    pub clock_label: &'a str,
    pub network_control_available: bool,
    /// The activated window and whether it is maximized — the one whose controls the bar holds.
    pub activated: Option<(WindowId, bool)>,
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
}

struct Layout {
    bar: Rect,
    network: Rect,
    clock: Rect,
    cells: Vec<Rect>,
    controls: Option<[Rect; 3]>,
    /// The open row's items, one rect per window of the open entry.
    row: Vec<Rect>,
}

/// The surface size the host should ask for, in logical pixels.
///
/// Normally the chips and nothing else: there is no bar to see when there is nothing on one. Two
/// things claim the whole width instead — a maximized window's controls, which belong at the
/// right-hand end of the screen, and an open hover row, which needs room to open into.
/// `full_width` is the output's own logical width, which the shell already knows from its Home
/// surface; the compositor never configures this surface's width, so the answer has to come from
/// here.
///
/// Pure, because getting it wrong means either a bar that clips its own clock or a full-width
/// band the user explicitly said they did not want, and neither is worth finding out by looking.
pub fn wanted_size(
    entries: &[Entry],
    activated: Option<(WindowId, bool)>,
    row_open: bool,
    full_width: i32,
) -> (i32, i32) {
    let full_width = full_width.max(1);
    let height = BAR_HEIGHT + if row_open { ROW_BAND } else { 0 };
    if row_open || activated.is_some_and(|(_, maximized)| maximized) {
        return (full_width, height);
    }
    let chips: i32 = [NETWORK_WIDTH, CLOCK_WIDTH]
        .into_iter()
        .chain(
            entries
                .iter()
                .map(|entry| if entry.active { ACTIVE_WIDTH } else { CELL }),
        )
        .map(|width| width + CHIP_GAP)
        .sum();
    ((2 * CHIP_PAD + chips - CHIP_GAP).min(full_width), height)
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

    /// The next moment the hover state changes on its own — when a dwell opens a row, or a grace
    /// period closes one — so the host can wake then and nothing else.
    pub fn next_deadline_ms(&self, entries: &[Entry]) -> Option<u64> {
        let open = self.hover.and_then(|(index, since)| {
            (self.open_row != Some(index)
                && entries
                    .get(index)
                    .is_some_and(|entry| entry.windows.len() > 1))
            .then_some(since + HOVER_OPEN_MS)
        });
        let close = self.left_at.map(|left| left + HOVER_CLOSE_MS);
        match (open, close) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn layout(ui: &Ui<'_>, area: Rect, data: &BarData<'_>, open_row: Option<usize>) -> Layout {
        let bar_height = ui.px(BAR_HEIGHT).min(area.h);
        let bar = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
        let chip_h = ui.px(CHIP_H).min(bar.h);
        let chip_y = bar.y + (bar.h - chip_h) / 2;
        let gap = ui.px(CHIP_GAP);

        // The maximized window's controls, if there are any, are placed first: they own the right
        // end of the bar, and the chips stop before them rather than sliding underneath.
        let control = ui.px(CONTROL).min(chip_h);
        let controls = data
            .activated
            .is_some_and(|(_, maximized)| maximized)
            .then(|| {
                let right = bar.right() - ui.px(CHIP_PAD);
                std::array::from_fn(|which| {
                    let from_right = i32::try_from(3 - which).unwrap_or(1);
                    Rect::new(right - from_right * control, chip_y, control, chip_h)
                })
            });
        let limit = controls.map_or(bar.right(), |c: [Rect; 3]| c[0].x - gap);

        // The chips, left to right from the bar's own left edge. Each one is clipped to whatever
        // room is left rather than being dropped: a half-width clock still says the time.
        let mut x = bar.x + ui.px(CHIP_PAD);
        let mut chip = |width: i32| {
            let rect = Rect::new(x, chip_y, width.min((limit - x).max(0)), chip_h);
            x += width + gap;
            rect
        };
        let network = chip(ui.px(NETWORK_WIDTH));
        let clock = chip(ui.px(CLOCK_WIDTH));
        let cells: Vec<Rect> = data
            .entries
            .iter()
            .map(|entry| chip(ui.px(if entry.active { ACTIVE_WIDTH } else { CELL })))
            .collect();

        let row =
            match open_row.and_then(|index| Some((cells.get(index)?, data.entries.get(index)?))) {
                Some((cell, entry)) if area.h > bar_height => {
                    let item_h = (area.h - bar_height - ui.px(8)).min(ui.px(ROW_BAND - 8));
                    let item_w = ui.px(ROW_ITEM_WIDTH).min(area.w);
                    let count = entry.windows.len() as i32;
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
        Layout {
            bar,
            network,
            clock,
            cells,
            controls,
            row,
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
        let layout = Self::layout(ui, area, data, self.open_row);
        self.update_hover(ui, &layout, data);
        let layout = Self::layout(ui, area, data, self.open_row);

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

        // The row first: it floats over whatever is above the bar and claims its clicks before
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
                    design::text(),
                    Align::Left,
                );
                if ui.input.take_click(*rect) {
                    actions.push(BarAction::Activate(*window));
                    self.open_row = None;
                    self.left_at = None;
                }
            }
        }

        // Indicators. The label comes from the action manifest, so the bar and the cheat sheet
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

        chip(ui, layout.clock, design::TAG_FILL);
        ui.label_styled(
            layout.clock.inset(ui.px(6)),
            data.clock_label,
            design::state_style(ui),
            design::TAG_INK,
            Align::Centre,
        );

        // The taskbar: one chip per entry, in the same colours the compositor gives that window's
        // own name tag, so the two read as the same object.
        for (index, (cell, entry)) in layout.cells.iter().zip(data.entries).enumerate() {
            let hovered = ui.input.hovering(*cell) || self.open_row == Some(index);
            let (fill, ink) = match (entry.active, hovered) {
                (true, _) => (design::accent_soft(), design::text()),
                (false, true) => (design::accent(), design::TAG_INK),
                (false, false) => (design::TAG_FILL, design::TAG_INK),
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
            if entry.active {
                // The active entry wears the window's name tag: its icon and its title.
                let title = Rect::new(
                    icon.right() + ui.px(6),
                    cell.y,
                    (cell.right() - icon.right() - ui.px(12)).max(0),
                    cell.h,
                );
                ui.label_styled(
                    title,
                    &entry.label,
                    design::state_style(ui),
                    ink,
                    Align::Left,
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

        // The maximized window's controls, at the right-hand end of the full-width bar — the
        // place they sit on the window's own strip, and the reason the bar takes the width.
        if let (Some(controls), Some((window, _))) = (&layout.controls, data.activated) {
            let stroke = ui.px(2).max(1);
            for (which, rect) in controls.iter().enumerate() {
                let hovered = ui.input.hovering(*rect);
                chip(
                    ui,
                    *rect,
                    if hovered {
                        design::danger()
                    } else {
                        design::TAG_FILL
                    },
                );
                let ink = design::TAG_INK;
                let glyph = Rect::new(
                    rect.x + (rect.w - ui.px(12)) / 2,
                    rect.y + (rect.h - ui.px(12)) / 2,
                    ui.px(12),
                    ui.px(12),
                );
                match which {
                    0 => ui.painter.fill_rect(
                        Rect::new(glyph.x, glyph.bottom() - stroke, glyph.w, stroke),
                        ink,
                    ),
                    1 => ui.painter.stroke_rect(glyph, stroke, ink),
                    _ => ui.label(*rect, "×", ink, Align::Centre),
                }
                if ui.input.take_click(*rect) {
                    actions.push(match which {
                        0 => BarAction::Window {
                            id: "window.park",
                            window,
                        },
                        1 => BarAction::SetMaximized(window, false),
                        _ => BarAction::Window {
                            id: "window.close",
                            window,
                        },
                    });
                }
            }
        }

        // A press on empty bar ground: the compositor gave this surface the keyboard for it, and
        // the activated window is the one that should have it. Only where there *is* ground —
        // when the strip is chips alone the surface is the chips, and the pixels between them are
        // padding rather than a band to click on.
        if (layout.controls.is_some() || self.open_row.is_some())
            && ui.input.take_click(layout.bar)
            && let Some((window, _)) = data.activated
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

    /// The output's logical width, which is what the host passes as `full_width`.
    const W: i32 = 1200;

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
                buffer: WindowBuffer::new(W as u32, (BAR_HEIGHT + ROW_BAND) as u32),
                input: Input::new(),
                now: 0,
                width: W,
                height: BAR_HEIGHT,
            }
        }

        fn activated(&self) -> Option<(WindowId, bool)> {
            self.toplevels
                .iter()
                .find(|t| t.activated)
                .map(|t| (t.window_id, t.maximized))
        }

        /// The surface the host would give the bar this frame — width included, exactly as
        /// `app.rs` computes it, so a test never clicks somewhere the real surface does not have.
        fn wanted(&self) -> (i32, i32) {
            wanted_size(
                &taskbar::entries(&self.toplevels),
                self.activated(),
                self.screen.open_row.is_some(),
                W,
            )
        }

        fn frame(&mut self) -> Vec<BarAction> {
            let entries = taskbar::entries(&self.toplevels);
            let activated = self.activated();
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                clock_label: "MON 7 SEP 12:00",
                network_control_available: true,
                activated,
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
            let top = BAR_HEIGHT + ROW_BAND - height;
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

        fn cells(&mut self) -> Vec<Rect> {
            let entries = taskbar::entries(&self.toplevels);
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                clock_label: "",
                network_control_available: true,
                activated: None,
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
            let top = BAR_HEIGHT + ROW_BAND - self.height;
            BarScreen::layout(&ui, Rect::new(0, top, self.width, self.height), &data, None).cells
        }
    }

    /// The bar surface has no alpha: softbuffer attaches an `Xrgb8888` shm buffer, so a pixel
    /// this paint does not write shows as black rather than as the desktop. Painting only the
    /// chips left a black block in the corner of the screen with the chips on it — the mess the
    /// user reported. Every pixel of the surface, in every state, must be written every frame.
    #[test]
    fn every_pixel_of_the_surface_is_painted() {
        const SENTINEL: u32 = 0x00ff_00ff;
        for toplevels in [
            Vec::new(),
            vec![window(1, "org.example.Editor", "notes", true, true)],
        ] {
            let mut h = Harness::new(toplevels);
            h.frame();
            let (width, height) = (h.width, h.height);
            h.buffer.pixels.fill(SENTINEL);
            h.frame();
            let stride = h.buffer.width as usize;
            let top = BAR_HEIGHT + ROW_BAND - height;
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
        assert_eq!(h.wanted(), (W, BAR_HEIGHT + ROW_BAND));
        h.frame();
        let actions = h.click(files.x + 20, 12);
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
        assert_eq!(h.screen.next_deadline_ms(&entries), Some(HOVER_OPEN_MS));
        h.now = HOVER_OPEN_MS;
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        // Off the bar entirely: a grace period, then closed.
        h.input.cursor_left();
        h.frame();
        assert_eq!(h.screen.open_row, Some(0));
        assert_eq!(
            h.screen.next_deadline_ms(&entries),
            Some(HOVER_OPEN_MS + HOVER_CLOSE_MS)
        );
        h.now += HOVER_CLOSE_MS;
        h.frame();
        assert_eq!(h.screen.open_row, None);
    }

    #[test]
    fn controls_exist_only_for_a_maximized_activated_window_and_act_on_it() {
        let mut h = Harness::new(vec![window(1, "org.example.Editor", "notes", true, false)]);
        h.frame();
        // Not maximized: the surface is the chip strip, so the right-hand end of the output is
        // not this window at all — there is no bar ground out there to press.
        assert!(h.width < W, "the strip should be narrower than the output");
        let actions = h.click(W - 20, BAR_HEIGHT + ROW_BAND - 24);
        assert!(actions.is_empty(), "{actions:?}");
        h.toplevels[0].maximized = true;
        // One frame to take the width the controls need, then the controls are there to press.
        h.frame();
        assert_eq!(h.width, W);
        h.frame();
        let close = h.click(W - 20, BAR_HEIGHT + ROW_BAND - 24);
        assert_eq!(
            close,
            vec![BarAction::Window {
                id: "window.close",
                window: WindowId(1)
            }]
        );
        let maximize = h.click(W - 60, BAR_HEIGHT + ROW_BAND - 24);
        assert_eq!(maximize, vec![BarAction::SetMaximized(WindowId(1), false)]);
        let park = h.click(W - 100, BAR_HEIGHT + ROW_BAND - 24);
        assert_eq!(
            park,
            vec![BarAction::Window {
                id: "window.park",
                window: WindowId(1)
            }]
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

    /// The user's rule, as arithmetic: no bar unless there is something on one that needs the
    /// width. Chips normally; the whole output only for a maximized window's controls or an open
    /// row.
    #[test]
    fn wanted_size_is_the_chips_alone_until_a_row_or_controls_need_the_width() {
        let none: Vec<Entry> = Vec::new();
        let bare = wanted_size(&none, None, false, W);
        assert_eq!(
            bare,
            (
                2 * CHIP_PAD + NETWORK_WIDTH + CHIP_GAP + CLOCK_WIDTH,
                BAR_HEIGHT
            ),
            "with nothing running the bar is the network chip and the clock chip"
        );
        assert!(bare.0 < W, "and nothing like the width of the output");

        let toplevels = vec![
            window(1, "org.example.Editor", "notes", true, false),
            window(2, "org.legeos.Files", "Downloads", false, false),
        ];
        let entries = taskbar::entries(&toplevels);
        let with_windows = wanted_size(&entries, Some((WindowId(1), false)), false, W);
        assert_eq!(with_windows.0, bare.0 + 2 * CHIP_GAP + ACTIVE_WIDTH + CELL);
        assert_eq!(
            with_windows.1, BAR_HEIGHT,
            "no row, so no band above the bar"
        );
        assert!(with_windows.0 < W);

        // A maximized window's controls belong at the right-hand end of the screen.
        assert_eq!(
            wanted_size(&entries, Some((WindowId(1), true)), false, W),
            (W, BAR_HEIGHT)
        );
        // An open row needs both the width to open into and the band to open in.
        assert_eq!(
            wanted_size(&entries, Some((WindowId(1), false)), true, W),
            (W, BAR_HEIGHT + ROW_BAND)
        );
        // A strip wider than the output is the output's width, never more.
        assert_eq!(wanted_size(&entries, None, false, 100), (100, BAR_HEIGHT));
    }

    #[test]
    fn the_network_indicator_runs_its_declared_action() {
        let mut h = Harness::new(Vec::new());
        h.frame();
        let actions = h.click(40, BAR_HEIGHT + ROW_BAND - 24);
        assert_eq!(
            actions,
            vec![BarAction::Field(FieldAction::RunAction(
                "network.open-control"
            ))]
        );
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
