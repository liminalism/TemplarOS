//! The bottom bar: network and clock at the left, the taskbar in the middle, and the maximized
//! window's controls at the right.
//!
//! One `layout` computes every rectangle and both the painting and the hit-testing read it, so a
//! click can never land on a rectangle the paint did not use. The bar's own surface is the
//! bottom `BAR_HEIGHT` of `area`; anything above it is the hover row, drawn only when the host
//! has grown the surface to make room (the exclusive zone stays `BAR_HEIGHT`, so opening the
//! row reflows nothing).
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
        let (network, rest) = bar.split_left(ui.px(NETWORK_WIDTH).min(bar.w / 4));
        let (clock, rest) = rest.split_left(ui.px(CLOCK_WIDTH).min(rest.w / 3));
        let controls_width = if data.activated.is_some_and(|(_, maximized)| maximized) {
            ui.px(CONTROL) * 3
        } else {
            0
        };
        let (controls_area, rest) = rest.split_right(controls_width.min(rest.w));
        let controls = (controls_width > 0).then(|| {
            let w = controls_area.w / 3;
            [
                Rect::new(controls_area.x, controls_area.y, w, controls_area.h),
                Rect::new(controls_area.x + w, controls_area.y, w, controls_area.h),
                Rect::new(
                    controls_area.x + 2 * w,
                    controls_area.y,
                    controls_area.w - 2 * w,
                    controls_area.h,
                ),
            ]
        });
        let widths: Vec<i32> = data
            .entries
            .iter()
            .map(|entry| ui.px(if entry.active { ACTIVE_WIDTH } else { CELL }))
            .collect();
        let total: i32 = widths.iter().sum();
        let mut x = rest.x + ((rest.w - total) / 2).max(0);
        let cells = widths
            .iter()
            .map(|w| {
                let cell = Rect::new(x, bar.y, (*w).min((rest.right() - x).max(0)), bar.h);
                x += w;
                cell
            })
            .collect::<Vec<_>>();
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
                        design::ACCENT_SOFT
                    } else {
                        design::SHELL
                    },
                    design::EDGE_STRONG,
                    Elevation::Lifted,
                );
                ui.label_styled(
                    rect.inset(ui.px(8)),
                    title,
                    design::state_style(ui),
                    design::TEXT,
                    Align::Left,
                );
                if ui.input.take_click(*rect) {
                    actions.push(BarAction::Activate(*window));
                    self.open_row = None;
                    self.left_at = None;
                }
            }
        }

        design::shell(
            ui,
            layout.bar,
            design::SHELL,
            design::EDGE_STRONG,
            Elevation::Rest,
        );

        // Indicators. The label comes from the action manifest, so the bar and the cheat sheet
        // cannot call the same capability two different things.
        let network_label = lege_types::action::action("network.open-control")
            .map_or("Network", |declaration| declaration.label);
        if data.network_control_available && ui.input.hovering(layout.network) {
            ui.painter
                .fill_rect(layout.network.inset(ui.px(4)), design::ACCENT_SOFT);
        }
        ui.label_styled(
            layout.network.inset(ui.px(8)),
            network_label,
            design::state_style(ui),
            if data.network_control_available {
                design::ACCENT
            } else {
                design::TEXT_DIM
            },
            Align::Centre,
        );
        if data.network_control_available && ui.input.take_click(layout.network) {
            actions.push(BarAction::Field(FieldAction::RunAction(
                "network.open-control",
            )));
        }
        ui.label_styled(
            layout.clock.inset(ui.px(8)),
            data.clock_label,
            design::state_style(ui),
            design::TEXT,
            Align::Centre,
        );
        ui.painter.fill_rect(
            Rect::new(
                layout.clock.right(),
                layout.bar.y + ui.px(10),
                1,
                layout.bar.h - ui.px(20),
            ),
            design::EDGE,
        );

        // The taskbar.
        for (index, (cell, entry)) in layout.cells.iter().zip(data.entries).enumerate() {
            let hovered = ui.input.hovering(*cell);
            if entry.active {
                ui.painter.fill_rounded_rect(
                    cell.inset(ui.px(4)),
                    ui.px(design::CHIP_RADIUS),
                    design::ACCENT_SOFT,
                );
            } else if hovered || self.open_row == Some(index) {
                ui.painter.fill_rounded_rect(
                    cell.inset(ui.px(4)),
                    ui.px(design::CHIP_RADIUS),
                    design::RECESSED,
                );
            }
            let icon_side = ui.px(ICON).min(cell.h - ui.px(8));
            let icon = Rect::new(
                cell.x + ui.px(8),
                cell.y + (cell.h - icon_side) / 2,
                icon_side,
                icon_side,
            );
            if let Some(bitmap) = entry.app_id.as_ref().and_then(|id| data.icons.get(id)) {
                ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
            } else {
                let plate = entry.accent.unwrap_or(design::ACCENT_SOFT);
                ui.painter
                    .fill_rounded_rect(icon, ui.px(design::CHIP_RADIUS), plate);
                let mark = if entry.accent.is_some() {
                    entry.windows.len().to_string()
                } else {
                    entry
                        .label
                        .chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string()
                };
                let ink = if entry.accent.is_some() {
                    0x00ff_ffff
                } else {
                    design::ACCENT
                };
                ui.label(icon, &mark, ink, Align::Centre);
            }
            if entry.active {
                // The active entry wears the window's name tag: its icon and its title.
                let title = Rect::new(
                    icon.right() + ui.px(8),
                    cell.y,
                    (cell.right() - icon.right() - ui.px(16)).max(0),
                    cell.h,
                );
                ui.label_styled(
                    title,
                    &entry.label,
                    design::state_style(ui),
                    design::TEXT,
                    Align::Left,
                );
            }
            if let Some(accent) = entry.accent {
                ui.painter.fill_rect(
                    Rect::new(
                        cell.x + ui.px(6),
                        cell.bottom() - ui.px(3),
                        cell.w - ui.px(12),
                        ui.px(3),
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

        // The maximized window's controls, bottom-right.
        if let (Some(controls), Some((window, _))) = (&layout.controls, data.activated) {
            let ink = design::TEXT_DIM;
            let stroke = ui.px(2).max(1);
            for (which, rect) in controls.iter().enumerate() {
                if ui.input.hovering(*rect) {
                    ui.painter
                        .fill_rect(rect.inset(ui.px(4)), design::ACCENT_SOFT);
                }
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
        // the activated window is the one that should have it.
        if ui.input.take_click(layout.bar)
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
                height: BAR_HEIGHT,
            }
        }

        fn frame(&mut self) -> Vec<BarAction> {
            let entries = taskbar::entries(&self.toplevels);
            let activated = self
                .toplevels
                .iter()
                .find(|t| t.activated)
                .map(|t| (t.window_id, t.maximized));
            let data = BarData {
                entries: &entries,
                icons: &self.icons,
                clock_label: "MON 7 SEP 12:00",
                network_control_available: true,
                activated,
                now_ms: self.now,
            };
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let top = BAR_HEIGHT + ROW_BAND - self.height;
            let actions = self
                .screen
                .draw(&mut ui, Rect::new(0, top, W, self.height), &data);
            drop(ui);
            self.input.end_frame();
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
            BarScreen::layout(&ui, Rect::new(0, top, W, self.height), &data, None).cells
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
        // The host makes room; the row's first item activates its window and closes the row.
        h.height = BAR_HEIGHT + ROW_BAND;
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
        let actions = h.click(W - 20, BAR_HEIGHT + ROW_BAND - 24);
        // No controls: a press on bar ground hands the keyboard back to the activated window.
        assert_eq!(actions, vec![BarAction::Activate(WindowId(1))]);
        h.toplevels[0].maximized = true;
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
        h.height = BAR_HEIGHT + ROW_BAND;
        h.screen.open_row = Some(2);
        h.frame();
        if let Ok(path) = std::env::var("LEGE_BAR_BASELINE_DUMP") {
            crate::surface::offscreen::write_png(std::path::Path::new(&path), &h.buffer)
                .expect("dumping the bar render");
        }
        assert!(h.buffer.pixels.iter().any(|&p| p != 0));
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
