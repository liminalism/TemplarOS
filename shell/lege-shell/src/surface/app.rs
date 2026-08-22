//! Wiring: one `winit::application::ApplicationHandler` hosting the Home Surface and the dock,
//! the trusted-shell protocol connection both declare their layer through, and the background
//! D-Bus refresh loop that keeps their content live. See `window.rs`'s module doc for why this
//! crate drives winit itself rather than `pixelkit_shell::run_app`.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pixelkit_raster::{Painter, RasterKernel, Rect};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use lege_client::proxies::{FilesSearchProxy, LauncherProxy, SettingsProxy};
use lege_field::{FieldDocument, FieldOp};
use lege_types::AppId;
use lege_window::{DockGrouping, RevealHome, geometry::Edge};

use crate::dock;
use crate::drawers::standard_paths::{self, StandardDrawer};
use crate::field_model::{self, LiveState};
use crate::home_surface::Drawer;
use crate::icons::{self, SystemIconRoots};
use crate::settings::{self, HomeFieldSettings};
use crate::surface::dock_screen::{DockAction, DockData, DockScreen};
use crate::surface::field_screen::{FieldAction, FieldData, FieldScreen};
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
    Grouping(DockGrouping),
    Apps(Vec<AppId>),
    HomeFieldSettings(HomeFieldSettings),
    /// The persisted document arrived from `HomeField1` (startup, or after an undo).
    Field(FieldDocument),
    /// `HomeField1` is not reachable: the field stays in-session.
    FieldUnavailable,
    SearchResults {
        drawer: Drawer,
        lines: Vec<String>,
    },
}

/// Which output's document the shell edits. The shell has one Home Surface window today and
/// no way to learn its output's name through winit; one field per output keyed by name is the
/// documented next step, and the key already exists so nothing is migrated then.
const OUTPUT: &str = "primary";

/// Run the graphical surface: the Home Surface and the dock, both real windows. Blocks until the
/// user closes one of them.
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
    dock: Option<HostedWindow>,

    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    theme: Theme,

    field_screen: FieldScreen,
    dock_screen: DockScreen,

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
    apps: Vec<AppId>,
    previews: HashMap<String, pixelkit_raster::Bitmap>,
    started: Instant,

    drawer_texts: HashMap<Drawer, Vec<String>>,
    search_results: Option<(Drawer, Vec<String>)>,
    dock_grouping: DockGrouping,
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
            dock: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            theme: Theme::default(),
            field_screen: FieldScreen::default(),
            dock_screen: DockScreen,
            field: FieldDocument::new(OUTPUT, 16, 8),
            field_synced: None,
            undo: Vec::new(),
            field_settings: HomeFieldSettings::default(),
            field_requested: false,
            apps: Vec::new(),
            previews: HashMap::new(),
            started: Instant::now(),
            drawer_texts: HashMap::new(),
            search_results: None,
            dock_grouping: DockGrouping::ByApplication,
            icons: HashMap::new(),
            icon_attempted: HashSet::new(),
            exit: false,
        }
    }

    fn spawn_refresh_loop(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            loop {
                let drawers = crate::data::fetch_all(20).await;
                let grouping = fetch_grouping().await;
                let apps = crate::data::fetch_apps().await;
                let field_settings = fetch_home_field_settings().await;
                if tx.send(DataUpdate::Drawers(drawers)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::Grouping(grouping));
                let _ = tx.send(DataUpdate::Apps(apps));
                let _ = tx.send(DataUpdate::HomeFieldSettings(field_settings));
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
            let lines = run_search(drawer, &query).await;
            let _ = tx.send(DataUpdate::SearchResults { drawer, lines });
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
                DataUpdate::Grouping(grouping) => self.dock_grouping = grouping,
                DataUpdate::Apps(apps) => self.apps = apps,
                DataUpdate::HomeFieldSettings(settings) => {
                    self.field_settings = settings;
                    self.field_screen.drag_cfg.throw_enabled = settings.throw;
                }
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
                DataUpdate::SearchResults { drawer, lines } => {
                    self.search_results = Some((drawer, lines));
                }
            }
        }
    }

    fn ensure_icon(&mut self, key: &str) {
        if key.is_empty() || self.icons.contains_key(key) || self.icon_attempted.contains(key) {
            return;
        }
        self.icon_attempted.insert(key.to_owned());
        if let Some(bitmap) = icons::load_app_icon(key, &SystemIconRoots) {
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
            apps: &self.apps,
            toplevels,
        };
        let merged = field_model::merge(&mut self.field, &live);
        if !merged.is_empty() && self.field_synced == Some(true) {
            self.spawn_field_apply(FieldOp::Batch { ops: merged });
        }
        field_model::refresh_titles(&mut self.field, &live);
        let keys: Vec<String> = self
            .apps
            .iter()
            .map(ToString::to_string)
            .chain(
                toplevels
                    .iter()
                    .filter_map(|t| t.canonical_app_id.as_ref().map(ToString::to_string)),
            )
            .collect();
        for key in keys {
            self.ensure_icon(&key);
        }
    }

    fn redraw_home(&mut self) {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let Some(home) = &self.home else { return };
        let scale = home.scale;
        let logical = home
            .window
            .inner_size()
            .to_logical::<f64>(home.window.scale_factor());
        self.refresh_field(logical.width as i32, logical.height as i32, &toplevels);

        let Some(home) = &mut self.home else { return };
        if !home.prepare() {
            return;
        }
        let (width, height) = (home.buffer.width as i32, home.buffer.height as i32);
        let search_results = self
            .search_results
            .as_ref()
            .filter(|(drawer, _)| Some(*drawer) == self.field_screen.expanded)
            .map(|(_, lines)| lines.as_slice());
        let data = FieldData {
            field: &self.field,
            toplevels: &toplevels,
            icons: &self.icons,
            previews: &self.previews,
            drawer_texts: &self.drawer_texts,
            search_results,
            connected: self.bridge.is_some(),
            home_revealed,
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
            FieldAction::RevealHome => {
                self.bridge_call("reveal_home", |bridge| {
                    bridge.reveal_home(RevealHome::Toggle)
                });
            }
            FieldAction::Apply(op) => match lege_field::apply(&self.field, &op) {
                Ok((next, inverse)) => {
                    self.field = next;
                    if self.field_synced == Some(true) {
                        self.spawn_field_apply(op);
                    } else {
                        self.undo.push(inverse);
                        self.undo.truncate(64);
                    }
                }
                Err(error) => tracing::warn!(%error, ?op, "field op refused"),
            },
            FieldAction::Undo => {
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

    fn spawn_launch(&self, app_id: AppId) {
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
                    if let Err(error) = proxy.launch(&app_id.to_string(), Vec::new(), "").await {
                        tracing::warn!(%error, %app_id, "Launcher1.Launch failed");
                    }
                }
                Err(error) => tracing::warn!(%error, "Launcher1 unavailable"),
            }
        });
    }

    fn redraw_dock(&mut self) {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        let entries = dock::group(&toplevels, self.dock_grouping);
        for entry in &entries {
            self.ensure_icon(&entry.key);
        }

        let Some(dock_window) = &mut self.dock else {
            return;
        };
        if !dock_window.prepare() {
            return;
        }
        let scale = dock_window.scale;
        let (width, height) = (
            dock_window.buffer.width as i32,
            dock_window.buffer.height as i32,
        );
        let data = DockData {
            entries: &entries,
            icons: &self.icons,
            home_revealed: self.bridge.as_ref().is_some_and(ShellBridge::home_revealed),
        };
        let action = {
            let painter = Painter::new(&mut dock_window.buffer);
            let mut ui = Ui::new(
                painter,
                &mut self.text,
                &mut dock_window.input,
                self.theme,
                scale,
                &mut self.kernel,
            );
            let area = Rect::new(0, 0, width, height);
            self.dock_screen.draw(&mut ui, area, &data)
        };
        dock_window.input.end_frame();
        dock_window.present();

        match action {
            DockAction::Activate(window_id) => {
                if let Some(bridge) = &self.bridge
                    && let Err(error) = bridge.activate(window_id)
                {
                    tracing::warn!(%error, "activate request failed");
                }
            }
            DockAction::RevealHome => {
                if let Some(bridge) = &self.bridge
                    && let Err(error) = bridge.reveal_home(RevealHome::Toggle)
                {
                    tracing::warn!(%error, "reveal_home request failed");
                }
            }
            DockAction::None => {}
        }
    }
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

async fn fetch_grouping() -> DockGrouping {
    match crate::data::session().await {
        Ok(connection) => match SettingsProxy::new(&connection).await {
            Ok(proxy) => settings::dock_grouping(&proxy).await,
            Err(_) => DockGrouping::ByApplication,
        },
        Err(_) => DockGrouping::ByApplication,
    }
}

#[derive(serde::Deserialize)]
struct SearchEntry {
    path: std::path::PathBuf,
    #[allow(dead_code)]
    is_dir: bool,
    size_bytes: u64,
    #[allow(dead_code)]
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

async fn run_search(drawer: Drawer, query: &str) -> Vec<String> {
    let connection = match crate::data::session().await {
        Ok(connection) => connection,
        Err(message) => return vec![format!("Search — {message}")],
    };
    let proxy = match FilesSearchProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => return vec![format!("Search — service unavailable: {error}")],
    };
    let roots = search_roots(drawer);
    match proxy.search(query, roots, 50).await {
        Ok(json) => parse_hits(&json),
        Err(error) => vec![format!("Search — service unavailable: {error}")],
    }
}

/// Parse `Files.Search1.Search`'s `[[entry, rank], ...]` payload into display lines.
fn parse_hits(json: &str) -> Vec<String> {
    let hits: Vec<(SearchEntry, String)> = match serde_json::from_str(json) {
        Ok(hits) => hits,
        Err(error) => return vec![format!("Search — malformed response: {error}")],
    };
    if hits.is_empty() {
        return vec!["No matches".to_owned()];
    }
    hits.into_iter()
        .map(|(entry, rank)| {
            format!(
                "{rank:<10} {:<12} {}",
                crate::format::bytes(entry.size_bytes),
                entry.path.display(),
            )
        })
        .collect()
}

impl ApplicationHandler<Wake> for ShellApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.home.is_some() {
            return;
        }

        let home_window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Home Surface")
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

        let dock_width = event_loop.primary_monitor().map_or(900.0, |monitor| {
            f64::from(monitor.size().width) / monitor.scale_factor()
        });
        let dock_window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Dock")
                .with_inner_size(LogicalSize::new(dock_width, 64.0))
                .with_resizable(false),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::error!(%error, "creating the dock window");
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
                match bridge.set_layer(&dock_window, lege_window::Layer::ShellOverlay) {
                    Ok(()) => tracing::info!("dock declared onto Layer::ShellOverlay"),
                    Err(error) => tracing::warn!(%error, "declaring the dock layer"),
                }
                match bridge.set_exclusive_zone(&dock_window, Edge::Bottom, 64) {
                    Ok(()) => {
                        tracing::info!("dock reserved a 64px exclusive zone at the bottom edge");
                    }
                    Err(error) => tracing::warn!(%error, "reserving the dock's exclusive zone"),
                }
                self.bridge = Some(bridge);
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "lege_shell_v1 unavailable; the dock and Window Shelf will show no live \
                     windows until this process is connected to the compositor's trusted-shell \
                     socket"
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
        self.dock = match HostedWindow::new(dock_window) {
            Ok(hosted) => Some(hosted),
            Err(error) => {
                tracing::error!(%error, "creating the dock presenter");
                None
            }
        };

        self.spawn_refresh_loop();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
        if let Some(dock) = &self.dock {
            dock.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _wake: Wake) {
        self.drain_updates();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
        if let Some(dock) = &self.dock {
            dock.request_redraw();
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
        let is_dock = self
            .dock
            .as_ref()
            .is_some_and(|dock| dock.window.id() == window_id);
        if !is_home && !is_dock {
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                self.exit = true;
                event_loop.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                if is_home {
                    self.redraw_home();
                } else {
                    self.redraw_dock();
                }
                return;
            }
            _ => {}
        }

        let hosted = if is_home {
            &mut self.home
        } else {
            &mut self.dock
        };
        let Some(hosted) = hosted else { return };
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
