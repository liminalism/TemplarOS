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
    let (drawer_texts, apps, projects, session_widget, browser_sources) = tokio::join!(
        crate::data::fetch_all(12),
        crate::data::fetch_apps(),
        crate::data::fetch_projects(),
        crate::data::fetch_session_widget(),
        crate::data::fetch_browser_sources(),
    );

    let (cols, rows) = field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
    let mut field = match crate::data::fetch_field("primary", cols, rows).await {
        Some(persisted) => persisted,
        None => lege_field::FieldDocument::new("primary", cols, rows),
    };
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

    let drawer_texts: HashMap<_, Vec<String>> = drawer_texts
        .into_iter()
        .map(|(drawer, text)| (drawer, text.lines().map(str::to_owned).collect()))
        .collect();

    let mut buffer = WindowBuffer::new(width, height);
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut input = Input::new();
    let mut screen = FieldScreen::default();

    let empty_icons = HashMap::new();
    let empty_previews = HashMap::new();
    let empty_treemaps = HashMap::new();
    let empty_compositions = HashMap::new();
    let browser = FileBrowserData {
        sources: &browser_sources,
        listing: None,
        pending: None,
        error: None,
        status: None,
        search_query: None,
        search_entries: None,
        search_error: None,
    };
    let clock_label = crate::surface::app::clock_label_now();
    let data = FieldData {
        field: &field,
        toplevels: &[],
        icons: &empty_icons,
        previews: &empty_previews,
        drawer_texts: &drawer_texts,
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
