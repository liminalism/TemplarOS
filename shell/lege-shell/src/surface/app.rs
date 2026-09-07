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
use lege_types::action;
use lege_window::RevealHome;

use crate::clipboard::Clipboard;
use crate::field_model::{self, LiveState, ProjectInfo};
use crate::file_browser::{BrowseEntry, BrowserSources, DirectoryListing, EntryKind};
use crate::files_surface::FilesLocation;
use crate::home_surface::Drawer;
use crate::icons::{self, SystemIconRoots};
use crate::omnibox::Scope;
use crate::settings::{self, HomeFieldSettings, PreviewMode};
use crate::surface::design;
use crate::surface::field_screen::{FieldAction, FieldData, FieldScreen};
use crate::surface::file_browser::{FileBrowserData, FileRequest, FileVerb};
use crate::surface::fonts;
use crate::surface::window::{
    HostedWindow, key_pressed, modifiers_of, scroll_pixels, translate_button, translate_keys,
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
    Drawers(Vec<(Drawer, crate::drawers::DrawerFeed)>),
    /// One drawer's refreshed contents, out of band: what a service's own change signal delivers
    /// between the periodic sweeps.
    Drawer(Drawer, crate::drawers::DrawerFeed),
    /// Every installed application, with the metadata a launcher needs
    /// (`AppIdentity1.ListApplications`). `None` when the registry did not answer, which the
    /// launcher widget reports rather than drawing an empty list.
    Apps(Option<Vec<crate::app_search::AppEntry>>),
    Projects(Vec<ProjectInfo>),
    /// One widget's refreshed content. Content only: by the field's own rule this moves nothing.
    Widget(String, crate::surface::widgets::WidgetSnapshot),
    /// The notifications widget's structured rows, beside its generic snapshot: ids and action
    /// keys the per-item dismiss and invoke controls act on.
    NotificationRows(Vec<crate::surface::widgets::notifications::NotificationRow>),
    HomeFieldSettings(HomeFieldSettings),
    /// The session's screen-off delay in seconds, from `Power1` (0 = never). Relayed straight to
    /// the compositor, which is the only process that can see the seat go idle.
    IdleTimeout(u32),
    /// Who fills an exclusive `Providers1` role the shell acts on: the `terminal` the drop-down
    /// chord toggles, and the `file-search` the search panel's last row hands the query to.
    /// `None` is "nobody", which is a fact worth relaying — the compositor stops looking for a
    /// terminal window it will never find.
    Provider {
        role: &'static str,
        app_id: Option<String>,
    },
    /// The persisted recent-apps list. Applied only while the in-memory list is still empty,
    /// so a launch that happened before the load landed is not overwritten by a stale read.
    RecentApps(crate::recents::RecentApps),
    /// Something the user asked for did not happen, in words meant for them. Shown in the field's
    /// notice strip, the same place a successful operation reports itself.
    Notice(String),
    /// A journalled file operation reached a terminal state: what to say about it, and the undo
    /// it leaves behind when there is one.
    FileOperation {
        undo: Option<FileOperationUndo>,
        notice: String,
    },
    /// One folder window's listing, keyed by the directory it lists. Many windows, many
    /// listings — unlike the single Files object's, which has exactly one.
    FolderListing {
        path: PathBuf,
        result: Result<DirectoryListing, String>,
    },
    /// One file thumbnail for the browse grid, rendered by Files.Previews1 and decoded
    /// off-thread. Tagged with the listing generation it was requested under, so a decode that
    /// outlives its directory is dropped instead of painted into the wrong one.
    FileThumbnail {
        generation: u64,
        path: PathBuf,
        bitmap: Option<pixelkit_raster::Bitmap>,
    },
    /// One application's icon, resolved and decoded off-thread. `None` when nothing was found —
    /// the attempt is already recorded, so the receiver simply has nothing to file.
    Icon(String, Option<pixelkit_raster::Bitmap>),
    /// The Files object's current directory as a treemap and a composition, rasterized
    /// off-thread.
    Treemap {
        path: PathBuf,
        bitmap: pixelkit_raster::Bitmap,
        composition: Vec<(u32, f32)>,
    },
    /// Every five minutes: drop the cached treemap so the next frame asks for a fresh one.
    TreemapsStale,
    /// The persisted document arrived from `HomeField1` (startup, or after an undo).
    Field(FieldDocument),
    /// `HomeField1` is not reachable: the field stays in-session.
    FieldUnavailable,
    SearchResults {
        scope: Scope,
        query: String,
        result: Result<Vec<BrowseEntry>, String>,
    },
    /// The copies of one logical application, resolved off the event loop.
    AppVariants(String, Vec<(String, String, String)>),
    BrowserSources(BrowserSources),
    BrowserListing {
        location: FilesLocation,
        show_hidden: bool,
        result: Result<DirectoryListing, String>,
    },
    FileOpened {
        path: PathBuf,
        result: Result<crate::data::OpenFileResult, String>,
    },
}

/// Whether the field is behind something and its window previews should stop.
///
/// Nothing of the field is visible under a window at all — `FieldScreen::occlude_windows` paints
/// flat ground over every window's rectangle — so with a fullscreen or maximized window in front
/// of it there is nothing to capture *for*. This is the capture half of that rule: it saves the
/// per-window shm round trip while the field cannot be seen. Revealing Home (bare Super) puts the
/// field in front again and the previews come back, as does un-maximizing or parking the window.
///
/// The cache is kept, never cleared, so coming back shows the picture that was already there
/// rather than a blank tile filling in a frame or two later.
///
/// A shelved window covers nothing: parking is how you get the field back.
fn previews_covered(home_revealed: bool, toplevels: &[lege_window::ShellToplevel]) -> bool {
    !home_revealed
        && toplevels
            .iter()
            .any(|window| !window.shelved && (window.fullscreen || window.maximized))
}

/// `1 item` or `3 items` — what an operation summary calls a path set.
fn items(count: usize) -> String {
    if count == 1 {
        "1 item".to_owned()
    } else {
        format!("{count} items")
    }
}

/// Whether Ctrl+Z should undo the file operation rather than the last layout change.
///
/// The rule, and the whole of it: whichever happened last. Home keeps two undo stacks because the
/// two things are genuinely different — one puts a tile back, the other puts a *file* back, and
/// the second is a service transaction with a journal behind it — but a person pressing Ctrl+Z is
/// not thinking about which stack they are addressing. They mean "undo what I just did".
///
/// A file operation with no layout change after it wins even when the field has nothing to undo,
/// which is the common case: drag some files into a folder on a fresh session and Ctrl+Z must put
/// them back rather than doing nothing at all.
fn undo_the_file_operation(
    file: Option<&FileOperationUndo>,
    last_field_change: Option<Instant>,
    has_field_undo: bool,
) -> bool {
    let Some(file) = file else {
        return false;
    };
    if !has_field_undo {
        return true;
    }
    last_field_change.is_none_or(|field_at| file.at >= field_at)
}

/// The most recent journalled file operation, and what to call it when offering to undo it.
#[derive(Clone, Debug)]
struct FileOperationUndo {
    op_id: String,
    /// What the person did, in their words — "Move 3 items to /home/u/Documents".
    summary: String,
    at: Instant,
}

/// How far one press of a level control moves it.
///
/// The same five percentage points `lege status louder` uses, and deliberately the same number:
/// a person who steps the volume from a tile and from a terminal is doing one thing, and two
/// different step sizes would make the two surfaces disagree about what "one step" means.
const STEP_PERCENT: i32 = 5;

/// The application `settings.open` opens — the id of `apps/lege-settings/org.legeos.Settings.desktop`.
const SETTINGS_APP_ID: &str = "org.legeos.Settings";

/// How long an armed EMPTY waits for its second press.
const EMPTY_TRASH_CONFIRM_WINDOW: Duration = Duration::from_secs(8);

/// How often an open folder window re-reads its directory. Slow on purpose: this is a poll
/// standing in for the change notification the shared file index will deliver
/// (`@legeos.work.shared-incremental-file-index`), and a fast poll would be a busy loop per window.
const FOLDER_REFRESH: Duration = Duration::from_secs(5);
/// How many three-second refreshes pass between full re-reads of the installed application set.
/// Thirty seconds: slow enough that the most expensive read on the loop stops dominating it,
/// soon enough that a package installed by a path that announces nothing still turns up while
/// the person is still looking for it.
const APPS_REFRESH_TICKS: u32 = 10;

/// Whether this EMPTY press is the confirming one: a press is only armed for a short window, so
/// an unanswered confirmation expires rather than sitting armed until a stray click hours later
/// finds it and empties the trash.
fn empty_trash_confirmed(armed_at: Option<Instant>, now: Instant) -> bool {
    armed_at.is_some_and(|at| now.duration_since(at) < EMPTY_TRASH_CONFIRM_WINDOW)
}

/// Which output's document the shell edits. The shell has one Home Surface window today and
/// no way to learn its output's name through winit; one field per output keyed by name is the
/// documented next step, and the key already exists so nothing is migrated then.
const OUTPUT: &str = "primary";

/// How many of the launcher's ranked rows are worth an icon: enough to cover the largest tile
/// plus what a scroll would reveal, and nowhere near the whole inventory.
const LAUNCHER_ICON_LOOKAHEAD: usize = 24;

/// How many *new* icon resolutions one frame may start. See `ShellApp::ensure_launcher_icons`.
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
    /// The screen-off delay last relayed to the compositor, so an unchanged reading costs
    /// nothing (`DataUpdate::IdleTimeout`). `None` until the first read lands.
    idle_timeout: Option<u32>,
    /// The selected `terminal` provider — what the drop-down chord launches when no terminal
    /// window exists. `None` when nothing fills the role.
    terminal_provider: Option<AppId>,
    /// What `set_terminal` last told the compositor, so an unchanged reading costs no round
    /// trip. `None` until the first read lands; the empty string is a real value meaning "no
    /// terminal is selected", and the compositor needs to be told that too.
    terminal_relayed: Option<String>,
    /// The selected `file-search` provider — FSearch by default. Read here rather than in the
    /// pure omnibox because which application answers "search everywhere" is a Providers1
    /// choice, and the panel names the intent rather than the program.
    file_search_provider: Option<AppId>,
    /// The last question asked of a location, so the panel's "Search everywhere" row can hand
    /// it on. The row is composed by a pure module with no bus and no clock; the question it is
    /// about is the one this host asked on its behalf.
    location_query: String,
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
    /// When the field document last changed, and the most recent journalled file operation that
    /// can still be undone.
    ///
    /// Home has two undos and they are not interchangeable: the field's own op log puts a tile
    /// back where it was, and `Files.Operations1` puts a *file* back where it was. Keeping them
    /// separate internally is right — one is a layout change and the other moves data — but a
    /// person pressing Ctrl+Z means "undo what I just did", so the surface picks whichever
    /// happened last and says which one it is undoing before it does.
    last_field_change_at: Option<Instant>,
    last_file_operation: Option<FileOperationUndo>,
    /// What Ctrl+C or Ctrl+X last put down. Held by the host rather than by a surface, because
    /// it outlives whichever folder was open when it was filled.
    clipboard: Option<Clipboard>,
    /// Whether a `wl_data_device` drag has already been started for the lift now in progress, so
    /// one lift starts exactly one drag rather than one per frame.
    drag_exported: bool,
    field_settings: HomeFieldSettings,
    /// Recently launched applications, newest first. Loaded once from Settings1; launches
    /// update it in memory immediately so the strip does not wait on the round trip.
    recent: crate::recents::RecentApps,
    /// Whether the one-time `HomeField1.Get` has been sent.
    field_requested: bool,
    apps: Vec<crate::app_search::AppEntry>,
    /// Whether the last `ListApplications` succeeded.
    apps_available: bool,
    projects: Vec<ProjectInfo>,
    /// The copies of each logical application that has more than one, keyed by logical id.
    /// Refreshed with the application list, and empty for nearly every application there is.
    app_variants: HashMap<String, Vec<(String, String, String)>>,
    /// The user's home directory, read once: the one folder whose icon its own name cannot give
    /// away, and the root every folder window opens relative to.
    home_dir: Option<PathBuf>,
    /// What each open folder window is showing, keyed by the directory. Bounded by how many
    /// folder tiles are on the field, which is bounded by the field.
    folder_listings: HashMap<PathBuf, Result<DirectoryListing, String>>,
    /// Directories a folder window has asked for and not yet received, and when each was last
    /// asked for, so a window refreshes on a slow beat instead of once and never again.
    folder_pending: HashSet<PathBuf>,
    folder_asked: HashMap<PathBuf, Instant>,
    /// Previews keyed the way session tiles are (`field_model::session_identifier`).
    previews: HashMap<String, pixelkit_raster::Bitmap>,
    /// When each window's preview was last requested, for the rate policy.
    preview_requested: HashMap<lege_window::WindowId, Instant>,
    /// Each widget's latest content, keyed by `TileKind::System`'s probe.
    widgets: HashMap<String, crate::surface::widgets::WidgetSnapshot>,
    /// The Files object's current directory, its treemap and its composition. One directory,
    /// because there is one Files object.
    treemap: Option<(PathBuf, pixelkit_raster::Bitmap)>,
    composition: Vec<(u32, f32)>,
    /// The directory with a scan in flight, so one request is outstanding at a time.
    treemap_pending: Option<PathBuf>,
    /// When the last pointer drop happened, for the experiment's undo-within-5-s measure.
    last_drop_at: Option<Instant>,
    started: Instant,

    drawer_feeds: HashMap<Drawer, crate::drawers::DrawerFeed>,
    search_results: Option<(Scope, String, Result<Vec<BrowseEntry>, String>)>,
    /// The question typed but not yet asked. A keystroke is not a search: the shell waits for
    /// the typing to stop before spending a round trip, and 180ms is about the gap between two
    /// keystrokes of ordinary typing.
    search_pending: Option<(Scope, String, Instant)>,
    browser_sources: BrowserSources,
    browser_listing: Option<(FilesLocation, bool, DirectoryListing)>,
    /// Decoded file thumbnails for the browse view's grid mode, keyed by path. Filled from the
    /// bounded requests the screen hands out; dropped whenever the listing generation moves.
    file_thumbnails: std::collections::HashMap<PathBuf, pixelkit_raster::Bitmap>,
    file_thumbnails_generation: u64,
    file_thumbnails_pending: std::collections::HashSet<PathBuf>,
    notification_rows: Vec<crate::surface::widgets::notifications::NotificationRow>,
    /// A toast waiting for the event loop (window creation needs `ActiveEventLoop`).
    pending_toast: Option<crate::surface::widgets::notifications::NotificationRow>,
    /// The live toast surface, its notification, and when it appeared.
    toast: Option<(
        HostedWindow,
        crate::surface::widgets::notifications::NotificationRow,
        Instant,
    )>,
    /// The newest notification id a toast has been shown for, so re-fetches do not re-toast.
    /// `None` until the first fetch: whatever was already active at startup is old news and
    /// only sets the watermark.
    last_toasted_id: Option<u32>,
    browser_pending: Option<(FilesLocation, bool)>,
    browser_error: Option<(FilesLocation, String)>,
    browser_status: Option<String>,
    icons: HashMap<String, pixelkit_raster::Bitmap>,
    icon_attempted: HashSet<String>,

    exit: bool,
    /// When EMPTY was last pressed on the Trash tile, if it is still awaiting its second press.
    ///
    /// `ActionDecl::confirm` is honoured by the context menu, which is not where this control
    /// lives, so the tile does the confirming itself: the first press says what will happen and
    /// the second does it. A window this short is deliberate — an unanswered confirmation must
    /// expire, not sit armed until a stray click hours later finds it.
    empty_trash_armed_at: Option<Instant>,
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
            idle_timeout: None,
            terminal_provider: None,
            terminal_relayed: None,
            file_search_provider: None,
            location_query: String::new(),
            home: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            theme: design::theme(),
            field_screen: FieldScreen::default(),
            field: FieldDocument::new(OUTPUT, 16, 8),
            field_synced: None,
            undo: Vec::new(),
            last_field_change_at: None,
            last_file_operation: None,
            clipboard: None,
            drag_exported: false,
            field_settings: HomeFieldSettings::default(),
            recent: crate::recents::RecentApps::default(),
            field_requested: false,
            apps: Vec::new(),
            apps_available: false,
            projects: Vec::new(),
            app_variants: HashMap::new(),
            home_dir: crate::drawers::standard_paths::home_dir(),
            folder_listings: HashMap::new(),
            folder_pending: HashSet::new(),
            folder_asked: HashMap::new(),
            previews: HashMap::new(),
            preview_requested: HashMap::new(),
            widgets: HashMap::new(),
            treemap: None,
            composition: Vec::new(),
            treemap_pending: None,
            last_drop_at: None,
            started: Instant::now(),
            drawer_feeds: HashMap::new(),
            search_results: None,
            search_pending: None,
            browser_sources: BrowserSources::default(),
            browser_listing: None,
            file_thumbnails: std::collections::HashMap::new(),
            file_thumbnails_generation: 0,
            file_thumbnails_pending: std::collections::HashSet::new(),
            notification_rows: Vec::new(),
            pending_toast: None,
            toast: None,
            last_toasted_id: None,
            browser_pending: None,
            browser_error: None,
            browser_status: None,
            icons: HashMap::new(),
            icon_attempted: HashSet::new(),
            exit: false,
            empty_trash_armed_at: None,
        }
    }

    fn spawn_refresh_loop(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn({
            let tx = tx.clone();
            let proxy = proxy.clone();
            async move {
                let recents = match crate::data::session().await {
                    Ok(connection) => match SettingsProxy::new(&connection).await {
                        Ok(settings) => crate::settings::load_recent_apps(&settings).await,
                        Err(_) => crate::recents::RecentApps::default(),
                    },
                    Err(_) => crate::recents::RecentApps::default(),
                };
                let _ = tx.send(DataUpdate::RecentApps(recents));
                let _ = proxy.send_event(Wake);
            }
        });
        self.spawn_applications_watch();
        self.runtime.spawn(async move {
            let mut ticks: u32 = 0;
            loop {
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(100) {
                    let _ = tx.send(DataUpdate::TreemapsStale);
                }
                // Concurrently, because none of these reads depends on another and they were
                // costing the sum of their round trips rather than the longest of them.
                let (drawers, field_settings, projects, browser_sources) = tokio::join!(
                    crate::data::fetch_all(20),
                    fetch_home_field_settings(),
                    crate::data::fetch_projects(),
                    crate::data::fetch_browser_sources(),
                );
                let (session_widget, notifications_widget, settings_widget) = tokio::join!(
                    crate::data::fetch_session_widget(),
                    crate::data::fetch_notifications_widget(),
                    crate::data::fetch_settings_widget(),
                );
                if tx.send(DataUpdate::Drawers(drawers)).is_err() {
                    return;
                }
                // The installed set is the most expensive read here by a wide margin — the whole
                // catalogue, then its logical grouping — and it is also the one that changes
                // least: a person installs software, they do not install it every three seconds.
                // `ApplicationsChanged` carries the prompt update; this is the backstop for the
                // paths that do not announce themselves, which is why it is slow rather than off.
                if ticks == 1 || ticks.is_multiple_of(APPS_REFRESH_TICKS) {
                    let _ = tx.send(DataUpdate::Apps(crate::data::fetch_apps().await));
                    // Same cadence and the same reason: a person changes when their screen turns
                    // off about as often as they install software, and `Power1` has no change
                    // signal to sleep on.
                    let _ = tx.send(DataUpdate::IdleTimeout(
                        crate::data::fetch_idle_timeout().await,
                    ));
                }
                // The two roles the shell itself acts on. Cheap (one `Report`), and the backstop
                // under the `Providers1.Changed` watch: a selection made while the shell was
                // starting still reaches the compositor.
                for role in ["terminal", "file-search"] {
                    let _ = tx.send(DataUpdate::Provider {
                        role,
                        app_id: crate::data::selected_provider(role).await,
                    });
                }
                let _ = tx.send(DataUpdate::Projects(projects));
                let _ = tx.send(DataUpdate::HomeFieldSettings(field_settings));
                let _ = tx.send(DataUpdate::BrowserSources(browser_sources));
                let _ = tx.send(DataUpdate::Widget("session".to_owned(), session_widget));
                let (notifications_snapshot, notification_rows) = notifications_widget;
                let _ = tx.send(DataUpdate::Widget(
                    "notifications".to_owned(),
                    notifications_snapshot,
                ));
                let _ = tx.send(DataUpdate::NotificationRows(notification_rows));
                let _ = tx.send(DataUpdate::Widget("settings".to_owned(), settings_widget));
                let _ = proxy.send_event(Wake);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    /// Re-read the installed applications whenever `AppIdentity1` says the set moved.
    ///
    /// A separate task from the refresh loop because it spends nearly all its time asleep on a
    /// signal, and folding it into a three-second poll would be the poll again by another name.
    /// The signal is best-effort by its own documentation — sessiond emits it when the autostart
    /// scan imports something, and a package installed later announces nothing — so this makes
    /// the common case immediate and leaves the loop's slow backstop to catch the rest.
    fn spawn_applications_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(identity) = lege_client::proxies::AppIdentityProxy::new(&connection).await
            else {
                return;
            };
            let Ok(mut changed) = identity.receive_applications_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                if tx
                    .send(DataUpdate::Apps(crate::data::fetch_apps().await))
                    .is_err()
                {
                    return;
                }
                let _ = proxy.send_event(Wake);
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

    /// Ask the service to discard the stored layout. The reply is the fresh document, which the
    /// next frame merges the current defaults into — so the reset and the new default arrive
    /// together and the field is never briefly empty.
    fn spawn_field_reset(&self, cols: u16, rows: u16) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            if let Some(doc) = crate::data::reset_field(OUTPUT, cols, rows).await {
                let _ = tx.send(DataUpdate::Field(doc));
                let _ = proxy.send_event(Wake);
            }
        });
    }

    /// The debounce interval: long enough that ordinary typing costs one search rather than one
    /// per keystroke, short enough that a person who stopped typing does not notice waiting.
    const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(180);

    /// Record who fills a role the shell acts on, and tell the compositor when it is the one it
    /// needs to know.
    ///
    /// Only on a change, and only over a live connection: the compositor keeps whatever it was
    /// last told, so re-sending the same id every three seconds would be a round trip for
    /// nothing — the same rule `DataUpdate::IdleTimeout` follows.
    fn note_provider(&mut self, role: &'static str, app_id: Option<String>) {
        let parsed = app_id.and_then(|id| match id.parse::<AppId>() {
            Ok(app_id) => Some(app_id),
            Err(error) => {
                tracing::warn!(%error, role, "the selected provider is not a valid app id");
                None
            }
        });
        match role {
            "terminal" => {
                let wire = parsed.as_ref().map_or("", AppId::as_str).to_owned();
                self.terminal_provider = parsed;
                if self.terminal_relayed.as_deref() == Some(wire.as_str()) {
                    return;
                }
                // Nothing is recorded as relayed until there is a connection to relay it over,
                // so a shell that started before the compositor answered still tells it once it
                // does rather than believing it already had.
                let Some(bridge) = &self.bridge else {
                    return;
                };
                self.terminal_relayed = Some(wire.clone());
                if let Err(error) = bridge.set_terminal(&wire) {
                    // Ordinary against a compositor older than version 5: the chord is not there
                    // to be told about yet.
                    tracing::debug!(%error, "relaying the terminal provider to the compositor");
                }
            }
            "file-search" => self.file_search_provider = parsed,
            other => tracing::debug!(role = other, "a provider nothing here acts on"),
        }
    }

    /// Answer the terminal chord pressed with no terminal window to toggle: launch the selected
    /// `terminal` provider through the ordinary launch chain, with no arguments.
    ///
    /// One launch per press. With no terminal selected there is nothing to start, and saying so
    /// is better than a chord that silently does nothing.
    fn drain_terminal_requests(&mut self) -> bool {
        let Some(bridge) = self.bridge.as_mut() else {
            return false;
        };
        let requests = bridge.take_terminal_requests();
        if requests == 0 {
            return false;
        }
        let Some(app_id) = self.terminal_provider.clone() else {
            let now_ms = self.started.elapsed().as_millis() as u64;
            self.field_screen.report(
                "No terminal is selected — choose one in Settings ▸ Providers".to_owned(),
                now_ms,
            );
            return true;
        };
        for _ in 0..requests {
            self.record_recent(&app_id);
            self.spawn_launch(app_id.clone());
        }
        true
    }

    /// Note a question. It is asked once the typing has stopped, in [`Self::flush_search`].
    fn note_search(&mut self, scope: Scope, query: String) {
        if matches!(scope, Scope::Location(_)) {
            query.trim().clone_into(&mut self.location_query);
        }
        if query.trim().is_empty() {
            self.search_pending = None;
            self.search_results = None;
            return;
        }
        self.search_pending = Some((scope, query, Instant::now()));
    }

    /// Ask the question, if it has been still for long enough.
    fn flush_search(&mut self) {
        let ready = self
            .search_pending
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() >= Self::SEARCH_DEBOUNCE);
        if !ready {
            return;
        }
        let Some((scope, query, _)) = self.search_pending.take() else {
            return;
        };
        // Only a location asks the file service; the launcher on bare Home has its answer in
        // hand already (`DataUpdate::Apps`), and a file search never runs for it.
        let roots: Vec<String> = match &scope {
            Scope::Location(location) => location
                .directory()
                .map(|path| vec![path.display().to_string()])
                .unwrap_or_default(),
            Scope::Applications => Vec::new(),
        };
        if roots.is_empty() {
            return;
        }
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = run_search(roots, &query).await;
            let _ = tx.send(DataUpdate::SearchResults {
                scope,
                query,
                result,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    /// Hand the browse grid's bounded thumbnail wants to the preview service. Requests carry
    /// the screen's listing generation; a decode arriving after the directory changed is
    /// dropped in the `FileThumbnail` handler. Decoding happens in the async task — the render
    /// thread never opens a file.
    fn spawn_thumbnails(&mut self) {
        let Some((_, _, listing)) = &self.browser_listing else {
            return;
        };
        let screen = &self.field_screen.files;
        if !screen.thumbnail_current(self.file_thumbnails_generation) {
            self.file_thumbnails.clear();
            self.file_thumbnails_pending.clear();
            self.file_thumbnails_generation = self.file_thumbnails_generation.wrapping_add(1);
            while !screen.thumbnail_current(self.file_thumbnails_generation) {
                self.file_thumbnails_generation = self.file_thumbnails_generation.wrapping_add(1);
            }
        }
        for (generation, path) in screen.begin_thumbnails(&listing.entries, &self.file_thumbnails) {
            if !self.file_thumbnails_pending.insert(path.clone()) {
                continue;
            }
            let tx = self.data_tx.clone();
            let proxy = self.wake_proxy.clone();
            self.runtime.spawn(async move {
                let bitmap = crate::data::file_thumbnail(&path).await;
                let _ = tx.send(DataUpdate::FileThumbnail {
                    generation,
                    path,
                    bitmap,
                });
                let _ = proxy.send_event(Wake);
            });
        }
    }

    /// Fetch what one location needs. A feed location needs nothing — its rows are the drawer
    /// feeds the background refresh already carries — so only a directory reaches the service.
    fn spawn_browse(&mut self, location: FilesLocation, show_hidden: bool) {
        let Some(path) = location.directory().map(std::path::Path::to_path_buf) else {
            return;
        };
        if self
            .browser_pending
            .as_ref()
            .is_some_and(|pending| *pending == (location.clone(), show_hidden))
        {
            return;
        }
        self.browser_pending = Some((location.clone(), show_hidden));
        self.browser_error = None;
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::data::browse_directory(&path, show_hidden).await;
            let _ = tx.send(DataUpdate::BrowserListing {
                location,
                show_hidden,
                result,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    /// One folder window's directory, on a slow refresh beat.
    ///
    /// There is no filesystem watcher behind Home yet, so a window that fetched once would show a
    /// directory as it was when it opened. Re-asking every few seconds is the honest interim: it
    /// is bounded by the number of folder windows, and it is the same call the Files object makes.
    fn spawn_folder_browse(&mut self, path: PathBuf) {
        if self.folder_pending.contains(&path) {
            return;
        }
        let now = Instant::now();
        let fresh = self
            .folder_asked
            .get(&path)
            .is_some_and(|at| now.duration_since(*at) < FOLDER_REFRESH);
        if fresh {
            return;
        }
        self.folder_pending.insert(path.clone());
        self.folder_asked.insert(path.clone(), now);
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::data::browse_directory(&path, false).await;
            let _ = tx.send(DataUpdate::FolderListing { path, result });
            let _ = proxy.send_event(Wake);
        });
    }

    /// Place a folder window for `path` on the field, or do nothing when one is already there —
    /// the tile id is derived from the path, so "already there" is exact rather than a guess.
    fn open_folder_window(&mut self, path: &std::path::Path) {
        let ops = field_model::open_folder_ops(&self.field, path);
        if ops.is_empty() {
            return;
        }
        for op in ops {
            match lege_field::apply(&self.field, &op) {
                Ok((next, _)) => {
                    self.field = next;
                    if self.field_synced == Some(true) {
                        self.spawn_field_apply(op);
                    }
                }
                Err(error) => tracing::warn!(%error, "opening a folder window"),
            }
        }
        self.spawn_folder_browse(path.to_path_buf());
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

    #[allow(clippy::too_many_lines)]
    fn drain_updates(&mut self) {
        while let Ok(update) = self.data_rx.try_recv() {
            match update {
                // Both shapes of the same news: the periodic sweep's whole set, and the one
                // drawer a service's change signal refreshed on its own.
                DataUpdate::Drawers(drawers) => self.drawer_feeds.extend(drawers),
                DataUpdate::Drawer(drawer, feed) => {
                    self.drawer_feeds.insert(drawer, feed);
                }
                DataUpdate::Apps(apps) => match apps {
                    Some(apps) => {
                        // Only an application installed more than once has a choice to offer, so
                        // only those cost a call — and their copies are asked for once per
                        // refresh rather than once per menu, because a context menu opening is
                        // not a moment to be waiting on a bus.
                        let wanted: Vec<String> = apps
                            .iter()
                            .filter(|entry| entry.variants > 1 && !entry.logical.is_empty())
                            .map(|entry| entry.logical.clone())
                            .collect();
                        self.apps = apps;
                        self.apps_available = true;
                        self.app_variants
                            .retain(|logical, _| wanted.contains(logical));
                        for logical in wanted {
                            if self.app_variants.contains_key(&logical) {
                                continue;
                            }
                            let tx = self.data_tx.clone();
                            let proxy = self.wake_proxy.clone();
                            self.runtime.spawn(async move {
                                let variants = crate::data::fetch_variants(&logical).await;
                                let _ = tx.send(DataUpdate::AppVariants(logical, variants));
                                let _ = proxy.send_event(Wake);
                            });
                        }
                    }
                    // Keep the last good inventory rather than blanking the launcher on one
                    // failed poll; the flag is what the widget reports.
                    None => self.apps_available = false,
                },
                DataUpdate::AppVariants(logical, variants) => {
                    self.app_variants.insert(logical, variants);
                }
                DataUpdate::Projects(projects) => self.projects = projects,
                DataUpdate::Widget(id, snapshot) => {
                    self.widgets.insert(id, snapshot);
                }
                DataUpdate::HomeFieldSettings(settings) => {
                    self.field_settings = settings;
                    self.field_screen.drag_cfg.throw_enabled =
                        settings.throw && !settings.reduced_motion;
                }
                DataUpdate::IdleTimeout(seconds) => {
                    // Only on a change, and only over a live connection: the compositor keeps
                    // whatever it was last told, so re-sending the same number every three
                    // seconds would be a protocol round trip for nothing.
                    if self.idle_timeout != Some(seconds) {
                        self.idle_timeout = Some(seconds);
                        if let Some(bridge) = &self.bridge
                            && let Err(error) = bridge.set_idle_timeout(seconds)
                        {
                            tracing::warn!(%error, "relaying the screen-off delay to the compositor");
                        }
                    }
                }
                DataUpdate::Provider { role, app_id } => self.note_provider(role, app_id),
                DataUpdate::RecentApps(loaded) => {
                    if self.recent.ids().is_empty() {
                        self.recent = loaded;
                    }
                }
                DataUpdate::Treemap {
                    path,
                    bitmap,
                    composition,
                } => {
                    if self.treemap_pending.as_deref() == Some(path.as_path()) {
                        self.treemap_pending = None;
                    }
                    self.treemap = Some((path, bitmap));
                    self.composition = composition;
                }
                // A journalled operation finished. This is the one progress-and-error surface
                // for everything the Home file surfaces do: one operation, one report.
                DataUpdate::FileOperation { undo, notice } => {
                    if let Some(undo) = undo {
                        self.last_file_operation = Some(undo);
                    }
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen.report(notice, now_ms);
                }
                DataUpdate::Notice(text) => {
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen.report(text, now_ms);
                }
                DataUpdate::Icon(key, bitmap) => {
                    if let Some(bitmap) = bitmap {
                        self.icons.insert(key, bitmap);
                    }
                }
                DataUpdate::FolderListing { path, result } => {
                    self.folder_pending.remove(&path);
                    if let Err(error) = &result {
                        tracing::debug!(%error, path = %path.display(), "a folder window's listing failed");
                    }
                    // Keep failures as view data: removing the entry made a failed request
                    // indistinguishable from one still loading, forever.
                    self.folder_listings.insert(path, result);
                }

                DataUpdate::FileThumbnail {
                    generation,
                    path,
                    bitmap,
                } => {
                    self.file_thumbnails_pending.remove(&path);
                    if generation == self.file_thumbnails_generation
                        && self.field_screen.files.thumbnail_current(generation)
                        && let Some(bitmap) = bitmap
                    {
                        self.file_thumbnails.insert(path, bitmap);
                    }
                }
                DataUpdate::NotificationRows(rows) => {
                    // Push, not poll-and-hope: the newest unseen notification becomes a toast
                    // the next time the event loop hands us a handle to create its window. The
                    // first fetch only sets the watermark — what was already active at startup
                    // is the tile's business, not a fresh arrival.
                    let newest = rows.iter().map(|row| row.id).max().unwrap_or(0);
                    match self.last_toasted_id {
                        None => self.last_toasted_id = Some(newest),
                        Some(seen) if newest > seen => {
                            self.last_toasted_id = Some(newest);
                            self.pending_toast = rows.iter().find(|row| row.id == newest).cloned();
                        }
                        Some(_) => {}
                    }
                    self.notification_rows = rows;
                }
                DataUpdate::TreemapsStale => self.treemap = None,
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
                    scope,
                    query,
                    result,
                } => {
                    self.search_results = Some((scope, query, result));
                }
                DataUpdate::BrowserSources(sources) => self.browser_sources = sources,
                DataUpdate::BrowserListing {
                    location,
                    show_hidden,
                    result,
                } => {
                    let is_current = self
                        .browser_pending
                        .as_ref()
                        .is_some_and(|pending| *pending == (location.clone(), show_hidden));
                    if !is_current {
                        continue;
                    }
                    self.browser_pending = None;
                    match result {
                        Ok(listing) => {
                            self.browser_listing = Some((location, show_hidden, listing));
                            self.browser_error = None;
                        }
                        Err(error) => {
                            self.browser_error = Some((location, error));
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
    /// Ask for one application's icon, off the event loop.
    ///
    /// Resolving an icon is not cheap and it is not bounded: it reads `.desktop` files across
    /// every `XDG_DATA_DIRS` entry, stats a candidate path per size bucket per extension per theme
    /// directory, and then decodes a PNG or rasterizes an SVG (`crate::icons`). Doing that on this
    /// thread means doing it *between a key press and the frame that shows the character*, because
    /// every accepted key asks for a redraw and every redraw calls through here — which is exactly
    /// how typing in the search bar came to feel slow, since each new character brings new rows,
    /// and new rows are new icons. So this only records that the attempt was made and hands the
    /// work to a blocking thread, the same shape `spawn_treemap` already uses; the bitmap arrives
    /// as a `DataUpdate` and a `Wake` asks for the frame that draws it.
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
        let icon = entry
            .map(|entry| entry.icon.clone())
            .filter(|icon| !icon.is_empty());
        let key = key.to_owned();
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn_blocking(move || {
            let hints = icons::IconHints {
                icon: icon.as_deref(),
                desktop_file_ids: &desktop_file_ids,
            };
            let bitmap = icons::load_app_icon_hinted(&key, hints, &SystemIconRoots);
            // A miss is sent too: `icon_attempted` already recorded it, so the receiver has
            // nothing to do, but sending unconditionally keeps the two sides symmetric.
            let _ = tx.send(DataUpdate::Icon(key, bitmap));
            let _ = proxy.send_event(Wake);
        });
    }

    /// Ask for one theme icon — a folder, a drive, the wastebasket — off the event loop, exactly
    /// as an application's icon is asked for and filed in the same map under a `theme:` key.
    fn ensure_theme_icon(&mut self, name: &str) {
        let key = crate::surface::file_browser::theme_icon_key(name);
        if name.is_empty() || self.icons.contains_key(&key) || self.icon_attempted.contains(&key) {
            return;
        }
        self.icon_attempted.insert(key.clone());
        let name = name.to_owned();
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn_blocking(move || {
            let bitmap = icons::load_theme_icon(&name, &SystemIconRoots);
            let _ = tx.send(DataUpdate::Icon(key, bitmap));
            let _ = proxy.send_event(Wake);
        });
    }

    /// Every theme icon the Files object's rail and the folder windows need, asked for once each.
    fn ensure_place_icons(&mut self) {
        let mut wanted: Vec<&'static str> =
            vec!["folder", "text-x-generic", "application-x-executable"];
        let home = self.home_dir.clone();
        for pinned in &self.browser_sources.pinned {
            wanted.push(pinned.icon_name(home.as_deref()));
        }
        wanted.sort_unstable();
        wanted.dedup();
        for name in wanted {
            self.ensure_theme_icon(name);
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
        let mut merged = field_model::merge(&mut self.field, &live);
        merged.extend(field_model::merge_desktop_folder(
            &mut self.field,
            crate::drawers::standard_paths::desktop_dir().as_deref(),
        ));
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
        self.ensure_place_icons();
    }

    /// Ask for icons for the launcher's currently visible rows, a few per frame.
    ///
    /// The list is every installed application — hundreds of rows on a stock system. The work
    /// itself no longer happens here (`ensure_icon` hands it to a blocking thread), so the budget
    /// is no longer protecting the frame from a stall; it now bounds how many resolutions are
    /// *started* at once, so a query that suddenly matches everything does not put three hundred
    /// jobs on the pool for rows nobody will look at. Each arrival wakes the loop, so the list
    /// still fills in over a second or two without this asking for a frame itself.
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
    }

    /// The preview policy (`docs/homefield.md`, "Resource and security implications"): `Off`
    /// asks for nothing; `Static` asks once per window, and again when it is parked; `Live`
    /// asks at the preview rate for every session tile. Arrived previews are filed under the
    /// tile's own identifier. The compositor's own floor of four frames a second per window
    /// holds whatever this asks.
    fn refresh_previews(&mut self, toplevels: &[lege_window::ShellToplevel], covered: bool) {
        let Some(bridge) = &mut self.bridge else {
            return;
        };
        if self.field_settings.previews == PreviewMode::Off || !bridge.previews_available() {
            return;
        }
        // Nothing on the field is being looked at, so nothing on it is captured. The cache is
        // kept, not cleared: that is what makes coming back instant instead of a blank tile
        // filling in a frame or two later.
        if covered {
            self.drain_previews(toplevels);
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
        self.drain_previews(toplevels);
    }

    /// File whatever the compositor has finished capturing and forget the windows that are gone.
    /// Separate from the request policy because a suppressed field still has to accept the frames
    /// already in flight — dropping them on the floor would leave the buffers un-released.
    fn drain_previews(&mut self, toplevels: &[lege_window::ShellToplevel]) {
        let Some(bridge) = &mut self.bridge else {
            return;
        };
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

    #[allow(clippy::too_many_lines)]
    fn redraw_home(&mut self) {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let previews_covered = previews_covered(home_revealed, &toplevels);
        self.refresh_previews(&toplevels, previews_covered);
        let Some(home) = &self.home else { return };
        let scale = home.scale;
        let logical = home
            .window
            .inner_size()
            .to_logical::<f64>(home.window.scale_factor());
        self.refresh_field(logical.width as i32, logical.height as i32, &toplevels);
        let network_control_available = network_control_app(&self.apps).is_some();
        let clock_label = local_clock_label(std::time::SystemTime::now());
        self.flush_search();

        let Some(home) = &mut self.home else { return };
        if !home.prepare() {
            return;
        }
        let (width, height) = (home.buffer.width as i32, home.buffer.height as i32);
        // A search result is shown only while it still answers the question on screen: the same
        // scope and the same query. The staleness guard is the one it always was, keyed on the
        // scope now that a drawer is not a thing.
        let scope = if self.field_screen.files_expanded {
            Scope::Location(self.field_screen.files.location().clone())
        } else {
            Scope::Applications
        };
        let active_search = self
            .search_results
            .as_ref()
            .filter(|(had, _, _)| *had == scope);
        let file_search = self
            .search_results
            .as_ref()
            .and_then(|(_, query, result)| {
                result
                    .as_ref()
                    .ok()
                    .map(|entries| crate::omnibox::FileResults {
                        query,
                        entries: entries.as_slice(),
                    })
            })
            .filter(|_| active_search.is_some());
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
                .map(|(location, hidden, listing)| (location, *hidden, listing)),
            pending: self
                .browser_pending
                .as_ref()
                .map(|(location, hidden)| (location, *hidden)),
            error: self
                .browser_error
                .as_ref()
                .map(|(location, error)| (location, error.as_str())),
            status: self.browser_status.as_deref(),
            search_query,
            search_entries,
            search_error,
            feeds: &self.drawer_feeds,
            thumbnails: &self.file_thumbnails,
            icons: &self.icons,
            home: self.home_dir.as_deref(),
        };
        let recents = self.recent.resolve(&self.apps);
        let data = FieldData {
            notifications: &self.notification_rows,
            field: &self.field,
            toplevels: &toplevels,
            icons: &self.icons,
            previews: &self.previews,
            drawer_feeds: &self.drawer_feeds,
            treemap: self.treemap.as_ref().map(|(_, bitmap)| bitmap),
            composition: &self.composition,
            browser: &browser_data,
            folder_listings: &self.folder_listings,
            connected: self.bridge.is_some(),
            home_revealed,
            network_control_available,
            apps: &self.apps,
            apps_available: self.apps_available,
            widgets: &self.widgets,
            variants: &self.app_variants,
            projects: &self.projects,
            actions: lege_types::action::MANIFEST,
            file_search,
            recents: &recents,
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
        // The browse grid told us during the draw which cells it can see; hand the bounded
        // thumbnail work to the preview service now, off-thread.
        self.spawn_thumbnails();
        self.export_drag_if_lifted();
    }

    #[allow(clippy::too_many_lines)]
    fn perform(&mut self, action: FieldAction) {
        match action {
            FieldAction::DismissNotification(id) => self.spawn_notification_call(move |proxy| {
                Box::pin(async move {
                    proxy.dismiss(id).await?;
                    Ok(())
                })
            }),
            FieldAction::InvokeNotification { id, key } => {
                self.spawn_notification_call(move |proxy| {
                    Box::pin(async move {
                        proxy.invoke(id, &key).await?;
                        Ok(())
                    })
                });
            }

            FieldAction::RunSearch { scope, query } => self.note_search(scope, query),
            FieldAction::Launch(app_id) => {
                // The panel's last row is not an application: it is the intent "search
                // everywhere", and which program that is is a Providers1 choice. Resolved here,
                // where the bus is, rather than in the pure module that composed the row.
                if app_id.as_str() == crate::omnibox::SEARCH_EVERYWHERE {
                    let query = self.location_query.clone();
                    self.search_everywhere(Some(query).filter(|q| !q.is_empty()));
                    return;
                }
                self.record_recent(&app_id);
                self.spawn_launch(app_id);
            }
            // Named on purpose, so it goes straight to `Launcher1.Launch` for that exact copy
            // rather than back through the daemon's default-variant policy.
            FieldAction::LaunchVariant(app_id) => {
                self.record_recent(&app_id);
                self.runtime.spawn(async move {
                    let Ok(connection) = crate::data::session().await else {
                        return;
                    };
                    if let Ok(proxy) = LauncherProxy::new(&connection).await
                        && let Err(error) = proxy.launch(&app_id.to_string(), Vec::new(), "").await
                    {
                        tracing::warn!(%error, %app_id, "launching a chosen copy failed");
                    }
                });
            }
            FieldAction::Activate(window_id) => {
                self.bridge_call("activate", |bridge| bridge.activate(window_id));
            }
            FieldAction::Park(window_id) => {
                self.bridge_call("shelve", |bridge| bridge.shelve(window_id));
            }
            FieldAction::Restore(window_id) => {
                self.bridge_call("unshelve", |bridge| bridge.unshelve(window_id));
            }
            FieldAction::BrowseLocation {
                location,
                show_hidden,
            } => self.spawn_browse(location, show_hidden),
            FieldAction::BrowseFolder(path) => self.spawn_folder_browse(path),
            FieldAction::OpenFolderWindow(path) => self.open_folder_window(&path),
            FieldAction::OpenFile(path) => self.spawn_open_file(path),
            FieldAction::OpenInFiles(path) => match "org.legeos.Files".parse::<AppId>() {
                Ok(app_id) => {
                    self.spawn_launch_with_args(app_id, vec![path.to_string_lossy().into_owned()]);
                }
                Err(error) => tracing::error!(%error, "the Files app id failed to parse"),
            },
            FieldAction::RestoreFromTrash(trashed_path) => self.spawn_trash_restore(trashed_path),
            FieldAction::DropFiles(drop) => self.spawn_file_drop(&drop),
            FieldAction::FileVerb(request) => self.perform_file_verb(&request),
            FieldAction::RunAction(id) => self.perform_declared(id),
            FieldAction::RunWindowAction { id, window } => self.perform_on_window(id, window),
            // `window.group`, which is the one window verb naming two windows and so cannot go
            // through `perform_on_window`.
            FieldAction::JoinWindowGroup { window, other } => {
                self.bridge_call("join_group", |bridge| bridge.join_group(window, other));
            }
            FieldAction::RenameWindowGroup { window, name } => {
                self.bridge_call("set_group_name", |bridge| {
                    bridge.set_group_name(window, &name)
                });
            }
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
                        self.last_field_change_at = Some(Instant::now());
                    }
                }
                Err(error) => tracing::warn!(%error, ?op, "field op refused"),
            },
            // Pinning is a field edit like any other, so it goes back through `perform` as the
            // ops it produced: one route to the document, one undo entry, one thing to persist.
            FieldAction::PinApp(app_id) => {
                let mut doc = self.field.clone();
                for op in field_model::pin_app(&mut doc, &app_id) {
                    self.perform(FieldAction::Apply(op));
                }
            }
            FieldAction::NeedTreemap {
                path,
                width,
                height,
            } => self.spawn_treemap(path, width, height),
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
            FieldAction::ResetField => {
                let (cols, rows) = (self.field.cols, self.field.rows);
                if self.field_synced == Some(true) {
                    self.spawn_field_reset(cols, rows);
                } else {
                    // No service to forget anything: the in-process document is the only copy,
                    // so emptying it is the whole reset. The merge on the next frame refills it.
                    self.field = lege_field::FieldDocument::new(OUTPUT, cols, rows);
                    self.undo.clear();
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
                let has_field_undo = self.field_synced == Some(true) || !self.undo.is_empty();
                if undo_the_file_operation(
                    self.last_file_operation.as_ref(),
                    self.last_field_change_at,
                    has_field_undo,
                ) {
                    // Taken rather than borrowed: an undo is spent whether or not the service
                    // accepts it, and offering the same one twice would be a second undo the
                    // person did not ask for.
                    let undo = self.last_file_operation.take().expect("checked above");
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen
                        .report(format!("Undoing: {}", undo.summary), now_ms);
                    self.spawn_file_undo(undo.op_id);
                } else if self.field_synced == Some(true) {
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

    /// Loads a directory's contents (the live index when a service answers; a bounded direct
    /// scan otherwise), lays it out and rasterizes it at the tile's size, off the event loop. One
    /// load in flight at a time.
    fn spawn_treemap(&mut self, path: PathBuf, width: u32, height: u32) {
        if self.treemap_pending.is_some() {
            return;
        }
        self.treemap_pending = Some(path.clone());
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let entries = load_treemap_entries(&path).await;
            let rasterized =
                tokio::task::spawn_blocking(move || rasterize_entries(&entries, width, height))
                    .await;
            if let Ok((bitmap, composition)) = rasterized {
                let _ = tx.send(DataUpdate::Treemap {
                    path,
                    bitmap,
                    composition,
                });
                let _ = proxy.send_event(Wake);
            }
        });
    }

    fn spawn_launch(&self, app_id: AppId) {
        self.spawn_launch_with_args(app_id, Vec::new());
    }

    fn record_recent(&mut self, app_id: &AppId) {
        self.recent.record(app_id.as_str());
        let json = crate::settings::recent_apps_json(self.recent.ids());
        self.runtime.spawn(async move {
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = SettingsProxy::new(&connection).await else {
                return;
            };
            if let Err(error) = proxy
                .set(crate::settings::RECENT_APPS_KEY, &json, "lege-shell")
                .await
            {
                tracing::debug!(%error, "persisting recent applications");
            }
        });
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
            // "Show all" focuses the search with nothing typed. Recents already sit under the
            // field; typing is what lists everything else.
            "launcher.show-all" => {
                self.field_screen.search.set_text("");
                self.field_screen.omnibox.highlighted = 0;
                self.field_screen.search_focused = true;
            }
            "notifications.clear" => self.spawn_clear_notifications(),
            "settings.open" => self.spawn_settings(),
            "trash.empty" => self.confirm_then_empty_trash(),
            "volume.increase" => self.spawn_volume_step(STEP_PERCENT),
            "volume.decrease" => self.spawn_volume_step(-STEP_PERCENT),
            "volume.mute" => self.spawn_volume_mute(),
            "brightness.increase" => self.spawn_brightness_step(STEP_PERCENT),
            "brightness.decrease" => self.spawn_brightness_step(-STEP_PERCENT),
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
            "pane.new" => {
                let ops = field_model::create_pane(&mut self.field, "Shortcuts");
                if !ops.is_empty() && self.field_synced == Some(true) {
                    self.spawn_field_apply(FieldOp::Batch { ops });
                }
            }
            "windows.restore-arrangement" => {
                self.bridge_call("restore_arrangement", ShellBridge::restore_arrangement);
            }
            id if id.starts_with("windows.arrange.") => {
                // With nothing to arrange the compositor would (correctly) do nothing — but a
                // button that does nothing silently reads as broken, so say why on the field.
                let arrangeable = self.bridge.as_ref().is_some_and(|bridge| {
                    bridge
                        .toplevels()
                        .iter()
                        .any(|toplevel| !toplevel.shelved && !toplevel.fullscreen)
                });
                if !arrangeable {
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen.report(
                        "Nothing to arrange — no windows are open".to_owned(),
                        now_ms,
                    );
                    return;
                }
                let name = id.trim_start_matches("windows.arrange.");
                if let Some(preset) = lege_window::ArrangementPreset::from_name(name) {
                    self.bridge_call("arrange_windows", |bridge| bridge.arrange_windows(preset));
                } else {
                    tracing::warn!(action = %id, "unknown arrangement preset");
                }
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
            // The window-group verbs. Every one is sent to a window and applies to its whole
            // group; the compositor owns membership, so the Home tile only catches up on the next
            // refresh. Against a compositor older than `lege_shell_v1` version 5 the bridge
            // refuses these and says so, rather than sending a request that would end the
            // connection the whole shell is drawn on.
            "window.ungroup" => {
                self.bridge_call("leave_group", |bridge| bridge.leave_group(window));
            }
            "window.activate-group" => {
                self.bridge_call("activate_group", |bridge| bridge.activate_group(window));
            }
            "window.shelve-group" => {
                self.bridge_call("shelve_group", |bridge| bridge.shelve_group(window));
            }
            "window.unshelve-group" => {
                self.bridge_call("unshelve_group", |bridge| bridge.unshelve_group(window));
            }
            other => tracing::warn!(action = %other, "no window route for this action yet"),
        }
    }

    /// Call one `Lifecycle1` method off the event loop, reporting a refusal **on screen** rather
    /// than letting it vanish.
    ///
    /// This function's first version said in its own comment that "shut down did nothing and said
    /// nothing" was the worst outcome here, and then did exactly that: every failure path was a
    /// `tracing::error!` and nothing else, so a session daemon that never registered `Lifecycle1`,
    /// a logind that refused, and a click that missed were indistinguishable from the user's
    /// chair. Each one now names itself in the field's notice strip. The wording says what did not
    /// happen and who did not answer, because "failed" alone tells a person nothing they can act
    /// on.
    fn spawn_lifecycle(&self, id: &'static str) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        let label = action::action(id).map_or(id, |declared| declared.label);
        self.runtime.spawn(async move {
            let report = |text: String| {
                tracing::error!(action = %id, "{text}");
                let _ = tx.send(DataUpdate::Notice(text));
                let _ = proxy_wake.send_event(Wake);
            };
            let connection = match crate::data::session().await {
                Ok(connection) => connection,
                Err(message) => {
                    report(format!("{label} unavailable — no session bus ({message})"));
                    return;
                }
            };
            let proxy = match LifecycleProxy::new(&connection).await {
                Ok(proxy) => proxy,
                Err(error) => {
                    report(format!(
                        "{label} unavailable — the session service is not offering Lifecycle1 \
                         ({error})"
                    ));
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
                report(format!("{label} refused — {error}"));
            }
        });
    }

    /// Open the Settings application through the real launch chain, exactly as the launcher would.
    ///
    /// No special case, no `Command::new`: identity comes from the cgroup the broker puts the
    /// process in, so a shell that started its own child would be launching an application the
    /// session cannot then identify, park or associate with anything.
    fn spawn_settings(&self) {
        match SETTINGS_APP_ID.parse::<AppId>() {
            Ok(app_id) => self.spawn_launch(app_id),
            // Unreachable: the id is a constant. Logged rather than unwrapped, because a panic in
            // the shell is a lost session and this is a button.
            Err(error) => tracing::error!(%error, "the Settings app id is not a valid AppId"),
        }
    }

    fn spawn_network_control(&self) {
        let Some(app_id) = network_control_app(&self.apps) else {
            tracing::warn!("nm-tray2 is not installed in the application registry");
            return;
        };
        self.spawn_launch_with_args(app_id, vec!["--popup".to_owned()]);
    }

    /// How long a toast stays up before it retires on its own.
    const TOAST_LIFETIME: Duration = Duration::from_secs(6);

    /// Create the toast window for a pending notification, if any. The compositor places
    /// Notification-layer surfaces at the top-right of the usable area
    /// (`lege_window::shell_surface_placement`); this side only sizes it and declares its
    /// layer over the trusted-shell protocol.
    fn ensure_toast(&mut self, event_loop: &ActiveEventLoop) {
        let Some(row) = self.pending_toast.take() else {
            return;
        };
        // A new arrival replaces whatever toast is up: newest wins, oldest was already seen.
        self.toast = None;
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Notification")
                .with_decorations(false)
                .with_inner_size(LogicalSize::new(380.0, 92.0)),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::warn!(%error, "creating the notification toast window");
                return;
            }
        };
        if let Some(bridge) = &self.bridge
            && let Err(error) = bridge.set_layer(&window, lege_window::Layer::Notification)
        {
            tracing::warn!(%error, "declaring the toast onto Layer::Notification");
        }
        match HostedWindow::new(window) {
            Ok(hosted) => {
                hosted.request_redraw();
                self.toast = Some((hosted, row, Instant::now()));
            }
            Err(error) => tracing::warn!(%error, "presenting the notification toast"),
        }
    }

    /// Paint the toast: app and summary, and the hint that a click dismisses it.
    fn redraw_toast(&mut self) {
        let Some((hosted, row, _)) = &mut self.toast else {
            return;
        };
        if !hosted.prepare() {
            return;
        }
        let width = i32::try_from(hosted.buffer.width).unwrap_or(1);
        let height = i32::try_from(hosted.buffer.height).unwrap_or(1);
        let area = pixelkit_raster::Rect::new(0, 0, width, height);
        let mut ui = pixelkit_ui::Ui::new(
            pixelkit_raster::Painter::new(&mut hosted.buffer),
            &mut self.text,
            &mut hosted.input,
            crate::surface::design::theme(),
            hosted.scale,
            &mut self.kernel,
        );
        ui.painter
            .fill_rounded_rect(area, ui.px(8), crate::surface::design::SHELL);
        ui.painter.stroke_rect(
            area,
            ui.scale.hairline(),
            crate::surface::design::EDGE_STRONG,
        );
        let inner = area.inset(ui.px(12));
        let (title, body) = inner.split_top(ui.px(22));
        ui.label_styled(
            title,
            &row.app,
            crate::surface::design::measurement_style(&ui),
            crate::surface::design::TEXT,
            pixelkit_text::Align::Left,
        );
        ui.label_styled(
            body,
            &row.summary,
            crate::surface::design::state_style(&ui),
            crate::surface::design::TEXT_DIM,
            pixelkit_text::Align::Left,
        );
        drop(ui);
        hosted.input.end_frame();
        hosted.present();
    }

    /// One notification call, then a widget refresh, off-thread. Shared by the per-item
    /// dismiss and invoke routes so both keep the tile live without duplicating the plumbing.
    fn spawn_notification_call<F>(&self, call: F)
    where
        F: FnOnce(
                lege_client::proxies::NotificationsProxy<'_>,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = zbus::Result<()>> + Send + '_>,
            > + Send
            + 'static,
    {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::NotificationsProxy::new(&connection).await?;
                call(proxy).await.map_err(anyhow::Error::from)
            }
            .await;
            if let Err(error) = result {
                let _ = tx.send(DataUpdate::Notice(format!(
                    "Notification action failed — {error:#}"
                )));
            } else {
                let (snapshot, rows) = crate::data::fetch_notifications_widget().await;
                let _ = tx.send(DataUpdate::Widget("notifications".to_owned(), snapshot));
                let _ = tx.send(DataUpdate::NotificationRows(rows));
            }
            let _ = proxy_wake.send_event(Wake);
        });
    }

    fn spawn_clear_notifications(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::NotificationsProxy::new(&connection).await?;
                proxy.clear_all().await.map_err(anyhow::Error::from)
            }
            .await;
            match result {
                Ok(count) => {
                    let _ = tx.send(DataUpdate::Notice(format!(
                        "Cleared {count} notification{}",
                        if count == 1 { "" } else { "s" }
                    )));
                    let (snapshot, rows) = crate::data::fetch_notifications_widget().await;
                    let _ = tx.send(DataUpdate::Widget("notifications".to_owned(), snapshot));
                    let _ = tx.send(DataUpdate::NotificationRows(rows));
                }
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(format!(
                        "Could not clear notifications — {error:#}"
                    )));
                }
            }
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// Emptying the trash cannot be undone, so the first press arms and says so, and only the
    /// second within [`Self::EMPTY_TRASH_CONFIRM_WINDOW`] performs it.
    fn confirm_then_empty_trash(&mut self) {
        let armed = empty_trash_confirmed(self.empty_trash_armed_at, Instant::now());
        self.empty_trash_armed_at = None;
        if !armed {
            self.empty_trash_armed_at = Some(Instant::now());
            let now_ms = self.started.elapsed().as_millis() as u64;
            self.field_screen.report(
                "Empty the Trash? This cannot be undone — press EMPTY again to confirm.".to_owned(),
                now_ms,
            );
            return;
        }
        self.spawn_trash_call("Empty", |proxy| async move { proxy.empty("").await });
    }

    fn spawn_trash_restore(&self, trashed_path: String) {
        self.spawn_trash_call("Restore", |proxy| async move {
            proxy.restore(vec![trashed_path], "").await
        });
    }

    /// One `Files.Trash1` call off the event loop, reporting what did not happen on screen.
    ///
    /// A refusal here is the interesting case and must never vanish: the service validates every
    /// path it is given, so "that is not in the trash any more" is a real answer a person needs
    /// to see rather than a click that appears to do nothing.
    fn spawn_trash_call<F, Fut>(&self, member: &'static str, call: F)
    where
        F: FnOnce(lege_client::proxies::FilesTrashProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<String>> + Send,
    {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesTrashProxy::new(&connection).await?;
                call(proxy).await.map_err(anyhow::Error::from)
            }
            .await;
            match result {
                // An empty operation ID means there was nothing to do, which `Trash1.Empty`
                // reports rather than treating as an error.
                Ok(op_id) if op_id.is_empty() => {
                    let _ = tx.send(DataUpdate::Notice("The Trash is already empty".to_owned()));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(format!(
                        "Could not {} the Trash — {error:#}",
                        member.to_lowercase()
                    )));
                }
            }
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// Refresh the Trash drawer the moment `Files.Trash1.Changed` says it changed, rather than
    /// on the next three-second tick.
    ///
    /// The signal is what makes the drawer live *whichever client did the mutating* — the file
    /// manager permanently deleting something, or `lege trash restore` in a terminal, and not
    /// only Home's own EMPTY. Losing the connection ends the watch rather than spinning: the
    /// periodic refresh is still there underneath, so the drawer degrades to slow, not stale.
    /// Refetch the notifications the moment the server says they changed — this is what makes
    /// a toast a push rather than a poll: a new arrival reaches the shell on the Changed
    /// signal, not on the next three-second sweep.
    fn spawn_notifications_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = lege_client::proxies::NotificationsProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                let (snapshot, rows) = crate::data::fetch_notifications_widget().await;
                if tx
                    .send(DataUpdate::Widget("notifications".to_owned(), snapshot))
                    .is_err()
                {
                    return;
                }
                let _ = tx.send(DataUpdate::NotificationRows(rows));
                let _ = proxy_wake.send_event(Wake);
            }
        });
    }

    /// Re-read the roles the shell acts on the moment `Providers1.Changed` says one changed —
    /// what makes choosing a terminal in Settings reach the compositor at once rather than on
    /// the next slow sweep.
    fn spawn_providers_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = lege_client::proxies::ProvidersProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                for role in ["terminal", "file-search"] {
                    if tx
                        .send(DataUpdate::Provider {
                            role,
                            app_id: crate::data::selected_provider(role).await,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = proxy_wake.send_event(Wake);
            }
        });
    }

    fn spawn_trash_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = lege_client::proxies::FilesTrashProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                let feed = crate::data::fetch_drawer(Drawer::Trash, 20).await;
                if tx.send(DataUpdate::Drawer(Drawer::Trash, feed)).is_err() {
                    return;
                }
                let _ = proxy_wake.send_event(Wake);
            }
        });
    }

    /// One `Audio1` or `Backlight1` call off the event loop, refreshing that widget the moment it
    /// returns rather than waiting for the next three-second sweep.
    ///
    /// Immediate feedback is the whole point of a level control: a volume button whose number
    /// only moves a beat or two later reads as a button that did not work, and the second press
    /// that follows is one the person did not mean.
    fn spawn_level_step<F, Fut>(&self, label: &'static str, call: F)
    where
        F: FnOnce(zbus::Connection) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send,
    {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                call(connection).await
            }
            .await;
            if let Err(error) = result {
                // A refusal is the interesting case and must not vanish: "this machine has no
                // backlight" is a real answer a person needs to see, not a click that did nothing.
                let _ = tx.send(DataUpdate::Notice(format!("{label} — {error:#}")));
            }
            // One tile reports both levels now, so one refetch serves either step.
            let snapshot = crate::data::fetch_settings_widget().await;
            let _ = tx.send(DataUpdate::Widget("settings".to_owned(), snapshot));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    fn spawn_volume_step(&self, delta: i32) {
        self.spawn_level_step("Volume", move |connection| async move {
            let proxy = lege_client::proxies::AudioProxy::new(&connection).await?;
            proxy.adjust_volume(delta).await?;
            Ok(())
        });
    }

    fn spawn_volume_mute(&self) {
        self.spawn_level_step("Mute", |connection| async move {
            let proxy = lege_client::proxies::AudioProxy::new(&connection).await?;
            proxy.toggle_muted().await?;
            Ok(())
        });
    }

    fn spawn_brightness_step(&self, delta: i32) {
        self.spawn_level_step("Brightness", move |connection| async move {
            let proxy = lege_client::proxies::BacklightProxy::new(&connection).await?;
            proxy.adjust_brightness(delta).await?;
            Ok(())
        });
    }

    /// Perform one ordinary file verb from a Home file surface.
    ///
    /// The whole point of this function is that it is the *only* one: keyboard and pointer both
    /// arrive here, and each verb goes to the same `lege-filed` interface the Files app uses, so
    /// the two surfaces cannot drift into different ideas of what "delete" means.
    #[allow(clippy::too_many_lines)]
    fn perform_file_verb(&mut self, request: &FileRequest) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        match request.verb {
            FileVerb::Copy | FileVerb::Cut => {
                let copied = request.verb == FileVerb::Copy;
                let clipboard = if copied {
                    Clipboard::copy(request.paths.clone())
                } else {
                    Clipboard::cut(request.paths.clone())
                };
                let Some(clipboard) = clipboard else {
                    return;
                };
                // Offered to the rest of the desktop as well as kept here, so Home's copy is
                // pasteable in the Files app and in anything else that reads a uri-list.
                let shared = self.publish_clipboard(&clipboard);
                self.field_screen.report(
                    format!(
                        "{} {} — paste with Ctrl+V{}",
                        if copied { "Copied" } else { "Cut" },
                        items(clipboard.paths().len()),
                        // Said plainly rather than left to be discovered: a copy that only works
                        // inside Home is a different promise from one the whole desktop can take.
                        if shared { "" } else { " (in Home only)" }
                    ),
                    now_ms,
                );
                self.clipboard = Some(clipboard);
            }
            FileVerb::Paste => {
                let Some(directory) = request.directory.as_ref() else {
                    return;
                };
                // Whoever owns the seat selection is what a paste reads. Ctrl+C in a browser
                // then Ctrl+V in Home must paste the browser's files, not the ones Home copied
                // ten minutes ago — and the compositor already told us which by cancelling
                // Home's source, so this is a fact rather than a guess.
                let Some(clipboard) = self.paste_clipboard() else {
                    self.field_screen
                        .report("Nothing to paste".to_owned(), now_ms);
                    return;
                };
                let Some(paste) = clipboard.paste_into(directory) else {
                    return;
                };
                let spent = clipboard.spent_by_paste();
                let kind = serde_json::json!({
                    "kind": if paste.copy { "copy" } else { "move" },
                    "sources": paste.sources,
                    "destination_dir": paste.destination,
                });
                let summary = format!(
                    "{} {} to {}",
                    if paste.copy { "Copy" } else { "Move" },
                    items(paste.sources.len()),
                    paste.destination.display()
                );
                self.spawn_file_operation(kind.to_string(), summary);
                if spent {
                    // A cut is consumed by the paste that honours it: the sources are no longer
                    // where they were, and offering them again would move files that have moved.
                    self.clipboard = None;
                }
            }
            FileVerb::Trash => {
                if request.paths.is_empty() {
                    return;
                }
                // The journalled trash, never an unlink: this is the reversible half of removal
                // and the undo it leaves is the whole reason to prefer it.
                let kind = serde_json::json!({ "kind": "trash", "paths": request.paths });
                let summary = format!("Move {} to Trash", items(request.paths.len()));
                self.spawn_file_operation(kind.to_string(), summary);
            }
            FileVerb::Rename => {
                // The surface only sends this once its dialogue has a name; a rename with none
                // is a request that was never made.
                let (Some(path), Some(new_name)) =
                    (request.paths.first(), request.new_name.as_ref())
                else {
                    return;
                };
                // The same journalled operation the Files app starts, so a rename is undoable
                // from either surface and means one thing on this machine.
                let kind = serde_json::json!({
                    "kind": "rename",
                    "path": path,
                    "new_name": new_name,
                });
                let summary = format!(
                    "Rename {} to {new_name}",
                    path.file_name().map_or_else(
                        || path.display().to_string(),
                        |name| name.to_string_lossy().into_owned()
                    )
                );
                self.spawn_file_operation(kind.to_string(), summary);
            }
            FileVerb::NewFolder => {
                let Some(directory) = request.directory.as_ref() else {
                    return;
                };
                self.spawn_new_folder(directory.clone());
            }
            FileVerb::Properties => {
                let Some(path) = request.paths.first().cloned() else {
                    return;
                };
                self.spawn_properties(path);
            }
            FileVerb::SearchHere => self.search_everywhere(request.query.clone()),
            FileVerb::OpenTerminal => {
                let Some(path) = request.paths.first().cloned() else {
                    return;
                };
                let Some(terminal) = self.terminal_provider.clone() else {
                    self.field_screen.report(
                        "No terminal is selected — choose one in Settings ▸ Providers".to_owned(),
                        now_ms,
                    );
                    return;
                };
                self.spawn_open_terminal(terminal, path);
            }
            FileVerb::CopyPath => {
                let Some(path) = request.paths.first() else {
                    return;
                };
                // The same `Files.Actions1.Render` form the Files app copies, so both surfaces
                // put the identical text on the clipboard for the identical file.
                self.spawn_copy_path(path.clone());
            }
        }
    }

    /// Start one journalled operation for a typed file drop and follow it to its end.
    fn spawn_file_drop(&self, drop: &crate::surface::field_screen::FileDrop) {
        let kind = if drop.copy {
            serde_json::json!({
                "kind": "copy",
                "sources": drop.sources,
                "destination_dir": drop.destination,
            })
        } else {
            serde_json::json!({
                "kind": "move",
                "sources": drop.sources,
                "destination_dir": drop.destination,
            })
        };
        let summary = format!(
            "{} {} to {}",
            if drop.copy { "Copy" } else { "Move" },
            items(drop.sources.len()),
            drop.destination.display()
        );
        self.spawn_file_operation(kind.to_string(), summary);
    }

    /// Start a `Files.Operations1` operation, follow it, and report it once.
    ///
    /// One operation for the whole path set, so a person who moved forty files gets one progress
    /// report, one error if it fails, and one undo. Following it to a terminal state here is what
    /// makes a partial failure visible: the service records which sources it skipped and why it
    /// stopped, and reporting only "started" would turn a half-finished move into apparent
    /// success.
    fn spawn_file_operation(&self, kind_json: String, summary: String) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let outcome = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesOperationsProxy::new(&connection).await?;
                let op_id = proxy
                    .start(&kind_json, "", crate::data::SHELL_APP_ID)
                    .await?;
                let record = crate::data::follow_operation(&proxy, &op_id).await?;
                Ok::<_, anyhow::Error>((proxy, op_id, record))
            }
            .await;
            let update = match outcome {
                Ok((proxy, op_id, record)) => {
                    let notice = crate::data::describe_operation(&summary, &record);
                    // Only offer an undo the service says it still has. `can_undo` is the
                    // service's answer, not a guess: a delete has none, and a partly failed move
                    // may have none either.
                    let undoable = proxy
                        .can_undo(&op_id)
                        .await
                        .is_ok_and(|(allowed, _)| allowed);
                    DataUpdate::FileOperation {
                        undo: undoable.then(|| FileOperationUndo {
                            op_id,
                            summary,
                            at: Instant::now(),
                        }),
                        notice,
                    }
                }
                Err(error) => DataUpdate::FileOperation {
                    undo: None,
                    notice: format!("{summary} — {error:#}"),
                },
            };
            let _ = tx.send(update);
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// Start a `wl_data_device` drag once per lift, when what is lifted is a file.
    ///
    /// Edge-triggered on the lift rather than on the press: a drag started on press would turn
    /// every click on a file tile into a compositor grab and the click would never arrive. Once
    /// per lift rather than per frame for the obvious reason — `start_drag` twice is two drags.
    ///
    /// The field's own drag continues underneath. That is deliberate: the tile has to keep
    /// following the pointer inside Home whatever the compositor is doing with the drag, and a
    /// drop back onto Home is still a Home drop.
    fn export_drag_if_lifted(&mut self) {
        let lifted = self.field_screen.drag_is_lifted();
        if !lifted {
            self.drag_exported = false;
            return;
        }
        if self.drag_exported {
            return;
        }
        let paths = self.field_screen.dragged_file_paths(&self.field);
        // Nothing to export: a window, a widget or a project carries no files, and offering an
        // empty uri-list would be a promise of files that turns out to be a lie.
        let Some(clipboard) = Clipboard::copy(paths) else {
            // Marked as handled anyway, so a non-file drag is not re-examined every frame.
            self.drag_exported = true;
            return;
        };
        let payload = crate::data_device::ClipboardPayload::of(&clipboard);
        self.drag_exported = true;
        let Some(window) = self
            .home
            .as_ref()
            .map(|home| std::sync::Arc::clone(&home.window))
        else {
            return;
        };
        if let Some(bridge) = self.bridge.as_mut()
            && let Err(error) = bridge.start_file_drag(&window, payload)
        {
            tracing::debug!(%error, "could not start a drag for the rest of the desktop");
        }
    }

    /// Offer the clipboard to the rest of the desktop as the seat selection.
    ///
    /// Returns whether the transport took it. `false` is the ordinary no-compositor case — a
    /// nested development session with no trusted socket, or a connection that has seen no input
    /// event and so has no serial to name — and Home's own clipboard still works either way.
    fn publish_clipboard(&mut self, clipboard: &Clipboard) -> bool {
        let payload = crate::data_device::ClipboardPayload::of(clipboard);
        let Some(bridge) = self.bridge.as_mut() else {
            return false;
        };
        match bridge.offer_selection(payload) {
            Ok(offered) => offered,
            Err(error) => {
                tracing::warn!(%error, "could not offer the clipboard to the seat");
                false
            }
        }
    }

    /// The clipboard a paste should read: Home's own, or whatever another application is offering.
    ///
    /// Reading a foreign selection is a synchronous, bounded transfer over a pipe — it happens
    /// here, on the frame thread, because a paste is something a person just asked for and
    /// deferring it a frame would paste into a folder they may already have left.
    fn paste_clipboard(&mut self) -> Option<Clipboard> {
        use crate::data_device::PasteSource;
        let (owns, foreign_available) = self.bridge.as_ref().map_or((false, false), |bridge| {
            (
                bridge.owns_selection(),
                bridge.foreign_selection_available(),
            )
        });
        match crate::data_device::paste_source(owns, self.clipboard.as_ref(), foreign_available) {
            PasteSource::Own => self.clipboard.clone(),
            PasteSource::Foreign => {
                let foreign = self.bridge.as_mut()?.read_foreign_selection();
                match foreign {
                    Ok(Some(selection)) => match selection.mode {
                        crate::clipboard::ClipboardMode::Cut => Clipboard::cut(selection.paths),
                        crate::clipboard::ClipboardMode::Copy => Clipboard::copy(selection.paths),
                    },
                    // Offered but unreadable, or offering something that is not files. Falling
                    // back to Home's own is better than refusing outright.
                    Ok(None) => self.clipboard.clone(),
                    Err(error) => {
                        tracing::warn!(%error, "could not read the selection on offer");
                        self.clipboard.clone()
                    }
                }
            }
            PasteSource::Nothing => None,
        }
    }

    /// `New folder` through the same service the Files app uses.
    ///
    /// The name is chosen here and not asked for, because this surface has no dialogue yet and a
    /// folder called "New folder" that a person then renames is the behaviour every file manager
    /// has. The service refuses a name already taken, and the refusal is reported.
    fn spawn_new_folder(&self, directory: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesBrowseProxy::new(&connection).await?;
                let name = proxy
                    .create_directory(&directory.to_string_lossy(), "New folder")
                    .await?;
                Ok::<_, anyhow::Error>(name)
            }
            .await;
            let notice = match result {
                Ok(name) => format!("Created {name}"),
                Err(error) => format!("Could not create a folder — {error:#}"),
            };
            let _ = tx.send(DataUpdate::Notice(notice));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// Properties, from `Files.Browse1.Inspect` — the same record the Files app's pane shows.
    fn spawn_properties(&self, path: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesBrowseProxy::new(&connection).await?;
                let json = proxy.inspect(&path.to_string_lossy()).await?;
                let info: lege_client::files::FileInfo = serde_json::from_str(&json)?;
                Ok::<_, anyhow::Error>(info)
            }
            .await;
            let notice = match result {
                Ok(info) => format!(
                    "{} · {} · {}",
                    info.name,
                    crate::format::bytes(info.size_bytes),
                    info.canonical_path.unwrap_or(info.path).display()
                ),
                Err(error) => format!("Could not read properties — {error:#}"),
            };
            let _ = tx.send(DataUpdate::Notice(notice));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// `Copy path`, rendered by `Files.Actions1` so every surface copies the identical text.
    ///
    /// Rendering is half of it; the text then has to reach the clipboard, and it does so the
    /// same way the Files app's Copy path does — through `wl-copy`, which takes the selection
    /// as an ordinary client. The shell's own data source only ever offers a path set
    /// (`crate::data_device`), and a path as *text* is for pasting into a terminal or a dialog,
    /// which wants `text/plain` and nothing else.
    fn spawn_copy_path(&self, path: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesActionsProxy::new(&connection).await?;
                let rendered = proxy.render(&path.to_string_lossy(), "canonical").await?;
                copy_text_to_clipboard(&rendered).await?;
                Ok::<_, anyhow::Error>(rendered)
            }
            .await;
            let notice = match result {
                Ok(rendered) => format!("Copied path: {rendered}"),
                Err(error) => format!("Could not copy the path — {error:#}"),
            };
            let _ = tx.send(DataUpdate::Notice(notice));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// Hand a question to the application that searches every file on the machine.
    ///
    /// `--search` is FSearch's own option for a pattern to start with. There is no option to
    /// scope it to a folder — it answers from its own index of everything — so "Search here…"
    /// with nothing typed opens it on that index rather than pretending to narrow it.
    fn search_everywhere(&mut self, query: Option<String>) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let Some(provider) = self.file_search_provider.clone() else {
            self.field_screen.report(
                "No search application is selected — choose one in Settings ▸ Providers".to_owned(),
                now_ms,
            );
            return;
        };
        let arguments = match query {
            Some(query) if !query.trim().is_empty() => {
                vec!["--search".to_owned(), query.trim().to_owned()]
            }
            _ => Vec::new(),
        };
        self.record_recent(&provider);
        self.spawn_launch_with_args(provider, arguments);
    }

    /// Start the selected terminal in the folder this file is in.
    ///
    /// *Which* folder is `Files.Actions1.TerminalDirectory`'s answer, not this shell's: a
    /// directory opens in itself and a file opens in its parent, and the service owning that
    /// rule is what keeps the Files app and Home from disagreeing about where a terminal lands.
    /// `--directory` is the argument, which is kitty's spelling and the one the plan named; a
    /// terminal that spells it differently will need its own form declared per provider.
    fn spawn_open_terminal(&self, terminal: AppId, path: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let actions = lege_client::proxies::FilesActionsProxy::new(&connection).await?;
                let directory = actions.terminal_directory(&path.to_string_lossy()).await?;
                let launcher = LauncherProxy::new(&connection).await?;
                launcher
                    .launch(
                        &terminal.to_string(),
                        vec!["--directory".to_owned(), directory.clone()],
                        "",
                    )
                    .await?;
                Ok::<_, anyhow::Error>(directory)
            }
            .await;
            if let Err(error) = result {
                let _ = tx.send(DataUpdate::Notice(format!(
                    "Could not open a terminal here — {error:#}"
                )));
                let _ = proxy_wake.send_event(Wake);
            }
        });
    }

    /// Reverse a journalled operation through the service that performed it.
    fn spawn_file_undo(&self, op_id: String) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let outcome = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = lege_client::proxies::FilesOperationsProxy::new(&connection).await?;
                let undo_id = proxy.undo(&op_id).await?;
                let record = crate::data::follow_operation(&proxy, &undo_id).await?;
                Ok::<_, anyhow::Error>(record)
            }
            .await;
            let notice = match outcome {
                Ok(record) => crate::data::describe_operation("Undo", &record),
                Err(error) => format!("Could not undo — {error:#}"),
            };
            // An undo that itself leaves an undo would let Ctrl+Z oscillate; the journal keeps
            // the record either way, and `lege files undo` can reach it.
            let _ = tx.send(DataUpdate::FileOperation { undo: None, notice });
            let _ = proxy_wake.send_event(Wake);
        });
    }

    fn spawn_launch_with_args(&self, app_id: AppId, arguments: Vec<String>) {
        // Which copy of an application to start is trust policy, and trust policy lives in the
        // daemon. When the shell knows the logical application it names *that* and lets sessiond
        // pick; only an application it cannot group is launched by its variant id directly.
        let logical = self
            .apps
            .iter()
            .find(|entry| entry.id == app_id)
            .map(|entry| entry.logical.clone())
            .filter(|logical| !logical.is_empty());
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
                    let launched = match &logical {
                        Some(logical) => proxy
                            .launch_logical(logical, "", arguments, "")
                            .await
                            .map(|_| ()),
                        None => proxy
                            .launch(&app_id.to_string(), arguments, "")
                            .await
                            .map(|_| ()),
                    };
                    if let Err(error) = launched {
                        tracing::warn!(%error, %app_id, ?logical, "launching failed");
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

/// The information row's clock, as of now. Shared with `surface::offscreen`, which draws the same
/// field without a running shell.
pub fn clock_label_now() -> String {
    local_clock_label(std::time::SystemTime::now())
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

/// The cushion treemap of `root` at `width`×`height`, plus its composition band. An unreadable
/// or empty directory yields a flat, dim bitmap and no band.
/// Loads one directory's contents for the treemap: the index-backed snapshot when
/// `org.legeos.Files` answers, `lege_treemap::scan` (bounded, off the event loop) when it does
/// not or the call fails. Keeps the shell's own thread from walking the disk on every file tray
/// redraw — the load this exists to avoid is exactly what the fallback still does, deliberately,
/// as the explicit path for "no service available".
async fn load_treemap_entries(path: &std::path::Path) -> Vec<lege_treemap::RawEntry> {
    if let Ok(connection) = crate::data::session().await
        && let Ok(load) = lege_client::tree_source::load_snapshot_tree(&connection, path).await
    {
        return load.entries;
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        lege_treemap::scan(&path, lege_treemap::ScanLimits::default())
    })
    .await
    .unwrap_or_default()
}

/// Lays out and rasterizes an already-loaded directory's contents. An unreadable or empty
/// directory yields a flat, dim bitmap and no band, same as it always has: `entries` is simply
/// short (the root's own entry, or nothing at all).
fn rasterize_entries(
    entries: &[lege_treemap::RawEntry],
    width: u32,
    height: u32,
) -> (pixelkit_raster::Bitmap, Vec<(u32, f32)>) {
    use lege_treemap::{ColorSettings, CushionConfig, LayoutConfig};
    let (width, height) = (width.max(1), height.max(1));
    let tree = lege_treemap::build_tree(entries);
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

/// Put `text` on the Wayland clipboard as `text/plain` through `wl-copy` (a runtime dependency
/// of the shell package), waiting only for it to take the selection: `wl-copy` forks and keeps
/// serving the clipboard after the parent has exited.
///
/// # Errors
/// If `wl-copy` cannot be started, refuses the text, or does not take ownership within a few
/// seconds — a compositor with no data-device support, say.
async fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use tokio::io::AsyncWriteExt as _;
    let mut child = tokio::process::Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain;charset=utf-8")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("starting wl-copy")?;
    let mut stdin = child
        .stdin
        .take()
        .context("wl-copy did not expose standard input")?;
    stdin
        .write_all(text.as_bytes())
        .await
        .context("writing the path to wl-copy")?;
    stdin.shutdown().await.context("closing wl-copy input")?;
    let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
        .await
        .context("wl-copy did not take ownership of the clipboard within 3 seconds")??;
    anyhow::ensure!(
        output.status.success(),
        "wl-copy failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
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

async fn run_search(roots: Vec<String>, query: &str) -> Result<Vec<BrowseEntry>, String> {
    let connection = match crate::data::session().await {
        Ok(connection) => connection,
        Err(message) => return Err(format!("Search — {message}")),
    };
    let proxy = match FilesSearchProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => return Err(format!("Search — service unavailable: {error}")),
    };
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
        self.spawn_trash_watch();
        self.spawn_notifications_watch();
        self.spawn_providers_watch();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _wake: Wake) {
        self.drain_updates();
        self.ensure_toast(event_loop);
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
        let is_toast = self
            .toast
            .as_ref()
            .is_some_and(|(hosted, _, _)| hosted.window.id() == window_id);
        if is_toast {
            match event {
                WindowEvent::RedrawRequested => self.redraw_toast(),
                WindowEvent::CloseRequested => self.toast = None,
                WindowEvent::MouseInput {
                    state: ElementState::Pressed,
                    ..
                } => {
                    // A click acknowledges the toast: dismiss that one notification and let
                    // the tile carry anything else.
                    if let Some((_, row, _)) = self.toast.take() {
                        let id = row.id;
                        self.perform(FieldAction::DismissNotification(id));
                    }
                }
                _ => {}
            }
            return;
        }
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
                let keys = translate_keys(&event.logical_key, event.text.as_deref());
                if !keys.is_empty() {
                    for key in keys {
                        hosted.input.key(key);
                    }
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
        // Answered here rather than on the next paint: the terminal chord is a keypress a person
        // is waiting on, and Home is only redrawn when something asks it to be.
        if self.drain_terminal_requests()
            && let Some(home) = &self.home
        {
            home.request_redraw();
        }
        self.drain_updates();
        self.ensure_toast(event_loop);
        if self
            .toast
            .as_ref()
            .is_some_and(|(_, _, shown)| shown.elapsed() >= Self::TOAST_LIFETIME)
        {
            self.toast = None;
        }
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
        } else if let Some((_, _, shown)) = &self.toast {
            let deadline = *shown + Self::TOAST_LIFETIME;
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline.max(Instant::now())));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(
        id: u64,
        fullscreen: bool,
        maximized: bool,
        shelved: bool,
    ) -> lege_window::ShellToplevel {
        lege_window::ShellToplevel {
            window_id: lege_window::WindowId(id),
            canonical_app_id: None,
            instance_id: None,
            title: "kitty".to_owned(),
            verified_identity: false,
            activated: true,
            fullscreen,
            shelved,
            maximized,
            geometry: lege_window::Rect::new(0, 0, 1920, 1080),
            layer: lege_window::Layer::Normal,
            group: None,
        }
    }

    #[test]
    fn a_window_covering_the_field_stops_its_previews_until_it_stops_covering_it() {
        // Nothing in front of the field: previews run.
        assert!(!previews_covered(false, &[]));
        assert!(!previews_covered(false, &[window(1, false, false, false)]));

        // A translucent fullscreen terminal is exactly the case that reported this: the field
        // behind it was still capturing, and its own preview of the terminal showed through.
        assert!(previews_covered(false, &[window(1, true, false, false)]));
        assert!(previews_covered(false, &[window(1, false, true, false)]));

        // The three ways back. Revealing Home puts the field in front; parking and un-maximizing
        // both stop the window covering it. Each is immediate, and the cache is untouched, so
        // what comes back is the picture that was already there rather than a blank.
        assert!(!previews_covered(true, &[window(1, true, false, false)]));
        assert!(!previews_covered(false, &[window(1, true, false, true)]));
        assert!(!previews_covered(false, &[window(1, false, false, false)]));
    }

    fn file_undo(at: Instant) -> FileOperationUndo {
        FileOperationUndo {
            op_id: "op-1".to_owned(),
            summary: "Move 3 items to /home/u/Documents".to_owned(),
            at,
        }
    }

    #[test]
    fn undo_goes_to_whichever_of_the_two_stacks_was_used_last() {
        let earlier = Instant::now();
        let later = earlier + Duration::from_secs(5);

        // Nothing to undo at all.
        assert!(!undo_the_file_operation(None, None, false));
        assert!(!undo_the_file_operation(None, Some(earlier), true));

        // A file operation and no layout change: Ctrl+Z must put the files back rather than do
        // nothing, which is the common case on a fresh session.
        assert!(undo_the_file_operation(
            Some(&file_undo(earlier)),
            None,
            false
        ));
        assert!(undo_the_file_operation(
            Some(&file_undo(earlier)),
            None,
            true
        ));

        // Both available: whichever happened last wins, because that is what "undo what I just
        // did" means to the person pressing the key.
        assert!(undo_the_file_operation(
            Some(&file_undo(later)),
            Some(earlier),
            true
        ));
        assert!(!undo_the_file_operation(
            Some(&file_undo(earlier)),
            Some(later),
            true
        ));
        // A layout change the field can no longer undo does not get to block the file undo.
        assert!(undo_the_file_operation(
            Some(&file_undo(earlier)),
            Some(later),
            false
        ));
    }

    #[test]
    fn emptying_the_trash_needs_a_second_press_and_the_arming_expires() {
        let armed = Instant::now();
        assert!(
            !empty_trash_confirmed(None, armed),
            "the first press must only arm"
        );
        assert!(empty_trash_confirmed(
            Some(armed),
            armed + Duration::from_secs(1)
        ));
        assert!(
            !empty_trash_confirmed(
                Some(armed),
                armed + EMPTY_TRASH_CONFIRM_WINDOW + Duration::from_secs(1)
            ),
            "an unanswered confirmation must expire, not stay armed"
        );
    }

    fn app_entry(id: &str) -> crate::app_search::AppEntry {
        crate::app_search::AppEntry {
            id: id.parse().unwrap(),
            name: id.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
            logical: String::new(),
            variants: 1,
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
