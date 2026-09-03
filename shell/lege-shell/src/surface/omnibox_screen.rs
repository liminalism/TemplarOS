//! The omnibox's dropdown: the federated result list, under the one search control.
//!
//! It draws over whatever is behind it rather than reserving space, because it exists only while
//! the control has focus and a person is mid-question. The row shape is the launcher tile's —
//! a mark, a name, and a second line saying which thing this is — kept because it was the right
//! shape and the launcher tile is what this replaces.

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::Ui;
use std::collections::HashMap;

use crate::omnibox::{FileState, Hit, Row};
use crate::surface::design::{self, Elevation};

/// One row's height in logical pixels: enough for a 20px mark, a name and a detail line, with
/// air around them. A row below this is a sliver, and a sliver is a control that cannot be hit.
pub const ROW_HEIGHT: i32 = 38;
/// The dropdown never grows past this many rows on screen, whatever the cap allowed.
const MAX_VISIBLE: usize = 12;

/// What the dropdown asks its caller to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OmniboxOutcome {
    None,
    /// One row was committed to, by pointer or by Enter.
    Chose(Hit),
}

/// The dropdown's own cursor. Kept by the caller so it survives a frame, and reset whenever the
/// query changes — a highlight that stays on row four while the list underneath it changes is a
/// highlight that launches the wrong thing.
#[derive(Debug, Default)]
pub struct OmniboxState {
    pub highlighted: usize,
    /// The query the highlight belongs to.
    seen_query: String,
}

impl OmniboxState {
    /// Move the highlight for this frame's keys, and report whether a row was committed to.
    ///
    /// Returns the index committed, if any. The caller owns what committing means.
    pub fn handle_keys(&mut self, ui: &Ui<'_>, query: &str, rows: usize) -> Option<usize> {
        if query != self.seen_query {
            query.clone_into(&mut self.seen_query);
            self.highlighted = 0;
        }
        if rows == 0 {
            self.highlighted = 0;
            return None;
        }
        self.highlighted = self.highlighted.min(rows - 1);
        let mut committed = None;
        for key in ui.input.keys() {
            match key {
                KeyInput::Down => self.highlighted = (self.highlighted + 1).min(rows - 1),
                KeyInput::Up => self.highlighted = self.highlighted.saturating_sub(1),
                KeyInput::Home => self.highlighted = 0,
                KeyInput::End => self.highlighted = rows - 1,
                KeyInput::Enter => committed = Some(self.highlighted),
                _ => {}
            }
        }
        committed
    }
}

/// Draw the dropdown under `anchor` and report what was asked for.
///
/// `bounds` is the whole surface, so the list can be clamped inside it rather than drawn off the
/// bottom of a short screen.
/// Where the dropdown will be, given what it has to show. Pure geometry, so the field can know
/// the pointer is over the list *before* the tiles underneath get a chance to claim the click —
/// the same rule the context menu follows, and for the same reason.
pub fn panel_rect(
    ui: &Ui<'_>,
    anchor: Rect,
    bounds: Rect,
    rows: usize,
    files: FileState,
) -> Option<Rect> {
    let note = matches!(files, FileState::Searching);
    let visible = rows.min(MAX_VISIBLE);
    if visible == 0 && !note {
        return None;
    }
    let row_height = ui.px(ROW_HEIGHT);
    let note_height = if note { ui.px(20) } else { 0 };
    let height = (row_height * visible as i32 + note_height + ui.px(8))
        .min((bounds.bottom() - anchor.bottom() - ui.px(8)).max(row_height));
    Some(Rect::new(
        anchor.x,
        anchor.bottom() + ui.px(4),
        anchor.w,
        height,
    ))
}

pub fn draw(
    ui: &mut Ui<'_>,
    anchor: Rect,
    bounds: Rect,
    rows: &[Row],
    state: &OmniboxState,
    icons: &HashMap<String, Bitmap>,
    files: FileState,
) -> OmniboxOutcome {
    let note = matches!(files, FileState::Searching);
    let visible = rows.len().min(MAX_VISIBLE);
    let Some(panel) = panel_rect(ui, anchor, bounds, rows.len(), files) else {
        return OmniboxOutcome::None;
    };
    let row_height = ui.px(ROW_HEIGHT);
    ui.painter
        .shadow_rect(panel, ui.px(6), ui.px(10), design::TEXT, 80);
    design::shell(
        ui,
        panel,
        design::SHELL,
        design::EDGE_STRONG,
        Elevation::Rest,
    );

    let mut outcome = OmniboxOutcome::None;
    let inner = panel.inset(ui.px(4));
    let mut y = inner.y;
    for (index, row) in rows.iter().take(visible).enumerate() {
        if y + row_height > inner.bottom() {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.w, row_height);
        if draw_row(ui, rect, row, index == state.highlighted, icons) {
            outcome = commit(row);
        }
        y += row_height;
    }
    // The file source's own state, said once at the bottom rather than as a fake row: an answer
    // that has not arrived is not a result, and must never look like one.
    if note && y + ui.px(20) <= inner.bottom() {
        ui.label_styled(
            Rect::new(inner.x + ui.px(10), y, inner.w, ui.px(20)),
            "still searching files…",
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Left,
        );
    }
    outcome
}

/// What choosing a row means: every row is a result.
pub fn commit(row: &Row) -> OmniboxOutcome {
    OmniboxOutcome::Chose(row.hit.clone())
}

/// One row: a mark saying what kind of thing this is, the name, and the detail under it.
///
/// The mark is a word rather than an icon for everything but an application, which has a real
/// one. A tracked, faint four-letter label distinguishes the classes at a glance without
/// inventing a private icon language for them.
fn draw_row(
    ui: &mut Ui<'_>,
    rect: Rect,
    row: &Row,
    highlighted: bool,
    icons: &HashMap<String, Bitmap>,
) -> bool {
    let hovered = ui.input.hovering(rect);
    if highlighted || hovered {
        ui.painter.fill_rounded_rect(
            rect.inset(ui.px(1)),
            ui.px(design::CHIP_RADIUS),
            design::ACCENT_SOFT,
        );
    }
    let mark_size = ui.px(22).min(rect.h - ui.px(8)).max(4);
    let mark = Rect::new(
        rect.x + ui.px(6),
        rect.y + (rect.h - mark_size) / 2,
        mark_size,
        mark_size,
    );
    let icon = match &row.hit {
        Hit::App { id } => icons.get(id.as_str()),
        _ => None,
    };
    if let Some(bitmap) = icon {
        ui.painter.blit(bitmap, mark, ScaleFilter::Bilinear);
    } else {
        ui.painter
            .fill_rounded_rect(mark, ui.px(design::CHIP_RADIUS), design::RECESSED);
        ui.label_styled(
            mark,
            kind_mark(&row.hit),
            TextStyle::tracked(ui.theme.body.face, 9.0, 0.06),
            design::TEXT_FAINT,
            Align::Centre,
        );
    }
    let text = Rect::new(
        mark.right() + ui.px(10),
        rect.y,
        (rect.right() - mark.right() - ui.px(16)).max(0),
        rect.h,
    );
    if text.w > 0 {
        let (name, detail) = text.split_top(rect.h / 2);
        ui.label_styled(
            name,
            &row.label,
            design::title_style(ui),
            if highlighted || hovered {
                design::ACCENT
            } else {
                design::TEXT
            },
            Align::Left,
        );
        ui.label_styled(
            detail,
            &row.detail,
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Left,
        );
    }
    ui.input.take_click(rect)
}

fn kind_mark(hit: &Hit) -> &'static str {
    match hit {
        Hit::App { .. } => "APP",
        Hit::Directory { .. } => "DIR",
        Hit::File { .. } => "FILE",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_search::MatchKind;

    fn row(label: &str, hit: Hit) -> Row {
        Row {
            hit,
            label: label.to_owned(),
            detail: String::new(),
            kind: MatchKind::NamePrefix,
        }
    }

    /// Every kind of hit is distinguishable without an icon, so the dropdown never shows a row
    /// whose class is a guess.
    #[test]
    fn every_kind_of_hit_carries_its_own_mark() {
        let marks = [
            Hit::App {
                id: "org.legeos.Files".parse().unwrap(),
            },
            Hit::Directory { path: "/a".into() },
            Hit::File {
                path: "/a/b".into(),
            },
        ]
        .map(|hit| kind_mark(&hit));
        let mut unique = marks.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), marks.len(), "{marks:?}");
        assert!(marks.iter().all(|mark| !mark.is_empty()));
    }

    /// Committing to a row chooses exactly that row.
    #[test]
    fn committing_to_a_row_chooses_it() {
        assert_eq!(
            commit(&row("Files", Hit::Directory { path: "/a".into() })),
            OmniboxOutcome::Chose(Hit::Directory { path: "/a".into() })
        );
    }
}
