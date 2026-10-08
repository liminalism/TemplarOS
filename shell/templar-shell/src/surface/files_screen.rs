//! The Files object: one tile that is the way into every file on the machine, drawn at whatever
//! size the field has given it.
//!
//! Home used to carry eight of these — Downloads, Recent, Documents, Media, Drives, Operations,
//! Trash and Projects — which were eight headings for one thing and a third of the field's width
//! before a single window existed. This is the replacement, and it is the same object at every
//! rung rather than eight objects at one:
//!
//! * **Small** — the name, how much room is left, the last file that was touched, and, only while
//!   something is actually copying, one line of progress.
//! * **Wide and above** — the same, plus the locations worth one press and the list of what has
//!   changed, with a row per condition that is currently true (downloads waiting, a trash with
//!   something in it, operations still running) and no row at all when it is not.
//!
//! The content is exactly what the drawer feeds already carry (`crate::drawers`): this composes,
//! it does not fetch. Pure like every other screen here — `draw(ui, rect, data) -> action`, no
//! D-Bus, no Wayland — so its layout is testable against a synthetic `Input`.

use std::collections::HashMap;

use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::Ui;

use templar_field::Representation;
use templar_types::action::ActionDecl;

use crate::drawers::DrawerFeed;
use crate::file_browser::BrowserSources;
use crate::files_surface::FilesLocation;
use crate::home_surface::Drawer;
use crate::surface::design;

/// What the Files object asks its host to do. Every route into it is the same route: open the
/// object at one of its locations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesAction {
    None,
    /// Open the Files surface at this location.
    Open(FilesLocation),
    /// Open the exact structured Recent path; directory MIME is carried rather than guessed.
    OpenRecent(std::path::PathBuf, bool),
    /// Perform one declared action (`templar_types::action::MANIFEST`) — the trash's own EMPTY, so
    /// far. Named rather than performed: like every other surface here, this module decides
    /// nothing that touches a service.
    Run(&'static str),
}

/// Everything the object reads and does not own.
#[derive(Clone, Copy, Debug)]
pub struct FilesData<'a> {
    pub feeds: &'a HashMap<Drawer, DrawerFeed>,
    /// The pinned locations, in the order the expanded surface offers them. The compact tile
    /// shows as many as fit, so the two surfaces cannot disagree about what is worth a press.
    pub sources: &'a BrowserSources,
}

/// How many locations the compact tile offers as chips. Past this the row stops being scannable
/// and starts being a list, and a list belongs at the expanded rung.
const CHIP_LIMIT: usize = 5;

impl<'a> FilesData<'a> {
    fn feed(self, drawer: Drawer) -> Option<&'a DrawerFeed> {
        self.feeds.get(&drawer)
    }

    /// A drawer's headline, or `None` when nothing has been fetched yet — which is a different
    /// thing from a service that answered with nothing, and is drawn differently.
    fn headline(self, drawer: Drawer) -> Option<&'a str> {
        self.feed(drawer).map(|feed| feed.headline.as_str())
    }

    /// How much room is left, across every volume. `Files.Volumes1` renders this as the Drives
    /// headline; the object shows the figure, never the volume count.
    fn storage(self) -> &'a str {
        self.headline(Drawer::Drives).unwrap_or("…")
    }

    /// The last file that was touched, by name. The name comes from the recent feed's row
    /// *target* — the path the service reported — never from parsing the rendered row back
    /// apart, which for a filename is always a guess.
    fn latest(self) -> Option<&'a str> {
        let feed = self.feed(Drawer::Recent)?;
        let target = feed.row_target(0)?;
        Some(file_name_of(target))
    }

    /// The running file operation worth a line, as a label and a fraction, or `None` when
    /// nothing is running. A finished operation is not news; a running one is.
    fn running(self) -> Option<(&'a str, f32)> {
        let (label, percent) = self.feed(Drawer::Operations)?.progress.as_ref()?;
        Some((label.as_str(), f32::from(*percent) / 100.0))
    }
}

/// The trailing path component of a target, or the whole thing when it has none.
fn file_name_of(target: &str) -> &str {
    target
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(target)
}

/// Draw the object into `rect` at `rung`, returning what the pointer asked for.
///
/// `rect` is the tile's whole body — the caller has already painted the shell and reserved
/// whatever chrome the rung calls for.
pub fn draw(ui: &mut Ui<'_>, rect: Rect, rung: Representation, data: FilesData<'_>) -> FilesAction {
    let inner = rect.inset(ui.px(design::FILES_PAD));
    if inner.w <= 0 || inner.h <= 0 {
        return FilesAction::None;
    }
    if rung <= Representation::Small {
        draw_compact(ui, inner, data)
    } else {
        draw_full(ui, inner, data)
    }
}

/// Small: three facts, one per line, in falling order of how often they matter — where the room
/// is going, what was last touched, and whether anything is moving right now.
fn draw_compact(ui: &mut Ui<'_>, inner: Rect, data: FilesData<'_>) -> FilesAction {
    let row = ui.px(design::FILES_ROW_HEIGHT);
    let mut y = inner.y;
    ui.label_styled(
        Rect::new(inner.x, y, inner.w, row),
        data.storage(),
        design::measurement_style(ui),
        design::text(),
        Align::Left,
    );
    y += row;
    if y + row <= inner.bottom() {
        let (text, colour) = match data.latest() {
            Some(name) => (name, design::text_dim()),
            None => (
                data.headline(Drawer::Recent).unwrap_or("…"),
                design::text_faint(),
            ),
        };
        ui.label_styled(
            Rect::new(inner.x, y, inner.w, row),
            text,
            design::state_style(ui),
            colour,
            Align::Left,
        );
        y += row;
    }
    if let Some((label, fraction)) = data.running()
        && y + row <= inner.bottom()
    {
        draw_progress(ui, Rect::new(inner.x, y, inner.w, row), label, fraction);
    }
    // No click handling at this rung: there is nothing here to aim at, so the whole tile is one
    // target and the field's own primary action opens it.
    FilesAction::None
}

/// Wide and above: the compact facts become a heading row, and the space that has appeared pays
/// for the locations and the list of what changed. Rows that report a condition only exist while
/// the condition does — an empty trash has nothing to say, so it says nothing.
fn draw_full(ui: &mut Ui<'_>, inner: Rect, data: FilesData<'_>) -> FilesAction {
    let row = ui.px(design::FILES_ROW_HEIGHT);

    // The heading: the storage figure on the left, where the eye starts, and the last file on
    // the right, where it need not be read to be noticed.
    ui.label_styled(
        Rect::new(inner.x, inner.y, inner.w, row),
        data.storage(),
        design::measurement_style(ui),
        design::text(),
        Align::Left,
    );
    if let Some(name) = data.latest() {
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, row),
            name,
            design::state_style(ui),
            design::text_dim(),
            Align::Right,
        );
    }
    let mut y = inner.y + row + ui.px(design::FILES_SECTION_GAP);
    let mut action = FilesAction::None;
    take(&mut action, draw_locations(ui, inner, &mut y, data));
    take(&mut action, draw_conditions(ui, inner, &mut y, data));
    if let Some((label, fraction)) = data.running()
        && y + row <= inner.bottom()
    {
        draw_progress(ui, Rect::new(inner.x, y, inner.w, row), label, fraction);
        y += row;
    }
    take(&mut action, draw_listing(ui, inner, y, data));
    action
}

/// Keep the first thing the pointer asked for. Two parts of the object can never both be clicked
/// in one frame, but saying so here is cheaper than proving it at every call site.
fn take(action: &mut FilesAction, next: FilesAction) {
    if *action == FilesAction::None {
        *action = next;
    }
}

/// The locations, as chips: one press each, wrapping onto as many lines as there is room for and
/// simply stopping when there is not. A chip that will not fit is left out rather than clipped to
/// an unreadable stub. `y` is advanced past the block.
fn draw_locations(ui: &mut Ui<'_>, inner: Rect, y: &mut i32, data: FilesData<'_>) -> FilesAction {
    let mut action = FilesAction::None;
    let chip_height = ui.px(design::FILES_CHIP_HEIGHT);
    let gap = ui.px(design::FILES_CHIP_GAP);
    let mut x = inner.x;
    for pinned in data.sources.pinned.iter().take(CHIP_LIMIT) {
        let label = pinned.label.as_str();
        let width = ui.text.measure(label, design::state_style(ui)) + ui.px(16);
        if x + width > inner.right() {
            x = inner.x;
            *y += chip_height + gap;
        }
        if *y + chip_height > inner.bottom() {
            break;
        }
        let chip = Rect::new(x, *y, width, chip_height);
        let hovered = ui.input.hovering(chip);
        ui.painter.fill_rounded_rect(
            chip,
            ui.px(design::CHIP_RADIUS),
            if hovered {
                design::accent_soft()
            } else {
                design::shell_low()
            },
        );
        ui.painter
            .stroke_rect(chip, ui.scale.hairline(), design::edge());
        ui.label_styled(
            chip,
            label,
            design::state_style(ui),
            if hovered {
                design::accent()
            } else {
                design::text_dim()
            },
            Align::Centre,
        );
        if super::window::custom_control(ui, chip, label) {
            action = FilesAction::Open(pinned.location.clone());
        }
        x += width + gap;
    }
    *y += chip_height + ui.px(design::FILES_SECTION_GAP);
    action
}

/// The conditional rows, before the listing: they are the reasons a person opens this object in a
/// hurry, and each one is present only while it is true.
fn draw_conditions(ui: &mut Ui<'_>, inner: Rect, y: &mut i32, data: FilesData<'_>) -> FilesAction {
    let mut action = FilesAction::None;
    let row = ui.px(design::FILES_ROW_HEIGHT);
    for (drawer, label) in conditions(data) {
        if *y + row > inner.bottom() {
            break;
        }
        let strip = Rect::new(inner.x, *y, inner.w, row);
        *y += row;
        // The trash is the one condition that carries a control rather than only a count: it is
        // the only one whose remedy is a single verb. It goes through `Files.Trash1`, which
        // journals it, so Home never keeps its own idea of what is in the trash.
        let empty_label =
            templar_types::action::action("trash.empty").map_or("Empty", ActionDecl::control_label);
        let (strip, control) = if drawer == Drawer::Trash {
            let width = ui.text.measure(empty_label, design::state_style(ui)) + ui.px(16);
            let gap = ui.px(design::FILES_CHIP_GAP);
            let (control, rest) = strip.split_right((width + gap).min(strip.w / 2));
            (
                rest,
                Some(Rect::new(
                    control.x + gap,
                    control.y,
                    control.w - gap,
                    control.h,
                )),
            )
        } else {
            (strip, None)
        };
        let hovered = ui.input.hovering(strip);
        if hovered {
            ui.painter.fill_rect(strip, design::accent_soft());
        }
        ui.label_styled(
            strip,
            drawer.name(),
            design::state_style(ui),
            if hovered {
                design::accent()
            } else {
                design::text()
            },
            Align::Left,
        );
        ui.label_styled(
            strip,
            &label,
            design::state_style(ui),
            design::text_dim(),
            Align::Right,
        );
        if super::window::custom_control(ui, strip, drawer.name()) {
            // Downloads is a real directory and the pinned list knows where it is; the rest are
            // feeds and are their own location.
            action = data.sources.by_label(drawer.name()).map_or_else(
                || FilesAction::Open(location_for(drawer)),
                |pinned| FilesAction::Open(pinned.location.clone()),
            );
        }
        if let Some(control) = control {
            let hovered = ui.input.hovering(control);
            ui.painter.fill_rounded_rect(
                control,
                ui.px(design::CHIP_RADIUS),
                if hovered {
                    design::danger_soft()
                } else {
                    design::shell_low()
                },
            );
            ui.painter
                .stroke_rect(control, ui.scale.hairline(), design::edge());
            ui.label_styled(
                control,
                empty_label,
                design::state_style(ui),
                if hovered {
                    design::danger()
                } else {
                    design::text_dim()
                },
                Align::Centre,
            );
            if super::window::custom_control(ui, control, empty_label) {
                action = FilesAction::Run("trash.empty");
            }
        }
    }
    action
}

/// What has changed, most recent first, filling whatever is left. It gets a label of its own:
/// without one the first changed file reads as one more condition row, and the object stops
/// having parts.
fn draw_listing(ui: &mut Ui<'_>, inner: Rect, y: i32, data: FilesData<'_>) -> FilesAction {
    let mut action = FilesAction::None;
    let row = ui.px(design::FILES_ROW_HEIGHT);
    let mut top = y + ui.px(design::FILES_SECTION_GAP);
    if top + row * 2 <= inner.bottom() {
        ui.label_styled(
            Rect::new(inner.x, top, inner.w, row),
            "Recent",
            design::section_label_style(ui),
            design::text_faint(),
            Align::Left,
        );
        top += row;
    }
    let listing = Rect::new(inner.x, top, inner.w, (inner.bottom() - top).max(0));
    if listing.h < row {
        return action;
    }
    ui.painter.push_clip(listing);
    let feed = data.feed(Drawer::Recent);
    let lines = feed.map_or(&[][..], |feed| feed.lines.as_slice());
    for (index, line) in lines
        .iter()
        .enumerate()
        .take((listing.h / row).max(0) as usize)
    {
        let strip = Rect::new(listing.x, listing.y + index as i32 * row, listing.w, row);
        let name = feed
            .and_then(|feed| feed.row_target(index))
            .map_or(line.as_str(), file_name_of);
        if let Some(entry) = feed.and_then(|feed| feed.recents.get(index)) {
            if ui.input.hovering(strip) {
                ui.painter.fill_rect(strip, design::accent_soft());
            }
            if super::window::custom_control(ui, strip, name) {
                action = FilesAction::OpenRecent(
                    entry.path.clone(),
                    entry.mime.as_deref() == Some("inode/directory"),
                );
            }
        }
        ui.label_styled(
            strip,
            name,
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
    }
    ui.painter.pop_clip();
    if lines.is_empty() {
        // One row tall, at the top of the listing — a label centred in whatever is left sits
        // orphaned halfway down the tile, a long way from the heading it belongs to.
        ui.label_styled(
            Rect::new(listing.x, listing.y, listing.w, row),
            "nothing recent",
            design::state_style(ui),
            design::text_faint(),
            Align::Left,
        );
    }
    action
}

/// The location behind a condition row. Conditions only ever name a feed, and a feed is a
/// location in its own right.
fn location_for(drawer: Drawer) -> FilesLocation {
    match drawer {
        Drawer::Trash => FilesLocation::Trash,
        Drawer::Operations => FilesLocation::Operations,
        Drawer::Drives => FilesLocation::Drives,
        _ => FilesLocation::Recent,
    }
}

/// The rows that are true right now: waiting downloads and a trash with something in it. Each is
/// the drawer and its own headline, so the figure comes from the service that counted it.
fn conditions(data: FilesData<'_>) -> Vec<(Drawer, String)> {
    let mut rows = Vec::new();
    for drawer in [Drawer::Downloads, Drawer::Trash] {
        let Some(feed) = data.feed(drawer) else {
            continue;
        };
        if !feed.available || feed.empty {
            continue;
        }
        rows.push((drawer, feed.headline.clone()));
    }
    rows
}

/// One running operation as a labelled bar. Accent means *activity* here, as everywhere else in
/// the language: the bar exists only while something is happening, and it goes away when it
/// stops.
fn draw_progress(ui: &mut Ui<'_>, area: Rect, label: &str, fraction: f32) {
    let bar_height = ui.px(design::FILES_PROGRESS_HEIGHT).max(2);
    let text = Rect::new(area.x, area.y, area.w, (area.h - bar_height).max(0));
    ui.label_styled(
        text,
        label,
        design::state_style(ui),
        design::accent(),
        Align::Left,
    );
    let track = Rect::new(area.x, area.bottom() - bar_height, area.w, bar_height);
    ui.painter.fill_rect(track, design::accent_soft());
    let filled = (f64::from(track.w) * f64::from(fraction.clamp(0.0, 1.0))) as i32;
    if filled > 0 {
        ui.painter.fill_rect(
            Rect::new(track.x, track.y, filled, bar_height),
            design::accent(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned list plays no part in what the object *says*; these tests are about the
    /// content, so they hand it an empty one.
    fn nowhere() -> BrowserSources {
        BrowserSources::default()
    }

    fn feeds(pairs: &[(Drawer, DrawerFeed)]) -> HashMap<Drawer, DrawerFeed> {
        pairs.iter().cloned().collect()
    }

    #[test]
    fn clicking_recent_rows_uses_structured_paths_and_directory_mime() {
        use crate::drawers::recent::{RecentAction, RecentEntry};
        use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
        use pixelkit_shell::{Input, MouseButton, Scale};
        for directory in [false, true] {
            let path = std::path::PathBuf::from("/project/notes with spaces\nand newline");
            let mut feed = DrawerFeed::new("1 item", vec!["decorative text is not a path".into()]);
            feed.recents.push(RecentEntry {
                path: path.clone(),
                app_id: Some("org.templaros.Editor".into()),
                at_ms: 1234,
                mime: Some(
                    if directory {
                        "inode/directory"
                    } else {
                        "text/plain"
                    }
                    .into(),
                ),
                action: RecentAction::Opened,
            });
            let map = feeds(&[(Drawer::Recent, feed)]);
            let sources = nowhere();
            let mut buffer = WindowBuffer::new(400, 200);
            let mut text = pixelkit_text::TextCache::new(crate::surface::fonts::set());
            let mut input = Input::new();
            input.cursor_moved(
                20.0,
                (design::FILES_SECTION_GAP
                    + design::FILES_ROW_HEIGHT
                    + design::FILES_ROW_HEIGHT / 2) as f32,
            );
            input.mouse(MouseButton::Left, true);
            input.mouse(MouseButton::Left, false);
            let mut kernel = RasterKernel::new();
            let mut ui = Ui::new(
                Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                Scale::ONE,
                &mut kernel,
            );
            assert_eq!(
                draw_listing(
                    &mut ui,
                    Rect::new(0, 0, 400, 200),
                    0,
                    FilesData {
                        feeds: &map,
                        sources: &sources
                    }
                ),
                FilesAction::OpenRecent(path, directory)
            );
        }
    }

    #[test]
    fn the_storage_line_is_the_figure_the_volume_service_reported() {
        let map = feeds(&[(
            Drawer::Drives,
            DrawerFeed::new("512.0 MiB usable", Vec::new()),
        )]);
        assert_eq!(
            FilesData {
                feeds: &map,
                sources: &nowhere(),
            }
            .storage(),
            "512.0 MiB usable"
        );
        assert_eq!(
            FilesData {
                feeds: &HashMap::new(),
                sources: &nowhere(),
            }
            .storage(),
            "…"
        );
    }

    /// The last file's name comes from the path the service reported, never from taking the
    /// rendered row apart — a filename may contain anything, spaces included.
    #[test]
    fn the_latest_file_is_named_from_its_target_not_its_rendered_row() {
        let mut feed = DrawerFeed::new(
            "1 item",
            vec!["10:15  opened  app  /a/b/my notes.md".into()],
        );
        feed.row_targets = vec![Some("/a/b/my notes.md".to_owned())];
        let map = feeds(&[(Drawer::Recent, feed)]);
        assert_eq!(
            FilesData {
                feeds: &map,
                sources: &nowhere(),
            }
            .latest(),
            Some("my notes.md")
        );
    }

    /// A finished operation is not news. Only a running one earns the line and the bar, and its
    /// fraction is the least-finished of them — the one a person is actually waiting on.
    #[test]
    fn only_a_running_operation_reports_progress() {
        let done = feeds(&[(
            Drawer::Operations,
            DrawerFeed::new("3 finished", Vec::new()),
        )]);
        assert_eq!(
            FilesData {
                feeds: &done,
                sources: &nowhere(),
            }
            .running(),
            None
        );

        let busy = feeds(&[(
            Drawer::Operations,
            DrawerFeed::new(
                "2 running of 3",
                vec![
                    "copying     70%  7 B/10 B  Copy a to b".into(),
                    "copying     40%  4 B/10 B  Copy c to d".into(),
                ],
            )
            .with_progress("2 running of 3", 40),
        )]);
        let nowhere = nowhere();
        let data = FilesData {
            feeds: &busy,
            sources: &nowhere,
        };
        let (label, fraction) = data.running().expect("running");
        assert_eq!(label, "2 running of 3");
        assert!((fraction - 0.40).abs() < 0.001, "{fraction}");
    }

    /// A condition row exists only while its condition does: an empty trash and an empty
    /// downloads directory are not worth a line, and a service that did not answer is not
    /// worth a plausible-looking one.
    #[test]
    fn a_condition_row_appears_only_while_it_is_true() {
        let quiet = feeds(&[
            (Drawer::Downloads, DrawerFeed::vacant("nothing")),
            (Drawer::Trash, DrawerFeed::vacant("empty")),
        ]);
        assert!(
            conditions(FilesData {
                feeds: &quiet,
                sources: &nowhere(),
            })
            .is_empty()
        );

        let busy = feeds(&[
            (Drawer::Downloads, DrawerFeed::new("4 places", Vec::new())),
            (Drawer::Trash, DrawerFeed::unavailable("no file service")),
        ]);
        assert_eq!(
            conditions(FilesData {
                feeds: &busy,
                sources: &nowhere(),
            }),
            vec![(Drawer::Downloads, "4 places".to_owned())]
        );
    }
}
