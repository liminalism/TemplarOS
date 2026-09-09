//! The Home Field rendered to a PNG, with no compositor and no display.
//!
//! `--print field` says what the field *contains*; this says what it *looks like*, which is the
//! other half of being able to work on it without logging out of a working desktop. Between them
//! there is no question about the Home Field's appearance that requires ending someone's session
//! to answer.
//!
//! Capturing the host's screen instead would have been easier and is worse in three ways: it
//! depends on which window happens to be in front, it photographs whatever else the person has
//! open, and it cannot run without a graphical session at all. This draws the same
//! [`FieldScreen`](crate::surface::field_screen::FieldScreen) the session draws, into an ordinary
//! buffer — the shape the field's own tests already use — so it is deterministic, private, and
//! works over SSH.
//!
//! What it cannot show is a real window: there is no compositor here, so the toplevel list is
//! empty and the field renders as a session with nothing open. Everything drawn from a service —
//! drawers, the launcher's inventory, the Session widget — is live if that service is answering.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_shell::{Input, Scale};
use pixelkit_ui::Ui;

use crate::field_model::{self, LiveState};
use crate::surface::design;
use crate::surface::field_screen::{FieldData, FieldScreen};
use crate::surface::file_browser::FileBrowserData;
use crate::surface::fonts;

/// Render the Home Field at `width`x`height` logical pixels and write it to `path` as a PNG.
#[allow(clippy::too_many_lines)]
pub async fn render_field(path: &Path, width: u32, height: u32) -> Result<()> {
    anyhow::ensure!(
        width >= 320 && height >= 240,
        "a field smaller than 320x240 has nothing to show; asked for {width}x{height}"
    );

    // Everything a live shell would have fetched. Each of these degrades on its own when its
    // service is absent, so this works on a machine with no session daemon at all.
    let (
        drawer_texts,
        apps,
        projects,
        session_widget,
        notifications_widget,
        settings_widget,
        browser_sources,
    ) = tokio::join!(
        crate::data::fetch_all(12),
        crate::data::fetch_apps(),
        crate::data::fetch_projects(),
        crate::data::fetch_session_widget(),
        crate::data::fetch_notifications_widget(),
        crate::data::fetch_settings_widget(),
        crate::data::fetch_browser_sources(),
    );

    let (cols, rows) = field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
    let mut field = match crate::data::fetch_field("primary", cols, rows).await {
        Some(persisted) => persisted,
        None => templar_field::FieldDocument::new("primary", cols, rows),
    };
    let (notifications_widget, _notification_rows) = notifications_widget;
    let apps_available = apps.is_some();
    let apps = apps.unwrap_or_default();
    // No compositor here, so no windows: the field is whatever the persisted document plus the
    // drawers and widgets amount to.
    let live = LiveState {
        toplevels: &[],
        projects: &projects,
    };
    field_model::merge(&mut field, &live);

    let mut widgets = HashMap::new();
    widgets.insert("session".to_owned(), session_widget);
    widgets.insert("notifications".to_owned(), notifications_widget);
    widgets.insert("settings".to_owned(), settings_widget);

    let drawer_feeds: HashMap<_, crate::drawers::DrawerFeed> = drawer_texts.into_iter().collect();

    let mut buffer = WindowBuffer::new(width, height);
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut input = Input::new();
    let mut screen = FieldScreen::default();

    let empty_icons = HashMap::new();
    let empty_previews = HashMap::new();
    let empty_thumbnails = std::collections::HashMap::new();
    let empty_folder_listings = HashMap::new();
    let browser = FileBrowserData {
        sources: &browser_sources,
        listing: None,
        pending: None,
        error: None,
        status: None,
        search_query: None,
        search_entries: None,
        search_error: None,
        feeds: &drawer_feeds,
        thumbnails: &empty_thumbnails,
        icons: &empty_icons,
        home: None,
    };
    let data = FieldData {
        notifications: &[],
        field: &field,
        toplevels: &[],
        icons: &empty_icons,
        previews: &empty_previews,
        drawer_feeds: &drawer_feeds,
        treemap: None,
        composition: &[],
        browser: &browser,
        folder_listings: &empty_folder_listings,
        connected: false,
        home_revealed: false,
        apps: &apps,
        apps_available,
        widgets: &widgets,
        variants: &HashMap::new(),
        projects: &projects,
        garden: &HashMap::new(),
        actions: templar_types::action::MANIFEST,
        file_search: None,
        recents: &[],
        recent_ids: &[],
        reduced_motion: true,
        now_ms: 0,
    };

    // Two passes: the first establishes every tile's rect (the field lays out while it draws), the
    // second paints with those rects known. The same reason `Harness::rect_of` draws twice.
    for _ in 0..2 {
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            design::theme(),
            Scale::ONE,
            &mut kernel,
        );
        screen.draw(&mut ui, Rect::new(0, 0, width as i32, height as i32), &data);
        input.end_frame();
    }

    write_png(path, &buffer).with_context(|| format!("writing {}", path.display()))
}

/// Render the bottom bar at rest to a PNG: the network and clock chips plus three taskbar
/// entries — a group (active, with its accent stripe and its plate), a plain application, and one
/// under the pointer — at `scale`, in a surface exactly the size the host would ask for.
///
/// The counterpart to [`render_field`] for the one surface nobody could see: the bar is a few
/// hundred pixels in the corner of a live session, and every question about it (is the ground
/// there, do the labels fit, does the stripe eat the plate) is answerable from an image.
pub fn render_bar(path: &Path, scale: f64) -> Result<()> {
    let buffer = bar_frame(crate::surface::bar_screen::Presence::Open, false, scale);
    write_png(path, &buffer).with_context(|| format!("writing {}", path.display()))
}

/// One frame of the bar in one presence, in a surface exactly the size the host would ask for
/// (`bar_screen::wanted_size`).
///
/// `Revealed` is *driven* rather than set: the fixture puts the pointer on the hint and lets the
/// dwell elapse, so this renders the state a person actually reaches rather than one only a test
/// can construct. `open_fan` is driven the same way: the pointer rests on the group's chip until
/// the hover dwell elapses, which is the only way a fan opens in the running shell.
#[allow(clippy::too_many_lines)]
pub(super) fn bar_frame(
    presence: crate::surface::bar_screen::Presence,
    open_fan: bool,
    scale: f64,
) -> WindowBuffer {
    use crate::surface::bar_screen::{
        BarData, BarScreen, HINT_OPEN_MS, HOVER_OPEN_MS, Presence, wanted_size,
    };

    // Built from toplevels rather than from `Entry` literals: `taskbar::entries` is a projection
    // of `templar_window::task::units`, so a fixture that hand-wrote the entries would be drawing
    // a grouping the running shell cannot produce.
    let toplevel = |id: u64, app: &str, title: &str, group: Option<(u64, &str, u32)>| {
        templar_window::ShellToplevel {
            window_id: templar_window::WindowId(id),
            canonical_app_id: Some(app.parse().expect("a fixture's app id")),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: id == 1,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            // A parked window exists only on the bar now that Home holds no window tiles.
            shelved: id == 4,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 800, 600),
            layer: templar_window::Layer::Normal,
            group: group.map(|(group_id, name, accent)| templar_window::GroupInfo {
                group_id,
                name: name.to_owned(),
                accent,
            }),
        }
    };
    let group = Some((7, "Notes and sources", 0x00e0_7b39));
    let entries = crate::taskbar::entries(&[
        toplevel(1, "org.templaros.Notes", "Notes", group),
        toplevel(2, "org.templaros.Notes", "Sources", group),
        // A terminal whose title is a path: the case that put a chip reading "/" on the bar. The
        // chip is named for the application now, so the path is only ever on the fan.
        toplevel(
            3,
            "desktop-import.kitty.desktop",
            "/mnt/Samsung980_1TB/Rust-projects/TemplarOS",
            None,
        ),
        toplevel(4, "org.templaros.Settings", "Appearance", None),
    ]);
    let logical = wanted_size(
        presence,
        open_fan.then(|| entries[0].visible()),
        false,
        entries.len(),
        2560,
    );
    let scale = Scale::new(scale);
    let (width, height) = (scale.px(logical.0) as u32, scale.px(logical.1) as u32);

    let mut buffer = WindowBuffer::new(width, height);
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut input = Input::new();
    let mut screen = BarScreen::default();
    let icons = HashMap::new();
    // One of the group's two windows has a capture and the other has not, so a single image
    // shows both halves of the rule: a preview when there is one, the icon fallback when there
    // is not. The gradient stands in for a real window's pixels.
    let mut previews = HashMap::new();
    previews.insert("window:1".to_owned(), preview_fixture());
    let data = BarData {
        entries: &entries,
        icons: &icons,
        previews: &previews,
        // A fixed clock, not the wall clock: this fixture is hashed as a visual baseline, and
        // a baseline that changes every minute is not one.
        activated: Some(templar_window::WindowId(1)),
        covered: presence != Presence::Open,
        peeked: false,
        home_revealed: false,
        now_ms: 0,
    };
    // Open: the pointer over the last chip, so one entry is drawn hovered. The dwell has not
    // elapsed (`now_ms` is 0), so no row opens and the strip stays its resting height.
    // Hint: no pointer at all — the chevron at rest, which is how it is nearly always seen.
    // Revealed: the pointer on the hint, then again once the reveal dwell has elapsed.
    let mut paint = |buffer: &mut WindowBuffer, input: &mut Input, data: &BarData<'_>| {
        let (w, h) = (buffer.width as i32, buffer.height as i32);
        let mut ui = Ui::new(
            Painter::new(buffer),
            &mut text,
            input,
            design::theme(),
            scale,
            &mut kernel,
        );
        screen.draw(&mut ui, Rect::new(0, 0, w, h), data);
        input.end_frame();
    };
    match presence {
        Presence::Open => {
            let last = width as f32 - scale.px(24) as f32;
            input.cursor_moved(last, height as f32 / 2.0);
        }
        Presence::Hint => {}
        Presence::Revealed => {
            let hint = wanted_size(Presence::Hint, None, false, entries.len(), 2560);
            let mut first = WindowBuffer::new(width, scale.px(hint.1) as u32);
            input.cursor_moved(width as f32 / 2.0, scale.px(hint.1) as f32 - 1.0);
            paint(&mut first, &mut input, &data);
            let revealed = BarData {
                now_ms: HINT_OPEN_MS,
                ..data
            };
            paint(&mut first, &mut input, &revealed);
            input.cursor_moved(width as f32 / 2.0, height as f32 - 1.0);
        }
    }
    if open_fan {
        // The fan is opened the way a person opens it: the pointer comes to the first chip and
        // stays there until the hover dwell elapses. This frame is the chip merely hovered; the
        // final paint below is the one with the fan on it.
        //
        // 32 is the island's 8-pixel margin plus half a 48-pixel cell — the middle of the first
        // chip, which is the group with two windows.
        input.cursor_moved(scale.px(32) as f32, height as f32 - scale.px(24) as f32);
        paint(&mut buffer, &mut input, &data);
    }
    let now = BarData {
        now_ms: match (presence, open_fan) {
            (Presence::Revealed, _) => HINT_OPEN_MS,
            (_, true) => HOVER_OPEN_MS,
            _ => 0,
        },
        ..data
    };
    paint(&mut buffer, &mut input, &now);
    buffer
}

/// A stand-in for a captured window: a gradient, so a preview in a render is unmistakably a
/// preview and not a flat fill that happens to be the well's colour.
fn preview_fixture() -> pixelkit_raster::Bitmap {
    let (width, height) = (160u32, 100u32);
    let pixels = (0..width * height)
        .map(|i| {
            let (x, y) = (i % width, i / width);
            let r = 0x20 + (x * 0x60 / width);
            let g = 0x30 + (y * 0x70 / height);
            0xff00_0000 | (r << 16) | (g << 8) | 0x90
        })
        .collect();
    pixelkit_raster::Bitmap {
        width,
        height,
        pixels,
    }
}

/// `WindowBuffer` is 0xAARRGGBB in native order; PNG wants RGBA bytes.
pub(super) fn write_png(path: &Path, buffer: &WindowBuffer) -> Result<()> {
    let mut rgba = Vec::with_capacity(buffer.pixels.len() * 4);
    for pixel in &buffer.pixels {
        rgba.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8, 0xff]);
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), buffer.width, buffer.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&rgba)
        .context("encoding the field as PNG")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use templar_field::{Cell, Span};

    /// One window, exactly as the compositor would report it, with no clock or host in it.
    fn baseline_toplevel(
        id: u64,
        app: &str,
        title: &str,
        group: Option<templar_window::GroupInfo>,
    ) -> templar_window::ShellToplevel {
        templar_window::ShellToplevel {
            window_id: templar_window::WindowId(id),
            canonical_app_id: Some(app.parse().expect("a canonical app id")),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: false,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 640, 480),
            layer: templar_window::Layer::Normal,
            group,
        }
    }

    /// The Places `TEMPLAR_BASELINE_GARDEN` grows into the baseline field: the named form at the
    /// top span, and a lightly used place at the smallest one, so every variant reviews the
    /// scale range as well as the form. Fixed roots, trees and scores — what is under review is
    /// the drawing, not the ledger's arithmetic, which has its own tests in `templar-session-core`.
    fn garden_fixture(
        form: &str,
    ) -> (
        Vec<templar_field::garden::ContextView>,
        HashMap<PathBuf, crate::garden::GardenPlace>,
    ) {
        fn dir(
            name: &str,
            score: f32,
            children: Vec<crate::garden::TreeNode>,
        ) -> crate::garden::TreeNode {
            crate::garden::TreeNode {
                name: name.to_owned(),
                is_dir: true,
                score,
                children,
            }
        }
        fn leaf(name: &str, score: f32) -> crate::garden::TreeNode {
            crate::garden::TreeNode {
                name: name.to_owned(),
                is_dir: false,
                score,
                children: Vec::new(),
            }
        }
        // A folder of projects whose branches are the repositories: the shape the activity tree
        // already records, and the one the branch form has to read as at a glance.
        let (root, tree, summary) = match form {
            "shelf" => (
                "/home/fixture/Pictures",
                vec![
                    leaf("2026-trip.jpg", 9.0),
                    leaf("sunset.png", 7.0),
                    leaf("workshop.mp4", 6.0),
                    leaf("portrait.webp", 4.0),
                    leaf("scan.avif", 2.0),
                ],
                "21 opens, 5 sessions",
            ),
            "plain" => ("/home/fixture/Documents", Vec::new(), "9 opens, 3 sessions"),
            _ => (
                "/home/fixture/Rust-projects",
                vec![
                    dir(
                        "templaros",
                        14.0,
                        vec![
                            dir("crates", 8.0, Vec::new()),
                            dir("shell", 5.0, Vec::new()),
                        ],
                    ),
                    dir("pixelkit", 6.0, vec![dir("crates", 3.0, Vec::new())]),
                    dir("Silvaview-rs", 3.0, Vec::new()),
                    dir("bpg-rs", 1.0, Vec::new()),
                ],
                "34 opens, 11 launches, 7 sessions",
            ),
        };
        // Fixed landings: the bottom-left of the baseline field is open, and a fixture whose
        // Places land wherever the search happens to reach is a fixture that stops showing the
        // form the day another object moves.
        let grown = (
            PathBuf::from(root),
            Span::new(6, 4),
            Cell::new(0, 5),
            "expanded",
            tree,
            summary,
        );
        // The other end of the ladder: a place used lightly enough to be a line, in every
        // variant, so the two extremes are always reviewed against each other.
        let line = (
            PathBuf::from("/home/fixture/Downloads"),
            Span::new(2, 1),
            Cell::new(7, 8),
            "trace",
            Vec::new(),
            "3 opens, 1 session",
        );
        let mut views = Vec::new();
        let mut places = HashMap::new();
        for (root, span, landing, stage, tree, summary) in [grown, line] {
            views.push(templar_field::garden::ContextView {
                root: root.clone(),
                wanted: Some(span),
                landing: Some(landing),
                pinned: false,
                user_span: None,
                summary: summary.to_owned(),
            });
            places.insert(
                root.clone(),
                crate::garden::GardenPlace {
                    root,
                    stage: stage.to_owned(),
                    form: if tree.is_empty() {
                        "plain".to_owned()
                    } else {
                        form.to_owned()
                    },
                    tree,
                    summary: summary.to_owned(),
                    ..crate::garden::GardenPlace::default()
                },
            );
        }
        (views, places)
    }

    /// Fixed-pixel baseline for the default Home Field.
    ///
    /// Last reviewed 2026-09-09, when the growth forms landed: the light palette's ground moved
    /// from a cool grey to a warm mineral off-white (`0x00ec_eef0` to `0x00ee_ebe4`), which is
    /// the substrate a garden is grown on. Nothing else in the resting field changed — the forms
    /// themselves are reviewed through `TEMPLAR_BASELINE_GARDEN`, which returns before this hash. Unlike `render_field`, this fixture reads
    /// no clock, filesystem, D-Bus service, persisted layout, or host font. A visual change must
    /// therefore be reviewed and deliberately update this hash; environment changes cannot make
    /// it flicker.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn visual_regression_home_field_baseline() {
        // 1280x832 -> a 12x8 grid: roomy enough that every object class — drawers, widgets,
        // the system shelf, a project, a window group, a bound folder and a shortcut pane —
        // actually places, so the baseline reviews all of their material roles at once.
        let size = std::env::var("TEMPLAR_BASELINE_SIZE").ok();
        let (width, height) = size.as_deref().map_or((1280_u32, 832_u32), |size| {
            let (width, height) = size.split_once('x').expect("WIDTHxHEIGHT");
            (
                width.parse().expect("width"),
                height.parse().expect("height"),
            )
        });
        let (cols, rows) =
            field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
        let mut field = templar_field::FieldDocument::new("visual-baseline", cols, rows);
        let projects = vec![field_model::ProjectInfo {
            id: "templaros".to_owned(),
            name: "TemplarOS".to_owned(),
            is_open: true,
            ..field_model::ProjectInfo::default()
        }];
        // Two windows in one group, so the baseline reviews the window-group tile as well: a
        // container drawn in the group's own accent, holding its members' session tiles. Fixed
        // ids, titles and accent, like everything else here — this fixture reads no clock, no
        // filesystem and no service.
        let group = templar_window::GroupInfo {
            group_id: 3,
            name: "Writing".to_owned(),
            accent: 0x0033_88ff,
        };
        let toplevels = vec![
            baseline_toplevel(1, "org.templaros.Files", "Notes", Some(group.clone())),
            baseline_toplevel(2, "org.templaros.Reader", "Draft", Some(group)),
        ];
        let live = LiveState {
            toplevels: &toplevels,
            projects: &projects,
        };
        field_model::merge(&mut field, &live);
        // One of each placeable object class the material roles distinguish: the merge already
        // adds widgets and the system shelf; a bound folder and a shortcut pane join here so
        // this baseline is also the reviewed render for their tints. The folder is placed the
        // way any folder now reaches Home — Home merges no Desktop tray of its own.
        for op in
            field_model::open_folder_ops(&field, std::path::Path::new("/home/fixture/Desktop"))
        {
            if let Ok((next, _)) = templar_field::apply(&field, &op) {
                field = next;
            }
        }
        field_model::route_file_drop(&mut field, std::path::Path::new("/home/fixture/notes.md"));
        // One pinned application, so the baseline also reviews the compact rung — which since
        // the chrome reduction is its own drawing path: no heading, the name as the body's first
        // line, and the controls only on hover or focus.
        field_model::pin_app(&mut field, &"org.templaros.Files".parse().unwrap());
        // `TEMPLAR_BASELINE_GARDEN=branch|shelf|plain` grows two Places into the field through the
        // real tender, so each growth form can be reviewed as a PNG with no compositor, no
        // daemon and no day of use. Like the other variants it returns before the hash.
        let garden = match std::env::var("TEMPLAR_BASELINE_GARDEN") {
            Ok(form) => {
                let (views, places) = garden_fixture(&form);
                for tended in templar_field::garden::tend(
                    &field,
                    &views,
                    templar_field::garden::HomeCorner::BottomLeft,
                ) {
                    field = templar_field::apply(&field, &tended.op)
                        .expect("the fixture field has room for the garden")
                        .0;
                }
                places
            }
            Err(_) => HashMap::new(),
        };

        let drawer_feeds = crate::home_surface::Drawer::DEFAULT
            .into_iter()
            .map(|drawer| {
                (
                    drawer,
                    crate::drawers::DrawerFeed::new(
                        format!("{} fixture", drawer.name()),
                        vec!["One stable row".to_owned()],
                    ),
                )
            })
            .collect();
        let mut widgets = HashMap::new();
        widgets.insert(
            "session".to_owned(),
            crate::surface::widgets::WidgetSnapshot {
                headline: "dk".to_owned(),
                detail: vec!["Lock and leave from here".to_owned()],
                available: true,
                refused: Vec::new(),
            },
        );
        widgets.insert(
            "notifications".to_owned(),
            crate::surface::widgets::WidgetSnapshot {
                headline: "quiet".to_owned(),
                detail: Vec::new(),
                available: true,
                refused: Vec::new(),
            },
        );
        // Fixed readings, so the baseline reviews the Settings widget's three-line body and its
        // control row rather than the "…" a tile with no snapshot yet would show.
        widgets.insert(
            "settings".to_owned(),
            crate::surface::widgets::settings::snapshot(
                [
                    "72% · 2 h 23 min remaining".to_owned(),
                    "45%".to_owned(),
                    "60%".to_owned(),
                ],
                Vec::new(),
            ),
        );
        let empty_bitmaps = HashMap::new();
        // Fixed pinned locations, so the baseline reviews the Files object's real chip row and
        // the expanded surface's real sidebar rather than the degraded form of both.
        let browser_sources = crate::file_browser::BrowserSources {
            pinned: vec![
                crate::files_surface::PinnedLocation {
                    label: "Recent".to_owned(),
                    detail: "Lately opened and saved".to_owned(),
                    location: crate::files_surface::FilesLocation::Recent,
                    kind: crate::files_surface::PinnedKind::Feed,
                },
                crate::files_surface::PinnedLocation {
                    label: "Home".to_owned(),
                    detail: "Personal files".to_owned(),
                    location: crate::files_surface::FilesLocation::Directory(
                        "/home/fixture".into(),
                    ),
                    kind: crate::files_surface::PinnedKind::Place,
                },
                crate::files_surface::PinnedLocation {
                    label: "Documents".to_owned(),
                    detail: "Standard folder".to_owned(),
                    location: crate::files_surface::FilesLocation::Directory(
                        "/home/fixture/Documents".into(),
                    ),
                    kind: crate::files_surface::PinnedKind::Place,
                },
                crate::files_surface::PinnedLocation {
                    label: "Downloads".to_owned(),
                    detail: "Standard folder".to_owned(),
                    location: crate::files_surface::FilesLocation::Directory(
                        "/home/fixture/Downloads".into(),
                    ),
                    kind: crate::files_surface::PinnedKind::Place,
                },
                crate::files_surface::PinnedLocation {
                    label: "Trash".to_owned(),
                    detail: "Restorable".to_owned(),
                    location: crate::files_surface::FilesLocation::Trash,
                    kind: crate::files_surface::PinnedKind::Feed,
                },
            ],
        };
        let empty_thumbnails = std::collections::HashMap::new();
        let browser = FileBrowserData {
            sources: &browser_sources,
            listing: None,
            pending: None,
            error: None,
            status: None,
            search_query: None,
            search_entries: None,
            search_error: None,
            feeds: &drawer_feeds,
            thumbnails: &empty_thumbnails,
            icons: &HashMap::new(),
            home: None,
        };
        let apps: Vec<crate::app_search::AppEntry> =
            if std::env::var("TEMPLAR_BASELINE_OMNIBOX").is_ok() {
                ["Reader", "Recorder", "Terminal", "Remote Desktop"]
                    .into_iter()
                    .map(|name| crate::app_search::AppEntry {
                        id: format!("org.templaros.{}", name.replace(' ', ""))
                            .parse()
                            .unwrap(),
                        name: name.to_owned(),
                        desktop_file_id: String::new(),
                        icon: String::new(),
                        categories: Vec::new(),
                        keywords: Vec::new(),
                        no_display: false,
                        logical: String::new(),
                        variants: 1,
                    })
                    .collect()
            } else {
                Vec::new()
            };
        let recent_entries: Vec<crate::app_search::AppEntry> = [
            ("org.templaros.Files", "Files"),
            ("org.templaros.Terminal", "Terminal"),
        ]
        .into_iter()
        .map(|(id, name)| crate::app_search::AppEntry {
            id: id.parse().unwrap(),
            name: name.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
            logical: String::new(),
            variants: 1,
        })
        .collect();
        let recents: Vec<&crate::app_search::AppEntry> = recent_entries.iter().collect();
        let data = FieldData {
            notifications: &[],
            field: &field,
            toplevels: &toplevels,
            icons: &empty_bitmaps,
            previews: &empty_bitmaps,
            drawer_feeds: &drawer_feeds,
            treemap: None,
            composition: &[],
            browser: &browser,
            folder_listings: &HashMap::new(),
            connected: true,
            home_revealed: true,
            apps: &apps,
            apps_available: true,
            widgets: &widgets,
            variants: &HashMap::new(),
            projects: &projects,
            garden: &garden,
            actions: templar_types::action::MANIFEST,
            file_search: None,
            recents: &recents,
            recent_ids: &[],
            reduced_motion: true,
            now_ms: 0,
        };

        let mut buffer = WindowBuffer::new(width, height);
        let mut text = pixelkit_text::TextCache::new(fonts::set());
        let mut kernel = RasterKernel::new();
        let mut input = Input::new();
        let mut screen = FieldScreen::default();
        // `TEMPLAR_BASELINE_OMNIBOX=1` renders the same fixture with the omnibox open over it, for
        // reviewing the dropdown — the one surface the resting field cannot show. It returns
        // before the hash, so it can never be mistaken for the baseline itself.
        if std::env::var("TEMPLAR_BASELINE_OMNIBOX").is_ok() {
            screen.search_focused = true;
            screen.search.set_text("re");
        }
        // `TEMPLAR_BASELINE_FILES=1` renders the fixture with the Files object expanded over the
        // field instead — the one surface whose rail, crumb and toolbar are otherwise only
        // seen at a real seat. Like the omnibox variant it returns before the hash.
        if std::env::var("TEMPLAR_BASELINE_FILES").is_ok() {
            screen.files_expanded = true;
            screen
                .files
                .go_to(crate::files_surface::FilesLocation::Directory(
                    "/home/fixture".into(),
                ));
        }
        // Review Help without opening a desktop session; the resting-field hash stays intact.
        if std::env::var("TEMPLAR_BASELINE_HELP").is_ok() {
            screen.help_open = true;
        }
        for _ in 0..2 {
            let mut ui = Ui::new(
                Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                Scale::ONE,
                &mut kernel,
            );
            screen.draw(&mut ui, Rect::new(0, 0, width as i32, height as i32), &data);
            if std::env::var("TEMPLAR_BASELINE_MENU").is_ok() {
                let mut menu = crate::surface::context_menu::ContextMenu::default();
                menu.open_at(
                    (width as i32 - 30, 100),
                    crate::surface::context_menu::MenuTarget {
                        tile: Some(templar_field::TileId(7)),
                        window: Some(templar_window::WindowId(1)),
                        shelved: true,
                        in_window_group: true,
                        ..crate::surface::context_menu::MenuTarget::default()
                    },
                );
                menu.paint(&mut ui, Rect::new(0, 0, width as i32, height as i32));
            }
            input.end_frame();
        }

        if let Ok(path) = std::env::var("TEMPLAR_BASELINE_DUMP") {
            write_png(std::path::Path::new(&path), &buffer).expect("dumping the baseline render");
        }
        if std::env::var("TEMPLAR_BASELINE_OMNIBOX").is_ok()
            || std::env::var("TEMPLAR_BASELINE_GARDEN").is_ok()
            || std::env::var("TEMPLAR_BASELINE_FILES").is_ok()
            || std::env::var("TEMPLAR_BASELINE_HELP").is_ok()
            || std::env::var("TEMPLAR_BASELINE_MENU").is_ok()
            || size.is_some()
        {
            return;
        }
        let hash = buffer
            .pixels
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, pixel| {
                pixel.to_le_bytes().into_iter().fold(hash, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
                })
            });
        assert_eq!(
            hash, 0x7985_0938_80e9_9795,
            "review the Home Field pixels before updating this baseline"
        );
    }

    /// Fixed-pixel baseline for the bottom bar: its three presences, and the hover row.
    ///
    /// The bar has no reservation any more, so what it looks like *is* the whole of how it
    /// behaves: open over Home, a six-pixel chevron under a window, and the bar back out again
    /// when the pointer asks. The fourth is the fan a dwell opens — the most recent window on top
    /// with its capture, the one behind it showing the sliver the top card does not cover. One hash each, reviewed
    /// by eye through the dump below. Fails on any pixel change — read it as "look at the render",
    /// not as "something is broken".
    ///
    /// `TEMPLAR_BAR_PRESENCE_DUMP=<dir>` writes the four renders as PNGs (at scale 1 and 2) and
    /// skips the hashes, which is how the reviewed images under `docs/measurements/` are made.
    #[test]
    fn visual_regression_bar_presence_baseline() {
        use crate::surface::bar_screen::Presence;

        let dump = std::env::var("TEMPLAR_BAR_PRESENCE_DUMP").ok();
        for (presence, open_fan, name, expected) in [
            (Presence::Open, false, "open", 0x7314_af6e_8b30_7d6eu64),
            (Presence::Hint, false, "hint", 0xe0c9_dac0_7b3a_e3a5),
            (Presence::Revealed, false, "revealed", 0x02ef_54cf_0ea3_3b74),
            (Presence::Open, true, "fan", 0x1b4f_7c23_d972_3b1d),
        ] {
            let buffer = bar_frame(presence, open_fan, 1.0);
            if let Some(dir) = &dump {
                let dir = std::path::Path::new(dir);
                std::fs::create_dir_all(dir).expect("the dump directory");
                write_png(&dir.join(format!("bar-{name}.png")), &buffer).expect("dumping");
                let at_2x = bar_frame(presence, open_fan, 2.0);
                write_png(&dir.join(format!("bar-{name}@2x.png")), &at_2x).expect("dumping");
                continue;
            }
            let hash = buffer
                .pixels
                .iter()
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, pixel| {
                    pixel.to_le_bytes().into_iter().fold(hash, |hash, byte| {
                        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
                    })
                });
            assert_eq!(
                hash, expected,
                "review the bar's {name} pixels before updating this baseline"
            );
        }
    }

    /// The hint is a strip of the bar's ground with a chevron on it, and nothing else: whatever
    /// the surface's height, the paint must fill it — this surface has no alpha, so an unpainted
    /// row is a black line across the bottom of the screen.
    #[test]
    fn the_hint_is_six_logical_pixels_and_every_one_of_them_is_painted() {
        use crate::surface::bar_screen::{HINT_HEIGHT, Presence};

        let buffer = bar_frame(Presence::Hint, false, 1.0);
        assert_eq!(buffer.height, HINT_HEIGHT as u32);
        let ground = design::ground();
        assert!(
            buffer.pixels.iter().any(|&p| p != ground),
            "the chevron is somewhere on the strip"
        );
        assert!(
            buffer.pixels.iter().all(|&p| p != 0),
            "and nothing was left black"
        );
    }

    #[tokio::test]
    async fn a_field_is_rendered_to_a_readable_png() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("home.png");
        render_field(&path, 1280, 800).await.expect("rendered");

        let decoder = png::Decoder::new(std::fs::File::open(&path).expect("open"));
        let reader = decoder.read_info().expect("a valid PNG");
        assert_eq!(reader.info().width, 1280);
        assert_eq!(reader.info().height, 800);
    }

    #[tokio::test]
    async fn a_field_too_small_to_show_anything_is_refused_rather_than_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("tiny.png");
        assert!(render_field(&path, 64, 64).await.is_err());
        assert!(!path.exists(), "nothing is written for a refused size");
    }
}
