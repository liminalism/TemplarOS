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
use std::path::{Path, PathBuf};

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
        garden,
    ) = tokio::join!(
        crate::data::fetch_all(12),
        crate::data::fetch_apps(),
        crate::data::fetch_projects(),
        crate::data::fetch_session_widget(),
        crate::data::fetch_notifications_widget(),
        crate::data::fetch_settings_widget(),
        crate::data::fetch_browser_sources(),
        crate::garden::fetch_places(),
    );

    let (cols, rows) = field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
    let mut field = match crate::data::fetch_field("primary", cols, rows).await {
        Some(persisted) => persisted,
        None => templar_field::FieldDocument::new("primary", cols, rows),
    };
    let (notifications_widget, _notification_rows) = notifications_widget;
    let apps = apps.unwrap_or_default();
    // No compositor here, so no windows: the field is whatever the persisted document plus the
    // drawers and widgets amount to.
    let live = LiveState {
        projects: &projects,
        toplevels: &[],
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
    let home = std::env::var_os("HOME").map(PathBuf::from);

    let empty_icons = HashMap::new();
    let empty_previews = HashMap::new();
    let empty_thumbnails = std::collections::HashMap::new();
    let mut folder_listings = HashMap::new();
    for tile in &field.tiles {
        if let templar_field::TileKind::DesktopDock { path, order } = &tile.kind {
            let listing = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let browse = templar_client::proxies::FilesBrowseProxy::new(&connection).await?;
                let json = browse
                    .list_directory_native_v1(
                        &templar_client::file_index::encode_path(path),
                        false,
                        500,
                    )
                    .await?;
                let mut dock = super::desktop_dock::DesktopDock::new(path.clone(), order.clone());
                dock.apply_listing(serde_json::from_str(&json)?)?;
                dock.browse_listing()
            }
            .await
            .map_err(|error| error.to_string());
            folder_listings.insert(path.clone(), listing);
        }
    }
    let browser = FileBrowserData {
        sources: &browser_sources,
        listing: None,
        pending: None,
        error: None,
        status: None,
        search_query: None,
        search_entries: None,
        search_pages: None,
        search_error: None,
        feeds: &drawer_feeds,
        thumbnails: &empty_thumbnails,
        icons: &empty_icons,
        home: home.as_deref(),
    };
    let mut data = FieldData {
        notifications: &[],
        field: &field,
        toplevels: &[],
        providers: &std::collections::BTreeMap::new(),
        icons: &empty_icons,
        previews: &empty_previews,
        drawer_feeds: &drawer_feeds,
        treemap: None,
        browser: &browser,
        folder_listings: &folder_listings,
        connected: false,
        home_revealed: false,
        apps: &apps,
        widgets: &widgets,
        variants: &HashMap::new(),
        garden: &garden,
        reduced_motion: true,
        now_ms: 0,
    };

    // Two passes: the first establishes every tile's rect (the field lays out while it draws), the
    // second paints with those rects known. The same reason `Harness::rect_of` draws twice.
    let mut paint = |data: &FieldData<'_>| {
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            design::theme(),
            Scale::ONE,
            &mut kernel,
        );
        let actions = screen.draw(&mut ui, Rect::new(0, 0, width as i32, height as i32), data);
        drop(ui);
        input.end_frame();
        actions
    };
    let actions = paint(&data);
    let preview = if let Some(crate::surface::field_screen::FieldAction::NeedTreemap {
        path,
        width,
        height,
    }) = actions.into_iter().find(|action| {
        matches!(
            action,
            crate::surface::field_screen::FieldAction::NeedTreemap { .. }
        )
    }) {
        Some(super::app::load_treemap_preview(path, width, height).await)
    } else {
        None
    };
    data.treemap = preview.as_ref();
    let _ = paint(&data);

    write_png(path, &buffer).with_context(|| format!("writing {}", path.display()))
}

/// Render the bottom bar at rest to a PNG: the network and clock chips plus three taskbar
/// entries and the launcher — at `scale`, in a surface exactly the size the host would ask for.
///
/// The counterpart to [`render_field`] for the one surface nobody could see: the bar is a few
/// hundred pixels in the corner of a live session, and every question about it (is the ground
/// there, do the labels fit, does the stripe eat the plate) is answerable from an image.
pub fn render_bar(path: &Path, scale: f64) -> Result<()> {
    let buffer = bar_frame(
        crate::surface::bar_screen::Presence::Open,
        false,
        scale,
        &fixture_icons(),
    );
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
    icons: &HashMap<String, pixelkit_raster::Bitmap>,
) -> WindowBuffer {
    use crate::surface::bar_screen::{
        BarData, BarScreen, HINT_OPEN_MS, HOVER_OPEN_MS, Presence, wanted_size,
    };

    let entries = crate::rail_model::reconcile(
        crate::taskbar::entries(&fixture_toplevels()),
        &templar_types::launcher::LauncherPreferences::default(),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let logical = wanted_size(
        presence,
        open_fan.then(|| entries[0].visible()),
        false,
        entries.len(),
        None,
        2560,
    );
    let scale = Scale::new(scale);
    let (width, height) = (scale.px(logical.0) as u32, scale.px(logical.1) as u32);

    let mut buffer = WindowBuffer::new(width, height);
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut input = Input::new();
    let mut screen = BarScreen::default();
    // Window-pixel previews are Home-only: the bar's fan cards wear icons, never captures.
    let data = BarData {
        entries: &entries,
        icons,
        // A fixed clock, not the wall clock: this fixture is hashed as a visual baseline, and
        // a baseline that changes every minute is not one.
        activated: Some(templar_window::WindowId(1)),
        covered: presence != Presence::Open,
        peeked: false,
        home_revealed: false,
        now_ms: 0,
        rail: None,
        rows: None,
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
            let hint = wanted_size(Presence::Hint, None, false, entries.len(), None, 2560);
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
        // The row is opened the way a person opens it: the pointer comes to the first chip and
        // stays there until the hover dwell elapses. This frame is the chip merely hovered; the
        // final paint below is the one with the row on it.
        //
        // The chips are centred in a surface the open row has widened, so the first chip's middle
        // is that centring offset plus the island's 8-pixel margin and half a 48-pixel cell. The
        // first chip is the group with two windows.
        let island = crate::surface::bar_screen::island_width(entries.len());
        let first = (logical.0 - island) / 2 + 32;
        input.cursor_moved(scale.px(first) as f32, height as f32 - scale.px(24) as f32);
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

/// The windows every bar fixture draws: a group of two (active, one with a capture), a terminal
/// whose title is a path, and a parked window.
///
/// Built from toplevels rather than from `Entry` literals: `taskbar::entries` is a projection of
/// `templar_window::task::units`, so a fixture that hand-wrote the entries would be drawing a
/// grouping the running shell cannot produce.
fn fixture_toplevels() -> Vec<templar_window::ShellToplevel> {
    let toplevel = |id: u64, app: &str, title: &str, group: Option<(u64, &str, u32)>| {
        templar_window::ShellToplevel {
            window_id: templar_window::WindowId(id),
            canonical_app_id: Some(app.parse().expect("a fixture's app id")),
            claimed_app_id: None,
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: id == 1,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            // The same parked state the switcher and active Home work both present.
            shelved: id == 4,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 800, 600),
            layer: templar_window::Layer::Normal,
            group: group.map(|(group_id, name, accent)| templar_window::GroupInfo {
                group_id,
                name: name.to_owned(),
                accent,
            }),
            tab_count: None,
        }
    };
    let group = Some((7, "Notes and sources", 0x00e0_7b39));
    vec![
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
    ]
}

/// Exercise the shell's real switcher painter with deterministic compositor-order fixtures.
/// Window-pixel previews are Home-only, so the rows wear icons; this does not claim a live
/// session.
pub fn render_switcher(prefix: &Path) -> Result<()> {
    let icons = fixture_icons();
    for (name, scale) in [("open", 1.0), ("hidpi", 2.0), ("fractional", 1.5)] {
        let (buffer, mask) = switcher_frame(scale, &icons);
        assert!(
            mask.pixels.iter().any(|pixel| *pixel != 0),
            "switcher records its painted coverage"
        );
        let path = PathBuf::from(format!("{}-{name}.png", prefix.display()));
        write_png_masked(&path, &buffer, &mask)?;
    }
    Ok(())
}

/// Bus-free fixtures rendered with the same screen and PixelKit painter as the popup.
pub fn render_launcher(prefix: &Path) -> Result<Vec<PathBuf>> {
    use super::launcher_screen::{LauncherData, LauncherScreen};
    use crate::launcher_model::{LauncherMode, LauncherRow, LauncherTarget};
    let mut outputs = Vec::new();
    for (layout, width, scale) in [
        ("desktop", 720, 1.0),
        ("narrow", 360, 1.0),
        ("hidpi", 720, 2.0),
    ] {
        for mode in [LauncherMode::Apps, LauncherMode::Files] {
            let scale = Scale::new(scale);
            let mut buffer = WindowBuffer::new(scale.px(width) as u32, scale.px(480) as u32);
            let mut text = pixelkit_text::TextCache::new(fonts::set());
            let mut input = Input::new();
            let mut kernel = RasterKernel::new();
            let mut screen = LauncherScreen::default();
            screen.show(mode, None, String::new());
            let rows = match mode {
                LauncherMode::Apps => vec![
                    Some(LauncherRow {
                        target: LauncherTarget::Favorite("development".into()),
                        label: "Development terminal".into(),
                        detail: "Opens in ~/Projects".into(),
                        icon_id: Some("desktop-import.kitty.desktop".into()),
                        running: false,
                        section: Some("FAVORITES"),
                        size_bytes: None,
                        mtime_ms: None,
                    }),
                    Some(LauncherRow {
                        target: LauncherTarget::App("org.templaros.Files".parse()?),
                        label: "Files".into(),
                        detail: "File manager".into(),
                        icon_id: None,
                        running: false,
                        section: Some("FAVORITES"),
                        size_bytes: None,
                        mtime_ms: None,
                    }),
                    Some(LauncherRow {
                        target: LauncherTarget::App("org.mozilla.firefox".parse()?),
                        label: "Firefox".into(),
                        detail: "Activate existing window".into(),
                        icon_id: None,
                        running: true,
                        section: Some("RECENT APPLICATIONS"),
                        size_bytes: None,
                        mtime_ms: None,
                    }),
                ],
                LauncherMode::Files => vec![Some(LauncherRow {
                    target: LauncherTarget::Directory(PathBuf::from("/home/example/Projects")),
                    label: "Projects".into(),
                    detail: "/home/example/Projects".into(),
                    icon_id: None,
                    running: false,
                    section: Some("RECENT FOLDERS"),
                    size_bytes: None,
                    mtime_ms: None,
                })],
            };
            let icons = fixture_icons();
            let area = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
            let mut ui = Ui::new(
                Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                scale,
                &mut kernel,
            );
            screen.draw(
                &mut ui,
                area,
                &LauncherData {
                    rows: &rows,
                    icons: &icons,
                    notice: None,
                    loading: false,
                    now_ms: 0,
                },
            );
            let name = match mode {
                LauncherMode::Apps => "apps",
                LauncherMode::Files => "files",
            };
            let path = PathBuf::from(format!("{}-{layout}-{name}.png", prefix.display()));
            write_png(&path, &buffer)?;
            outputs.push(path);
        }
    }
    Ok(outputs)
}

fn switcher_frame(
    scale: f64,
    icons: &HashMap<String, pixelkit_raster::Bitmap>,
) -> (WindowBuffer, WindowBuffer) {
    let scale = Scale::new(scale);
    let mut windows = fixture_toplevels();
    // More rows than this output fits: the selected parked window must remain visible.
    for id in 5..=12 {
        let mut window = windows[2].clone();
        window.window_id = templar_window::WindowId(id);
        window.title = format!("Window {id}");
        windows.push(window);
    }
    let status = crate::wayland_bridge::SwitcherStatus {
        active: true,
        selected: Some(templar_window::WindowId(4)),
        windows: windows.iter().map(|window| window.window_id).collect(),
    };
    let mut buffer = WindowBuffer::new(scale.px(1000) as u32, scale.px(600) as u32);
    let area = Rect::new(0, 0, buffer.width as i32, buffer.height as i32);
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut input = Input::new();
    let mut kernel = RasterKernel::new();
    let mut screen = crate::surface::taskfield_screen::TaskfieldScreen::default();
    {
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            &mut text,
            &mut input,
            design::theme(),
            scale,
            &mut kernel,
        );
        screen.draw_switcher(&mut ui, area, &status, &windows, icons);
    }
    let mut mask = WindowBuffer::new(buffer.width, buffer.height);
    let mut painter = Painter::new(&mut mask);
    for (rect, _) in screen.opaque_shapes() {
        painter.fill_rect(*rect, 0x00ff_ffff);
    }
    (buffer, mask)
}

/// The fixture applications' icons as this machine's themes draw them
/// (`icons::SystemIconRoots`, so `XDG_DATA_DIRS` chooses which installation is asked). Only the
/// command-line renders use this: what they are for is seeing the bar a person would see, and a
/// person's bar wears pictures. The hashed baselines keep an empty map, because a picture that
/// depends on the installed theme is not a baseline.
fn fixture_icons() -> HashMap<String, pixelkit_raster::Bitmap> {
    let mut ids: Vec<String> = fixture_toplevels()
        .iter()
        .filter_map(|toplevel| toplevel.canonical_app_id.as_ref().map(ToString::to_string))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .filter_map(|id| {
            let icon = crate::icons::load_app_icon(&id, &crate::icons::SystemIconRoots)?;
            Some((id, icon))
        })
        .collect()
}

/// The output width the rail renders assume, in logical pixels.
const RAIL_OUTPUT_WIDTH: i32 = 1920;

/// Render both gesture rails half and fully revealed, as the compositor would show them: the
/// apps rail running left from the centre and the system rail running right, on a 1920-pixel
/// output at scale 1, with the surface's mask as the PNG's alpha so whatever the rail has not
/// revealed is transparent. Writes `PATH-apps-half.png`, `PATH-apps-full.png`,
/// `PATH-system-half.png` and `PATH-system-full.png`, and returns their paths.
///
/// The rails are the one surface whose look depends on the pointer's path, so an image of each
/// end of that path is the review a person can do without making the gesture.
pub fn render_rails(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    use crate::surface::rail::{RailSide, RailView};
    use crate::surface::{bar_screen, cluster_screen};

    let width = RAIL_OUTPUT_WIDTH;
    let origin = width / 2;
    let entries = crate::rail_model::reconcile(
        crate::taskbar::entries(&fixture_toplevels()),
        &templar_types::launcher::LauncherPreferences::default(),
        &[],
        &HashMap::new(),
        &HashMap::new(),
    );
    let icons = fixture_icons();
    let machine = crate::data::MachineReadings::default();
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut written = Vec::new();

    for (name, length) in [
        ("apps", bar_screen::rail_length(entries.len(), width)),
        ("system", cluster_screen::rail_length()),
    ] {
        for (extent, revealed) in [("half", length / 2), ("full", length)] {
            let (w, h) = (width, bar_screen::BAR_HEIGHT);
            let area = Rect::new(0, 0, w, h);
            let mut buffer = WindowBuffer::new(w as u32, h as u32);
            let mut input = Input::new();
            let mut ui = Ui::new(
                Painter::new(&mut buffer),
                &mut text,
                &mut input,
                design::theme(),
                Scale::ONE,
                &mut kernel,
            );
            let shapes = if name == "apps" {
                let rail = RailView {
                    origin,
                    side: RailSide::Left,
                    revealed,
                };
                let mut screen = bar_screen::BarScreen::default();
                let data = bar_screen::BarData {
                    entries: &entries,
                    icons: &icons,
                    activated: Some(templar_window::WindowId(1)),
                    covered: false,
                    peeked: false,
                    home_revealed: false,
                    now_ms: 0,
                    rail: Some(rail),
                    rows: None,
                };
                screen.draw(&mut ui, area, &data);
                screen.opaque_shapes().to_vec()
            } else {
                let rail = RailView {
                    origin,
                    side: RailSide::Right,
                    revealed,
                };
                let mut screen = cluster_screen::ClusterScreen::default();
                let data = cluster_screen::ClusterData {
                    // A fixed clock, as the bar baseline has: an image that changes every minute
                    // is not one anybody can compare.
                    clock_label: "Mon 28 Sep 10:00",
                    network_control_available: true,
                    local_now: chrono::NaiveDate::from_ymd_opt(2026, 9, 28)
                        .and_then(|day| day.and_hms_opt(10, 0, 0)),
                    account_label: "templar",
                    machine: &machine,
                    now_ms: 0,
                    rail: Some(rail),
                };
                screen.draw(&mut ui, area, &data);
                screen.opaque_shapes().to_vec()
            };
            drop(ui);
            let mut mask = WindowBuffer::new(w as u32, h as u32);
            let mut painter = Painter::new(&mut mask);
            for (rect, radius) in &shapes {
                painter.fill_rounded_rect_aa(
                    &mut kernel,
                    *rect,
                    *radius as f32,
                    0x00ff_ffff,
                    u8::MAX,
                );
            }
            drop(painter);
            let out = std::path::PathBuf::from(format!("{}-{name}-{extent}.png", path.display()));
            write_png_masked(&out, &buffer, &mask)
                .with_context(|| format!("writing {}", out.display()))?;
            written.push(out);
        }
    }
    Ok(written)
}
/// Exercise the actual clock popup through pointer and keyboard input, then write its masked
/// frames. Fixed leap-day local time makes this fixture bus-free and deterministic.
pub fn render_calendar(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    use crate::surface::rail::{RailSide, RailView};
    let mut written = Vec::new();
    for (name, scale, width, rail) in [
        ("corner", 1.0, 640, None),
        ("narrow", 1.0, 320, None),
        ("fractional", 1.25, 640, None),
        ("hidpi", 2.0, 640, None),
        ("rail-right", 1.0, 1920, Some(RailSide::Right)),
        ("rail-left", 1.0, 1920, Some(RailSide::Left)),
    ] {
        let rail = rail.map(|side| RailView {
            origin: width / 2,
            side,
            revealed: crate::surface::cluster_screen::rail_length(),
        });
        let mut fixture = CalendarFixture::new(scale, width, rail);
        fixture.frame()?;
        let clock = fixture.clock_rect();
        fixture.click(clock)?;
        anyhow::ensure!(
            fixture.screen.calendar_open(),
            "clock click did not open the calendar"
        );
        fixture.frame()?;
        fixture.check_geometry()?;
        let open = fixture.output(path, name, "open")?;
        written.push(open);

        let next = fixture.geometry()?.next;
        fixture.click(next)?;
        anyhow::ensure!(
            fixture.dates()?.0 == chrono::NaiveDate::from_ymd_opt(2024, 3, 1).unwrap(),
            "Next did not show March"
        );
        written.push(fixture.output(path, name, "next")?);
        let previous = fixture.geometry()?.previous;
        fixture.click(previous)?;
        anyhow::ensure!(
            fixture.dates()?.0 == chrono::NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(),
            "Previous did not restore February"
        );
        fixture.input.key(pixelkit_shell::KeyInput::PageUp);
        fixture.frame()?;
        anyhow::ensure!(
            fixture.dates()?.0 == chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            "keyboard month navigation did not show January"
        );
        let today = fixture.geometry()?.today;
        fixture.click(today)?;
        anyhow::ensure!(
            fixture.dates()?.1 == fixture.now.date(),
            "Today did not select leap day"
        );
        fixture.check_geometry()?;
        written.push(fixture.output(path, name, "today")?);
        fixture.input.cursor_left();
        fixture.frame()?;
        anyhow::ensure!(
            fixture.screen.calendar_open(),
            "pointer leave dismissed keyboard work"
        );
        fixture.input.key(pixelkit_shell::KeyInput::Escape);
        fixture.frame()?;
        anyhow::ensure!(
            !fixture.screen.calendar_open(),
            "Escape did not dismiss the calendar"
        );
        anyhow::ensure!(
            fixture.screen.calendar_frame().is_none(),
            "dismissed calendar retained input"
        );
        written.push(fixture.output(path, name, "closed")?);
    }
    Ok(written)
}

struct CalendarFixture {
    screen: crate::surface::cluster_screen::ClusterScreen,
    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    input: Input,
    buffer: WindowBuffer,
    machine: crate::data::MachineReadings,
    scale: Scale,
    output_width: i32,
    rail: Option<crate::surface::rail::RailView>,
    now: chrono::NaiveDateTime,
}

impl CalendarFixture {
    fn new(scale: f64, output_width: i32, rail: Option<crate::surface::rail::RailView>) -> Self {
        let scale = Scale::new(scale);
        let logical = if rail.is_some() {
            crate::surface::cluster_screen::rail_wanted_size(false, true, output_width)
        } else {
            crate::surface::cluster_screen::wanted_size(false, true, output_width)
        };
        Self {
            screen: crate::surface::cluster_screen::ClusterScreen::default(),
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            input: Input::new(),
            buffer: WindowBuffer::new(scale.px(logical.0) as u32, scale.px(logical.1) as u32),
            machine: crate::data::MachineReadings::default(),
            scale,
            output_width,
            rail,
            now: chrono::NaiveDate::from_ymd_opt(2024, 2, 29)
                .unwrap()
                .and_hms_opt(10, 15, 0)
                .unwrap(),
        }
    }

    fn area(&self) -> Rect {
        let size = if self.rail.is_some() {
            crate::surface::cluster_screen::rail_wanted_size(
                self.screen.account_open(),
                self.screen.calendar_open(),
                self.output_width,
            )
        } else {
            crate::surface::cluster_screen::wanted_size(
                self.screen.account_open(),
                self.screen.calendar_open(),
                self.output_width,
            )
        };
        let height = self.scale.px(size.1);
        Rect::new(
            0,
            self.buffer.height as i32 - height,
            self.buffer.width as i32,
            height,
        )
    }

    fn clock_rect(&mut self) -> Rect {
        let area = self.area();
        let ui = Ui::new(
            Painter::new(&mut self.buffer),
            &mut self.text,
            &mut self.input,
            design::theme(),
            self.scale,
            &mut self.kernel,
        );
        crate::surface::cluster_screen::ClusterScreen::clock_rect(&ui, area, self.rail)
    }

    fn geometry(&mut self) -> Result<crate::surface::calendar::Layout> {
        let area = self.area();
        let ui = Ui::new(
            Painter::new(&mut self.buffer),
            &mut self.text,
            &mut self.input,
            design::theme(),
            self.scale,
            &mut self.kernel,
        );
        self.screen
            .calendar_layout(&ui, area, self.rail)
            .context("calendar has no frame")
    }

    fn dates(&self) -> Result<(chrono::NaiveDate, chrono::NaiveDate)> {
        self.screen
            .calendar_dates()
            .context("calendar has no date selection")
    }

    fn frame(&mut self) -> Result<()> {
        let area = self.area();
        let data = crate::surface::cluster_screen::ClusterData {
            clock_label: "THU 29 FEB 10:15",
            local_now: Some(self.now),
            network_control_available: true,
            account_label: "templar",
            machine: &self.machine,
            now_ms: 1000,
            rail: self.rail,
        };
        let mut ui = Ui::new(
            Painter::new(&mut self.buffer),
            &mut self.text,
            &mut self.input,
            design::theme(),
            self.scale,
            &mut self.kernel,
        );
        let actions = self.screen.draw(&mut ui, area, &data);
        drop(ui);
        self.input.end_frame();
        for action in actions {
            anyhow::ensure!(
                matches!(
                    action,
                    crate::surface::bar_screen::BarAction::Field(
                        crate::surface::field_screen::FieldAction::RunAction("clock.open-calendar")
                    )
                ),
                "calendar fixture unexpectedly invoked {action:?}"
            );
            // Exactly the screen state transition used by ShellApp::open_calendar, without a
            // window server or a synthetic replacement for any calendar control.
            self.screen.show_calendar(self.now.date());
        }
        Ok(())
    }

    fn click(&mut self, rect: Rect) -> Result<()> {
        self.input
            .cursor_moved((rect.x + rect.w / 2) as f32, (rect.y + rect.h / 2) as f32);
        self.input.mouse(pixelkit_shell::MouseButton::Left, true);
        self.input.mouse(pixelkit_shell::MouseButton::Left, false);
        self.frame()
    }

    fn check_geometry(&mut self) -> Result<()> {
        let geometry = self.geometry()?;
        let frame = geometry.frame;
        let area = self.area();
        anyhow::ensure!(
            frame.x >= area.x
                && frame.y >= area.y
                && frame.right() <= area.right()
                && frame.bottom()
                    <= area.bottom() - self.scale.px(crate::surface::bar_screen::BAR_HEIGHT),
            "calendar does not fit its host: {frame:?}, {area:?}"
        );
        anyhow::ensure!(
            self.screen
                .opaque_shapes()
                .iter()
                .any(|(shape, _)| *shape == frame),
            "calendar frame is absent from alpha/input coverage"
        );
        for control in [geometry.previous, geometry.today, geometry.next]
            .into_iter()
            .chain((0..42).map(|index| geometry.cell(index)))
        {
            anyhow::ensure!(
                control.w > 0
                    && control.h > 0
                    && control.x >= frame.x
                    && control.y >= frame.y
                    && control.right() <= frame.right()
                    && control.bottom() <= frame.bottom(),
                "calendar control lies outside input coverage: {control:?}, {frame:?}"
            );
        }
        Ok(())
    }

    fn output(&mut self, path: &Path, name: &str, step: &str) -> Result<std::path::PathBuf> {
        let mut mask = WindowBuffer::new(self.buffer.width, self.buffer.height);
        let mut painter = Painter::new(&mut mask);
        for (rect, radius) in self.screen.opaque_shapes() {
            painter.fill_rounded_rect_aa(
                &mut self.kernel,
                *rect,
                *radius as f32,
                0x00ff_ffff,
                u8::MAX,
            );
        }
        drop(painter);
        let output = std::path::PathBuf::from(format!("{}-{name}-{step}.png", path.display()));
        write_png_masked(&output, &self.buffer, &mask)?;
        Ok(output)
    }
}

/// Render contained project work and one Loose tray at normal/HiDPI and narrow sizes,
/// exercising real merge, pointer focus/restore, stable park targets and overflow.
/// Also render coding/attention/resting/fading, template bodies and real Files/Apps
/// tabs from explicit fixture snapshots. No service or display is read.
#[allow(clippy::too_many_lines)]
pub fn render_home_work(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    use crate::surface::field_screen::FieldAction;
    let mut written = Vec::new();
    for (name, width, height, scale) in [
        ("wide", 1280, 832, 1.0),
        ("hidpi", 1280, 832, 2.0),
        ("narrow", 400, 640, 1.0),
    ] {
        let mut fixture = HomeWorkFixture::new(width, height, scale)?;
        if name == "narrow" {
            // Overflow is a workload boundary, not an assumption that five windows
            // cannot fit at this width. Keep the project links unchanged and fill Loose.
            for index in 0..16_u32 {
                let mut window = fixture.windows[1].clone();
                window.window_id = templar_window::WindowId(u64::from(100 + index));
                window.title = format!("Loose task {}", index + 1);
                window.instance_id = None;
                window.activated = false;
                window.mru_rank = 10 + index;
                fixture.previews.insert(
                    field_model::session_identifier(&window, None),
                    preview_fixture(),
                );
                fixture.windows.push(window);
            }
            field_model::merge(
                &mut fixture.field,
                &LiveState {
                    projects: &[],
                    toplevels: &fixture.windows,
                },
            );
        }
        fixture.frame();
        fixture.frame();
        written.push(fixture.output(path, name, "open")?);
        let persistent = fixture.field.located.clone();
        let recents = fixture.feeds.clone();
        let sources = fixture.sources.clone();
        let cards = fixture.cards();
        let overflow = fixture.screen.work_overflow_rect();
        fixture.check_geometry(&cards, overflow)?;
        if name != "narrow" {
            anyhow::ensure!(
                cards.len() == fixture.windows.len(),
                "{name}: a wide field should hold every window's card: {cards:?}"
            );
        }

        // Belonging is containment, never proximity to the centre or a Recent-folder guess.
        let project = {
            let ui = Ui::new(
                Painter::new(&mut fixture.buffer),
                &mut fixture.text,
                &mut fixture.input,
                design::theme(),
                fixture.scale,
                &mut fixture.kernel,
            );
            fixture
                .screen
                .tile_rect(
                    &fixture.field,
                    field_model::folder_tile_id(Path::new("/home/fixture/Documents")),
                    &ui,
                )
                .context("project has no rectangle")?
        };
        for (index, rect) in &cards {
            if HomeWorkFixture::PROJECT_WINDOWS.contains(index) {
                anyhow::ensure!(
                    project.contains(rect.x, rect.y)
                        && project.contains(rect.right() - 1, rect.bottom() - 1),
                    "{name}: attributed window is outside its project: {rect:?}"
                );
            } else {
                anyhow::ensure!(
                    !intersects(project, *rect),
                    "{name}: Loose window appeared inside an unproved project"
                );
            }
        }

        // Every card restores or activates its own window.
        for (index, rect) in &cards {
            let id = fixture.windows[*index].window_id;
            let actions = fixture.click(*rect);
            let expected = if fixture.windows[*index].shelved {
                FieldAction::Restore(id)
            } else {
                FieldAction::Activate(id)
            };
            anyhow::ensure!(
                actions.contains(&expected),
                "{name}: card did not directly restore/activate {id:?}: {actions:?}"
            );
        }

        // Whatever has no room is one overflow card, which opens the overview.
        if cards.len() < fixture.windows.len() {
            let card = overflow.context("windows without a card but no overflow card")?;
            let actions = fixture.click(card);
            anyhow::ensure!(
                actions.contains(&FieldAction::RunAction("window.show-all")),
                "{name}: the overflow card did not open the overview: {actions:?}"
            );
        } else {
            anyhow::ensure!(
                overflow.is_none(),
                "{name}: overflow card with nothing over"
            );
        }
        if name == "narrow" {
            anyhow::ensure!(
                overflow.is_some(),
                "narrow: every window fit, nothing overflowed"
            );
        }

        // Parking minimises into the card's own cells (or leaves the overflow card where it is),
        // and a parked card then restores.
        let parked = cards.first().map_or(0, |(index, _)| *index);
        fixture.windows[parked].shelved = true;
        fixture.windows[parked].activated = false;
        fixture.frame();
        anyhow::ensure!(
            fixture.cards() == cards && fixture.screen.work_overflow_rect() == overflow,
            "{name}: parking a window moved a card"
        );
        if let Some((_, rect)) = cards.first() {
            let actions = fixture.click(*rect);
            anyhow::ensure!(
                actions.contains(&FieldAction::Restore(fixture.windows[parked].window_id)),
                "{name}: the parked card did not restore its window: {actions:?}"
            );
        }
        written.push(fixture.output(path, name, "parked")?);

        // Activating another window changes recency and moves nothing.
        let other = (0..fixture.windows.len())
            .find(|index| *index != parked)
            .context("only one window")?;
        for window in &mut fixture.windows {
            window.mru_rank += 1;
            window.activated = false;
        }
        fixture.windows[other].mru_rank = 0;
        fixture.windows[other].activated = true;
        fixture.frame();
        anyhow::ensure!(
            fixture.cards() == cards && fixture.screen.work_overflow_rect() == overflow,
            "{name}: activating another window moved a card"
        );
        anyhow::ensure!(
            fixture.field.located == persistent,
            "using active work changed persistent tile positions"
        );

        let closed = fixture.windows.remove(1);
        field_model::merge(
            &mut fixture.field,
            &LiveState {
                projects: &[],
                toplevels: &fixture.windows,
            },
        );
        fixture.frame();
        anyhow::ensure!(
            field_model::session_tile_for(&fixture.field, &closed).is_none(),
            "closed work retained an active card"
        );
        anyhow::ensure!(
            fixture.field.located == persistent,
            "closing a window moved persistent folders"
        );
        anyhow::ensure!(
            fixture.feeds == recents && fixture.sources == sources,
            "closing work changed existing recent files/folders"
        );
        written.push(fixture.output(path, name, "closed")?);

        // Exercise the actual alpha buffer path, including live preview erasure and disable.
        fixture.premultiplied = true;
        fixture.screen.wallpaper_active = true;
        fixture.frame();
        anyhow::ensure!(
            fixture.buffer.pixels.contains(&0),
            "{name}: wallpaper ground is not transparent"
        );
        anyhow::ensure!(
            fixture.buffer.pixels.iter().any(|pixel| pixel >> 24 == 255),
            "{name}: transparent Home lost its readable objects"
        );
        written.push(fixture.output(path, name, "wallpaper-revealed")?);
        fixture.home_revealed = false;
        fixture.windows[0].shelved = false;
        fixture.windows[0].geometry = templar_window::Rect::new(
            0,
            0,
            fixture.scale.logical(fixture.buffer.width as i32) as i32,
            fixture.scale.logical(fixture.buffer.height as i32) as i32,
        );
        fixture.frame();
        anyhow::ensure!(
            fixture.buffer.pixels.iter().all(|pixel| *pixel == 0),
            "{name}: Home content/preview leaked underneath a covering client"
        );
        written.push(fixture.output(path, name, "wallpaper-occluded")?);
        fixture.screen.wallpaper_active = false;
        fixture.frame();
        anyhow::ensure!(
            fixture.buffer.pixels.iter().all(|pixel| pixel >> 24 == 255),
            "{name}: disabling wallpaper left transparent ground"
        );
        written.push(fixture.output(path, name, "wallpaper-disabled")?);
    }
    written.extend(render_project_work_bodies(path)?);
    Ok(written)
}

#[allow(clippy::too_many_lines)]
fn render_project_work_bodies(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    for (name, template, activity, attention) in [
        ("coding", "coding", "active", "needs-you"),
        ("resting", "coding", "resting", ""),
        ("fading", "coding", "fading", ""),
        ("video", "video", "resting", ""),
        ("images", "images", "resting", ""),
        ("documents", "documents", "resting", ""),
        ("files-tab", "coding", "resting", ""),
        ("apps-tab", "coding", "resting", ""),
        ("text-only", "coding", "active", "failed"),
    ] {
        let mut fixture = HomeWorkFixture::new(1280, 832, 1.0)?;
        let root = std::path::PathBuf::from("/home/fixture/Documents");
        if activity != "active" {
            fixture.windows.clear();
            field_model::merge(
                &mut fixture.field,
                &LiveState {
                    projects: &[],
                    toplevels: &[],
                },
            );
        }
        fixture.screen.previews_on = name != "text-only";
        let place = fixture
            .garden
            .get_mut(&root)
            .context("missing fixture project")?;
        place.material_seed = 7;
        template.clone_into(&mut place.presentation.template);
        activity.clone_into(&mut place.presentation.activity);
        attention.clone_into(&mut place.presentation.attention);
        match (activity, attention) {
            (_, "needs-you") => "claude waiting for you · Focus",
            (_, "failed") => "cargo test failed · Focus",
            ("resting", _) => "Resume: auth refactor",
            ("fading", _) => "Last worked yesterday · Resume",
            _ => "Running · Focus",
        }
        .clone_into(&mut place.presentation.state_line);
        place.presentation.sessions = (0..8)
            .map(|index| crate::garden::ProjectSession {
                id: format!("fixture-session-{index}"),
                kind: "terminal".to_owned(),
                instance_id: if activity == "active" && index < 2 {
                    place.instances.get(index).cloned()
                } else {
                    None
                },
                program: if index == 0 { "claude" } else { "cargo test" }.to_owned(),
                subject: if index == 0 {
                    "auth refactor"
                } else {
                    "preview cache"
                }
                .to_owned(),
                state: if activity != "active" || index >= 2 {
                    "ended"
                } else if attention == "failed" {
                    "failed"
                } else {
                    "waiting"
                }
                .to_owned(),
                cwd: root.clone(),
                commands: vec!["cargo test".to_owned(), "git status".to_owned()],
                ..Default::default()
            })
            .collect();
        place.presentation.apps = fixture
            .apps
            .iter()
            .map(|app| crate::garden::ProjectApp {
                app_id: app.id.to_string(),
                name: app.name.clone(),
                template: template.to_owned(),
            })
            .collect();
        place.presentation.recent_files = [
            ("edit.kdenlive", "video-project"),
            ("final.mp4", "render"),
            ("new-footage.mkv", "footage"),
            ("garden.png", "image"),
            ("portrait.jpg", "image"),
            ("report.pdf", "document"),
            ("notes.odt", "document"),
        ]
        .into_iter()
        .map(|(name, category)| crate::garden::ProjectFile {
            path: root.join(name),
            name: name.to_owned(),
            category: category.to_owned(),
            ..Default::default()
        })
        .collect();
        let entries = place
            .presentation
            .recent_files
            .iter()
            .map(|file| crate::file_browser::BrowseEntry {
                path: file.path.clone(),
                name: file.name.clone(),
                kind: crate::file_browser::EntryKind::File,
                is_symlink: false,
                hidden: false,
                size_bytes: 0,
                mtime_ms: 0,
            })
            .collect();
        fixture.folder_listings.insert(
            root.clone(),
            Ok(crate::file_browser::DirectoryListing {
                directory: root.clone(),
                parent: root.parent().map(Path::to_path_buf),
                entries,
                truncated: false,
                limit: 128,
            }),
        );
        fixture.thumbnails = place
            .presentation
            .recent_files
            .iter()
            .filter(|file| matches!(file.category.as_str(), "image" | "document"))
            .map(|file| (file.path.clone(), preview_fixture()))
            .collect();
        fixture.frame();
        if matches!(name, "files-tab" | "apps-tab") {
            let ui = Ui::new(
                Painter::new(&mut fixture.buffer),
                &mut fixture.text,
                &mut fixture.input,
                design::theme(),
                fixture.scale,
                &mut fixture.kernel,
            );
            let project = fixture
                .screen
                .tile_rect(&fixture.field, field_model::folder_tile_id(&root), &ui)
                .context("missing project rectangle")?;
            drop(ui);
            fixture.input.cursor_moved(
                (project.x + project.w * if name == "files-tab" { 3 } else { 5 } / 6) as f32,
                (project.bottom() - 20) as f32,
            );
            fixture.input.mouse(pixelkit_shell::MouseButton::Left, true);
            fixture
                .input
                .mouse(pixelkit_shell::MouseButton::Left, false);
            fixture.frame();
        }
        fixture.frame();
        written.push(fixture.output(path, "project", name)?);
    }
    Ok(written)
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

struct HomeWorkFixture {
    field: templar_field::FieldDocument,
    screen: FieldScreen,
    windows: Vec<templar_window::ShellToplevel>,
    apps: Vec<crate::app_search::AppEntry>,
    providers:
        std::collections::BTreeMap<templar_types::provider::ProviderRole, templar_types::AppId>,
    feeds: HashMap<crate::home_surface::Drawer, crate::drawers::DrawerFeed>,
    sources: crate::file_browser::BrowserSources,
    garden: HashMap<std::path::PathBuf, crate::garden::GardenPlace>,
    previews: HashMap<String, pixelkit_raster::Bitmap>,
    thumbnails: HashMap<std::path::PathBuf, pixelkit_raster::Bitmap>,
    folder_listings:
        HashMap<std::path::PathBuf, Result<crate::file_browser::DirectoryListing, String>>,
    buffer: WindowBuffer,
    text: pixelkit_text::TextCache,
    input: Input,
    kernel: RasterKernel,
    scale: Scale,
    home_revealed: bool,
    premultiplied: bool,
}

impl HomeWorkFixture {
    /// Exact fixture instances belong to one project; Recent data never creates a link.
    const PROJECT_WINDOWS: &'static [usize] = &[0, 2];

    #[allow(clippy::too_many_lines)]
    fn new(width: i32, height: i32, scale: f64) -> Result<Self> {
        use crate::files_surface::{FilesLocation, PinnedKind, PinnedLocation};
        use templar_types::provider::ProviderRole;
        let (mut windows, apps) = Self::windows_and_apps();
        let associated = [
            templar_types::InstanceId::new(),
            templar_types::InstanceId::new(),
        ];
        windows[0].instance_id = Some(associated[0]);
        windows[2].instance_id = Some(associated[1]);
        let providers = std::collections::BTreeMap::from([
            (
                ProviderRole::Terminal,
                "org.fixture.Terminal".parse().unwrap(),
            ),
            (
                ProviderRole::FileManager,
                "org.fixture.Files".parse().unwrap(),
            ),
        ]);
        let (cols, rows) = field_model::grid_for(width, height, field_model::TILE_SIZE);
        let mut field = templar_field::FieldDocument::new("active-work-fixture", cols, rows);
        let root = std::path::PathBuf::from("/home/fixture/Documents");
        for op in field_model::open_folder_ops(&field, &root) {
            field = templar_field::apply(&field, &op)?.0;
        }
        field = templar_field::apply(
            &field,
            &templar_field::FieldOp::Resize {
                id: field_model::folder_tile_id(&root),
                span: templar_field::Span::new(4, 3),
            },
        )?
        .0;
        field_model::merge(
            &mut field,
            &LiveState {
                projects: &[],
                toplevels: &windows,
            },
        );
        let mut recent = crate::drawers::DrawerFeed::new(
            "Recent files and folders",
            vec![
                "/home/fixture/Documents/draft.md".to_owned(),
                root.display().to_string(),
            ],
        );
        recent.row_targets = vec![
            Some("/home/fixture/Documents/draft.md".to_owned()),
            Some(root.display().to_string()),
        ];
        // What `recent::feed_json` derives from those entries: the editor saved the draft, so
        // its application last worked in the draft's folder.
        recent.folders = vec![
            (Some("org.fixture.Editor".to_owned()), root.clone()),
            (Some("org.fixture.Files".to_owned()), root.clone()),
        ];
        let feeds = HashMap::from([(crate::home_surface::Drawer::Recent, recent)]);
        let sources = crate::file_browser::BrowserSources {
            pinned: vec![
                PinnedLocation {
                    label: "Recent".to_owned(),
                    detail: "Files and folders".to_owned(),
                    location: FilesLocation::Recent,
                    kind: PinnedKind::Feed,
                },
                PinnedLocation {
                    label: "Documents".to_owned(),
                    detail: "Persistent place".to_owned(),
                    location: FilesLocation::Directory(root.clone()),
                    kind: PinnedKind::Place,
                },
            ],
        };
        let garden = HashMap::from([(
            root.clone(),
            crate::garden::GardenPlace {
                root: root.clone(),
                stage: "trace".to_owned(),
                form: "plain".to_owned(),
                summary: "Recent writing".to_owned(),
                instances: associated.iter().map(ToString::to_string).collect(),
                presentation: crate::garden::ProjectPresentation {
                    root: root.clone(),
                    name: "Documents".to_owned(),
                    template: "plain".to_owned(),
                    activity: "active".to_owned(),
                    state_line: "2 running · Focus".to_owned(),
                    instances: associated.iter().map(ToString::to_string).collect(),
                    ..Default::default()
                },
                ..crate::garden::GardenPlace::default()
            },
        )]);
        let previews = windows
            .iter()
            .map(|window| {
                (
                    field_model::session_identifier(window, None),
                    preview_fixture(),
                )
            })
            .collect();
        let scale = Scale::new(scale);
        Ok(Self {
            field,
            screen: FieldScreen::default(),
            windows,
            apps,
            providers,
            feeds,
            sources,
            garden,
            previews,
            thumbnails: HashMap::new(),
            folder_listings: HashMap::new(),
            buffer: WindowBuffer::new(scale.px(width) as u32, scale.px(height) as u32),
            text: pixelkit_text::TextCache::new(fonts::set()),
            input: Input::new(),
            kernel: RasterKernel::new(),
            scale,
            home_revealed: true,
            premultiplied: false,
        })
    }

    fn windows_and_apps() -> (
        Vec<templar_window::ShellToplevel>,
        Vec<crate::app_search::AppEntry>,
    ) {
        let records = [
            (
                "org.fixture.Terminal",
                "Terminal",
                "TerminalEmulator",
                "/home/fixture/work",
                false,
            ),
            (
                "org.fixture.Terminal",
                "Terminal",
                "TerminalEmulator",
                "Paused build",
                true,
            ),
            (
                "org.fixture.Files",
                "Files",
                "FileManager",
                "/home/fixture/Documents",
                false,
            ),
            (
                "org.fixture.Editor",
                "Editor",
                "TextEditor",
                "Draft notes",
                false,
            ),
            (
                "org.fixture.Images",
                "Images",
                "Graphics;Viewer",
                "Holiday photos",
                false,
            ),
        ];
        let windows: Vec<_> = records
            .iter()
            .enumerate()
            .map(
                |(index, (app, _, _, title, shelved))| templar_window::ShellToplevel {
                    window_id: templar_window::WindowId(index as u64 + 1),
                    canonical_app_id: Some(app.parse().unwrap()),
                    claimed_app_id: None,
                    instance_id: None,
                    title: (*title).to_owned(),
                    verified_identity: true,
                    activated: index == 0,
                    mru_rank: index as u32,
                    fullscreen: false,
                    shelved: *shelved,
                    maximized: false,
                    geometry: templar_window::Rect::new(0, 0, 800, 600),
                    layer: templar_window::Layer::Normal,
                    group: None,
                    tab_count: None,
                },
            )
            .collect();
        let apps = records
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != 1)
            .map(
                |(_, (app, name, category, _, _))| crate::app_search::AppEntry {
                    id: app.parse().unwrap(),
                    name: (*name).to_owned(),
                    categories: category.split(';').map(str::to_owned).collect(),
                    desktop_file_id: String::new(),
                    icon: String::new(),
                    keywords: Vec::new(),
                    no_display: false,
                    logical: String::new(),
                    variants: 1,
                },
            )
            .collect();
        (windows, apps)
    }

    fn frame(&mut self) -> Vec<crate::surface::field_screen::FieldAction> {
        let empty_bitmaps = HashMap::new();
        let widgets = HashMap::new();
        let browser = FileBrowserData {
            sources: &self.sources,
            listing: None,
            pending: None,
            error: None,
            status: None,
            search_query: None,
            search_entries: None,
            search_pages: None,
            search_error: None,
            feeds: &self.feeds,
            thumbnails: &self.thumbnails,
            icons: &empty_bitmaps,
            home: Some(Path::new("/home/fixture")),
        };
        let data = FieldData {
            field: &self.field,
            toplevels: &self.windows,
            providers: &self.providers,
            icons: &empty_bitmaps,
            previews: &self.previews,
            drawer_feeds: &self.feeds,
            treemap: None,
            browser: &browser,
            folder_listings: &self.folder_listings,
            notifications: &[],
            connected: true,
            home_revealed: self.home_revealed,
            apps: &self.apps,
            variants: &HashMap::new(),
            garden: &self.garden,
            widgets: &widgets,
            reduced_motion: true,
            now_ms: 0,
        };
        let area = Rect::new(0, 0, self.buffer.width as i32, self.buffer.height as i32);
        let mut ui = Ui::new(
            if self.premultiplied {
                Painter::new_premultiplied(&mut self.buffer)
            } else {
                Painter::new(&mut self.buffer)
            },
            &mut self.text,
            &mut self.input,
            design::theme(),
            self.scale,
            &mut self.kernel,
        );
        let actions = self.screen.draw(&mut ui, area, &data);
        drop(ui);
        self.input.end_frame();
        actions
    }

    /// Every window that has a card, by index, with the card's rect.
    fn cards(&self) -> Vec<(usize, Rect)> {
        (0..self.windows.len())
            .filter_map(|index| {
                let tile = field_model::session_tile_for(&self.field, &self.windows[index])?;
                Some((index, self.screen.work_card_rect(tile)?))
            })
            .collect()
    }

    /// Cards and the overflow card lie inside the field, overlap neither each other nor any
    /// persistent tile.
    fn check_geometry(&mut self, cards: &[(usize, Rect)], overflow: Option<Rect>) -> Result<()> {
        let bounds = self.screen.bounds;
        let ui = Ui::new(
            Painter::new(&mut self.buffer),
            &mut self.text,
            &mut self.input,
            design::theme(),
            self.scale,
            &mut self.kernel,
        );
        let persistent: Vec<Rect> = self
            .field
            .placements_in(None)
            .into_iter()
            .filter(|(id, _)| {
                !matches!(
                    self.field.kind_of(*id),
                    Some(
                        templar_field::TileKind::ParkedSession { .. }
                            | templar_field::TileKind::Folder { .. }
                    )
                )
            })
            .filter_map(|(id, _)| self.screen.tile_rect(&self.field, id, &ui))
            .collect();
        let all: Vec<Rect> = cards
            .iter()
            .map(|(_, rect)| *rect)
            .chain(overflow)
            .collect();
        anyhow::ensure!(
            !all.is_empty(),
            "no window has a card and nothing overflowed"
        );
        for (i, rect) in all.iter().enumerate() {
            anyhow::ensure!(
                rect.x >= bounds.x
                    && rect.y >= bounds.y
                    && rect.right() <= bounds.right()
                    && rect.bottom() <= bounds.bottom(),
                "card {rect:?} leaves the field {bounds:?}"
            );
            anyhow::ensure!(
                all[i + 1..].iter().all(|other| !intersects(*rect, *other)),
                "cards overlap: {all:?}"
            );
            anyhow::ensure!(
                persistent.iter().all(|tile| !intersects(*rect, *tile)),
                "card {rect:?} covers a persistent tile: {persistent:?}"
            );
        }
        Ok(())
    }

    fn click(&mut self, rect: Rect) -> Vec<crate::surface::field_screen::FieldAction> {
        self.input.cursor_moved(
            (rect.x + rect.w / 2) as f32,
            (rect.bottom() - self.scale.px(10)) as f32,
        );
        self.input.mouse(pixelkit_shell::MouseButton::Left, true);
        self.input.mouse(pixelkit_shell::MouseButton::Left, false);
        self.frame()
    }

    fn output(&self, path: &Path, name: &str, step: &str) -> Result<std::path::PathBuf> {
        let output = std::path::PathBuf::from(format!("{}-{name}-{step}.png", path.display()));
        write_png_mode(&output, &self.buffer, self.premultiplied)?;
        Ok(output)
    }
}

/// Like [`write_png`], with `mask`'s coverage (any channel of it; the host paints it white) as
/// each pixel's alpha — what the compositor blends, rather than the opaque fallback.
fn write_png_masked(path: &Path, buffer: &WindowBuffer, mask: &WindowBuffer) -> Result<()> {
    let mut rgba = Vec::with_capacity(buffer.pixels.len() * 4);
    for (pixel, coverage) in buffer.pixels.iter().zip(&mask.pixels) {
        rgba.extend_from_slice(&[
            (pixel >> 16) as u8,
            (pixel >> 8) as u8,
            *pixel as u8,
            *coverage as u8,
        ]);
    }
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), buffer.width, buffer.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&rgba)
        .context("encoding a rail as PNG")?;
    Ok(())
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

/// Write the ordinary opaque XRGB field as RGBA bytes.
pub(super) fn write_png(path: &Path, buffer: &WindowBuffer) -> Result<()> {
    write_png_mode(path, buffer, false)
}

/// PNG stores straight alpha, so unpremultiply only at this diagnostic output boundary.
fn write_png_mode(path: &Path, buffer: &WindowBuffer, premultiplied: bool) -> Result<()> {
    let mut rgba = Vec::with_capacity(buffer.pixels.len() * 4);
    for pixel in &buffer.pixels {
        let alpha = if premultiplied { pixel >> 24 } else { 255 };
        let channel = |shift: u32| {
            let value = (*pixel >> shift) & 255_u32;
            (value * 255 + alpha / 2)
                .checked_div(alpha)
                .unwrap_or(0)
                .min(255) as u8
        };
        rgba.extend_from_slice(&[channel(16), channel(8), channel(0), alpha as u8]);
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

/// One synthetic Place: fixed root, form, tree and scores. What is under review wherever these
/// are drawn is the drawing, not the ledger's arithmetic, which has its own tests in
/// `templar-session-core`.
pub(crate) struct FixturePlace {
    pub slug: &'static str,
    pub root: &'static str,
    pub form: &'static str,
    pub tree: Vec<crate::garden::TreeNode>,
    pub summary: &'static str,
    /// Arrivals not yet opened, relative to `root`.
    pub bloomed: &'static [&'static str],
    pub seed: u64,
}

fn fixture_dir(
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

fn fixture_leaf(name: &str, score: f32) -> crate::garden::TreeNode {
    crate::garden::TreeNode {
        name: name.to_owned(),
        is_dir: false,
        score,
        children: Vec::new(),
    }
}

/// The four places the garden is reviewed with: a folder of code projects worked through
/// (`code`), a photo folder looked at (`photos`), a manuscript's working set (`manuscript`) and a
/// place that has only just earned a form (`sparse`). Within the daemon's own bounds — three
/// levels, at most 32 nodes — so nothing here is a tree the ledger could not have served.
#[allow(clippy::too_many_lines)]
pub(crate) fn fixture_place(slug: &str) -> FixturePlace {
    let (dir, leaf) = (fixture_dir, fixture_leaf);
    match slug {
        "photos" => FixturePlace {
            slug: "photos",
            root: "/home/fixture/Pictures",
            form: "shelf",
            tree: vec![
                dir(
                    "2026-lisbon",
                    9.0,
                    vec![
                        leaf("alfama.jpg", 3.5),
                        leaf("tram-28.jpg", 2.6),
                        leaf("sintra.jpg", 1.8),
                        leaf("azulejos.jpg", 1.1),
                    ],
                ),
                leaf("sunset.png", 7.0),
                leaf("workshop.mp4", 6.0),
                leaf("portrait.webp", 4.0),
                dir(
                    "family",
                    3.2,
                    vec![leaf("grandma.jpg", 1.6), leaf("garden-party.jpg", 1.2)],
                ),
                leaf("scan.avif", 2.0),
                dir("screenshots", 1.2, vec![leaf("bug-report.png", 0.8)]),
            ],
            summary: "21 opens, 5 sessions",
            bloomed: &["portrait.webp"],
            seed: 0x51f0_2a77_c3d1_9e04,
        },
        "manuscript" => FixturePlace {
            slug: "manuscript",
            root: "/home/fixture/Documents/The-Salt-Road",
            form: "folio",
            tree: vec![
                dir(
                    "chapters",
                    11.0,
                    vec![
                        leaf("01-harbour.md", 2.2),
                        leaf("02-inland.md", 3.1),
                        leaf("03-the-salt-road.md", 4.6),
                        leaf("04-winter.md", 1.4),
                    ],
                ),
                leaf("draft-v3.odt", 8.0),
                leaf("outline.md", 5.0),
                leaf("notes.md", 3.5),
                dir(
                    "research",
                    2.8,
                    vec![leaf("maps.pdf", 1.5), leaf("letters.odt", 0.9)],
                ),
                leaf("cover-letter.md", 1.0),
            ],
            summary: "46 opens, 9 sessions",
            bloomed: &[],
            seed: 0x9a3e_0c55_1b7f_d2e8,
        },
        "sparse" => FixturePlace {
            slug: "sparse",
            root: "/home/fixture/Music/Field-recordings",
            form: "branch",
            tree: vec![
                dir("2026-09", 1.4, vec![leaf("rain-on-tin.wav", 0.9)]),
                leaf("index.md", 0.6),
            ],
            summary: "4 opens, 2 sessions",
            bloomed: &["2026-09/rain-on-tin.wav"],
            seed: 0x0cc4_7d19_e2a0_5b36,
        },
        _ => FixturePlace {
            slug: "code",
            root: "/home/fixture/Rust-projects",
            form: "branch",
            tree: vec![
                dir(
                    "templaros",
                    14.0,
                    vec![
                        dir(
                            "crates",
                            8.0,
                            vec![
                                dir("templar-field", 3.4, Vec::new()),
                                dir("templar-treemap", 2.2, Vec::new()),
                                dir("templar-session-core", 1.6, Vec::new()),
                            ],
                        ),
                        dir("shell", 5.0, vec![dir("templar-shell", 4.4, Vec::new())]),
                        dir(
                            "docs",
                            2.0,
                            vec![leaf("homegarden.md", 1.2), dir("garden", 0.7, Vec::new())],
                        ),
                        leaf("Cargo.toml", 1.1),
                    ],
                ),
                dir(
                    "pixelkit",
                    6.0,
                    vec![
                        dir(
                            "crates",
                            3.0,
                            vec![
                                dir("pixelkit-raster", 1.8, Vec::new()),
                                dir("pixelkit-ui", 1.0, Vec::new()),
                            ],
                        ),
                        leaf("README.md", 0.8),
                    ],
                ),
                dir(
                    "Silvaview-rs",
                    3.0,
                    vec![dir("src", 2.1, vec![leaf("cushion.rs", 1.3)])],
                ),
                dir("bpg-rs", 1.0, vec![leaf("lib.rs", 0.6)]),
            ],
            summary: "34 opens, 11 launches, 7 sessions",
            bloomed: &[],
            seed: 0x7e11_c0de_4a5b_0913,
        },
    }
}

/// The places the gallery draws, in the order its contact sheet shows them.
const GALLERY_PLACES: [&str; 4] = ["code", "photos", "manuscript", "sparse"];
/// The tile sizes the gallery draws each place at: the first rung that shows a form, and the
/// largest a place grows to.
const GALLERY_SIZES: [(&str, u16, u16); 2] = [("wide", 4, 3), ("dominant", 6, 4)];
/// Ground left around each cropped tile, logical px.
const GALLERY_MARGIN: i32 = 14;

/// Every Place theme × every fixture place × every gallery size, as
/// `<theme>-<place>-<size>.png` under `dir`, plus `gallery.png`, a contact sheet of all of them
/// with one theme per row. Headless and bus-free: the fixture places are grown into an otherwise
/// empty field by the real tender and drawn by the real `FieldScreen`, then cropped to the tile.
pub fn render_garden_gallery(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut text = pixelkit_text::TextCache::new(fonts::set());
    let mut kernel = RasterKernel::new();
    let mut written = Vec::new();
    let mut rows: Vec<(&str, Vec<(String, WindowBuffer)>)> = Vec::new();
    for theme in crate::surface::place_styles::THEMES {
        let mut row = Vec::new();
        for fixture in GALLERY_PLACES.map(fixture_place) {
            let slug = fixture.slug;
            for (size, cols, rows) in GALLERY_SIZES {
                let frame =
                    garden_tile_frame(theme.name, &fixture, (cols, rows), &mut text, &mut kernel)
                        .with_context(|| format!("drawing {} {slug} {size}", theme.name))?;
                let path = dir.join(format!("{}-{slug}-{size}.png", theme.name));
                write_png(&path, &frame).with_context(|| format!("writing {}", path.display()))?;
                written.push(path);
                row.push((format!("{slug} · {size}"), frame));
            }
        }
        rows.push((theme.name, row));
    }
    let sheet = contact_sheet(&rows, &mut text, &mut kernel);
    let path = dir.join("gallery.png");
    write_png(&path, &sheet).with_context(|| format!("writing {}", path.display()))?;
    written.push(path);
    Ok(written)
}

/// One fixture place grown at `span` into an empty field and drawn in `theme`, cropped to its
/// tile with a margin of ground around it.
#[allow(clippy::too_many_lines)]
fn garden_tile_frame(
    theme: &str,
    fixture: &FixturePlace,
    (cols, rows): (u16, u16),
    text: &mut pixelkit_text::TextCache,
    kernel: &mut RasterKernel,
) -> Result<WindowBuffer> {
    let (width, height) = (1280_u32, 832_u32);
    let (grid_cols, grid_rows) =
        field_model::grid_for(width as i32, height as i32, field_model::TILE_SIZE);
    let mut field = templar_field::FieldDocument::new("garden-gallery", grid_cols, grid_rows);
    let root = std::path::PathBuf::from(fixture.root);
    let view = templar_field::garden::ContextView {
        root: root.clone(),
        wanted: Some(templar_field::Span::new(cols, rows)),
        landing: Some(templar_field::Cell::new(1, 1)),
        pinned: false,
        user_span: None,
        summary: fixture.summary.to_owned(),
    };
    for tended in
        templar_field::garden::tend(&field, &[view], templar_field::garden::HomeCorner::TopLeft)
    {
        field = templar_field::apply(&field, &tended.op)
            .map_err(|error| anyhow::anyhow!("the gallery field refused a tending: {error:?}"))?
            .0;
    }
    let tile = templar_field::garden::garden_tile_id(&root);
    anyhow::ensure!(
        field.location(tile).is_some(),
        "the tender did not place {}",
        fixture.root
    );
    let mut garden = HashMap::new();
    garden.insert(
        root.clone(),
        crate::garden::GardenPlace {
            root: root.clone(),
            stage: "expanded".to_owned(),
            form: fixture.form.to_owned(),
            tree: fixture.tree.clone(),
            material_seed: fixture.seed,
            summary: fixture.summary.to_owned(),
            bloom_total: fixture.bloomed.len() as u32,
            bloomed: fixture.bloomed.iter().map(|path| root.join(path)).collect(),
            ..crate::garden::GardenPlace::default()
        },
    );

    let sources = crate::file_browser::BrowserSources { pinned: Vec::new() };
    let (feeds, bitmaps, thumbnails, listings, widgets, variants) = (
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
    );
    let browser = FileBrowserData {
        sources: &sources,
        listing: None,
        pending: None,
        error: None,
        status: None,
        search_query: None,
        search_entries: None,
        search_pages: None,
        search_error: None,
        feeds: &feeds,
        thumbnails: &thumbnails,
        icons: &bitmaps,
        home: None,
    };
    let data = FieldData {
        notifications: &[],
        field: &field,
        toplevels: &[],
        providers: &std::collections::BTreeMap::new(),
        icons: &bitmaps,
        previews: &bitmaps,
        drawer_feeds: &feeds,
        treemap: None,
        browser: &browser,
        folder_listings: &listings,
        connected: true,
        home_revealed: true,
        apps: &[],
        widgets: &widgets,
        variants: &variants,
        garden: &garden,
        reduced_motion: true,
        now_ms: 0,
    };

    let mut buffer = WindowBuffer::new(width, height);
    let mut input = Input::new();
    let mut screen = FieldScreen::default();
    screen.place_styles.set_theme(theme);
    let mut crop = Rect::new(0, 0, 0, 0);
    for _ in 0..2 {
        let mut ui = Ui::new(
            Painter::new(&mut buffer),
            text,
            &mut input,
            design::theme(),
            Scale::ONE,
            kernel,
        );
        screen.draw(&mut ui, Rect::new(0, 0, width as i32, height as i32), &data);
        crop = screen
            .tile_rect(&field, tile, &ui)
            .context("the placed tile has a rect")?
            .inset(-GALLERY_MARGIN)
            .intersect(Rect::new(0, 0, width as i32, height as i32));
        input.end_frame();
    }
    Ok(cropped(&buffer, crop))
}

fn cropped(buffer: &WindowBuffer, rect: Rect) -> WindowBuffer {
    let mut out = WindowBuffer::new(rect.w.max(1) as u32, rect.h.max(1) as u32);
    for y in 0..rect.h.max(0) {
        let from = ((rect.y + y) as u32 * buffer.width + rect.x as u32) as usize;
        let to = (y as u32 * out.width) as usize;
        out.pixels[to..to + rect.w as usize]
            .copy_from_slice(&buffer.pixels[from..from + rect.w as usize]);
    }
    out
}

/// The gallery's frames on one sheet: a row per theme, a column per place and size, each
/// labelled, on the field's own ground.
fn contact_sheet(
    rows: &[(&str, Vec<(String, WindowBuffer)>)],
    text: &mut pixelkit_text::TextCache,
    kernel: &mut RasterKernel,
) -> WindowBuffer {
    let (gutter, caption, side) = (16_i32, 22_i32, 110_i32);
    let columns = rows.first().map_or(0, |(_, row)| row.len());
    let column_widths: Vec<i32> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|(_, row)| row.get(column))
                .map(|(_, frame)| frame.width as i32)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let row_heights: Vec<i32> = rows
        .iter()
        .map(|(_, row)| {
            row.iter()
                .map(|(_, frame)| frame.height as i32)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let width = side + column_widths.iter().map(|w| w + gutter).sum::<i32>() + gutter;
    let height = caption + row_heights.iter().map(|h| h + gutter).sum::<i32>() + gutter;
    let mut sheet = WindowBuffer::new(width as u32, height as u32);
    let mut input = Input::new();
    let mut ui = Ui::new(
        Painter::new(&mut sheet),
        text,
        &mut input,
        design::theme(),
        Scale::ONE,
        kernel,
    );
    ui.painter.clear(design::ground());
    let mut y = caption + gutter;
    for ((theme, row), row_height) in rows.iter().zip(&row_heights) {
        ui.label_styled(
            Rect::new(gutter, y, side - gutter, 20),
            theme,
            design::title_style(&ui),
            design::text(),
            pixelkit_text::Align::Left,
        );
        let mut x = side;
        for ((name, frame), column_width) in row.iter().zip(&column_widths) {
            if y == caption + gutter {
                ui.label_styled(
                    Rect::new(x, gutter / 2, *column_width, caption),
                    name,
                    design::state_style(&ui),
                    design::text_dim(),
                    pixelkit_text::Align::Left,
                );
            }
            let bitmap = pixelkit_raster::Bitmap {
                width: frame.width,
                height: frame.height,
                pixels: frame
                    .pixels
                    .iter()
                    .map(|pixel| pixel | 0xff00_0000)
                    .collect(),
            };
            ui.painter.blit(
                &bitmap,
                Rect::new(x, y, frame.width as i32, frame.height as i32),
                pixelkit_raster::ScaleFilter::Nearest,
            );
            x += column_width + gutter;
        }
        y += row_height + gutter;
    }
    drop(ui);
    sheet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switcher_real_painter_wears_icons_and_retains_selected_overflow_row() {
        let icons = ["org.templaros.Settings", "org.templaros.Notes"]
            .into_iter()
            .map(|app| {
                (
                    app.to_owned(),
                    pixelkit_raster::Bitmap {
                        width: 16,
                        height: 16,
                        pixels: vec![0xffab_cdef; 256],
                    },
                )
            })
            .collect();
        // Window-pixel previews are Home-only: the switcher rows wear icons, so two frames
        // of the same fixtures are identical — no capture to refresh.
        let (before, mask) = switcher_frame(1.0, &icons);
        let (after, _) = switcher_frame(1.0, &icons);
        assert_eq!(before.pixels, after.pixels);
        assert!(mask.pixels.iter().any(|pixel| *pixel != 0));
        // The parked selected window is drawn even though the twelve-window ring overflows.
        assert!(
            before
                .pixels
                .iter()
                .any(|pixel| *pixel & 0x00ff_ffff == 0x00ab_cdef)
        );
        let parked_icon = before.pixels[318 * before.width as usize + 442] & 0x00ff_ffff;
        assert_ne!(parked_icon, design::shell_fill());
        assert_ne!(
            parked_icon, 0x00ab_cdef,
            "parked dimming covers the icon too"
        );
        let (scaled, _) = switcher_frame(1.5, &icons);
        assert_eq!((scaled.width, scaled.height), (1500, 900));
    }

    /// The hint is a strip of the bar's ground with a chevron on it, and nothing else: whatever
    /// the surface's height, the paint must fill it — this surface has no alpha, so an unpainted
    /// row is a black line across the bottom of the screen.
    #[test]
    fn the_hint_is_six_logical_pixels_and_every_one_of_them_is_painted() {
        use crate::surface::bar_screen::{HINT_HEIGHT, Presence};

        let buffer = bar_frame(Presence::Hint, false, 1.0, &HashMap::new());
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

    #[test]
    fn wallpaper_diagnostic_png_preserves_real_alpha_not_opaque_or_premultiplied_png() {
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = HomeWorkFixture::new(1280, 832, 1.0).unwrap();
        fixture.premultiplied = true;
        fixture.screen.wallpaper_active = true;
        fixture.frame();
        let output = fixture
            .output(&directory.path().join("home"), "wide", "wallpaper")
            .unwrap();
        let decoded = pixelkit_raster::png::decode(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(decoded.pixels.len(), fixture.buffer.pixels.len());
        for (png, native) in decoded.pixels.iter().zip(&fixture.buffer.pixels) {
            let alpha = native >> 24;
            assert_eq!(png >> 24, alpha);
            for shift in [0, 8, 16] {
                let premultiplied = (native >> shift) & 255;
                let straight = (png >> shift) & 255;
                assert!(premultiplied <= alpha);
                if alpha == 0 {
                    assert_eq!(*png, 0);
                } else {
                    assert_eq!(
                        straight,
                        ((premultiplied * 255 + alpha / 2) / alpha).min(255)
                    );
                }
            }
        }
        assert!(decoded.pixels.iter().any(|pixel| *pixel == 0));
        assert!(decoded.pixels.iter().any(|pixel| pixel >> 24 == 255));
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
