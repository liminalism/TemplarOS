//! The dock screen: one cell per [`DockEntry`](crate::dock::DockEntry), an icon where one
//! resolved (`crate::icons`) and a group-initial placeholder otherwise, clickable to activate the
//! group's first window.

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
}

const CELL_SIZE: i32 = 56;
const ICON_SIZE: i32 = 36;
const GAP: i32 = 8;

#[derive(Debug, Clone, Copy, Default)]
pub struct DockScreen;

impl DockScreen {
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &DockData<'_>) -> DockAction {
        ui.painter.fill_rect(area, ui.theme.panel);
        ui.painter
            .horizontal_rule(area.x, area.y, area.w, ui.theme.panel_edge);

        if data.entries.is_empty() {
            ui.label(
                area.inset(ui.px(8)),
                "Dock — no windows",
                ui.theme.text_dim,
                Align::Left,
            );
            return DockAction::None;
        }

        let cell = ui.px(CELL_SIZE);
        let gap = ui.px(GAP);
        let mut action = DockAction::None;
        let mut x = area.x + gap;
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

    #[test]
    fn the_dock_label_is_the_last_dotted_segment() {
        assert_eq!(short_label("org.legeos.Editor"), "Editor");
        assert_eq!(short_label("unknown-window-7"), "unknown-window-7");
        assert_eq!(short_label(""), "");
    }
}
