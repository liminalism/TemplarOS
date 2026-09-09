//! Taskfield's labels: the words over the compositor's window overview.
//!
//! Taskfield is a **view, not a place**. The compositor pulls the scene back and draws the live
//! windows at card size — no configure, no geometry change, nothing stored against a window — and
//! this surface floats over it on [`templar_window::Layer::Presentation`] with the things a
//! picture of a window cannot say: whose window it is, which unit it belongs to, what was typed,
//! and how many did not fit.
//!
//! **It draws and does not touch.** Every key and every click in the mode is the compositor's:
//! it owns the selection, the query and the activation, and it reflects all three back through
//! `taskfield_state`. So this surface commits an *empty input region* — a title band that took
//! the pointer would swallow the click that was meant for the window under it, and a shell that
//! is not running must change nothing about how the mode works. That is also why nothing here is
//! interactive: with this process dead the compositor draws its own labels and the overview is
//! exactly as usable.
//!
//! The cards are not on the wire either. This runs [`templar_window::taskfield::layout`] over the
//! toplevels and outputs it already holds — the same function the compositor runs, on the same
//! records — so a label cannot land anywhere but on its card. If the two ever diverge the answer
//! is a `taskfield_card` event, not a second layout.

use std::collections::HashMap;

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use templar_window::taskfield::{Section, TaskFilter};
use templar_window::{ShellToplevel, WindowId};

use crate::surface::design::{self, Elevation};
use crate::wayland_bridge::TaskfieldStatus;

/// A compositor-logical rectangle in the surface's own coordinates. The Taskfield surface is the
/// whole output and its origin is the output's, so the two differ only in which crate's `Rect`
/// they are — but they are different types on purpose, and this is the one place they meet.
fn on_surface(rect: templar_window::Rect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

/// The label band along the bottom of a card: an icon and a title.
const LABEL_BAND: i32 = 26;
/// The icon in that band, and the gap either side of it.
const LABEL_ICON: i32 = 16;
const LABEL_PAD: i32 = 6;
/// The banner at the top of the output: the place's name, the shelf, or what is being typed.
const BANNER_H: i32 = 32;
const BANNER_TOP: i32 = 24;

/// What Taskfield draws this frame.
#[derive(Debug)]
pub struct TaskfieldData<'a> {
    /// What the compositor says the mode is showing.
    pub status: &'a TaskfieldStatus,
    /// The layout, from `taskfield::layout` over the same records the compositor used.
    pub sections: &'a [Section],
    pub toplevels: &'a [ShellToplevel],
    /// Application icons by canonical id; absent keys draw nothing rather than a wrong picture.
    pub icons: &'a HashMap<String, Bitmap>,
}

/// The overview's chrome. Holds no state of its own — the mode's state is the compositor's, and
/// arrives whole in every `taskfield_state` — beyond the shapes the last frame painted solid.
#[derive(Debug, Default)]
pub struct TaskfieldScreen {
    opaque: Vec<(Rect, i32)>,
}

impl TaskfieldScreen {
    /// The shapes this frame painted solid, for the host's alpha mask. **Not** the input region:
    /// the mode's input is the compositor's, and this surface claims none of it.
    #[must_use]
    pub fn opaque_shapes(&self) -> &[(Rect, i32)] {
        &self.opaque
    }

    /// Paint the labels over the cards the compositor is presenting.
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &TaskfieldData<'_>) {
        self.opaque.clear();
        // The colour buffer has no alpha of its own, so every pixel is written and the coverage
        // recorded beside it. Where nothing is recorded the mask is clear and the compositor's
        // own scrim — and the windows under it — show through untouched.
        ui.painter.fill_rect(area, design::ground());
        if !data.status.active {
            return;
        }

        self.draw_banner(ui, area, data);
        for section in data.sections {
            self.draw_section(ui, section, data);
        }
    }

    /// The one line at the top: what this view is, and what has been typed into it.
    ///
    /// A filter the person chose deliberately says so — an overview of one place's windows that
    /// does not name the place is an overview of a mystery — and `All` says nothing at all,
    /// because "every window" is what the screen already looks like.
    fn draw_banner(&mut self, ui: &mut Ui<'_>, area: Rect, data: &TaskfieldData<'_>) {
        let scope = match &data.status.filter {
            // `All` says nothing, and neither does a unit — that one is already named by its own
            // section header, right over its cards.
            TaskFilter::All | TaskFilter::Unit(_) => None,
            // The compositor never learns what a place is: the key is the name the shell sent
            // with the ids, and this is the only place it means anything.
            TaskFilter::Windows(_) => Some(if data.status.key.is_empty() {
                "Windows".to_owned()
            } else {
                data.status.key.clone()
            }),
            TaskFilter::Shelved => Some("Shelf".to_owned()),
        };
        let query = data.status.query.trim();
        let text = match (scope, query.is_empty()) {
            (None, true) => return,
            (Some(scope), true) => scope,
            (None, false) => format!("\u{2315} {query}"),
            (Some(scope), false) => format!("{scope} \u{b7} \u{2315} {query}"),
        };
        let style = design::state_style(ui);
        let width = (ui.text.measure(&text, style) + ui.px(2 * BANNER_H)).min(area.w);
        let height = ui.px(BANNER_H);
        let frame = Rect::new(
            area.x + (area.w - width) / 2,
            area.y + ui.px(BANNER_TOP),
            width,
            height,
        );
        self.opaque.push((frame, ui.px(design::TILE_RADIUS)));
        design::shell(
            ui,
            frame,
            design::shell_fill(),
            design::edge_strong(),
            Elevation::Lifted,
        );
        ui.label_styled(frame, &text, style, design::text(), Align::Centre);
    }

    /// One unit's header and the labels on its cards.
    fn draw_section(&mut self, ui: &mut Ui<'_>, section: &Section, data: &TaskfieldData<'_>) {
        let style = design::state_style(ui);
        let name = crate::taskbar::display_label(
            &section.key,
            &section.label,
            section.cards.len() + section.overflow,
        );
        let header = on_surface(section.header);
        if header.w > 0 && header.h > 0 {
            self.opaque.push((header, ui.px(design::CHIP_RADIUS)));
            design::shell(
                ui,
                header,
                design::shell_fill(),
                design::edge(),
                Elevation::Rest,
            );
            // A group's accent, the same colour its windows wear on their own tags, so the
            // section and the band on the window read as one thing.
            let mut text_x = header.x + ui.px(LABEL_PAD);
            if let Some(accent) = section.accent {
                let swatch = Rect::new(
                    text_x,
                    header.y + ui.px(LABEL_PAD),
                    ui.px(4),
                    (header.h - ui.px(2 * LABEL_PAD)).max(1),
                );
                ui.painter.fill_rect(swatch, accent);
                text_x = swatch.right() + ui.px(LABEL_PAD);
            }
            let label = Rect::new(
                text_x,
                header.y,
                (header.right() - ui.px(LABEL_PAD) - text_x).max(1),
                header.h,
            );
            ui.label_styled(label, &name, style, design::text(), Align::Left);
            // What did not fit. The layout drops the oldest of a unit rather than shrinking every
            // card below the floor, and a card that is not on screen has to be admitted to.
            if section.overflow > 0 {
                ui.label_styled(
                    label,
                    &format!("+{} more", section.overflow),
                    style,
                    design::text_faint(),
                    Align::Right,
                );
            }
        }

        for card in &section.cards {
            let Some(toplevel) = data
                .toplevels
                .iter()
                .find(|top| top.window_id == card.window)
            else {
                continue;
            };
            self.draw_card_label(ui, on_surface(card.rect), toplevel, data);
        }
    }

    /// A card's own name, in a band along its bottom edge.
    ///
    /// Over the window rather than under it: the card *is* the window, drawn small, and a caption
    /// floating below it would be a caption on the desktop. The band is opaque so the title is
    /// legible over whatever the window happens to be showing there.
    fn draw_card_label(
        &mut self,
        ui: &mut Ui<'_>,
        card: Rect,
        toplevel: &ShellToplevel,
        data: &TaskfieldData<'_>,
    ) {
        let height = ui.px(LABEL_BAND).min(card.h);
        let band = Rect::new(card.x, card.bottom() - height, card.w, height);
        if band.w <= 0 || band.h <= 0 {
            return;
        }
        self.opaque.push((band, 0));
        ui.painter.fill_rect(band, design::shell_fill());
        let pad = ui.px(LABEL_PAD);
        let side = ui.px(LABEL_ICON).min(band.h - ui.px(4)).max(1);
        let mut text_x = band.x + pad;
        if let Some(bitmap) = toplevel
            .canonical_app_id
            .as_ref()
            .and_then(|id| data.icons.get(&id.to_string()))
        {
            let icon = Rect::new(text_x, band.y + (band.h - side) / 2, side, side);
            ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
            text_x = icon.right() + pad;
        }
        let title = Rect::new(text_x, band.y, (band.right() - pad - text_x).max(1), band.h);
        ui.label_styled(
            title,
            title_of(toplevel),
            design::state_style(ui),
            design::text(),
            Align::Left,
        );
    }
}

fn title_of(toplevel: &ShellToplevel) -> &str {
    if toplevel.title.is_empty() {
        "(untitled)"
    } else {
        &toplevel.title
    }
}

/// The one output this shell knows about, as `taskfield::layout` wants it.
///
// ponytail: one output. The shell learns its geometry from its own Home surface and has no
// `wl_output` list of its own, so on a second monitor the labels would be laid out for the first.
// The upgrade is the `taskfield_card` event the protocol already reserves the option of, not a
// second output model here.
#[must_use]
pub fn outputs(width: i32, height: i32) -> Vec<templar_window::geometry::Output> {
    vec![templar_window::geometry::Output {
        name: "shell".to_owned(),
        logical: templar_window::Rect::new(0, 0, width.max(1), height.max(1)),
        scale_milli: 1000,
        work_area: None,
    }]
}

/// Which window a point is over, for a host that needs to know. Topmost is not a question here —
/// `layout` never overlaps two cards.
#[must_use]
pub fn card_at(sections: &[Section], x: i32, y: i32) -> Option<WindowId> {
    sections
        .iter()
        .flat_map(|section| section.cards.iter())
        .find(|card| on_surface(card.rect).contains(x, y))
        .map(|card| card.window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_window::taskfield::layout;
    use templar_window::{Layer, task};

    fn window(id: u64, app: &str, title: &str) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: false,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 800, 600),
            layer: Layer::Normal,
            group: None,
        }
    }

    /// Every label lands on the card it names. The shell does not receive the rectangles: it runs
    /// the compositor's own layout over the same records, and this is the whole of why that is
    /// allowed to be true.
    #[test]
    fn a_label_is_inside_the_card_it_names() {
        let toplevels = vec![
            window(1, "org.templaros.Files", "Downloads"),
            window(2, "org.templaros.Files", "Pictures"),
            window(3, "org.example.Editor", "notes"),
        ];
        let outputs = outputs(1920, 1080);
        let sections = layout(
            &task::units(&toplevels),
            &toplevels,
            &outputs,
            &TaskFilter::All,
        );
        assert!(!sections.is_empty());
        for section in &sections {
            for card in &section.cards {
                let found = card_at(
                    &sections,
                    card.rect.x + card.rect.width / 2,
                    card.rect.bottom() - 1,
                );
                assert_eq!(
                    found,
                    Some(card.window),
                    "the label band of {:?} is not on its own card",
                    card.window
                );
                assert!(
                    card.rect.y >= 0 && card.rect.bottom() <= 1080,
                    "{:?} left the output",
                    card.rect
                );
            }
        }
    }
}
