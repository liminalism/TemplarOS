//! The omnibox's dropdown: the federated result list, under the one search control.
//!
//! It draws over whatever is behind it rather than reserving space, because it exists only while
//! the control has focus and a person is mid-question. The row shape is the launcher tile's —
//! a mark, a name, and a second line saying which thing this is — kept because it was the right
//! shape and the launcher tile is what this replaces.
//!
//! Two shapes, one list. The launcher's answer is a dropdown under the field: a name and an id
//! per row, and nothing that needs room. A file search's answer is a **panel** (plan §4.2) — a
//! partial-screen sheet, about two thirds of the field each way and centred, with a hairline
//! edge and no shadow (plan §6) — because each of its rows carries a path, a size and a time,
//! and a dropdown row has room for one of the three. Which shape is not a mode and not a
//! setting: [`crate::omnibox::is_panel`] reads it off the rows, so the surface that composed
//! them decides, and the two can never disagree about which is on screen.

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::Ui;
use std::collections::HashMap;

use crate::omnibox::{FileState, Hit, Row, RowExtra};
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
/// How much of the field the search panel takes, as a fraction: two thirds each way, centred.
const PANEL_NUMERATOR: i32 = 2;
const PANEL_DENOMINATOR: i32 = 3;
/// The panel's own header, naming its three columns.
const PANEL_HEADER: i32 = 22;

/// Where the results are, whichever shape they take. The one function a caller should ask, so
/// that hit-testing and drawing cannot end up with two different rectangles.
pub fn area(
    ui: &Ui<'_>,
    anchor: Rect,
    bounds: Rect,
    rows: &[Row],
    files: FileState,
) -> Option<Rect> {
    if crate::omnibox::is_panel(rows) {
        return Some(panel_area(bounds));
    }
    panel_rect(ui, anchor, bounds, rows.len(), files)
}

/// The dropdown row under `(x, y)`, by the arithmetic `draw` places rows with — the panel's
/// inset and one `ROW_HEIGHT` per row, only rows that fit. `None` in the panel shape, whose rows
/// are not draggable onto the field.
pub fn row_at(
    ui: &Ui<'_>,
    anchor: Rect,
    bounds: Rect,
    rows: &[Row],
    files: FileState,
    x: i32,
    y: i32,
) -> Option<usize> {
    if crate::omnibox::is_panel(rows) {
        return None;
    }
    let inner = panel_rect(ui, anchor, bounds, rows.len(), files)?.inset(ui.px(4));
    if !inner.contains(x, y) {
        return None;
    }
    let row_height = ui.px(ROW_HEIGHT);
    let index = usize::try_from((y - inner.y) / row_height).ok()?;
    let fits = inner.y + (index as i32 + 1) * row_height <= inner.bottom();
    (fits && index < rows.len().min(MAX_VISIBLE)).then_some(index)
}

/// The search panel's rectangle: two thirds of the field each way, centred on it. Independent of
/// how many rows there are — a panel that resized as answers arrived would move the row under
/// the pointer between one frame and the next.
fn panel_area(bounds: Rect) -> Rect {
    let w = (bounds.w * PANEL_NUMERATOR / PANEL_DENOMINATOR).max(1);
    let h = (bounds.h * PANEL_NUMERATOR / PANEL_DENOMINATOR).max(1);
    Rect::new(
        bounds.x + (bounds.w - w) / 2,
        bounds.y + (bounds.h - h) / 2,
        w,
        h,
    )
}

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
    if crate::omnibox::is_panel(rows) {
        return draw_panel(ui, panel_area(bounds), rows, state, files);
    }
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

/// The search panel: directories then files, each with its path, size and time, and the row
/// that hands the question to the search application pinned to the bottom.
///
/// Pinned rather than merely last: it is always reachable without scrolling, because it is the
/// answer to "this is not what I was looking for" and that is exactly the moment a person is
/// least willing to hunt for a control.
fn draw_panel(
    ui: &mut Ui<'_>,
    panel: Rect,
    rows: &[Row],
    state: &OmniboxState,
    files: FileState,
) -> OmniboxOutcome {
    // A hairline edge and no shadow: this is a sheet of the same surface, not a card floating
    // over it (plan §6).
    design::shell(
        ui,
        panel,
        design::SHELL,
        design::EDGE_STRONG,
        Elevation::Rest,
    );
    let inner = panel.inset(ui.px(6));
    let row_height = ui.px(ROW_HEIGHT);
    let (header, body) = inner.split_top(ui.px(PANEL_HEADER));
    draw_panel_header(ui, header);

    // The last row is the way out, and it keeps its place at the foot of the panel whatever the
    // list above it does.
    let everywhere = rows
        .iter()
        .position(|row| row.extra == RowExtra::Everywhere);
    let (body, footer) = if everywhere.is_some() {
        body.split_top((body.h - row_height).max(0))
    } else {
        (body, Rect::new(body.x, body.bottom(), body.w, 0))
    };

    let results: Vec<(usize, &Row)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.extra != RowExtra::Everywhere)
        .collect();
    let visible = (body.h / row_height).max(0) as usize;
    // Scroll only as far as the highlight demands, so the list does not move under a person who
    // is reading rather than stepping.
    let first = state
        .highlighted
        .min(results.len().saturating_sub(1))
        .saturating_sub(visible.saturating_sub(1));

    let mut outcome = OmniboxOutcome::None;
    let mut y = body.y;
    for (index, row) in results.iter().skip(first).take(visible) {
        let rect = Rect::new(body.x, y, body.w, row_height);
        if draw_panel_row(ui, rect, row, *index == state.highlighted) {
            outcome = commit(row);
        }
        y += row_height;
    }
    if results.is_empty() {
        ui.label_styled(
            Rect::new(body.x + ui.px(10), body.y, body.w, row_height),
            match files {
                // An answer that has not arrived is not "nothing found", and saying so would be
                // a lie that flickers into truth a moment later.
                FileState::Ready => "No matches here.",
                FileState::Idle | FileState::Searching => "Searching…",
            },
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Left,
        );
    }

    if let Some(index) = everywhere {
        ui.painter.fill_rect(
            Rect::new(footer.x, footer.y, footer.w, ui.scale.hairline()),
            design::EDGE,
        );
        if draw_panel_row(ui, footer, &rows[index], index == state.highlighted) {
            outcome = commit(&rows[index]);
        }
    }
    outcome
}

/// The three columns, named once at the top rather than guessed at per row.
fn draw_panel_header(ui: &mut Ui<'_>, header: Rect) {
    let (modified, rest) = header.split_right(ui.px(150).min(header.w / 4));
    let (size, name) = rest.split_right(ui.px(100).min(rest.w / 4));
    let style = TextStyle::tracked(ui.theme.body.face, 10.0, 0.10);
    for (rect, text, align) in [
        (
            Rect::new(name.x + ui.px(10), name.y, name.w, name.h),
            "NAME",
            Align::Left,
        ),
        (size, "SIZE", Align::Right),
        (
            Rect::new(
                modified.x,
                modified.y,
                (modified.w - ui.px(10)).max(0),
                modified.h,
            ),
            "MODIFIED",
            Align::Right,
        ),
    ] {
        ui.label_styled(rect, text, style, design::TEXT_FAINT, align);
    }
    ui.painter.fill_rect(
        Rect::new(
            header.x,
            header.bottom() - ui.scale.hairline(),
            header.w,
            ui.scale.hairline(),
        ),
        design::EDGE,
    );
}

/// One panel row: the name and its folder on the left, the size and the time in their columns.
fn draw_panel_row(ui: &mut Ui<'_>, rect: Rect, row: &Row, highlighted: bool) -> bool {
    let hovered = ui.input.hovering(rect);
    if highlighted || hovered {
        ui.painter.fill_rounded_rect(
            rect.inset(ui.px(1)),
            ui.px(design::CHIP_RADIUS),
            design::ACCENT_SOFT,
        );
    }
    let (changed_at, left_of_it) = rect.split_right(ui.px(150).min(rect.w / 4));
    let (size, name) = left_of_it.split_right(ui.px(100).min(left_of_it.w / 4));
    let text = Rect::new(
        name.x + ui.px(10),
        name.y,
        (name.w - ui.px(10)).max(0),
        name.h,
    );
    let (title, detail) = text.split_top(rect.h / 2);
    let marker = if matches!(row.hit, Hit::Directory { .. }) {
        "/"
    } else {
        ""
    };
    ui.label_styled(
        title,
        &format!("{}{marker}", row.label),
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
    if let RowExtra::File {
        size_bytes,
        mtime_ms,
    } = row.extra
    {
        let size_label = if matches!(row.hit, Hit::Directory { .. }) {
            "—".to_owned()
        } else {
            crate::format::bytes(size_bytes)
        };
        ui.label_styled(
            size,
            &size_label,
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Right,
        );
        ui.label_styled(
            Rect::new(
                changed_at.x,
                changed_at.y,
                (changed_at.w - ui.px(10)).max(0),
                changed_at.h,
            ),
            &crate::format::timestamp_ms(mtime_ms),
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Right,
        );
    }
    ui.input.take_click(rect)
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
            extra: RowExtra::None,
        }
    }

    fn file_row(label: &str, path: &str) -> Row {
        Row {
            extra: RowExtra::File {
                size_bytes: 2048,
                mtime_ms: 1_700_000_000_000,
            },
            ..row(label, Hit::File { path: path.into() })
        }
    }

    fn everywhere_row() -> Row {
        Row {
            extra: RowExtra::Everywhere,
            ..row(
                "Search everywhere with FSearch",
                Hit::App {
                    id: crate::omnibox::SEARCH_EVERYWHERE.parse().unwrap(),
                },
            )
        }
    }

    /// A launcher answer is a dropdown under the field; a file search's is a panel, about two
    /// thirds of the field each way and centred on it. The rect a click is tested against and
    /// the rect that gets drawn are the same one, which is what [`area`] exists to guarantee.
    #[test]
    fn a_file_search_is_a_centred_panel_and_a_launcher_answer_is_a_dropdown() {
        let mut buffer = pixelkit_raster::WindowBuffer::new(1200, 900);
        let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
        let mut input = pixelkit_shell::Input::new();
        let mut kernel = pixelkit_raster::RasterKernel::new();
        let ui = Ui::new(
            pixelkit_raster::Painter::new(&mut buffer),
            &mut text,
            &mut input,
            crate::surface::design::theme(),
            pixelkit_shell::Scale::ONE,
            &mut kernel,
        );
        let bounds = Rect::new(0, 0, 1200, 900);
        let anchor = Rect::new(450, 40, 300, 30);

        let panel = area(
            &ui,
            anchor,
            bounds,
            &[file_row("paper.pdf", "/home/a/paper.pdf"), everywhere_row()],
            FileState::Ready,
        )
        .expect("a search always has a panel, even with no matches");
        assert_eq!(panel, Rect::new(200, 150, 800, 600));

        let dropdown = area(
            &ui,
            anchor,
            bounds,
            &[row(
                "Files",
                Hit::App {
                    id: "org.legeos.Files".parse().unwrap(),
                },
            )],
            FileState::Idle,
        )
        .expect("one row is a dropdown");
        assert_eq!(dropdown.x, anchor.x, "anchored under the field it answers");
        assert!(dropdown.y > anchor.bottom());
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
