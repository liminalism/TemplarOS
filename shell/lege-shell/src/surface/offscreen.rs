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
        None => lege_field::FieldDocument::new("primary", cols, rows),
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
    field_model::refresh_titles(&mut field, &live);

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
    let empty_treemaps = HashMap::new();
    let empty_compositions = HashMap::new();
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
        thumbnails: &empty_thumbnails,
    };
    let clock_label = crate::surface::app::clock_label_now();
    let data = FieldData {
        notifications: &[],
        field: &field,
        toplevels: &[],
        icons: &empty_icons,
        previews: &empty_previews,
        drawer_feeds: &drawer_feeds,
        treemaps: &empty_treemaps,
        compositions: &empty_compositions,
        browser: &browser,
        connected: false,
        home_revealed: false,
        network_control_available: false,
        apps: &apps,
        apps_available,
        widgets: &widgets,
        clock_label: &clock_label,
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

/// `WindowBuffer` is 0xAARRGGBB in native order; PNG wants RGBA bytes.
fn write_png(path: &Path, buffer: &WindowBuffer) -> Result<()> {
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

    /// Fixed-pixel baseline for the default Home Field. Unlike `render_field`, this fixture reads
    /// no clock, filesystem, D-Bus service, persisted layout, or host font. A visual change must
    /// therefore be reviewed and deliberately update this hash; environment changes cannot make
    /// it flicker.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn visual_regression_home_field_baseline() {
        // 1280x832 -> a 12x8 grid: roomy enough that every object class — drawers, widgets,
        // the system shelf, a project, a bound folder and a shortcut pane — actually places,
        // so the baseline reviews all of their material roles at once.
        let (width, height) = (1280_u32, 832_u32);
        let (cols, rows) =
            field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
        let mut field = lege_field::FieldDocument::new("visual-baseline", cols, rows);
        let projects = vec![field_model::ProjectInfo {
            id: "legeos".to_owned(),
            name: "LegeOS".to_owned(),
            is_open: true,
        }];
        let live = LiveState {
            toplevels: &[],
            projects: &projects,
        };
        field_model::merge(&mut field, &live);
        field_model::refresh_titles(&mut field, &live);
        // One of each placeable object class the material roles distinguish: the merge already
        // adds widgets and the system shelf; a bound folder and a shortcut pane join here so
        // this baseline is also the reviewed render for their tints.
        field_model::merge_desktop_folder(
            &mut field,
            Some(std::path::Path::new("/home/fixture/Desktop")),
        );
        field_model::route_file_drop(&mut field, std::path::Path::new("/home/fixture/notes.md"));

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
                headline: "Session ready".to_owned(),
                detail: vec!["Balanced power".to_owned()],
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
        let empty_treemaps = HashMap::new();
        let empty_compositions = HashMap::new();
        let browser_sources = crate::file_browser::BrowserSources::default();
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
            thumbnails: &empty_thumbnails,
        };
        let apps = Vec::new();
        let data = FieldData {
            notifications: &[],
            field: &field,
            toplevels: &[],
            icons: &empty_bitmaps,
            previews: &empty_bitmaps,
            drawer_feeds: &drawer_feeds,
            treemaps: &empty_treemaps,
            compositions: &empty_compositions,
            browser: &browser,
            connected: true,
            home_revealed: true,
            network_control_available: false,
            apps: &apps,
            apps_available: true,
            widgets: &widgets,
            clock_label: "Thu 27 Aug 10:15",
            reduced_motion: true,
            now_ms: 0,
        };

        let mut buffer = WindowBuffer::new(width, height);
        let mut text = pixelkit_text::TextCache::new(fonts::set());
        let mut kernel = RasterKernel::new();
        let mut input = Input::new();
        let mut screen = FieldScreen::default();
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

        if let Ok(path) = std::env::var("LEGE_BASELINE_DUMP") {
            write_png(std::path::Path::new(&path), &buffer).expect("dumping the baseline render");
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
            hash, 0xf597_8489_6e30_f9f9,
            "review the Home Field pixels before updating this baseline"
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
