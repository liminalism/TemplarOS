//! Wiring: one `winit::application::ApplicationHandler` hosting the Home Surface, the
//! trusted-shell protocol connection that declares its layer, and the background D-Bus refresh
//! loop that keeps its content live. See `window.rs`'s module doc for why this crate drives winit
//! itself rather than `pixelkit_shell::run_app`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pixelkit_raster::{Painter, RasterKernel, Rect};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use lege_client::proxies::{
    FilesSearchProxy, LauncherProxy, LifecycleProxy, ProjectsProxy, SettingsProxy,
};
use lege_field::{FieldDocument, FieldOp};
use lege_types::AppId;
use lege_window::RevealHome;

use crate::drawers::standard_paths::{self, StandardDrawer};
use crate::field_model::{self, LiveState, ProjectInfo};
use crate::file_browser::{BrowseEntry, BrowserSources, DirectoryListing, EntryKind};
use crate::home_surface::Drawer;
use crate::icons::{self, SystemIconRoots};
use crate::settings::{self, HomeFieldSettings, PreviewMode};
use crate::surface::design;
use crate::surface::field_screen::{FieldAction, FieldData, FieldScreen};
use crate::surface::file_browser::FileBrowserData;
use crate::surface::fonts;
use crate::surface::window::{
    HostedWindow, key_pressed, modifiers_of, scroll_pixels, translate_button, translate_key,
};
use crate::wayland_bridge::ShellBridge;

/// The one user event: something arrived off the event loop's own thread (a fetch, a search
/// result) and a redraw should reconsider it. Carries nothing on purpose — see
/// `pixelkit_shell::shell::Wake`'s own doc for the same reasoning; this crate does not reuse that
/// type directly because building one needs a private field only `pixelkit_shell::run_app` can
/// set (see `window.rs`'s module doc for the fuller account of what is and is not reachable there).
#[derive(Debug, Clone, Copy)]
pub struct Wake;

enum DataUpdate {
    Drawers(Vec<(Drawer, String)>),
    /// Every installed application, with the metadata a launcher needs
    /// (`AppIdentity1.ListApplications`). `None` when the registry did not answer, which the
    /// launcher widget reports rather than drawing an empty list.
    Apps(Option<Vec<crate::app_search::AppEntry>>),
    Projects(Vec<ProjectInfo>),
    /// One widget's refreshed content. Content only: by the field's own rule this moves nothing.
    Widget(String, crate::surface::widgets::WidgetSnapshot),
    HomeFieldSettings(HomeFieldSettings),
    /// A drawer directory's treemap and composition, rasterized off-thread.
    Treemap {
        drawer: Drawer,
        bitmap: pixelkit_raster::Bitmap,
        composition: Vec<(u32, f32)>,
    },
    /// Every five minutes: drop the cached treemaps so the next frame asks for fresh ones.
    TreemapsStale,
    /// The persisted document arrived from `HomeField1` (startup, or after an undo).
    Field(FieldDocument),
    /// `HomeField1` is not reachable: the field stays in-session.
    FieldUnavailable,
    SearchResults {
        drawer: Drawer,
        query: String,
        result: Result<Vec<BrowseEntry>, String>,
    },
    BrowserSources(BrowserSources),
    BrowserListing {
        drawer: Drawer,
        show_hidden: bool,
        path: PathBuf,
        result: Result<DirectoryListing, String>,
    },
    FileOpened {
        path: PathBuf,
        result: Result<crate::data::OpenFileResult, String>,
    },
}

/// Which output's document the shell edits. The shell has one Home Surface window today and
/// no way to learn its output's name through winit; one field per output keyed by name is the
/// documented next step, and the key already exists so nothing is migrated then.
const OUTPUT: &str = "primary";

/// How many of the launcher's ranked rows are worth an icon: enough to cover the largest tile
/// plus what a scroll would reveal, and nowhere near the whole inventory.
const LAUNCHER_ICON_LOOKAHEAD: usize = 24;

/// How many *new* icons one frame may resolve. See `ShellApp::ensure_launcher_icons`.
const ICON_BUDGET_PER_FRAME: usize = 6;

/// Run the graphical Home Surface. Blocks until the user closes it.
pub fn run() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    let (data_tx, data_rx) = channel();
    let mut app = ShellApp::new(runtime, data_tx, data_rx, event_loop.create_proxy());
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct ShellApp {
    runtime: tokio::runtime::Handle,
    data_tx: Sender<DataUpdate>,
    data_rx: Receiver<DataUpdate>,
    wake_proxy: EventLoopProxy<Wake>,

    bridge: Option<ShellBridge>,
    home: Option<HostedWindow>,

    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    theme: Theme,

    field_screen: FieldScreen,

    /// The Home Field document: `HomeField1`'s persisted one once it has arrived, with live
    /// state merged in on every refresh. Every op is applied here first (so a drop never waits
    /// on the bus) and forwarded to sessiond, which applies the same deterministic op.
    field: FieldDocument,
    /// `Some` once sessiond answered: ops are forwarded, undo is the service's. `None` while
    /// unknown or unavailable: ops and undo stay in-session.
    field_synced: Option<bool>,
    /// Inverses of the user's field ops, most recent last — Ctrl+Z applies the last one when
    /// `HomeField1` is unavailable.
    undo: Vec<FieldOp>,
    field_settings: HomeFieldSettings,
    /// Whether the one-time `HomeField1.Get` has been sent.
    field_requested: bool,
    apps: Vec<crate::app_search::AppEntry>,
    /// Whether the last `ListApplications` succeeded.
    apps_available: bool,
    projects: Vec<ProjectInfo>,
    /// Previews keyed the way session tiles are (`field_model::session_identifier`).
    previews: HashMap<String, pixelkit_raster::Bitmap>,
    /// When each window's preview was last requested, for the rate policy.
    preview_requested: HashMap<lege_window::WindowId, Instant>,
    /// Each widget's latest content, keyed by `TileKind::System`'s probe.
    widgets: HashMap<String, crate::surface::widgets::WidgetSnapshot>,
    treemaps: HashMap<Drawer, pixelkit_raster::Bitmap>,
    compositions: HashMap<Drawer, Vec<(u32, f32)>>,
    /// Drawers with a scan in flight, so one request per drawer at a time.
    treemap_pending: HashSet<Drawer>,
    /// When the last pointer drop happened, for the experiment's undo-within-5-s measure.
    last_drop_at: Option<Instant>,
    started: Instant,

    drawer_texts: HashMap<Drawer, Vec<String>>,
    search_results: Option<(Drawer, String, Result<Vec<BrowseEntry>, String>)>,
    browser_sources: BrowserSources,
    browser_listing: Option<(Drawer, bool, DirectoryListing)>,
    browser_pending: Option<(Drawer, bool, PathBuf)>,
    browser_error: Option<(Drawer, PathBuf, String)>,
    browser_status: Option<String>,
    icons: HashMap<String, pixelkit_raster::Bitmap>,
    icon_attempted: HashSet<String>,

    exit: bool,
}

impl ShellApp {
    fn new(
        runtime: tokio::runtime::Handle,
        data_tx: Sender<DataUpdate>,
        data_rx: Receiver<DataUpdate>,
        wake_proxy: EventLoopProxy<Wake>,
    ) -> ShellApp {
        ShellApp {
            runtime,
            data_tx,
            data_rx,
            wake_proxy,
            bridge: None,
            home: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            theme: design::theme(),
            field_screen: FieldScreen::default(),
            field: FieldDocument::new(OUTPUT, 16, 8),
            field_synced: None,
            undo: Vec::new(),
            field_settings: HomeFieldSettings::default(),
            field_requested: false,
            apps: Vec::new(),
            apps_available: false,
            projects: Vec::new(),
            previews: HashMap::new(),
            preview_requested: HashMap::new(),
            widgets: HashMap::new(),
            treemaps: HashMap::new(),
            compositions: HashMap::new(),
            treemap_pending: HashSet::new(),
            last_drop_at: None,
            started: Instant::now(),
            drawer_texts: HashMap::new(),
            search_results: None,
            browser_sources: BrowserSources::default(),
            browser_listing: None,
            browser_pending: None,
            browser_error: None,
            browser_status: None,
            icons: HashMap::new(),
            icon_attempted: HashSet::new(),
            exit: false,
        }
    }

    fn spawn_refresh_loop(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let mut ticks: u32 = 0;
            loop {
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(100) {
                    let _ = tx.send(DataUpdate::TreemapsStale);
                }
                let drawers = crate::data::fetch_all(20).await;
                let apps = crate::data::fetch_apps().await;
                let field_settings = fetch_home_field_settings().await;
                let projects = crate::data::fetch_projects().await;
                let browser_sources = crate::data::fetch_browser_sources().await;
                let session_widget = crate::data::fetch_session_widget().await;
                if tx.send(DataUpdate::Drawers(drawers)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::Apps(apps));
                let _ = tx.send(DataUpdate::Projects(projects));
                let _ = tx.send(DataUpdate::HomeFieldSettings(field_settings));
                let _ = tx.send(DataUpdate::BrowserSources(browser_sources));
                let _ = tx.send(DataUpdate::Widget("session".to_owned(), session_widget));
                let _ = proxy.send_event(Wake);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    /// Ask `HomeField1` for the persisted document once the grid size is known.
    fn spawn_field_fetch(&self, cols: u16, rows: u16) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let update = match crate::data::fetch_field(OUTPUT, cols, rows).await {
                Some(doc) => DataUpdate::Field(doc),
                None => DataUpdate::FieldUnavailable,
            };
            let _ = tx.send(update);
            let _ = proxy.send_event(Wake);
        });
    }

    /// Forward one already-applied op to `HomeField1`.
    fn spawn_field_apply(&self, op: FieldOp) {
        self.runtime.spawn(async move {
            crate::data::apply_field_op(OUTPUT, &op).await;
        });
    }

    fn spawn_field_undo(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            if let Some(doc) = crate::data::undo_field(OUTPUT).await {
                let _ = tx.send(DataUpdate::Field(doc));
                let _ = proxy.send_event(Wake);
            }
        });
    }

    fn spawn_search(&self, drawer: Drawer, query: String) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = run_search(drawer, &query).await;
            let _ = tx.send(DataUpdate::SearchResults {
                drawer,
                query,
                result,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    fn spawn_browse(&mut self, drawer: Drawer, path: PathBuf, show_hidden: bool) {
        if self.browser_pending.as_ref().is_some_and(
            |(pending_drawer, pending_hidden, pending_path)| {
                (*pending_drawer, *pending_hidden, pending_path) == (drawer, show_hidden, &path)
            },
        ) {
            return;
        }
        self.browser_pending = Some((drawer, show_hidden, path.clone()));
        self.browser_error = None;
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::data::browse_directory(&path, show_hidden).await;
            let _ = tx.send(DataUpdate::BrowserListing {
                drawer,
                show_hidden,
                path,
                result,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    fn spawn_open_file(&mut self, path: PathBuf) {
        self.browser_status = Some(format!("Opening {}…", path.display()));
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::data::open_file(&path).await;
            let _ = tx.send(DataUpdate::FileOpened { path, result });
            let _ = proxy.send_event(Wake);
        });
    }

    fn drain_updates(&mut self) {
        while let Ok(update) = self.data_rx.try_recv() {
            match update {
                DataUpdate::Drawers(drawers) => {
                    for (drawer, text) in drawers {
                        self.drawer_texts
                            .insert(drawer, text.lines().map(str::to_owned).collect());
                    }
                }
                DataUpdate::Apps(apps) => match apps {
                    Some(apps) => {
                        self.apps = apps;
                        self.apps_available = true;
                    }
                    // Keep the last good inventory rather than blanking the launcher on one
                    // failed poll; the flag is what the widget reports.
                    None => self.apps_available = false,
                },
                DataUpdate::Projects(projects) => self.projects = projects,
                DataUpdate::Widget(id, snapshot) => {
                    self.widgets.insert(id, snapshot);
                }
                DataUpdate::HomeFieldSettings(settings) => {
                    self.field_settings = settings;
                    self.field_screen.drag_cfg.throw_enabled =
                        settings.throw && !settings.reduced_motion;
                }
                DataUpdate::Treemap {
                    drawer,
                    bitmap,
                    composition,
                } => {
                    self.treemap_pending.remove(&drawer);
                    self.treemaps.insert(drawer, bitmap);
                    self.compositions.insert(drawer, composition);
                }
                DataUpdate::TreemapsStale => self.treemaps.clear(),
                DataUpdate::Field(doc) => {
                    self.field = doc;
                    self.field_synced = Some(true);
                    self.undo.clear();
                }
                DataUpdate::FieldUnavailable => {
                    if self.field_synced.is_none() {
                        self.field_synced = Some(false);
                    }
                }
                DataUpdate::SearchResults {
                    drawer,
                    query,
                    result,
                } => {
                    self.search_results = Some((drawer, query, result));
                }
                DataUpdate::BrowserSources(sources) => self.browser_sources = sources,
                DataUpdate::BrowserListing {
                    drawer,
                    show_hidden,
                    path,
                    result,
                } => {
                    let is_current = self.browser_pending.as_ref().is_some_and(
                        |(pending_drawer, pending_hidden, pending_path)| {
                            (*pending_drawer, *pending_hidden, pending_path)
                                == (drawer, show_hidden, &path)
                        },
                    );
                    if !is_current {
                        continue;
                    }
                    self.browser_pending = None;
                    match result {
                        Ok(listing) => {
                            self.browser_listing = Some((drawer, show_hidden, listing));
                            self.browser_error = None;
                        }
                        Err(error) => {
                            self.browser_error = Some((drawer, path, error));
                        }
                    }
                }
                DataUpdate::FileOpened { path, result } => {
                    self.browser_status = Some(match result {
                        Ok(opened) => format!(
                            "Opened {} with {} · instance {}",
                            path.display(),
                            opened.app_id,
                            opened.instance
                        ),
                        Err(error) => error,
                    });
                }
            }
        }
    }

    /// Resolve one application's icon, once. `hints` is what the registry said about it (the
    /// `Icon=` value and the real `.desktop` file id); without them the lookup can only guess the
    /// file name back out of the canonical id, which for an imported application usually fails.
    fn ensure_icon(&mut self, key: &str) {
        if key.is_empty() || self.icons.contains_key(key) || self.icon_attempted.contains(key) {
            return;
        }
        self.icon_attempted.insert(key.to_owned());
        let entry = self.apps.iter().find(|entry| entry.id.as_str() == key);
        let desktop_file_ids: Vec<String> = entry
            .map(|entry| entry.desktop_file_id.clone())
            .filter(|id| !id.is_empty())
            .into_iter()
            .collect();
        let hints = icons::IconHints {
            icon: entry
                .map(|entry| entry.icon.as_str())
                .filter(|icon| !icon.is_empty()),
            desktop_file_ids: &desktop_file_ids,
        };
        if let Some(bitmap) = icons::load_app_icon_hinted(key, hints, &SystemIconRoots) {
            self.icons.insert(key.to_owned(), bitmap);
        }
    }

    /// Bring the field document up to date with the window's size and the live state before a
    /// frame: grid size from the logical size, then `field_model::merge`.
    fn refresh_field(
        &mut self,
        logical_width: i32,
        logical_height: i32,
        toplevels: &[lege_window::ShellToplevel],
    ) {
        let (cols, rows) =
            field_model::grid_for(logical_width, logical_height, self.field_settings.tile_size);
        if self.field_synced.is_none() && !self.field_requested {
            self.field_requested = true;
            self.spawn_field_fetch(cols, rows);
        }
        if (cols, rows) != (self.field.cols, self.field.rows) {
            let op = FieldOp::SetGrid { cols, rows };
            match lege_field::apply(&self.field, &op) {
                Ok((next, _)) => {
                    self.field = next;
                    if self.field_synced == Some(true) {
                        self.spawn_field_apply(op);
                    }
                }
                Err(error) => tracing::warn!(%error, "resizing the field grid"),
            }
        }
        let live = LiveState {
            toplevels,
            projects: &self.projects,
        };
        let merged = field_model::merge(&mut self.field, &live);
        if !merged.is_empty() && self.field_synced == Some(true) {
            self.spawn_field_apply(FieldOp::Batch { ops: merged });
        }
        field_model::refresh_titles(&mut self.field, &live);
        let keys: Vec<String> = toplevels
            .iter()
            .filter_map(|t| t.canonical_app_id.as_ref().map(ToString::to_string))
            .collect();
        for key in keys {
            self.ensure_icon(&key);
        }
        self.ensure_launcher_icons();
    }

    /// Resolve icons for the launcher's currently visible rows, a few per frame.
    ///
    /// The list is every installed application — hundreds of rows on a stock system — and each
    /// miss is a directory walk plus, for an SVG, a rasterization. Doing them all at once would
    /// stall the frame that first shows the widget, so this takes a small bite each time and asks
    /// for another frame while there is more to do: the list fills in over a second or two
    /// instead of arriving late all at once.
    fn ensure_launcher_icons(&mut self) {
        let query = self.field_screen.search.text().to_owned();
        let wanted: Vec<String> = crate::app_search::search(&self.apps, &query)
            .into_iter()
            .take(LAUNCHER_ICON_LOOKAHEAD)
            .map(|row| row.entry.id.as_str().to_owned())
            .filter(|key| !self.icons.contains_key(key) && !self.icon_attempted.contains(key))
            .take(ICON_BUDGET_PER_FRAME)
            .collect();
        if wanted.is_empty() {
            return;
        }
        for key in wanted {
            self.ensure_icon(&key);
        }
        if let Some(home) = &self.home {
            home.request_redraw();
        }
    }

    /// The preview policy (`docs/homefield.md`, "Resource and security implications"): `Off`
    /// asks for nothing; `Static` asks once per window, and again when it is parked; `Live`
    /// asks at the preview rate for every session tile. Arrived previews are filed under the
    /// tile's own identifier. The compositor's own floor of four frames a second per window
    /// holds whatever this asks.
    fn refresh_previews(&mut self, toplevels: &[lege_window::ShellToplevel]) {
        let Some(bridge) = &mut self.bridge else {
            return;
        };
        if self.field_settings.previews == PreviewMode::Off || !bridge.previews_available() {
            return;
        }
        let now = Instant::now();
        let live_interval =
            Duration::from_millis(1000 / u64::from(self.field_settings.preview_rate.max(1)));
        for toplevel in toplevels {
            let Some(identifier) = bridge.identifier_of(toplevel.window_id).map(str::to_owned)
            else {
                continue;
            };
            let key = field_model::session_identifier(toplevel, None);
            let due = match self.field_settings.previews {
                PreviewMode::Off => false,
                PreviewMode::Static => {
                    !self.previews.contains_key(&key)
                        || (toplevel.shelved
                            && self
                                .preview_requested
                                .get(&toplevel.window_id)
                                .is_none_or(|at| now.duration_since(*at) > Duration::from_secs(2)))
                }
                PreviewMode::Live => self
                    .preview_requested
                    .get(&toplevel.window_id)
                    .is_none_or(|at| now.duration_since(*at) >= live_interval),
            };
            if !due {
                continue;
            }
            // A parked window's snapshot is taken once per parking, not every two seconds: the
            // timestamp moves only when the request went out.
            if toplevel.shelved
                && self.field_settings.previews == PreviewMode::Static
                && self.previews.contains_key(&key)
                && self.preview_requested.contains_key(&toplevel.window_id)
            {
                continue;
            }
            match bridge.request_preview(&identifier) {
                Ok(()) => {
                    self.preview_requested.insert(toplevel.window_id, now);
                }
                Err(error) => tracing::debug!(%error, "preview request skipped"),
            }
        }
        for (identifier, bitmap) in bridge.take_previews() {
            if let Some(toplevel) = toplevels
                .iter()
                .find(|t| bridge.identifier_of(t.window_id) == Some(identifier.as_str()))
            {
                self.previews
                    .insert(field_model::session_identifier(toplevel, None), bitmap);
            }
        }
        let live: std::collections::HashSet<String> = toplevels
            .iter()
            .map(|t| field_model::session_identifier(t, None))
            .collect();
        self.previews.retain(|key, _| live.contains(key));
        self.preview_requested
            .retain(|id, _| toplevels.iter().any(|t| t.window_id == *id));
    }

    fn redraw_home(&mut self) {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        self.refresh_previews(&toplevels);
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let Some(home) = &self.home else { return };
        let scale = home.scale;
        let logical = home
            .window
            .inner_size()
            .to_logical::<f64>(home.window.scale_factor());
        self.refresh_field(logical.width as i32, logical.height as i32, &toplevels);
        let network_control_available = network_control_app(&self.apps).is_some();
        let clock_label = local_clock_label(std::time::SystemTime::now());

        let Some(home) = &mut self.home else { return };
        if !home.prepare() {
            return;
        }
        let (width, height) = (home.buffer.width as i32, home.buffer.height as i32);
        let active_search = self.search_results.as_ref().filter(|(drawer, query, _)| {
            Some(*drawer) == self.field_screen.expanded
                && query == self.field_screen.drawer_view.search.text()
        });
        let search_query = active_search.map(|(_, query, _)| query.as_str());
        let search_entries = active_search
            .and_then(|(_, _, result)| result.as_ref().ok())
            .map(Vec::as_slice);
        let search_error = active_search
            .and_then(|(_, _, result)| result.as_ref().err())
            .map(String::as_str);
        let browser_data = FileBrowserData {
            sources: &self.browser_sources,
            listing: self
                .browser_listing
                .as_ref()
                .map(|(drawer, hidden, listing)| (*drawer, *hidden, listing)),
            pending: self
                .browser_pending
                .as_ref()
                .map(|(drawer, hidden, path)| (*drawer, *hidden, path.as_path())),
            error: self
                .browser_error
                .as_ref()
                .map(|(drawer, path, error)| (*drawer, path.as_path(), error.as_str())),
            status: self.browser_status.as_deref(),
            search_query,
            search_entries,
            search_error,
        };
        let data = FieldData {
            field: &self.field,
            toplevels: &toplevels,
            icons: &self.icons,
            previews: &self.previews,
            drawer_texts: &self.drawer_texts,
            treemaps: &self.treemaps,
            compositions: &self.compositions,
            browser: &browser_data,
            connected: self.bridge.is_some(),
            home_revealed,
            network_control_available,
            apps: &self.apps,
            apps_available: self.apps_available,
            widgets: &self.widgets,
            clock_label: &clock_label,
            reduced_motion: self.field_settings.reduced_motion,
            now_ms: self.started.elapsed().as_millis() as u64,
        };
        let actions = {
            let painter = Painter::new(&mut home.buffer);
            let mut ui = Ui::new(
                painter,
                &mut self.text,
                &mut home.input,
                self.theme,
                scale,
                &mut self.kernel,
            );
            let area = Rect::new(0, 0, width, height);
            self.field_screen.draw(&mut ui, area, &data)
        };
        home.input.end_frame();
        home.present();

        for action in actions {
            self.perform(action);
        }
    }

    fn perform(&mut self, action: FieldAction) {
        match action {
            FieldAction::RunSearch { drawer, query } => self.spawn_search(drawer, query),
            FieldAction::Launch(app_id) => self.spawn_launch(app_id),
            FieldAction::Activate(window_id) => {
                self.bridge_call("activate", |bridge| bridge.activate(window_id));
            }
            FieldAction::Park(window_id) => {
                self.bridge_call("shelve", |bridge| bridge.shelve(window_id));
            }
            FieldAction::Restore(window_id) => {
                self.bridge_call("unshelve", |bridge| bridge.unshelve(window_id));
            }
            FieldAction::BrowseDirectory {
                drawer,
                path,
                show_hidden,
            } => {
                self.search_results = None;
                self.field_screen.drawer_view.search = pixelkit_ui::TextFieldState::new();
                self.field_screen.drawer_view.search_focused = false;
                self.spawn_browse(drawer, path, show_hidden);
            }
            FieldAction::OpenFile(path) => self.spawn_open_file(path),
            FieldAction::RunAction(id) => self.perform_declared(id),
            FieldAction::RunWindowAction { id, window } => self.perform_on_window(id, window),
            FieldAction::RevealHome => {
                self.bridge_call("reveal_home", |bridge| {
                    bridge.reveal_home(RevealHome::Toggle)
                });
            }
            FieldAction::Apply(op) => match lege_field::apply(&self.field, &op) {
                Ok((next, inverse)) => {
                    if let FieldOp::CreateProject { project, .. } = &op
                        && let lege_field::TileKind::Project { project_id } = &project.kind
                    {
                        self.spawn_project_create(project_id.clone(), project.title.clone());
                    }
                    self.field = next;
                    self.field_screen
                        .confirm(&op, self.started.elapsed().as_millis() as u64);
                    if self.field_synced == Some(true) {
                        self.spawn_field_apply(op);
                    } else {
                        self.undo.push(inverse);
                        self.undo.truncate(64);
                    }
                }
                Err(error) => tracing::warn!(%error, ?op, "field op refused"),
            },
            FieldAction::NeedTreemap {
                drawer,
                width,
                height,
            } => self.spawn_treemap(drawer, width, height),
            FieldAction::DropMetrics(metrics) => {
                self.last_drop_at = Some(Instant::now());
                if self.field_settings.throw && !self.field_settings.reduced_motion {
                    log_experiment(&format!(
                        "{{\"event\":\"drop\",\"thrown\":{},\"lifted_for_ms\":{},\"semantic\":{},\"relift\":{},\"op\":{:?}}}",
                        metrics.thrown,
                        metrics.lifted_for_ms,
                        metrics.semantic,
                        metrics.relift,
                        metrics.op
                    ));
                }
            }
            FieldAction::Undo => {
                if self.field_settings.throw
                    && let Some(at) = self.last_drop_at
                    && at.elapsed() < Duration::from_secs(5)
                {
                    log_experiment(&format!(
                        "{{\"event\":\"undo-after-drop\",\"since_drop_ms\":{}}}",
                        at.elapsed().as_millis()
                    ));
                }
                if self.field_synced == Some(true) {
                    self.spawn_field_undo();
                } else if let Some(inverse) = self.undo.pop() {
                    match lege_field::apply(&self.field, &inverse) {
                        Ok((next, _)) => self.field = next,
                        Err(error) => tracing::warn!(%error, "undo refused"),
                    }
                }
            }
        }
    }

    fn bridge_call(&self, what: &str, call: impl FnOnce(&ShellBridge) -> anyhow::Result<()>) {
        if let Some(bridge) = &self.bridge
            && let Err(error) = call(bridge)
        {
            tracing::warn!(%error, "{what} request failed");
        }
    }

    /// A project grouped on the field becomes a real `Projects1` project, rooted under
    /// `~/Projects/<id>` — so what the field shows is a session object, not decoration.
    fn spawn_project_create(&self, project_id: String, name: String) {
        self.runtime.spawn(async move {
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = ProjectsProxy::new(&connection).await else {
                return;
            };
            let root = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join("Projects")
                .join(&project_id);
            if let Err(error) = proxy
                .create(&project_id, &name, &root.to_string_lossy())
                .await
            {
                tracing::warn!(%error, %project_id, "Projects1.Create failed");
            }
        });
    }

    /// Scan the drawer's first directory (bounded: `lege_treemap::ScanLimits`), lay it out and
    /// rasterize it at the tile's size, off the event loop. One in flight per drawer.
    fn spawn_treemap(&mut self, drawer: Drawer, width: u32, height: u32) {
        if !self.treemap_pending.insert(drawer) {
            return;
        }
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        let roots = search_roots(drawer);
        self.runtime.spawn_blocking(move || {
            let (bitmap, composition) = rasterize_drawer(&roots, width, height);
            let _ = tx.send(DataUpdate::Treemap {
                drawer,
                bitmap,
                composition,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    fn spawn_launch(&self, app_id: AppId) {
        self.spawn_launch_with_args(app_id, Vec::new());
    }

    /// Perform one declared action (`lege_types::action::MANIFEST`).
    ///
    /// This is the single place an action id becomes a thing that happens, so every surface —
    /// a widget region, the information row, a future menu — reaches a capability the same way
    /// and none of them needs its own idea of what an action means. An id with no route yet is
    /// logged rather than silently dropped: a region wired to nothing should be noisy.
    fn perform_declared(&mut self, id: &'static str) {
        match id {
            "network.open-control" => self.spawn_network_control(),
            // Two view-state actions: they change what the field's own search bar is doing, so
            // there is no service behind them — but they are still declared, still reachable by
            // pointer, and still routed through this one place like everything else.
            "launcher.search" => self.field_screen.search_focused = true,
            "launcher.show-all" => {
                self.field_screen.search.set_text("");
                self.field_screen.search_focused = false;
            }
            "session.logout" | "session.reboot" | "session.poweroff" | "session.suspend"
            | "session.lock" => self.spawn_lifecycle(id),
            "window.show-all" => {
                self.bridge_call("show_all_windows", ShellBridge::show_all_windows);
            }
            "window.escape-fullscreen" => {
                self.bridge_call("escape_fullscreen_or_modal", |bridge| {
                    bridge.escape_fullscreen_or_modal(None)
                });
            }
            other => tracing::warn!(action = %other, "no route for this action yet"),
        }
    }

    /// Perform one declared action against a named window — the context menu's rows over a
    /// session tile (`crate::surface::context_menu`).
    ///
    /// Separate from [`Self::perform_declared`] because these need a `WindowId` and that one has
    /// nowhere to get it: an action reachable both with and without a target is routed by whether
    /// the surface had one, not by two different ids. `window.close` is the reason this exists at
    /// all — it was declared, and reachable from nothing.
    fn perform_on_window(&self, id: &'static str, window: lege_window::WindowId) {
        match id {
            // The confirmation happened in the menu (`ActionDecl::confirm`); by the time it
            // reaches here the user has pressed twice and this is simply the request.
            "window.close" => self.bridge_call("close", |bridge| bridge.close(window)),
            "window.park" => self.bridge_call("shelve", |bridge| bridge.shelve(window)),
            "window.escape-fullscreen" => {
                self.bridge_call("escape_fullscreen_or_modal", |bridge| {
                    bridge.escape_fullscreen_or_modal(Some(window))
                });
            }
            "window.show-all" => {
                self.bridge_call("show_all_windows", ShellBridge::show_all_windows);
            }
            other => tracing::warn!(action = %other, "no window route for this action yet"),
        }
    }

    /// Call one `Lifecycle1` method off the event loop, reporting a refusal rather than letting
    /// it vanish — "shut down did nothing and said nothing" is the worst outcome here.
    fn spawn_lifecycle(&self, id: &'static str) {
        self.runtime.spawn(async move {
            let connection = match crate::data::session().await {
                Ok(connection) => connection,
                Err(message) => {
                    tracing::error!(%message, action = %id, "no session bus");
                    return;
                }
            };
            let proxy = match LifecycleProxy::new(&connection).await {
                Ok(proxy) => proxy,
                Err(error) => {
                    tracing::error!(%error, action = %id, "Lifecycle1 unavailable");
                    return;
                }
            };
            let result = match id {
                "session.logout" => proxy.logout().await,
                "session.reboot" => proxy.reboot().await,
                "session.poweroff" => proxy.power_off().await,
                "session.suspend" => proxy.suspend().await,
                "session.lock" => proxy.lock().await,
                other => {
                    tracing::warn!(action = %other, "not a Lifecycle1 action");
                    return;
                }
            };
            if let Err(error) = result {
                tracing::error!(%error, action = %id, "Lifecycle1 refused");
            }
        });
    }

    fn spawn_network_control(&self) {
        let Some(app_id) = network_control_app(&self.apps) else {
            tracing::warn!("nm-tray2 is not installed in the application registry");
            return;
        };
        self.spawn_launch_with_args(app_id, vec!["--popup".to_owned()]);
    }

    fn spawn_launch_with_args(&self, app_id: AppId, arguments: Vec<String>) {
        self.runtime.spawn(async move {
            let connection = match crate::data::session().await {
                Ok(connection) => connection,
                Err(message) => {
                    tracing::warn!(%message, "launch: no session bus");
                    return;
                }
            };
            match LauncherProxy::new(&connection).await {
                Ok(proxy) => {
                    if let Err(error) = proxy.launch(&app_id.to_string(), arguments, "").await {
                        tracing::warn!(%error, %app_id, "Launcher1.Launch failed");
                    }
                }
                Err(error) => tracing::warn!(%error, "Launcher1 unavailable"),
            }
        });
    }
}

fn network_control_app(apps: &[crate::app_search::AppEntry]) -> Option<AppId> {
    apps.iter()
        .find(|app| app.id.as_str() == "org.legeos.NetworkTray")
        .or_else(|| {
            apps.iter().find(|app| {
                let id = app.id.as_str().to_ascii_lowercase();
                id.ends_with("nm-tray.desktop") && !id.contains("autostart")
            })
        })
        .map(|app| app.id.clone())
}

#[allow(unsafe_code)]
fn local_clock_label(now: std::time::SystemTime) -> String {
    let seconds = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs()) as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid for this call, and `local` is read only after
    // `localtime_r` reports success. `localtime_r` does not retain either pointer.
    let result = unsafe { libc::localtime_r(&raw const seconds, local.as_mut_ptr()) };
    if result.is_null() {
        return "TIME UNAVAILABLE".to_owned();
    }
    // SAFETY: the successful `localtime_r` call initialized the whole `libc::tm` value.
    let local = unsafe { local.assume_init() };
    format_clock_fields(
        local.tm_wday,
        local.tm_mday,
        local.tm_mon,
        local.tm_hour,
        local.tm_min,
    )
}

fn format_clock_fields(weekday: i32, day: i32, month: i32, hour: i32, minute: i32) -> String {
    const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let weekday = usize::try_from(weekday)
        .ok()
        .and_then(|index| WEEKDAYS.get(index))
        .copied()
        .unwrap_or("---");
    let month = usize::try_from(month)
        .ok()
        .and_then(|index| MONTHS.get(index))
        .copied()
        .unwrap_or("---");
    format!("{weekday} {day:02} {month} {hour:02}:{minute:02}")
}

/// The cushion treemap of the first directory in `roots` at `width`×`height`, plus its
/// composition band. An unreadable or empty directory yields a flat, dim bitmap and no band.
fn rasterize_drawer(
    roots: &[String],
    width: u32,
    height: u32,
) -> (pixelkit_raster::Bitmap, Vec<(u32, f32)>) {
    use lege_treemap::{ColorSettings, CushionConfig, LayoutConfig, ScanLimits};
    let (width, height) = (width.max(1), height.max(1));
    let Some(root) = roots.first() else {
        return (pixelkit_raster::Bitmap::new(width, height), Vec::new());
    };
    let entries = lege_treemap::scan(std::path::Path::new(root), ScanLimits::default());
    let tree = lege_treemap::build_tree(&entries);
    let colours = ColorSettings::default();
    let layout = lege_treemap::compute_layout_in_rect(
        &tree,
        tree.root,
        0.0,
        0.0,
        width as f32,
        height as f32,
        &LayoutConfig::default(),
    );
    let pixels = lege_treemap::rasterize_argb(
        width,
        height,
        &layout.rects,
        &tree,
        &CushionConfig::default(),
        &colours,
    );
    let weights = &tree.category_weights[tree.root.index()];
    let total: u64 = weights.iter().sum();
    let composition = if total == 0 {
        Vec::new()
    } else {
        lege_treemap::colors::top_categories(weights, lege_treemap::CATEGORY_COUNT)
            .into_iter()
            .map(|(category, weight)| {
                (
                    lege_treemap::colors::category_color(category).to_rgb_u32(),
                    weight as f32 / total as f32,
                )
            })
            .collect()
    };
    (
        pixelkit_raster::Bitmap {
            width,
            height,
            pixels,
        },
        composition,
    )
}

/// Append one JSON line to the throw experiment's log, `$XDG_STATE_HOME/lege-shell/throw-spike.jsonl`
/// (`docs/research/throw-spike.md`). Only called while `shell.home-field.throw` is `full`; never
/// fatal.
fn log_experiment(line: &str) {
    use std::io::Write as _;
    let Some(state_home) = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })
    else {
        return;
    };
    let dir = state_home.join("lege-shell");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("throw-spike.jsonl"))
    else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let _ = writeln!(file, "{{\"t\":{stamp},{}", &line[1..]);
}

async fn fetch_home_field_settings() -> HomeFieldSettings {
    match crate::data::session().await {
        Ok(connection) => match SettingsProxy::new(&connection).await {
            Ok(proxy) => settings::home_field(&proxy).await,
            Err(_) => HomeFieldSettings::default(),
        },
        Err(_) => HomeFieldSettings::default(),
    }
}

#[derive(serde::Deserialize)]
struct SearchEntry {
    path: std::path::PathBuf,
    is_dir: bool,
    size_bytes: u64,
    mtime_ms: i64,
    #[allow(dead_code)]
    mime: Option<String>,
}

/// `Files.Search1.Search`'s roots for `drawer` — the same directories `standard_paths::render`
/// already resolves for its text form, so a search never looks somewhere the drawer itself
/// disagrees about.
fn search_roots(drawer: Drawer) -> Vec<String> {
    let Some(home) = standard_paths::home_dir() else {
        return Vec::new();
    };
    let user_dirs = standard_paths::read_user_dirs(&home)
        .map(|text| standard_paths::parse_user_dirs(&text, &home))
        .unwrap_or_default();
    let standard = match drawer {
        Drawer::Downloads => StandardDrawer::Downloads,
        Drawer::Documents => StandardDrawer::Documents,
        _ => StandardDrawer::Media,
    };
    standard_paths::directories_for(standard, &user_dirs, &home)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect()
}

async fn run_search(drawer: Drawer, query: &str) -> Result<Vec<BrowseEntry>, String> {
    let connection = match crate::data::session().await {
        Ok(connection) => connection,
        Err(message) => return Err(format!("Search — {message}")),
    };
    let proxy = match FilesSearchProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => return Err(format!("Search — service unavailable: {error}")),
    };
    let roots = search_roots(drawer);
    match proxy.search(query, roots, 50).await {
        Ok(json) => parse_hits(&json),
        Err(error) => Err(format!("Search — service unavailable: {error}")),
    }
}

/// Parse `Files.Search1.Search` into the same actionable rows as directory browsing.
fn parse_hits(json: &str) -> Result<Vec<BrowseEntry>, String> {
    let hits: Vec<(SearchEntry, String)> = serde_json::from_str(json)
        .map_err(|error| format!("Search — malformed response: {error}"))?;
    Ok(hits
        .into_iter()
        .map(|(entry, _rank)| {
            let name = entry.path.file_name().map_or_else(
                || entry.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            BrowseEntry {
                hidden: name.starts_with('.'),
                path: entry.path,
                name,
                kind: if entry.is_dir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                is_symlink: false,
                size_bytes: entry.size_bytes,
                mtime_ms: entry.mtime_ms,
            }
        })
        .collect())
}

impl ApplicationHandler<Wake> for ShellApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.home.is_some() {
            return;
        }

        // Wayland deliberately has no "primary" output, so `primary_monitor()` is commonly None
        // even though the compositor has already advertised a real output. Use the first monitor
        // as the fallback and request its logical size up front. Otherwise winit's 800×600 default
        // can seed and persist a tiny Home Field before the compositor's full-output configure
        // arrives one round trip later.
        let output_size = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .map_or_else(
                || LogicalSize::new(800.0, 600.0),
                |monitor| monitor.size().to_logical::<f64>(monitor.scale_factor()),
            );
        let home_window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Home Surface")
                .with_inner_size(output_size)
                .with_fullscreen(Some(Fullscreen::Borderless(None))),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::error!(%error, "creating the Home Surface window");
                self.exit = true;
                event_loop.exit();
                return;
            }
        };

        match ShellBridge::connect(&home_window) {
            Ok(bridge) => {
                tracing::info!("connected to the compositor's trusted-shell lege_shell_v1 socket");
                match bridge.set_layer(&home_window, lege_window::Layer::HomeSurface) {
                    Ok(()) => tracing::info!("Home Surface declared onto Layer::HomeSurface"),
                    Err(error) => tracing::warn!(%error, "declaring the Home Surface layer"),
                }
                self.bridge = Some(bridge);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "lege_shell_v1 unavailable; the Window Shelf will show no live windows until \
                     this process is connected to the compositor's trusted-shell socket"
                );
            }
        }

        self.home = match HostedWindow::new(home_window) {
            Ok(hosted) => Some(hosted),
            Err(error) => {
                tracing::error!(%error, "creating the Home Surface presenter");
                None
            }
        };
        self.spawn_refresh_loop();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: Wake) {
        self.drain_updates();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WinitWindowId,
        event: WindowEvent,
    ) {
        let is_home = self
            .home
            .as_ref()
            .is_some_and(|home| home.window.id() == window_id);
        if !is_home {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                self.exit = true;
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw_home();
                return;
            }
            _ => {}
        }

        let Some(hosted) = &mut self.home else {
            return;
        };
        match event {
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                    hosted.scale = pixelkit_shell::Scale::new(scale_factor);
                }
                hosted.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if !key_pressed(event.state) {
                    return;
                }
                if let Some(key) = translate_key(&event.logical_key, event.text.as_deref()) {
                    hosted.input.key(key);
                    hosted.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                hosted.input.modifiers_changed(modifiers_of(modifiers));
            }
            WindowEvent::CursorMoved { position, .. } => {
                hosted
                    .input
                    .cursor_moved(position.x as f32, position.y as f32);
                hosted.request_redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                hosted.input.cursor_left();
                hosted.request_redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = translate_button(button) {
                    hosted.input.mouse(button, state == ElementState::Pressed);
                    hosted.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = scroll_pixels(delta);
                hosted.input.scrolled(dx, dy);
                hosted.request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(bridge) = &mut self.bridge
            && let Err(error) = bridge.pump()
        {
            tracing::warn!(%error, "pumping the trusted-shell connection; treating it as gone");
            self.bridge = None;
        }
        self.drain_updates();
        if self.exit {
            event_loop.exit();
            return;
        }
        if self
            .bridge
            .as_ref()
            .is_some_and(ShellBridge::has_previews_ready)
            && let Some(home) = &self.home
        {
            home.request_redraw();
        }
        // Event-driven at rest; a steady frame cadence only while a tile is lifted, so the
        // ghost follows the pointer and a throw's velocity is sampled evenly.
        if self.field_screen.animating() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            if let Some(home) = &self.home {
                home.request_redraw();
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_entry(id: &str) -> crate::app_search::AppEntry {
        crate::app_search::AppEntry {
            id: id.parse().unwrap(),
            name: id.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
        }
    }

    #[test]
    fn network_control_prefers_the_native_id_and_falls_back_to_the_imported_desktop() {
        let imported = app_entry("desktop-import.nm-tray.desktop");
        let native = app_entry("org.legeos.NetworkTray");
        let autostart = app_entry("desktop-import.nm-tray-autostart.desktop");
        assert_eq!(
            network_control_app(&[imported.clone(), native.clone()]),
            Some(native.id)
        );
        assert_eq!(
            network_control_app(&[autostart, imported.clone()]),
            Some(imported.id)
        );
        assert_eq!(network_control_app(&[]), None);
    }

    #[test]
    fn information_row_clock_has_stable_local_date_and_time_formatting() {
        assert_eq!(format_clock_fields(1, 24, 7, 8, 5), "MON 24 AUG 08:05");
        assert_eq!(format_clock_fields(99, 1, 99, 0, 0), "--- 01 --- 00:00");
    }
}
