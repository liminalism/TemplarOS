//! The launcher widget's body: the list of applications a person can start from Home.
//!
//! Every other widget's body is a headline and a few lines of detail, which
//! [`super::draw_snapshot`] handles. A launcher is the one that genuinely needs its own drawing —
//! its content is *rows*, each of which is a target — so this module owns the rows and nothing
//! else: the tile shell, the title band and the declared region strip all still come from the
//! widget contract in [`super`], and the ranking comes from [`crate::app_search`], which knows
//! nothing about pixels.
//!
//! The contract's "never draw a control too small to hit" rule is applied to rows as well as to
//! regions: rows are a fixed [`ROW_HEIGHT`] and a tile with room for none shows only its summary
//! line, rather than a stack of slivers. A bigger tile simply gets more rows, and says how many
//! matches it could not fit.

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_text::Align;
use pixelkit_ui::Ui;
use std::collections::HashMap;

use lege_field::Representation;
use lege_types::AppId;
use lege_types::action::ActionDecl;

use crate::app_search::{self, Match};
use crate::surface::design;
use crate::surface::widgets::WidgetDescriptor;

/// Everything the launcher body reads. Borrowed, never owned: the entries live in the shell's
/// refresh state and the query lives in the field's search bar.
#[derive(Debug)]
pub struct LauncherData<'a> {
    /// Every installed application, as `AppIdentity1.ListApplications` reported it.
    pub entries: &'a [app_search::AppEntry],
    /// What the Home search bar currently holds. Empty means "no filter", not "no results".
    pub query: &'a str,
    /// Decoded icons by canonical app id, filled in by the host as it resolves them.
    pub icons: &'a HashMap<String, Bitmap>,
    /// Whether the registry answered at all. A launcher that cannot reach `AppIdentity1` says so
    /// rather than showing an empty list, which reads as "nothing is installed".
    pub available: bool,
}

/// What a click on the launcher asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherOutcome {
    None,
    /// A row was clicked: start this application. The declared action is `launcher.launch`.
    Launch(AppId),
    /// A declared region was clicked.
    Action(&'static ActionDecl),
}

/// Row height in logical pixels. Large enough for a 24px icon and an 11pt label with air around
/// them — below this a row is a sliver, and the rung check drops the list instead of shrinking it.
const ROW_HEIGHT: i32 = 28;

/// What the widget says when the application registry did not answer. A launcher that cannot
/// reach `AppIdentity1` must say so: an empty list reads as "nothing is installed", which is a
/// different and much more alarming claim.
pub const UNAVAILABLE: &str = "Applications — registry unavailable";

/// Draw the launcher into `content` and report what a click asked for.
pub fn draw(
    ui: &mut Ui<'_>,
    content: Rect,
    widget: &'static WidgetDescriptor,
    data: &LauncherData<'_>,
    rung: Representation,
    confirming: Option<&'static str>,
) -> LauncherOutcome {
    let (body, region_action) =
        super::draw_region_strip(ui, content, widget, rung, confirming, &[]);
    if let Some(declaration) = region_action {
        return LauncherOutcome::Action(declaration);
    }

    let inner = body.inset(ui.px(8));
    if inner.w <= 0 || inner.h <= 0 {
        return LauncherOutcome::None;
    }

    if !data.available {
        ui.label_styled(
            inner,
            UNAVAILABLE,
            design::state_style(ui),
            design::TEXT_DIM,
            Align::Left,
        );
        return LauncherOutcome::None;
    }

    let matches = app_search::search(data.entries, data.query);
    let (summary, list) = inner.split_top(ui.px(16).min(inner.h));
    ui.label_styled(
        summary,
        &summary_line(data, matches.len()),
        design::state_style(ui),
        design::TEXT_DIM,
        Align::Left,
    );

    // Rows never shrink to fit: a tile with room for none shows its summary and stops, because a
    // three-pixel row is a control that cannot be hit.
    let row_height = ui.px(ROW_HEIGHT);
    let capacity = (list.h / row_height.max(1)).max(0) as usize;
    if capacity == 0 {
        return LauncherOutcome::None;
    }

    let mut chosen = None;
    for (index, row) in matches.iter().take(capacity).enumerate() {
        let rect = Rect::new(
            list.x,
            list.y + row_height * index as i32,
            list.w,
            row_height,
        );
        if draw_row(ui, rect, row, data.icons) {
            chosen = Some(row.entry.id.clone());
        }
    }

    if matches.is_empty() {
        ui.label_styled(
            Rect::new(list.x, list.y, list.w, row_height),
            &no_match_line(data.query),
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Left,
        );
    } else if matches.len() > capacity {
        // Say what is hidden rather than truncating in silence: "type to narrow" is the whole
        // interaction, and a list that simply stops looks like the launcher is broken.
        let overflow = Rect::new(
            list.x,
            list.y + row_height * capacity as i32,
            list.w,
            list.bottom() - (list.y + row_height * capacity as i32),
        );
        if overflow.h > 0 {
            ui.label_styled(
                overflow,
                &format!("+{} more — type to narrow", matches.len() - capacity),
                design::state_style(ui),
                design::TEXT_FAINT,
                Align::Left,
            );
        }
    }

    chosen.map_or(LauncherOutcome::None, LauncherOutcome::Launch)
}

fn summary_line(data: &LauncherData<'_>, matched: usize) -> String {
    let query = data.query.trim();
    if query.is_empty() {
        let offerable = data
            .entries
            .iter()
            .filter(|entry| entry.offerable())
            .count();
        return format!("{offerable} INSTALLED — TYPE IN THE SEARCH BAR TO FILTER");
    }
    format!("{matched} MATCHING “{}”", query.to_uppercase())
}

fn no_match_line(query: &str) -> String {
    let query = query.trim();
    if query.is_empty() {
        "Nothing installed that this session may offer".to_owned()
    } else {
        format!("No application matches “{query}”")
    }
}

/// One row: the application's icon (or a lettered placeholder) and its name. Returns whether it
/// was clicked, which the caller turns into `launcher.launch` for that application.
fn draw_row(ui: &mut Ui<'_>, rect: Rect, row: &Match<'_>, icons: &HashMap<String, Bitmap>) -> bool {
    let hovered = ui.input.hovering(rect);
    if hovered {
        ui.painter.fill_rounded_rect(
            rect.inset(ui.px(1)),
            ui.px(design::CHIP_RADIUS),
            design::ACCENT_SOFT,
        );
    }
    let icon_size = ui.px(20).min(rect.h - ui.px(6)).max(4);
    let icon_rect = Rect::new(
        rect.x + ui.px(4),
        rect.y + (rect.h - icon_size) / 2,
        icon_size,
        icon_size,
    );
    if let Some(bitmap) = icons.get(row.entry.id.as_str()) {
        ui.painter.blit(bitmap, icon_rect, ScaleFilter::Bilinear);
    } else {
        // A placeholder, never a blank: the row must still read as a thing you can click.
        ui.painter
            .fill_rounded_rect(icon_rect, ui.px(design::CHIP_RADIUS), design::RECESSED);
        let initial: String = row
            .entry
            .label()
            .chars()
            .next()
            .map(|character| character.to_uppercase().to_string())
            .unwrap_or_default();
        ui.label_styled(
            icon_rect,
            &initial,
            design::state_style(ui),
            design::TEXT_FAINT,
            Align::Centre,
        );
    }
    let label = Rect::new(
        icon_rect.right() + ui.px(8),
        rect.y,
        (rect.right() - icon_rect.right() - ui.px(12)).max(0),
        rect.h,
    );
    if label.w > 0 {
        ui.label_styled(
            label,
            row.entry.label(),
            design::title_style(ui),
            if hovered {
                design::ACCENT
            } else {
                design::TEXT
            },
            Align::Left,
        );
    }
    ui.input.take_click(rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_search::AppEntry;

    fn entry(id: &str, name: &str) -> AppEntry {
        AppEntry {
            id: id.parse().unwrap(),
            name: name.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
        }
    }

    #[test]
    fn an_unreachable_registry_says_so_instead_of_reporting_nothing_installed() {
        assert!(UNAVAILABLE.contains("unavailable"));
        // The same line the widget contract uses for a service it cannot reach.
        assert!(!crate::surface::widgets::WidgetSnapshot::unavailable(UNAVAILABLE).available);
    }

    #[test]
    fn an_empty_query_says_how_to_filter_rather_than_leaving_the_row_blank() {
        let entries = vec![entry("org.legeos.Files", "Files")];
        let icons = HashMap::new();
        let data = LauncherData {
            entries: &entries,
            query: "",
            icons: &icons,
            available: true,
        };
        assert!(summary_line(&data, 1).contains("TYPE IN THE SEARCH BAR"));
        assert_eq!(summary_line(&data, 1).split(' ').next(), Some("1"));
    }

    #[test]
    fn a_query_that_matches_nothing_is_named_rather_than_shown_as_an_empty_list() {
        assert_eq!(no_match_line("qq"), "No application matches “qq”");
        assert!(no_match_line("  ").contains("Nothing installed"));
    }
}
