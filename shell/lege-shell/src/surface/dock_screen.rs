//! The dock screen: a Home cell, then one cell per [`DockEntry`](crate::dock::DockEntry), an
//! icon where one resolved (`crate::icons`) and a group-initial placeholder otherwise, clickable
//! to activate the group's first window.
//!
//! The Home cell is the mouse's route to "go home" (`lege_shell_manager_v1.reveal_home`): there
//! is no taskbar and no desktop to click on, so without it the reveal would be keyboard-only.

use std::collections::HashMap;

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use lege_window::WindowId;

use crate::dock::DockEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockAction {
    None,
    Activate(WindowId),
    /// The Home cell was clicked: toggle the "go home" reveal.
    RevealHome,
}

/// What the dock draws this frame, and the icons already resolved for it — resolution itself
/// (filesystem lookups) stays out of this module, in `crate::icons`, so this stays testable with
/// a plain buffer and no `.desktop` fixtures of its own.
#[derive(Debug)]
pub struct DockData<'a> {
    pub entries: &'a [DockEntry],
    /// Keyed by [`DockEntry::key`] — meaningful only when grouping is by application, where the
    /// key *is* an `AppId`; absent keys draw the placeholder.
    pub icons: &'a HashMap<String, Bitmap>,
    /// Whether a "go home" reveal is in effect: the Home cell draws selected while it is.
    pub home_revealed: bool,
}

const CELL_SIZE: i32 = 56;
const ICON_SIZE: i32 = 36;
const GAP: i32 = 8;

#[derive(Debug, Clone, Copy, Default)]
pub struct DockScreen;

impl DockScreen {
    /// Where the Home cell sits in `area` — the first cell, at the left edge. Exposed so a test
    /// can click it without re-deriving the dock's layout.
    pub fn home_cell(ui: &Ui<'_>, area: Rect) -> Rect {
        let cell = ui.px(CELL_SIZE);
        let gap = ui.px(GAP);
        Rect::new(area.x + gap, area.y + (area.h - cell) / 2, cell, cell)
    }

    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &DockData<'_>) -> DockAction {
        ui.painter.fill_rect(area, ui.theme.panel);
        ui.painter
            .horizontal_rule(area.x, area.y, area.w, ui.theme.panel_edge);

        let cell = ui.px(CELL_SIZE);
        let gap = ui.px(GAP);
        let mut action = DockAction::None;

        let home = Self::home_cell(ui, area);
        let home_fill = if data.home_revealed {
            ui.theme.selection
        } else if ui.input.hovering(home) {
            ui.theme.hover
        } else {
            ui.theme.panel_alt
        };
        ui.painter
            .fill_rounded_rect(home, ui.theme.corner_radius, home_fill);
        ui.label(home, "Home", ui.theme.text, Align::Centre);
        if ui.input.take_click(home) {
            action = DockAction::RevealHome;
        }
        let mut x = home.right() + gap;
        // A hairline separates the always-present Home cell from the windows that come and go.
        ui.painter.fill_rect(
            Rect::new(x, home.y + ui.px(8), 1, cell - ui.px(16)),
            ui.theme.panel_edge,
        );
        x += gap;

        if data.entries.is_empty() {
            ui.label(
                Rect::new(x, area.y, area.right() - x, area.h),
                "no windows",
                ui.theme.text_dim,
                Align::Left,
            );
            return action;
        }

        for entry in data.entries {
            let rect = Rect::new(x, area.y + (area.h - cell) / 2, cell, cell);
            if x + cell > area.right() {
                break; // Overflow handling (scrolling/wrapping the dock) is a follow-up.
            }
            let hovered = ui.input.hovering(rect);
            if hovered {
                ui.painter
                    .fill_rounded_rect(rect, ui.theme.corner_radius, ui.theme.hover);
            }

            let icon_size = ui.px(ICON_SIZE);
            let icon_rect = Rect::new(
                rect.x + (rect.w - icon_size) / 2,
                rect.y + (rect.h - icon_size) / 2 - ui.px(4),
                icon_size,
                icon_size,
            );
            if let Some(bitmap) = data.icons.get(&entry.key) {
                ui.painter.blit(bitmap, icon_rect, ScaleFilter::Bilinear);
            } else {
                let initial = entry
                    .key
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                ui.painter.fill_rounded_rect(
                    icon_rect,
                    ui.theme.corner_radius,
                    ui.theme.accent_soft,
                );
                ui.label(icon_rect, &initial, ui.theme.accent, Align::Centre);
            }

            if entry.window_count > 1 {
                let badge = Rect::new(rect.right() - ui.px(16), rect.y, ui.px(16), ui.px(14));
                ui.badge(
                    badge,
                    &entry.window_count.to_string(),
                    ui.theme.text,
                    ui.theme.selection,
                    ui.theme.panel_edge,
                );
            }

            let label_area = Rect::new(rect.x - gap / 2, rect.bottom(), cell + gap, ui.px(14));
            ui.label_styled(
                label_area,
                short_label(&entry.key),
                pixelkit_text::TextStyle::new(ui.theme.body.face, 11.0),
                ui.theme.text_dim,
                Align::Centre,
            );

            if ui.input.take_click(rect)
                && let Some(&window_id) = entry.window_ids.first()
            {
                action = DockAction::Activate(window_id);
            }
            x += cell + gap;
        }
        action
    }
}

/// The dock's own name for a group — the part after the last `.` of an `AppId`-shaped key, or the
/// whole key when it is not one (an `unknown-window-N`/`window-N` fallback, `grouping_key`'s own
/// doc). A full reverse-DNS id does not fit under a 56px icon.
fn short_label(key: &str) -> &str {
    key.rsplit('.').next().unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, MouseButton, Scale};
    use pixelkit_ui::Theme;

    #[test]
    fn the_dock_label_is_the_last_dotted_segment() {
        assert_eq!(short_label("org.legeos.Editor"), "Editor");
        assert_eq!(short_label("unknown-window-7"), "unknown-window-7");
        assert_eq!(short_label(""), "");
    }

    fn draw_with_click(at: Option<(f32, f32)>, entries: &[DockEntry]) -> DockAction {
        let mut buffer = WindowBuffer::new(600, 64);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut kernel = RasterKernel::new();
        let mut input = Input::new();
        if let Some((x, y)) = at {
            input.cursor_moved(x, y);
            input.mouse(MouseButton::Left, true);
            input.mouse(MouseButton::Left, false);
        }
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            Theme::default(),
            Scale::ONE,
            &mut kernel,
        );
        let icons = HashMap::new();
        let data = DockData {
            entries,
            icons: &icons,
            home_revealed: false,
        };
        DockScreen.draw(&mut ui, Rect::new(0, 0, 600, 64), &data)
    }

    #[test]
    fn clicking_the_home_cell_reveals_home_even_with_no_windows() {
        let home = Rect::new(GAP, (64 - CELL_SIZE) / 2, CELL_SIZE, CELL_SIZE);
        let centre = (
            home.x as f32 + home.w as f32 / 2.0,
            home.y as f32 + home.h as f32 / 2.0,
        );
        assert_eq!(draw_with_click(Some(centre), &[]), DockAction::RevealHome);
        assert_eq!(draw_with_click(None, &[]), DockAction::None);
    }

    #[test]
    fn clicking_a_window_cell_activates_its_first_window() {
        let entries = vec![DockEntry {
            key: "org.legeos.Files".to_owned(),
            window_count: 2,
            titles: vec!["a".to_owned(), "b".to_owned()],
            window_ids: vec![WindowId(7), WindowId(8)],
        }];
        // The first window cell follows the Home cell, a gap, the separator and another gap.
        let x = GAP + CELL_SIZE + GAP + 1 + GAP + CELL_SIZE / 2;
        let y = 32;
        assert_eq!(
            draw_with_click(Some((x as f32, y as f32)), &entries),
            DockAction::Activate(WindowId(7))
        );
    }
}
