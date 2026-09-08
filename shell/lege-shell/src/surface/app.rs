//! Wiring: one `winit::application::ApplicationHandler` hosting the Home Surface, the
//! trusted-shell protocol connection that declares its layer, and D-Bus subscriptions that keep
//! its content live. See `window.rs`'s module doc for why this crate drives winit
//! itself rather than `pixelkit_shell::run_app`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use lege_client::proxies::{
    FilesIndexProxy, FilesSearchProxy, LauncherProxy, LifecycleProxy, ProjectsProxy, SettingsProxy,
};
use lege_field::{FieldDocument, FieldOp};
use lege_types::AppId;
use lege_types::action;
use lege_types::provider::ProviderRole;
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
    /// One drawer's refreshed contents after its authority emits a change signal.
    Drawer(Drawer, crate::drawers::DrawerFeed),
    /// Every installed application, with the metadata a launcher needs
    /// (`AppIdentity1.ListApplications`). `None` when the registry did not answer, which the
    /// launcher widget reports rather than drawing an empty list.
    Apps(Option<Vec<crate::app_search::AppEntry>>),
    Projects(Vec<ProjectInfo>),
    /// One widget's refreshed content. Content only: by the field's own rule this moves nothing.
    Widget(String, crate::surface::widgets::WidgetSnapshot),
    /// Charge, volume and backlight for the bar's machine cluster.
    Machine(crate::data::MachineReadings),
    /// The notifications widget's structured rows, beside its generic snapshot: ids and action
    /// keys the per-item dismiss and invoke controls act on.
    NotificationRows(Vec<crate::surface::widgets::notifications::NotificationRow>),
    /// `HomeGarden1.Contexts` by root.
    Garden(HashMap<PathBuf, crate::garden::GardenPlace>),
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
    /// `Lifecycle1.LockRequested`: somebody asked for the session to be locked.
    LockRequested,
    /// What `lege-unlock` said about a password: `Ok` unlocks, `Err` is the message shown.
    Unlock(std::result::Result<(), String>),
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
    /// `HomeField1.Changed` fired: some client (this shell included) changed the document.
    FieldChanged,
    /// The document re-read after a change. Only what is new is taken from it, so an op this
    /// shell applied a moment ago and has not yet heard back about is never undone by the read.
    FieldRemote(FieldDocument),
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
    /// The file authority committed a generation; re-read only open views and file-backed feeds.
    FilesChanged,
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
            .any(lege_window::ShellToplevel::covers_screen)
}

/// Whether the switcher has to collapse to its hint rather than paint over a window
/// (`bar_screen::Presence`).
///
/// Three things decide it, and nothing else:
///
/// - a window that fills the screen (`ShellToplevel::covers_screen`) — maximized or fullscreen.
///   A fullscreen one sorts *above* `Layer::ShellOverlay` (contract Z3), so the bar would be under
///   it whatever it painted; a maximized one does not, but it is just as much the thing the user
///   is working in, and an island parked on top of it is the taskbar this desktop is not.
/// - the pointer resting on the bottom edge (`lege_shell_manager_v1.bar_peeked`), which uncovers
///   it. That is the whole of hover-to-appear, and it has to come from the compositor: under a
///   window filling the screen the pointer never reaches this surface to be seen arriving.
/// - Home being revealed, which uncovers it too: the compositor has lifted Home over the windows,
///   so the bar has Home to paint on and the switcher should simply be visible.
///
/// An ordinary floating window over the bottom of the screen counts for nothing. It is not what
/// the screen is *for* at that moment, the island is a small strip in the middle of the edge, and
/// there is no hover route back out from under one — the compositor's dwell fires only under a
/// window that covers the screen.
fn bar_covered(
    home_revealed: bool,
    peeked: bool,
    toplevels: &[lege_window::ShellToplevel],
) -> bool {
    !home_revealed
        && !peeked
        && toplevels
            .iter()
            .any(lege_window::ShellToplevel::covers_screen)
}

/// When the wall clock next changes the minute the cluster is showing.
///
/// The cluster draws a clock, so it has to wake for the minute even with nothing else happening —
/// and it must wake for *that* minute rather than every sixty seconds from an arbitrary start, or
/// a clock reading 12:04 would sit there for most of 12:05.
fn next_minute() -> Instant {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Instant::now() + Duration::from_secs(60 - now % 60)
}

/// Move a bottom-anchored surface's stored pointer with the surface, after it resized.
///
/// The compositor sends no motion when a surface moves under a pointer that has not moved, and
/// these surfaces grow *upward*: the chips stay exactly where they were on screen, but everything
/// gains height at the top, so a surface-local `y` that was on a chip is now in the empty band
/// above it. Left alone, a motionless mouse resting on a chip would open its row, lose the chip to
/// the growth, close the row, shrink, find the chip again — a cycle of about six hundred
/// milliseconds. Shifting the stored cursor by the height delta is the whole fix, and it is the
/// same arithmetic the compositor would do if it re-sent the event.
fn follow_bottom_anchored_resize(window: &mut HostedWindow) {
    let Some((x, y)) = window.input.cursor() else {
        return;
    };
    let height = window
        .window
        .inner_size()
        .to_logical::<f64>(window.window.scale_factor())
        .height as i32;
    let was = window.buffer.height as i32;
    if height == was || was == 0 {
        return;
    }
    let moved = y + (height - was);
    window.input.cursor_moved(x as f32, moved as f32);
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

/// The two values the bar's coverage buffer holds: desktop, and island. Only the low byte is
/// read back (`previews::ShmBuffer::write_masked`), so this is a greyscale mask that happens to
/// be stored in a colour buffer.
const MASK_CLEAR: u32 = 0x0000_0000;
const MASK_SOLID: u32 = 0x00ff_ffff;

/// Every provider role the shell itself acts on, as `Providers1` names them.
///
/// One list, read by the `Providers1.Changed` watch and its startup snapshot, because two lists
/// is how `file-manager` came to be missing from both: every folder press took
/// `open_in_file_manager`'s "no file manager is selected" branch on a session that had one
/// selected, because nothing ever asked for the role. A role the shell acts on and does not fetch
/// is a control that silently does nothing.
const SHELL_ROLES: [&str; 5] = [
    "terminal",
    "file-search",
    "file-manager",
    "code-editor",
    // The bar's network chip. `network.open-control` has declared
    // `Requires::Provider(NetworkStatusUi)` all along; without the role here the shell never
    // asked who filled it and resolved the control by file name instead.
    "network-status-ui",
];

/// How long an armed EMPTY waits for its second press.
const EMPTY_TRASH_CONFIRM_WINDOW: Duration = Duration::from_secs(8);

/// A reliability backstop for sources that do not yet signal every external change. Normal
/// operation is signal-driven; this is reconciliation, not the shell's observation path.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5 * 60);

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
    /// Who fills each exclusive role the shell acts on, from `Providers1.Report`. A map rather
    /// than a field per role: "open this at the project root" is the same code for a terminal, a
    /// file manager and an editor, and the only difference between them is which role to resolve
    /// and what arguments that program takes.
    providers: BTreeMap<ProviderRole, AppId>,
    /// What `set_terminal` last told the compositor, so an unchanged reading costs no round
    /// trip. `None` until the first read lands; the empty string is a real value meaning "no
    /// terminal is selected", and the compositor needs to be told that too.
    terminal_relayed: Option<String>,
    /// The lock screen's own state, and whether a lock is up. The surfaces belong to the
    /// bridge (`crate::session_lock`); this is what is painted on them.
    lock_screen: crate::surface::lock_screen::LockScreen,
    /// A pointer/keyboard state for the lock screen's painter. The lock screen takes no pointer
    /// input at all — this exists because `Ui` needs one, and a shared one would let a stray
    /// click queued for Home land on the lock.
    lock_input: pixelkit_shell::Input,
    /// The last question asked of a location, so the panel's "Search everywhere" row can hand
    /// it on. The row is composed by a pure module with no bus and no clock; the question it is
    /// about is the one this host asked on its behalf.
    location_query: String,
    home: Option<HostedWindow>,

    text: pixelkit_text::TextCache,
    kernel: RasterKernel,
    /// The bar's coverage, the same size as its colour buffer: black where the surface is
    /// desktop, white where the island is. `bar_screen` records the shapes; this is where they
    /// become the alpha channel the compositor blends with.
    bar_mask: WindowBuffer,
    /// Whether the compositor accepted `set_edge_alignment`, and the island is therefore centred
    /// along the bottom edge rather than sitting at the start of it.
    ///
    /// A compositor older than the request is not an error and not a bug — it is the session the
    /// user is in until they log out — it simply leaves the island at the start of its edge.
    /// Recorded so the log says which of the two the running session got.
    bar_centred: bool,
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
    /// Homegarden's places by root (`HomeGarden1.Contexts`), refreshed after every tending.
    garden: HashMap<PathBuf, crate::garden::GardenPlace>,
    /// Whether Home was revealed at the last frame: the false→true edge is a tending moment.
    home_was_revealed: bool,
    garden_tended_at_start: bool,
    /// The copies of each logical application that has more than one, keyed by logical id.
    /// Refreshed with the application list, and empty for nearly every application there is.
    app_variants: HashMap<String, Vec<(String, String, String)>>,
    /// The user's home directory, read once: the one folder whose icon its own name cannot give
    /// away, and the root every folder window opens relative to.
    home_dir: Option<PathBuf>,
    /// What each open folder window is showing, keyed by the directory. Bounded by how many
    /// folder tiles are on the field, which is bounded by the field.
    folder_listings: HashMap<PathBuf, Result<DirectoryListing, String>>,
    /// Directories a folder window has asked for and not yet received.
    folder_pending: HashSet<PathBuf>,
    /// Previews keyed the way session tiles are (`field_model::session_identifier`).
    previews: HashMap<String, pixelkit_raster::Bitmap>,
    /// When each window's preview was last requested, for the rate policy.
    preview_requested: HashMap<lege_window::WindowId, Instant>,
    /// Each widget's latest content, keyed by `TileKind::System`'s probe.
    widgets: HashMap<String, crate::surface::widgets::WidgetSnapshot>,
    /// What the machine is doing, for the bar's readings chip and its account popover.
    machine: crate::data::MachineReadings,
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
    /// The bottom bar (`crate::surface::bar_screen`): a `Layer::ShellOverlay` surface with a
    /// bottom exclusive zone, absent only while a window is fullscreen.
    bar: Option<HostedWindow>,
    /// The machine cluster's own surface, at the bottom-right corner. Separate from the bar
    /// because it answers a different question and behaves differently: the island is a switcher
    /// that collapses under a window and comes back on a dwell, and this is what the machine is
    /// doing, which stays where it is until a fullscreen window makes it irrelevant.
    cluster: Option<HostedWindow>,
    cluster_screen: crate::surface::cluster_screen::ClusterScreen,
    cluster_mask: WindowBuffer,
    cluster_wanted: (i32, i32),
    bar_screen: crate::surface::bar_screen::BarScreen,
    /// The size the bar surface was last asked for (`bar_screen::wanted_size`). The compositor
    /// never configures this surface's width, so the shell is the only thing that knows it.
    bar_wanted: (i32, i32),
    /// The bridge generation the bar and Home were last drawn for.
    bridge_generation: u64,
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
            providers: BTreeMap::new(),
            terminal_relayed: None,
            lock_screen: crate::surface::lock_screen::LockScreen::default(),
            lock_input: pixelkit_shell::Input::new(),
            location_query: String::new(),
            home: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            bar_mask: WindowBuffer::new(0, 0),
            bar_centred: false,
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
            garden: HashMap::new(),
            home_was_revealed: false,
            garden_tended_at_start: false,
            app_variants: HashMap::new(),
            home_dir: crate::drawers::standard_paths::home_dir(),
            folder_listings: HashMap::new(),
            folder_pending: HashSet::new(),
            previews: HashMap::new(),
            preview_requested: HashMap::new(),
            widgets: HashMap::new(),
            machine: crate::data::MachineReadings::default(),
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
            bar: None,
            bar_screen: crate::surface::bar_screen::BarScreen::default(),
            bar_wanted: (0, 0),
            cluster: None,
            cluster_screen: crate::surface::cluster_screen::ClusterScreen::default(),
            cluster_mask: WindowBuffer::new(0, 0),
            cluster_wanted: (0, 0),
            bridge_generation: 0,
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

    fn spawn_reconciliation_loop(&self) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
            loop {
                interval.tick().await;
                // Signals are the normal path. This infrequent full snapshot recovers from a
                // lost signal, an authority restart, and legacy sources with no signal yet.
                let (drawers, field_settings, projects, browser_sources, apps) = tokio::join!(
                    crate::data::fetch_all(20),
                    fetch_home_field_settings(),
                    crate::data::fetch_projects(),
                    crate::data::fetch_browser_sources(),
                    crate::data::fetch_apps(),
                );
                let (session_widget, notifications_widget, settings_widget, machine) = tokio::join!(
                    crate::data::fetch_session_widget(),
                    crate::data::fetch_notifications_widget(),
                    crate::data::fetch_settings_widget(),
                    crate::data::fetch_machine_readings(),
                );
                let _ = tx.send(DataUpdate::Machine(machine));
                if tx.send(DataUpdate::Drawers(drawers)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::Apps(apps));
                let _ = tx.send(DataUpdate::IdleTimeout(
                    crate::data::fetch_idle_timeout().await,
                ));
                for role in SHELL_ROLES {
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
                let _ = tx.send(DataUpdate::TreemapsStale);
                let _ = tx.send(DataUpdate::FilesChanged);
                let _ = proxy.send_event(Wake);
            }
        });
    }

    /// Re-read the installed applications whenever `AppIdentity1` says the set moved.
    ///
    /// Subscribe before the startup snapshot so a change between the two cannot be missed.
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
            if tx
                .send(DataUpdate::Apps(crate::data::fetch_apps().await))
                .is_err()
            {
                return;
            }
            let _ = proxy.send_event(Wake);
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

    fn spawn_field_fetch_remote(&self, cols: u16, rows: u16) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            if let Some(doc) = crate::data::fetch_field(OUTPUT, cols, rows).await {
                let _ = tx.send(DataUpdate::FieldRemote(doc));
                let _ = proxy.send_event(Wake);
            }
        });
    }

    /// Take what another client did to the field (`field_model::reconcile_remote`): a tile
    /// placed elsewhere, a Place the garden resized, a Place removed, hidden or forgotten.
    fn adopt_remote_tiles(&mut self, remote: &FieldDocument) {
        for op in field_model::reconcile_remote(&self.field, remote) {
            match lege_field::apply(&self.field, &op) {
                Ok((next, _)) => self.field = next,
                Err(error) => {
                    tracing::warn!(%error, op = op.label(), "taking a change made elsewhere");
                }
            }
        }
    }

    /// Re-read the field when `HomeField1.Changed` says another client wrote to it.
    fn spawn_field_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = lege_client::proxies::HomeFieldProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                if tx.send(DataUpdate::FieldChanged).is_err() {
                    return;
                }
                let _ = proxy_wake.send_event(Wake);
            }
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
    /// last told, so re-sending the same id would be a round trip for nothing — the same rule
    /// `DataUpdate::IdleTimeout` follows.
    fn note_provider(&mut self, role: &'static str, app_id: Option<String>) {
        let parsed = app_id.and_then(|id| match id.parse::<AppId>() {
            Ok(app_id) => Some(app_id),
            Err(error) => {
                tracing::warn!(%error, role, "the selected provider is not a valid app id");
                None
            }
        });
        let Some(known) = ProviderRole::ALL.iter().find(|r| r.as_str() == role) else {
            tracing::debug!(role, "a provider nothing here acts on");
            return;
        };
        match parsed.clone() {
            Some(app_id) => self.providers.insert(*known, app_id),
            None => self.providers.remove(known),
        };
        // Only the terminal has a second consumer that has to be told; every other role is
        // recorded above and read where it is needed.
        if role == "terminal" {
            let wire = parsed.as_ref().map_or("", AppId::as_str).to_owned();
            if self.terminal_relayed.as_deref() == Some(wire.as_str()) {
                return;
            }
            // Nothing is recorded as relayed until there is a connection to relay it over, so a
            // shell that started before the compositor answered still tells it once it does
            // rather than believing it already had.
            let Some(bridge) = &self.bridge else {
                return;
            };
            self.terminal_relayed = Some(wire.clone());
            if let Err(error) = bridge.set_terminal(&wire) {
                // Ordinary against a compositor older than version 5: the chord is not there to
                // be told about yet.
                tracing::debug!(%error, "relaying the terminal provider to the compositor");
            }
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
        let Some(app_id) = self.providers.get(&ProviderRole::Terminal).cloned() else {
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

    /// Fetch what one location needs. A feed location needs nothing — its authority-backed
    /// drawer already carries the rows — so only a directory reaches the service.
    fn spawn_browse(&mut self, location: FilesLocation, show_hidden: bool, observe_activity: bool) {
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
            if observe_activity && result.is_ok() {
                crate::garden::observe("directory-opened", &path, "").await;
            }
            let _ = tx.send(DataUpdate::BrowserListing {
                location,
                show_hidden,
                result,
            });
            let _ = proxy.send_event(Wake);
        });
    }

    /// Fetch one folder window's directory once. `Files.Index1.GenerationAdvanced` invalidates
    /// the stored result and triggers another fetch; no folder owns a timer.
    fn spawn_folder_browse(&mut self, path: PathBuf, observe_activity: bool) {
        if self.folder_pending.contains(&path) || self.folder_listings.contains_key(&path) {
            return;
        }
        self.folder_pending.insert(path.clone());
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::data::browse_directory(&path, false).await;
            if observe_activity && result.is_ok() {
                crate::garden::observe("directory-opened", &path, "").await;
            }
            let _ = tx.send(DataUpdate::FolderListing { path, result });
            let _ = proxy.send_event(Wake);
        });
    }

    /// Open a folder in whichever application holds [`ProviderRole::FileManager`].
    ///
    /// Not a hardcoded application. LegeOS's own Files is no longer the file manager — pcmanfm-qt
    /// is — and the role is the only thing that knows which one this session chose, so this takes
    /// the same route `project.open-files` already took (`tool_launch`) rather than naming one.
    fn open_in_file_manager(&mut self, path: &std::path::Path) {
        // `project.open-files` *is* this route's argument shape — the bare directory. Asking
        // [`tool_launch`] for it rather than writing it out a second time is what stops the Place
        // menu's row and a Folder tile from drifting apart, which is the whole of K3.
        let root = path.to_string_lossy().into_owned();
        if let Some((role, arguments)) = tool_launch("project.open-files", &root) {
            self.launch_provider(role, arguments);
        }
    }

    /// Start whichever application holds `role`, with `arguments`, or say that none does.
    ///
    /// The one place a provider role becomes a running program. Every caller that resolved the
    /// role itself wrote the same four lines and the same sentence; two of them had already
    /// started to differ.
    fn launch_provider(&mut self, role: ProviderRole, arguments: Vec<String>) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let Some(app_id) = self.providers.get(&role).cloned() else {
            // The sentence every unfilled role uses, pointing at the same page.
            self.field_screen.report(
                format!("No {role} is selected — choose one in Settings ▸ Providers"),
                now_ms,
            );
            return;
        };
        self.record_recent(&app_id);
        self.spawn_launch_with_args(app_id, arguments);
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
                // Both shapes of the same news: a reconciliation snapshot's whole set, and the
                // one drawer a service's change signal refreshed on its own.
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
                    // failed read; the flag is what the widget reports.
                    None => self.apps_available = false,
                },
                DataUpdate::AppVariants(logical, variants) => {
                    self.app_variants.insert(logical, variants);
                }
                DataUpdate::Projects(projects) => self.projects = projects,
                DataUpdate::Garden(places) => self.garden = places,
                DataUpdate::Widget(id, snapshot) => {
                    self.widgets.insert(id, snapshot);
                }
                DataUpdate::Machine(machine) => {
                    self.machine = machine;
                    // The cluster draws the charge, the volume and the backlight now, and has no
                    // other reason to redraw: without this its readings would sit at whatever they
                    // were when the pointer last touched it.
                    if let Some(cluster) = &self.cluster {
                        cluster.request_redraw();
                    }
                }
                DataUpdate::HomeFieldSettings(settings) => {
                    self.field_settings = settings;
                    self.field_screen.drag_cfg.throw_enabled =
                        settings.throw && !settings.reduced_motion;
                }
                DataUpdate::IdleTimeout(seconds) => {
                    // Only on a change, and only over a live connection: the compositor keeps
                    // whatever it was last told, so re-sending the same number would be a
                    // protocol round trip for nothing.
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
                DataUpdate::LockRequested => self.engage_lock(),
                DataUpdate::Unlock(Ok(())) => {
                    self.lock_screen.accepted();
                    self.release_lock();
                }
                DataUpdate::Unlock(Err(reason)) => {
                    self.lock_screen.rejected(reason);
                    self.paint_lock();
                }
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
                    if !self.garden_tended_at_start {
                        // Session start is a tending moment too, and the field is only now
                        // known to be the daemon's.
                        self.garden_tended_at_start = true;
                        self.spawn_tend();
                    }
                    self.undo.clear();
                }
                DataUpdate::FieldUnavailable => {
                    if self.field_synced.is_none() {
                        self.field_synced = Some(false);
                    }
                }
                DataUpdate::FieldChanged => {
                    if self.field_synced == Some(true) {
                        self.spawn_field_fetch_remote(self.field.cols, self.field.rows);
                    }
                }
                DataUpdate::FieldRemote(doc) => self.adopt_remote_tiles(&doc),
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
                DataUpdate::FilesChanged => {
                    let paths: Vec<PathBuf> = self.folder_listings.keys().cloned().collect();
                    self.folder_listings.clear();
                    for path in paths {
                        self.spawn_folder_browse(path, false);
                    }
                    let browser = self
                        .browser_listing
                        .as_ref()
                        .map(|(location, show_hidden, _)| (location.clone(), *show_hidden));
                    if let Some((location, show_hidden)) = browser {
                        self.browser_pending = None;
                        self.spawn_browse(location, show_hidden, false);
                    }
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
        // One apply for the whole refresh, in the order it was applied locally.
        //
        // These used to be two `spawn_field_apply` calls, and `spawn_field_apply` is
        // fire-and-forget: two independent tasks on two D-Bus calls, with no ordering between
        // them. The resize has to land first — the merge's placements were chosen against the
        // new grid — and when it did not, the daemon refused the batch, the shell kept the state
        // it had already applied to itself, and the two documents were diverged for the rest of
        // the session (visible as a run of `HomeField1.Apply refused` in the journal, because
        // every later sweep then names a tile the daemon never took). One batch is ordered, and
        // is taken or refused whole.
        let mut sending = Vec::new();
        if (cols, rows) != (self.field.cols, self.field.rows) {
            let op = FieldOp::SetGrid { cols, rows };
            match lege_field::apply(&self.field, &op) {
                Ok((next, _)) => {
                    self.field = next;
                    sending.push(op);
                }
                Err(error) => tracing::warn!(%error, "resizing the field grid"),
            }
        }
        let live = LiveState {
            toplevels,
            projects: &self.projects,
        };
        sending.extend(field_model::merge(&mut self.field, &live));
        sending.extend(field_model::merge_desktop_folder(
            &mut self.field,
            crate::drawers::standard_paths::desktop_dir().as_deref(),
        ));
        if !sending.is_empty() && self.field_synced == Some(true) {
            self.spawn_field_apply(FieldOp::Batch { ops: sending });
        }
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
        let wanted: Vec<String> = crate::app_search::search(&self.apps, &query, self.recent.ids())
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

    /// The preview policy (`@legeos.decision.home-field-stable-bounded-semantic-grammar`): `Off`
    /// asks for nothing; `Static` asks once per window, and again when it is parked; `Live`
    /// asks at the preview rate for every session tile. Arrived previews are filed under the
    /// tile's own identifier. The compositor's own floor of four frames a second per window
    /// holds whatever this asks.
    /// Capture the windows of the bar's open hover row, at most twice a second each. The rate
    /// policy is `preview_requested`, the same map Home's tiles use — a row that is open is a
    /// deliberate look, so it refreshes; a row that is closed asks for nothing. Draining is
    /// Home's (`drain_previews`), woken by `has_previews_ready`.
    fn refresh_row_previews(
        &mut self,
        toplevels: &[lege_window::ShellToplevel],
        wants: &[lege_window::WindowId],
    ) {
        if wants.is_empty() {
            return;
        }
        let Some(bridge) = &mut self.bridge else {
            return;
        };
        if self.field_settings.previews == PreviewMode::Off || !bridge.previews_available() {
            return;
        }
        let now = Instant::now();
        for window in wants {
            if !toplevels.iter().any(|t| t.window_id == *window) {
                continue;
            }
            if self
                .preview_requested
                .get(window)
                .is_some_and(|at| now.duration_since(*at) < Duration::from_millis(500))
            {
                continue;
            }
            let Some(identifier) = bridge.identifier_of(*window).map(str::to_owned) else {
                continue;
            };
            match bridge.request_preview(&identifier) {
                Ok(()) => {
                    self.preview_requested.insert(*window, now);
                }
                Err(error) => tracing::debug!(%error, "row preview request skipped"),
            }
        }
    }

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
        if home_revealed && !self.home_was_revealed {
            // A reveal is the moment the user asks what is current: a tending moment.
            self.spawn_tend();
        }
        self.home_was_revealed = home_revealed;
        let previews_covered = previews_covered(home_revealed, &toplevels);
        self.refresh_previews(&toplevels, previews_covered);
        let Some(home) = &self.home else { return };
        let scale = home.scale;
        let logical = home
            .window
            .inner_size()
            .to_logical::<f64>(home.window.scale_factor());
        // The bar covers the bottom strip of the output, on Home too; the field ends above it.
        let bar_inset = if self.bar.is_some() {
            crate::surface::bar_screen::BAR_HEIGHT
        } else {
            0
        };
        self.refresh_field(
            logical.width as i32,
            logical.height as i32 - bar_inset,
            &toplevels,
        );
        self.flush_search();

        let Some(home) = &mut self.home else { return };
        if !home.prepare() {
            return;
        }
        // `bar_inset` is logical, and the buffer is physical: on a scaled output the two are not
        // the same number, and subtracting the logical one from a physical height left the field
        // painting into the strip the bar covers.
        let (width, height) = (
            home.buffer.width as i32,
            home.buffer.height as i32 - scale.px(bar_inset),
        );
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
            apps: &self.apps,
            apps_available: self.apps_available,
            widgets: &self.widgets,
            variants: &self.app_variants,
            projects: &self.projects,
            garden: &self.garden,
            actions: lege_types::action::MANIFEST,
            file_search,
            recents: &recents,
            recent_ids: self.recent.ids(),
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
        if let Some(bar) = &self.bar {
            bar.request_redraw();
        }

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
            } => self.spawn_browse(location, show_hidden, true),
            FieldAction::BrowseFolder(path) => self.spawn_folder_browse(path, true),
            FieldAction::OpenFile(path) => self.spawn_open_file(path),
            FieldAction::OpenInFiles(path) => self.open_in_file_manager(&path),
            FieldAction::RestoreFromTrash(trashed_path) => self.spawn_trash_restore(trashed_path),
            FieldAction::DropFiles(drop) => self.spawn_file_drop(&drop),
            FieldAction::FileVerb(request) => self.perform_file_verb(&request),
            FieldAction::RunAction(id) => self.perform_declared(id),
            FieldAction::Project { id, project_id } => self.perform_project(id, &project_id),
            FieldAction::Place { id, path } => self.perform_place(id, &path),
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

    /// Enter a project, restore its workspace, or open one of its tools at its root.
    ///
    /// Entering and restoring are deliberately different requests. `Projects1.SetCurrent` selects
    /// the context and launches nothing; `Projects1.Open` launches what the project remembers. A
    /// person who wanted to say "I am working on this now" and got eight windows has been
    /// answered with something they did not ask for, so the two are never the same row.
    ///
    /// A tool opens at the project's **real** root, from `Projects1`'s own record — never a
    /// directory this shell inferred from the project's name.
    fn perform_project(&mut self, id: &'static str, project_id: &str) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let root = self
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .map(|project| project.root.clone())
            .unwrap_or_default();
        if let Some(request) = project_request(id) {
            self.spawn_project_request(id, project_id.to_owned(), request);
            return;
        }
        let Some((role, arguments)) = tool_launch(id, &root) else {
            return;
        };
        if root.is_empty() {
            self.field_screen
                .report(format!("{project_id} has no folder to open"), now_ms);
            return;
        }
        let Some(app_id) = self.providers.get(&role).cloned() else {
            // The same sentence the terminal chord uses, pointing at the same page: a role
            // nobody fills is a choice the person has not made yet, not a failure.
            self.field_screen.report(
                format!("No {role} is selected — choose one in Settings ▸ Providers"),
                now_ms,
            );
            return;
        };
        self.record_recent(&app_id);
        self.spawn_launch_with_args(app_id, arguments);
    }

    /// A Place's tools and its gardening (`@legeos.decision.homegarden-activity-grown-places`).
    /// The tools are the project tools with the folder itself as root; the garden verbs go to
    /// `HomeGarden1`, and `garden.why` answers on the field's notice line.
    fn perform_place(&mut self, id: &'static str, path: &Path) {
        if let Some(project_id) = id.strip_prefix("place.") {
            // A folder is a folder: this row is the same press as a Folder tile and takes the
            // same route, rather than resolving the `FileManager` role a second time with its own
            // copy of the argument shape. One route means one thing to get right.
            if project_id == "open-files" {
                self.open_in_file_manager(path);
                return;
            }
            let project_id: &'static str = match project_id {
                "open-terminal" => "project.open-terminal",
                "open-editor" => "project.open-editor",
                _ => return,
            };
            let root = path.to_string_lossy().into_owned();
            if let Some((role, arguments)) = tool_launch(project_id, &root) {
                self.launch_provider(role, arguments);
            }
            return;
        }
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        let path = path.to_path_buf();
        self.runtime.spawn(async move {
            match crate::garden::manage(id, &path).await {
                Ok(Some(answer)) => {
                    let _ = tx.send(DataUpdate::Notice(answer));
                }
                Ok(None) => {
                    // The record changed; the tile follows at the next tending moment, which is
                    // now: the person asked, and waiting for the next reveal would read as
                    // "nothing happened".
                    let _ = crate::garden::tend(OUTPUT).await;
                    let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
                }
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(error));
                }
            }
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// One tending moment: `HomeGarden1.Tend`, then the places for the tiles to draw. The field
    /// itself arrives through `HomeField1.Changed` like any other change.
    fn spawn_tend(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            if let Err(error) = crate::garden::tend(OUTPUT).await {
                tracing::debug!(%error, "tending skipped");
            }
            let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// `Projects1.SetCurrent` or `Projects1.Open`, whichever the row asked for.
    fn spawn_project_request(&self, id: &'static str, project_id: String, request: ProjectRequest) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = ProjectsProxy::new(&connection).await?;
                match request {
                    ProjectRequest::SetCurrent => proxy.set_current(&project_id).await?,
                    ProjectRequest::Open => proxy.open(&project_id).await?,
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = tx.send(DataUpdate::Notice(format!("{id} failed — {error:#}")));
                let _ = proxy_wake.send_event(Wake);
            }
        });
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
            "clock.open-calendar" => self.spawn_calendar(),
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
            "garden.tend" => self.spawn_tend(),
            "garden.forget-all" => self.perform_place(id, Path::new("/")),
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
    /// Open Settings — or, if it is already open, go to the copy that is.
    ///
    /// Settings is one place, not a document: a second window of it is a second set of the same
    /// switches, and pressing the chip twice was opening one. So the running window wins. This is
    /// deliberately not a general rule — a second terminal or a second file window is a normal
    /// thing to want — which is why it lives here rather than in the launcher.
    fn spawn_settings(&mut self) {
        let Ok(app_id) = SETTINGS_APP_ID.parse::<AppId>() else {
            // Unreachable: the id is a constant. Logged rather than unwrapped, because a panic in
            // the shell is a lost session and this is a button.
            tracing::error!("the Settings app id is not a valid AppId");
            return;
        };
        let open = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default()
            .into_iter()
            .find(|window| window.canonical_app_id.as_ref() == Some(&app_id));
        match open {
            Some(window) => {
                if window.shelved {
                    self.perform(FieldAction::Restore(window.window_id));
                }
                self.perform(FieldAction::Activate(window.window_id));
            }
            None => self.spawn_launch(app_id),
        }
    }

    /// Open the network control: the application that fills the `network-status-ui` role, with
    /// `--popup`.
    ///
    /// The role, never a file called `nm-tray`. Stock `nm-tray` is packaged by Ubuntu, declares no
    /// `X-LegeOS-Provides`, ignores `--popup` and only ever draws a tray icon — and this desktop
    /// has no tray, so resolving it by name produced a chip that launched something invisible.
    /// `legeos-nm-tray` (`distro/forks/nm-tray-2/`) is the build that answers `--popup` and
    /// declares the role; until it is installed the role is empty and the chip is drawn dimmed.
    fn spawn_network_control(&self) {
        let Some(app_id) = self.providers.get(&ProviderRole::NetworkStatusUi).cloned() else {
            tracing::warn!(
                "no application fills the network-status-ui role — install legeos-nm-tray \
                 (distro/forks/nm-tray-2); stock nm-tray declares no X-LegeOS-Provides and \
                 ignores --popup, and this desktop has no tray for it to draw into"
            );
            return;
        };
        self.spawn_launch_with_args(app_id, vec!["--popup".to_owned()]);
    }

    /// Open the installed calendar, through the same launch chain as everything else.
    fn spawn_calendar(&self) {
        let Some(app_id) = calendar_app(&self.apps) else {
            tracing::warn!(
                "no installed application declares Categories=Calendar — install gsimplecal"
            );
            return;
        };
        self.spawn_launch(app_id);
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

    /// Lock the session: take the compositor's lock and paint the screen onto it.
    ///
    /// Only the compositor can actually lock — this drives `ext-session-lock-v1` on the trusted
    /// connection (`crate::session_lock`), which is why the shell is where this lives and
    /// sessiond only emits `Lifecycle1.LockRequested`. A compositor that offers no lock manager
    /// is told about rather than pretended past: a lock screen that did not lock would be worse
    /// than none, because it looks locked.
    fn engage_lock(&mut self) {
        let now_ms = self.started.elapsed().as_millis() as u64;
        let Some(bridge) = &mut self.bridge else {
            self.field_screen
                .report("Cannot lock: no compositor connection".to_owned(), now_ms);
            return;
        };
        if bridge.lock_engaged() {
            return;
        }
        if !bridge.lock_available() {
            self.field_screen.report(
                "Cannot lock: this compositor offers no session lock".to_owned(),
                now_ms,
            );
            return;
        }
        self.lock_screen.accepted();
        if let Err(error) = bridge.lock() {
            tracing::warn!(%error, "taking the session lock");
            self.field_screen
                .report(format!("Could not lock the session — {error:#}"), now_ms);
            return;
        }
        self.paint_lock();
    }

    /// Give the compositor's lock back. The only thing that unlocks the session.
    fn release_lock(&mut self) {
        if let Some(bridge) = &mut self.bridge
            && let Err(error) = bridge.unlock()
        {
            tracing::warn!(%error, "releasing the session lock");
        }
        // Only this process knows the lock is gone, and session time is counted from that edge.
        self.runtime.spawn(crate::session_lock::notify_unlocked());
    }

    /// Draw the lock screen onto every lock surface.
    ///
    /// The bridge is moved out for the duration because painting needs this app's text cache and
    /// raster kernel at the same time as the bridge's surfaces, and they are all fields of the
    /// same struct.
    fn paint_lock(&mut self) {
        let Some(mut bridge) = self.bridge.take() else {
            return;
        };
        let user = std::env::var("USER").unwrap_or_else(|_| "Locked".to_owned());
        let clock = local_clock_label(std::time::SystemTime::now());
        let result = bridge.paint_lock(|width, height| {
            let mut buffer = pixelkit_raster::WindowBuffer::new(width, height);
            {
                let mut ui = Ui::new(
                    Painter::new(&mut buffer),
                    &mut self.text,
                    &mut self.lock_input,
                    self.theme,
                    pixelkit_shell::Scale::ONE,
                    &mut self.kernel,
                );
                let area = Rect::new(0, 0, width as i32, height as i32);
                self.lock_screen.draw(&mut ui, area, &user, &clock);
            }
            self.lock_input.end_frame();
            buffer.pixels
        });
        if let Err(error) = result {
            tracing::warn!(%error, "painting the lock screen");
        }
        self.bridge = Some(bridge);
    }

    /// Feed the lock surfaces' keys to the lock screen, and act on what it asks for.
    ///
    /// Returns whether anything changed, so the caller repaints only when it did — a lock screen
    /// redrawn on every wake is a lock screen burning a core all night.
    fn drain_lock_keys(&mut self) -> bool {
        use crate::session_lock::LockKey;

        let Some(bridge) = &mut self.bridge else {
            return false;
        };
        if !bridge.lock_engaged() {
            return false;
        }
        if bridge.lock_finished() {
            // The compositor refused the lock or ended it; there is nothing to hold up.
            tracing::warn!("the session lock finished without being granted; tearing it down");
            self.release_lock();
            return false;
        }
        let keys = bridge.take_lock_keys();
        let needs_paint = bridge.lock_needs_paint();
        if keys.is_empty() && !needs_paint {
            return false;
        }
        let mut request = None;
        for key in keys {
            match key {
                LockKey::Text(character) => self.lock_screen.typed(character),
                LockKey::Backspace => self.lock_screen.backspace(),
                LockKey::Escape => self.lock_screen.clear(),
                LockKey::Enter => request = self.lock_screen.submit().or(request),
            }
        }
        if let Some(crate::surface::lock_screen::LockRequest::Authenticate(password)) = request {
            self.spawn_unlock(password);
        }
        true
    }

    /// Ask `lege-unlock` whether this password is the person's.
    ///
    /// A separate process on purpose: PAM wants to be linked into something small and
    /// short-lived, and the shell is neither. The password goes over its stdin — never an
    /// argument, which would put it in `/proc/<pid>/cmdline` for every process on the machine to
    /// read — and the helper answers with its exit status alone.
    fn spawn_unlock(&self, password: String) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = crate::unlock::authenticate(password).await;
            let _ = tx.send(DataUpdate::Unlock(result));
            let _ = proxy_wake.send_event(Wake);
        });
    }

    /// The output's logical size, taken from the Home surface — which the compositor sizes to
    /// the whole output, so it is the one thing in this process that knows how big the screen is.
    /// The bar needs the width because the compositor no longer forces a zone-holding surface to
    /// the output width (whatever this surface asks for is what it gets), and the height to know
    /// where the bottom strip is when it asks whether a window covers it.
    fn output_size(&self) -> (i32, i32) {
        self.home.as_ref().map_or((800, 600), |home| {
            let logical = home
                .window
                .inner_size()
                .to_logical::<f64>(home.window.scale_factor());
            (logical.width as i32, logical.height as i32)
        })
    }

    /// Just the width, for the callers that only size the bar.
    fn output_width(&self) -> i32 {
        self.output_size().0
    }

    /// Create the bottom bar once the compositor connection is up. Both claims are declared
    /// before the first commit maps the surface, so the compositor never sees it as an app window.
    ///
    /// The bar used to destroy itself while a window was fullscreen, because `Layer::ShellOverlay`
    /// is above `Layer::Normal` and would otherwise cover the video. That is the compositor's
    /// question now: a visible fullscreen window sorts above the overlay on its own output
    /// (`WindowService::stacking_order`). Destroying the surface released its exclusive zone,
    /// which reflowed every maximized window on that output twice — once going in, once coming
    /// out. The zone is 0 now, so there is nothing left to reflow either way.
    fn ensure_bar(
        &mut self,
        event_loop: &ActiveEventLoop,
        toplevels: &[lege_window::ShellToplevel],
    ) {
        if self.bar.is_some() {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let wanted = crate::surface::bar_screen::wanted_size(
            crate::surface::bar_screen::Presence::Open,
            None,
            false,
            crate::taskbar::entries(toplevels).len(),
            self.output_width(),
        );
        let (width, height) = (f64::from(wanted.0), f64::from(wanted.1));
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Bar")
                .with_decorations(false)
                // Declared, not merely implied by `present_masked`'s `set_opaque_region(None)`:
                // winit re-sets a full opaque region from its own `resize` on every configure, and
                // relying on which of the two lands last is relying on an ordering nothing
                // promises. This is the one line that makes the surface transparent by contract.
                .with_transparent(true)
                .with_inner_size(LogicalSize::new(width, height)),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::warn!(%error, "creating the bar window");
                return;
            }
        };
        if let Err(error) = bridge.set_layer(&window, lege_window::Layer::ShellOverlay) {
            tracing::warn!(%error, "declaring the bar onto Layer::ShellOverlay");
        }
        // A zone of 0, and the claim is still made: it is what anchors the surface to the bottom
        // edge (its bottom on the output's bottom, its left end at x=0, its own committed
        // height), and it is the difference between a bar that is at the bottom of the screen and
        // a floating window that happens to be near it. Reserving *nothing* is the point — the
        // output's usable area stays whole, so a maximized window fills the screen and wears its
        // own tags and controls, and the bar paints over Home rather than pushing it up.
        if let Err(error) =
            bridge.set_exclusive_zone(&window, lege_window::geometry::Edge::Bottom, 0)
        {
            tracing::warn!(%error, "anchoring the bar to the bottom edge");
        }
        // Centred along that edge, not at its start. Two reasons, and the second is the one that
        // matters: an island the width of its contents sitting hard against the bottom-left corner
        // is a dock in the corner rather than a dock, and — the corner is Home's now. The
        // compositor suppresses the hot corner while a shell overlay is under the pointer, and the
        // island's collapsed hint tab is 72×6 at exactly that corner, so an island left at the
        // start of its edge would sit on the corner and stop it firing at all.
        self.bar_centred = match bridge.set_edge_alignment(
            &window,
            lege_window::geometry::Edge::Bottom,
            lege_window::geometry::Alignment::Centre,
        ) {
            Ok(centred) => centred,
            Err(error) => {
                tracing::warn!(%error, "centring the bar on the bottom edge");
                false
            }
        };
        match HostedWindow::new(window) {
            Ok(hosted) => {
                hosted.request_redraw();
                self.bar = Some(hosted);
                self.bar_wanted = wanted;
            }
            Err(error) => tracing::warn!(%error, "presenting the bar"),
        }
    }

    /// Paint the bar and act on what was clicked. The window list is the bridge's; the entries
    /// are computed fresh each frame (`crate::taskbar::entries`), which is a few dozen windows at
    /// most.
    #[allow(clippy::too_many_lines)]
    fn redraw_bar(&mut self) {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        let entries = crate::taskbar::entries(&toplevels);
        let keys: Vec<String> = entries.iter().filter_map(|e| e.app_id.clone()).collect();
        for key in keys {
            self.ensure_icon(&key);
        }
        let activated = toplevels.iter().find(|t| t.activated).map(|t| t.window_id);
        let now_ms = self.started.elapsed().as_millis() as u64;
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let output_width = self.output_width();
        let covered = bar_covered(
            home_revealed,
            self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked),
            &toplevels,
        );
        let Some(bar) = &mut self.bar else {
            return;
        };
        if !bar.prepare() {
            return;
        }
        let area =
            pixelkit_raster::Rect::new(0, 0, bar.buffer.width as i32, bar.buffer.height as i32);
        let data = crate::surface::bar_screen::BarData {
            entries: &entries,
            icons: &self.icons,
            previews: &self.previews,
            activated,
            covered,
            home_revealed,
            now_ms,
        };
        let actions = {
            let mut ui = Ui::new(
                Painter::new(&mut bar.buffer),
                &mut self.text,
                &mut bar.input,
                self.theme,
                bar.scale,
                &mut self.kernel,
            );
            self.bar_screen.draw(&mut ui, area, &data)
        };
        bar.input.end_frame();
        // The island is a shape, not a rectangle. Paint its coverage — the shapes the draw just
        // recorded, white on black, anti-aliased — and hand both buffers to the compositor as one
        // `Argb8888` frame, so the desktop shows through beside and between the chips.
        //
        // Every failure here falls back to the opaque present, which is the bar as it was before
        // any of this: no wl_shm, an older compositor, a window that is not on Wayland at all.
        // A bar that cannot float is still a bar.
        if self.bar_mask.width != bar.buffer.width || self.bar_mask.height != bar.buffer.height {
            self.bar_mask = WindowBuffer::new(bar.buffer.width, bar.buffer.height);
        }
        let shapes = self.bar_screen.opaque_shapes();
        let floated = !shapes.is_empty();
        if floated {
            let mut painter = Painter::new(&mut self.bar_mask);
            painter.fill_rect(pixelkit_raster::Rect::new(0, 0, area.w, area.h), MASK_CLEAR);
            for (rect, radius) in shapes {
                painter.fill_rounded_rect_aa(
                    &mut self.kernel,
                    *rect,
                    *radius as f32,
                    MASK_SOLID,
                    u8::MAX,
                );
            }
        }
        // The same shapes again, as plain rectangles: the compositor's input region takes no
        // radius, so a chip's four corner pixels stay clickable. Four pixels of slack on a target
        // that is already rounded is not worth a region per arc.
        let regions: Vec<(i32, i32, i32, i32)> = self
            .bar_screen
            .opaque_shapes()
            .iter()
            .map(|(rect, _)| (rect.x, rect.y, rect.w, rect.h))
            .collect();
        let presented = floated
            && self.bridge.as_mut().is_some_and(|bridge| {
                match bridge.present_masked(
                    &bar.window,
                    &bar.buffer.pixels,
                    &self.bar_mask.pixels,
                    &regions,
                    bar.buffer.width,
                    bar.buffer.height,
                ) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::debug!(%error, "the bar could not float; presenting it opaque");
                        false
                    }
                }
            });
        if !presented {
            bar.present();
        }
        // The hover row shows the window, not its name, so the windows in the open row are
        // captured while it is open — and nothing else is. Home's own policy stops at
        // `previews_covered`, which is exactly when this row is the thing being looked at.
        let wants: Vec<lege_window::WindowId> = self
            .bar_screen
            .preview_wants(&entries)
            .iter()
            .map(|window| window.id)
            .collect();
        self.refresh_row_previews(&toplevels, &wants);
        // The surface is the output's width, and its height is whatever the presence the draw
        // just settled on asks for: the hint under a window, the bar over Home, plus whatever
        // band an open hover row or the account popover needs. The exclusive zone is 0 through
        // all of it, so no size this surface takes reflows anything.
        let wanted = crate::surface::bar_screen::wanted_size(
            self.bar_screen.presence(covered),
            self.bar_screen.row_windows(&entries),
            self.bar_screen.tooltip_open(now_ms),
            entries.len(),
            output_width,
        );
        if wanted != self.bar_wanted {
            self.bar_wanted = wanted;
            if let Some(bar) = &self.bar {
                let _ = bar
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
        }
        for action in actions {
            use crate::surface::bar_screen::BarAction;
            match action {
                BarAction::Activate(window) => self.perform(FieldAction::Activate(window)),
                BarAction::Window { id, window } => {
                    self.perform(FieldAction::RunWindowAction { id, window });
                }
                BarAction::Field(action) => self.perform(action),
            }
        }
    }

    /// Create, resize or destroy the cluster's surface.
    ///
    /// Destroyed outright while any window is fullscreen — the user's rule, and the honest way to
    /// say it: not hidden, not collapsed to a hint, not there. Rebuilt when the fullscreen window
    /// goes. Its zone is 0 like the island's, so neither creating nor destroying it moves a window.
    fn ensure_cluster(
        &mut self,
        event_loop: &ActiveEventLoop,
        toplevels: &[lege_window::ShellToplevel],
    ) {
        let wanted_gone = toplevels
            .iter()
            .any(lege_window::ShellToplevel::covers_screen);
        if wanted_gone {
            self.cluster = None;
            self.cluster_wanted = (0, 0);
            // And with it whatever was open on it. The screen's state does not survive the screen:
            // a popover left open when something went fullscreen would otherwise be waiting when
            // the window closed, over a desktop nobody had asked it about.
            self.cluster_screen = crate::surface::cluster_screen::ClusterScreen::default();
            return;
        }
        if self.cluster.is_some() {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let wanted = crate::surface::cluster_screen::wanted_size(false, self.output_width());
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Cluster")
                .with_decorations(false)
                .with_transparent(true)
                .with_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1))),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::warn!(%error, "creating the cluster window");
                return;
            }
        };
        if let Err(error) = bridge.set_layer(&window, lege_window::Layer::ShellOverlay) {
            tracing::warn!(%error, "declaring the cluster onto Layer::ShellOverlay");
        }
        if let Err(error) =
            bridge.set_exclusive_zone(&window, lege_window::geometry::Edge::Bottom, 0)
        {
            tracing::warn!(%error, "anchoring the cluster to the bottom edge");
        }
        // The *end* of the edge, where the island asks for the middle of it.
        if let Err(error) = bridge.set_edge_alignment(
            &window,
            lege_window::geometry::Edge::Bottom,
            lege_window::geometry::Alignment::End,
        ) {
            tracing::warn!(%error, "anchoring the cluster to the right-hand end");
        }
        match HostedWindow::new(window) {
            Ok(hosted) => {
                self.cluster = Some(hosted);
                self.cluster_wanted = wanted;
            }
            Err(error) => tracing::warn!(%error, "hosting the cluster window"),
        }
    }

    /// Paint the cluster and act on what it returns.
    #[allow(clippy::too_many_lines)]
    fn redraw_cluster(&mut self) {
        let network_control_available = self.providers.contains_key(&ProviderRole::NetworkStatusUi);
        let calendar_available = calendar_app(&self.apps).is_some();
        let account_label = std::env::var("USER")
            .ok()
            .filter(|user| !user.is_empty())
            .unwrap_or_else(account_uid_label);
        let clock_label = local_clock_label(std::time::SystemTime::now());
        let now_ms = self.started.elapsed().as_millis() as u64;
        let Some(cluster) = &mut self.cluster else {
            return;
        };
        if !cluster.prepare() {
            return;
        }
        let area = Rect::new(
            0,
            0,
            cluster.buffer.width as i32,
            cluster.buffer.height as i32,
        );
        let data = crate::surface::cluster_screen::ClusterData {
            clock_label: &clock_label,
            network_control_available,
            calendar_available,
            account_label: &account_label,
            machine: &self.machine,
            now_ms,
        };
        let actions = {
            let mut ui = Ui::new(
                Painter::new(&mut cluster.buffer),
                &mut self.text,
                &mut cluster.input,
                self.theme,
                cluster.scale,
                &mut self.kernel,
            );
            self.cluster_screen.draw(&mut ui, area, &data)
        };
        cluster.input.end_frame();
        if self.cluster_mask.width != cluster.buffer.width
            || self.cluster_mask.height != cluster.buffer.height
        {
            self.cluster_mask = WindowBuffer::new(cluster.buffer.width, cluster.buffer.height);
        }
        let shapes = self.cluster_screen.opaque_shapes();
        let floated = !shapes.is_empty();
        if floated {
            let mut painter = Painter::new(&mut self.cluster_mask);
            painter.fill_rect(Rect::new(0, 0, area.w, area.h), MASK_CLEAR);
            for (rect, radius) in shapes {
                painter.fill_rounded_rect_aa(
                    &mut self.kernel,
                    *rect,
                    *radius as f32,
                    MASK_SOLID,
                    u8::MAX,
                );
            }
        }
        let regions: Vec<(i32, i32, i32, i32)> = self
            .cluster_screen
            .opaque_shapes()
            .iter()
            .map(|(rect, _)| (rect.x, rect.y, rect.w, rect.h))
            .collect();
        let presented = floated && self.bridge.as_mut().is_some_and(|bridge| {
            match bridge.present_masked(
                &cluster.window,
                &cluster.buffer.pixels,
                &self.cluster_mask.pixels,
                &regions,
                cluster.buffer.width,
                cluster.buffer.height,
            ) {
                Ok(()) => true,
                Err(error) => {
                    tracing::debug!(%error, "the cluster could not float; presenting it opaque");
                    false
                }
            }
        });
        if !presented {
            cluster.present();
        }
        let wanted = crate::surface::cluster_screen::wanted_size(
            self.cluster_screen.account_open(),
            self.output_width(),
        );
        if wanted != self.cluster_wanted {
            self.cluster_wanted = wanted;
            if let Some(cluster) = &self.cluster {
                let _ = cluster
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
        }
        for action in actions {
            use crate::surface::bar_screen::BarAction;
            match action {
                BarAction::Activate(window) => self.perform(FieldAction::Activate(window)),
                BarAction::Window { id, window } => {
                    self.perform(FieldAction::RunWindowAction { id, window });
                }
                BarAction::Field(action) => self.perform(action),
            }
        }
    }

    /// The cluster's window events: pointer and redraw, exactly as the bar's.
    fn cluster_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw_cluster(),
            WindowEvent::CloseRequested => self.cluster = None,
            _ => {
                let Some(cluster) = &mut self.cluster else {
                    return;
                };
                match event {
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                            cluster.scale = pixelkit_shell::Scale::new(*scale_factor);
                        }
                        follow_bottom_anchored_resize(cluster);
                        cluster.request_redraw();
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        cluster
                            .input
                            .cursor_moved(position.x as f32, position.y as f32);
                        cluster.request_redraw();
                    }
                    WindowEvent::CursorLeft { .. } => {
                        cluster.input.cursor_left();
                        cluster.request_redraw();
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        if let Some(button) = translate_button(*button) {
                            cluster.input.mouse(button, *state == ElementState::Pressed);
                            cluster.request_redraw();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// The bar's window events: pointer and redraw only — the bar never reads the keyboard.
    fn bar_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw_bar(),
            WindowEvent::CloseRequested => self.bar = None,
            _ => {
                let Some(bar) = &mut self.bar else {
                    return;
                };
                match event {
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                            bar.scale = pixelkit_shell::Scale::new(*scale_factor);
                        }
                        follow_bottom_anchored_resize(bar);
                        bar.request_redraw();
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        bar.input.cursor_moved(position.x as f32, position.y as f32);
                        bar.request_redraw();
                    }
                    WindowEvent::CursorLeft { .. } => {
                        bar.input.cursor_left();
                        bar.request_redraw();
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        if let Some(button) = translate_button(*button) {
                            bar.input.mouse(button, *state == ElementState::Pressed);
                            bar.request_redraw();
                        }
                    }
                    _ => {}
                }
            }
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
        // Every pixel first: this surface has no alpha either, and a rounded fill leaves the
        // four corners holding whatever the buffer had (see `field_screen::draw`).
        ui.painter.clear(crate::surface::design::shell_fill());
        ui.painter
            .fill_rounded_rect(area, ui.px(8), crate::surface::design::shell_fill());
        ui.painter.stroke_rect(
            area,
            ui.scale.hairline(),
            crate::surface::design::edge_strong(),
        );
        let inner = area.inset(ui.px(12));
        let (title, body) = inner.split_top(ui.px(22));
        ui.label_styled(
            title,
            &row.app,
            crate::surface::design::measurement_style(&ui),
            crate::surface::design::text(),
            pixelkit_text::Align::Left,
        );
        ui.label_styled(
            body,
            &row.summary,
            crate::surface::design::state_style(&ui),
            crate::surface::design::text_dim(),
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

    /// Refetch notifications when the server says they changed. The subscription is installed
    /// before the startup snapshot, so arrivals are pushed without a read/subscribe race.
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
            let (snapshot, rows) = crate::data::fetch_notifications_widget().await;
            if tx
                .send(DataUpdate::Widget("notifications".to_owned(), snapshot))
                .is_err()
            {
                return;
            }
            let _ = tx.send(DataUpdate::NotificationRows(rows));
            let _ = proxy_wake.send_event(Wake);
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

    /// Sleep on the file authority's generation signal. A committed generation invalidates the
    /// open directory views and refreshes only file-backed feeds.
    fn spawn_files_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(index) = FilesIndexProxy::new(&connection).await else {
                return;
            };
            let Ok(mut generation) = index.receive_generation_advanced().await else {
                return;
            };
            let Ok(mut roots) = index.receive_root_state_changed().await else {
                return;
            };
            loop {
                for drawer in [
                    Drawer::Recent,
                    Drawer::Downloads,
                    Drawer::Documents,
                    Drawer::Media,
                    Drawer::Drives,
                    Drawer::Operations,
                ] {
                    let feed = crate::data::fetch_drawer(drawer, 20).await;
                    if tx.send(DataUpdate::Drawer(drawer, feed)).is_err() {
                        return;
                    }
                }
                let _ = tx.send(DataUpdate::BrowserSources(
                    crate::data::fetch_browser_sources().await,
                ));
                let _ = tx.send(DataUpdate::FilesChanged);
                let _ = proxy_wake.send_event(Wake);
                tokio::select! {
                    event = generation.next() => if event.is_none() { return; },
                    event = roots.next() => if event.is_none() { return; },
                }
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
            loop {
                for role in SHELL_ROLES {
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
                if changed.next().await.is_none() {
                    return;
                }
            }
        });
    }

    /// Projects owns both its list and current selection. Subscribe to both signals before the
    /// startup read, then sleep until either changes.
    fn spawn_projects_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(proxy) = ProjectsProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            let Ok(mut current_changed) = proxy.receive_current_changed().await else {
                return;
            };
            loop {
                let projects = crate::data::fetch_projects().await;
                if tx.send(DataUpdate::Projects(projects)).is_err() {
                    return;
                }
                let feed = crate::data::fetch_drawer(Drawer::Projects, 20).await;
                let _ = tx.send(DataUpdate::Drawer(Drawer::Projects, feed));
                let _ = proxy_wake.send_event(Wake);
                tokio::select! {
                    event = changed.next() => if event.is_none() { return; },
                    event = current_changed.next() => if event.is_none() { return; },
                }
            }
        });
    }

    /// Settings changes drive both Home's representation policy and the persisted launcher
    /// recents. The signal is installed before either startup read.
    fn spawn_settings_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(settings) = SettingsProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = settings.receive_changed().await else {
                return;
            };
            loop {
                let (field, recents) = tokio::join!(
                    fetch_home_field_settings(),
                    crate::settings::load_recent_apps(&settings),
                );
                if tx.send(DataUpdate::HomeFieldSettings(field)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::RecentApps(recents));
                let _ = tx.send(DataUpdate::IdleTimeout(
                    crate::data::fetch_idle_timeout().await,
                ));
                let _ = proxy_wake.send_event(Wake);
                if changed.next().await.is_none() {
                    return;
                }
            }
        });
    }

    /// Listen for `Lifecycle1.LockRequested` and lock when it arrives.
    ///
    /// A raw match rule rather than a generated proxy signal: `lege-client`'s `LifecycleProxy`
    /// declares the methods only, and the shell has no business growing its own copy of that
    /// interface. The rule names the interface and the member, so nothing else on the bus can
    /// lock this session by accident.
    fn spawn_lock_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let rule = match zbus::MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface("org.legeos.Session.Lifecycle1")
                .and_then(|builder| builder.member("LockRequested"))
            {
                Ok(builder) => builder.build(),
                Err(error) => {
                    tracing::warn!(%error, "building the LockRequested match rule");
                    return;
                }
            };
            let Ok(mut stream) =
                zbus::MessageStream::for_match_rule(rule, &connection, Some(4)).await
            else {
                tracing::debug!("no session bus to watch for LockRequested");
                return;
            };
            while stream.next().await.is_some() {
                if tx.send(DataUpdate::LockRequested).is_err() {
                    return;
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
            loop {
                let feed = crate::data::fetch_drawer(Drawer::Trash, 20).await;
                if tx.send(DataUpdate::Drawer(Drawer::Trash, feed)).is_err() {
                    return;
                }
                let _ = proxy_wake.send_event(Wake);
                if changed.next().await.is_none() {
                    return;
                }
            }
        });
    }

    /// One `Audio1` or `Backlight1` call off the event loop, refreshing that widget the moment it
    /// returns rather than waiting for the next authority update.
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
                let Some(terminal) = self.providers.get(&ProviderRole::Terminal).cloned() else {
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
        let Some(provider) = self.providers.get(&ProviderRole::FileSearch).cloned() else {
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

/// What a project row asks of `Projects1`. Two requests, never one with a mode flag: the whole
/// point is that entering a project and restoring its workspace are different things.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectRequest {
    /// `Projects1.SetCurrent`: select the context. Launches nothing.
    SetCurrent,
    /// `Projects1.Open`: launch the applications the project remembers.
    Open,
}

/// Which `Projects1` request a project row makes, if it makes one at all.
///
/// Pure, and the reason this is a function rather than two arms inside a spawn: mapping Enter
/// onto `Open` would open eight windows at somebody who asked to change context, and that is a
/// mistake nothing downstream can catch.
fn project_request(id: &str) -> Option<ProjectRequest> {
    match id {
        "project.enter" => Some(ProjectRequest::SetCurrent),
        "project.restore" => Some(ProjectRequest::Open),
        _ => None,
    }
}

/// Which provider role a project tool row resolves, and what that program is handed.
///
/// Pure, and separate from the launch, because the argument form is the part that differs per
/// tool and the part a test can check: a terminal takes `--directory <root>` (kitty's spelling,
/// the one the drop-down chord already uses), and a file manager and an editor both take the
/// path itself, which is what every one of them has accepted for thirty years.
fn tool_launch(id: &str, root: &str) -> Option<(ProviderRole, Vec<String>)> {
    match id {
        "project.open-terminal" => Some((
            ProviderRole::Terminal,
            vec!["--directory".to_owned(), root.to_owned()],
        )),
        "project.open-files" => Some((ProviderRole::FileManager, vec![root.to_owned()])),
        "project.open-editor" => Some((ProviderRole::CodeEditor, vec![root.to_owned()])),
        _ => None,
    }
}

/// The account chip's label when `USER` is unset: the numeric uid, which is at least true.
#[allow(unsafe_code)]
fn account_uid_label() -> String {
    // SAFETY: `getuid` takes no arguments, touches no memory and cannot fail.
    format!("uid {}", unsafe { libc::getuid() })
}

/// The first installed application that offers a calendar, for the bar's clock.
///
/// `Categories=Calendar` is the desktop-entry spec's own word for this, so anything a person
/// installs to get a calendar answers here without LegeOS knowing its name. `gsimplecal` is
/// preferred only because it is the one the composition ships (`distro/seeds/selection.toml`) and
/// a panel clock that opens a full groupware suite is a surprise.
///
/// ponytail: first match by category, not a provider role. Upgrade to a `Calendar` role in
/// `lege-session-core::providers` if a second surface ever needs to open one.
fn calendar_app(apps: &[crate::app_search::AppEntry]) -> Option<AppId> {
    let calendars = || {
        apps.iter()
            .filter(|app| app.categories.iter().any(|c| c == "Calendar"))
    };
    calendars()
        .find(|app| app.id.as_str().contains("gsimplecal"))
        .or_else(|| calendars().next())
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
        // Every watcher installs its match rule before taking its startup snapshot. The slow
        // reconciliation task starts last and exists only as a continuity backstop.
        self.spawn_applications_watch();
        self.spawn_field_watch();
        self.spawn_files_watch();
        self.spawn_notifications_watch();
        self.spawn_providers_watch();
        self.spawn_projects_watch();
        self.spawn_settings_watch();
        self.spawn_trash_watch();
        self.spawn_lock_watch();
        self.spawn_reconciliation_loop();
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

    #[allow(clippy::too_many_lines)]
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
        let is_cluster = self
            .cluster
            .as_ref()
            .is_some_and(|cluster| cluster.window.id() == window_id);
        if is_cluster {
            self.cluster_event(&event);
            return;
        }
        let is_bar = self
            .bar
            .as_ref()
            .is_some_and(|bar| bar.window.id() == window_id);
        if is_bar {
            self.bar_event(&event);
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

    #[allow(clippy::too_many_lines)]
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(bridge) = &mut self.bridge
            && let Err(error) = bridge.pump()
        {
            tracing::warn!(%error, "pumping the trusted-shell connection; treating it as gone");
            self.bridge = None;
        }
        // The lock screen has no winit window, so nothing else would ever redraw it: its keys
        // arrive on the bridge's own queue and its frames are committed straight to the
        // compositor.
        if self.drain_lock_keys() {
            self.paint_lock();
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
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        self.ensure_bar(event_loop, &toplevels);
        self.ensure_cluster(event_loop, &toplevels);
        // The window list moved: Home's session tiles and the bar both show it, so both redraw
        // now rather than on the next slow sweep.
        let generation = self.bridge.as_ref().map_or(0, ShellBridge::generation);
        if generation != self.bridge_generation {
            self.bridge_generation = generation;
            if let Some(home) = &self.home {
                home.request_redraw();
            }
            if let Some(bar) = &self.bar {
                bar.request_redraw();
            }
        }
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
        {
            if let Some(home) = &self.home {
                home.request_redraw();
            }
            // The bar too: an arriving capture is the row's whole content, and without this it
            // would keep its icon fallback until the pointer happened to move.
            if let Some(bar) = &self.bar {
                bar.request_redraw();
            }
        }
        // Event-driven at rest; a steady frame cadence only while a tile is lifted, so the
        // ghost follows the pointer and a throw's velocity is sampled evenly. Otherwise the next
        // wake is the earliest of the toast's lifetime and the bar's hover timers.
        if self.field_screen.animating() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(16),
            ));
            if let Some(home) = &self.home {
                home.request_redraw();
            }
            return;
        }
        let toast_deadline = self
            .toast
            .as_ref()
            .map(|(_, _, shown)| *shown + Self::TOAST_LIFETIME);
        // The bar's own timers: a hover row's dwell and grace, and — while a window covers the
        // bottom strip — the dwell that reveals the bar out from under it and the grace that
        // collapses it again. `covered` has to be recomputed here because it decides which of
        // those two pairs is running.
        let bar_covered = bar_covered(
            self.bridge.as_ref().is_some_and(ShellBridge::home_revealed),
            self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked),
            &toplevels,
        );
        let bar_deadline = self.bar.as_ref().and_then(|bar| {
            let entries = crate::taskbar::entries(&toplevels);
            let deadline_ms = self.bar_screen.next_deadline_ms(
                &entries,
                bar_covered,
                self.started.elapsed().as_millis() as u64,
            )?;
            let deadline = self.started + Duration::from_millis(deadline_ms);
            if deadline <= Instant::now() {
                bar.request_redraw();
            }
            Some(deadline)
        });
        // The cluster's own two: a popover the pointer has left, and the next minute — it draws
        // the clock, so it has to wake for one whether or not anything else happens.
        let cluster_deadline = self.cluster.as_ref().map(|cluster| {
            let popover = self
                .cluster_screen
                .next_deadline_ms()
                .map(|ms| self.started + Duration::from_millis(ms));
            let minute = next_minute();
            if popover.is_some_and(|at| at <= Instant::now()) {
                cluster.request_redraw();
            }
            popover.map_or(minute, |at| at.min(minute))
        });
        let deadline = [toast_deadline, bar_deadline, cluster_deadline]
            .into_iter()
            .flatten()
            .min();
        match deadline {
            Some(deadline) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline.max(Instant::now())));
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
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

    /// Every role in [`SHELL_ROLES`] is a role `note_provider` can actually record, and every
    /// role the shell acts on is in the list. The second half is the bug this const was written
    /// for: `file-manager` was acted on and never fetched.
    #[test]
    fn every_role_the_shell_acts_on_is_fetched_and_recognised() {
        for role in SHELL_ROLES {
            assert!(
                ProviderRole::ALL.iter().any(|known| known.as_str() == role),
                "{role} is fetched but `ProviderRole` does not know it"
            );
        }
        for role in [
            ProviderRole::Terminal,
            ProviderRole::FileSearch,
            ProviderRole::FileManager,
            ProviderRole::CodeEditor,
        ] {
            assert!(
                SHELL_ROLES.contains(&role.as_str()),
                "the shell acts on {role} and never fetches it"
            );
        }
    }

    /// The bar reserves nothing, so "is anything in the way?" is a question the shell has to
    /// answer for itself every frame. This is that rule, and the whole of it (contracts Z2/Z3).
    #[test]
    fn the_switcher_hides_under_a_window_that_fills_the_screen_and_comes_back_on_a_peek() {
        assert!(
            !bar_covered(false, false, &[]),
            "a bare desktop covers nothing"
        );

        // Maximized and fullscreen alike: the window fills the screen, and the switcher is the
        // one thing on the bottom edge that is not part of it.
        for filling in [window(1, false, true, false), window(1, true, false, false)] {
            assert!(bar_covered(false, false, std::slice::from_ref(&filling)));
            // The pointer resting on the bottom edge is what asks for it back.
            assert!(!bar_covered(false, true, std::slice::from_ref(&filling)));
            // Home revealed: the compositor lifted Home over the windows, so the bar has Home to
            // paint on and the switcher is simply visible.
            assert!(!bar_covered(true, false, std::slice::from_ref(&filling)));
        }

        // Parked is not on screen, so it covers nothing.
        assert!(!bar_covered(false, false, &[window(1, false, true, true)]));

        // An ordinary floating window over the bottom of the screen is not what the screen is
        // for at that moment, and there is no hover route back out from under one — the
        // compositor's dwell fires only under a window that covers the screen.
        let mut floating = window(2, false, false, false);
        floating.geometry = lege_window::Rect::new(0, 0, 1920, 1080);
        assert!(!bar_covered(false, false, &[floating]));
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

    /// The clock resolves a calendar by what the desktop entry says it is, and prefers the one
    /// the composition ships. Matching by name instead is exactly the mistake the network chip
    /// used to make.
    #[test]
    fn the_clock_resolves_a_calendar_by_category_and_prefers_the_shipped_one() {
        let calendar = |id: &str| {
            let mut entry = app_entry(id);
            entry.categories = vec!["Utility".to_owned(), "Calendar".to_owned()];
            entry
        };
        let simple = calendar("desktop-import.gsimplecal.desktop");
        let other = calendar("desktop-import.org.gnome.Calendar.desktop");
        assert_eq!(
            calendar_app(&[other.clone(), simple.clone()]),
            Some(simple.id)
        );
        assert_eq!(calendar_app(std::slice::from_ref(&other)), Some(other.id));
        assert_eq!(
            calendar_app(&[app_entry("desktop-import.gsimplecal.desktop")]),
            None,
            "a name that looks like a calendar is not a declared calendar"
        );
        assert_eq!(calendar_app(&[]), None);
    }

    #[test]
    fn information_row_clock_has_stable_local_date_and_time_formatting() {
        assert_eq!(format_clock_fields(1, 24, 7, 8, 5), "MON 24 AUG 08:05");
        assert_eq!(format_clock_fields(99, 1, 99, 0, 0), "--- 01 --- 00:00");
    }

    /// A tool row resolves a provider role, never a copied path, and hands that program the
    /// The Place menu's "open in files" row is not its own route. It resolves nothing of its
    /// own: it is the same press as a Folder tile, and `open_in_file_manager` is where both land.
    /// `tool_launch` therefore has nothing to say about it any more.
    #[test]
    fn opening_a_place_in_files_is_not_a_second_route_of_its_own() {
        assert_eq!(
            tool_launch("project.open-files", "/home/dk/src/legeos"),
            Some((
                ProviderRole::FileManager,
                vec!["/home/dk/src/legeos".to_owned()]
            )),
            "a project's own tool row still resolves the role"
        );
        // And what it resolves is exactly what the one route passes: the bare directory, which
        // is what pcmanfm-qt (and every file manager for thirty years) takes.
        let (role, arguments) =
            tool_launch("project.open-files", "/home/dk/src/legeos").expect("the row exists");
        assert_eq!(role, ProviderRole::FileManager);
        assert_eq!(arguments, vec!["/home/dk/src/legeos".to_owned()]);
    }

    /// project's real root in the form it takes. Getting the argument wrong opens the editor on
    /// the person's home directory, which looks like it worked.
    #[test]
    fn each_project_tool_resolves_its_role_and_opens_at_the_root() {
        assert_eq!(
            tool_launch("project.open-terminal", "/home/dk/src/legeos"),
            Some((
                ProviderRole::Terminal,
                vec!["--directory".to_owned(), "/home/dk/src/legeos".to_owned()]
            ))
        );
        assert_eq!(
            tool_launch("project.open-files", "/home/dk/src/legeos"),
            Some((
                ProviderRole::FileManager,
                vec!["/home/dk/src/legeos".to_owned()]
            ))
        );
        assert_eq!(
            tool_launch("project.open-editor", "/home/dk/src/legeos"),
            Some((
                ProviderRole::CodeEditor,
                vec!["/home/dk/src/legeos".to_owned()]
            ))
        );
        // Entering and restoring are not tools: they are requests to the service, and routing
        // one of them through a launcher would start an application nobody asked for.
        assert_eq!(tool_launch("project.enter", "/x"), None);
        assert_eq!(tool_launch("project.restore", "/x"), None);
        assert_eq!(tool_launch("window.close", "/x"), None);
    }

    /// The separation the whole deliverable exists for: Enter selects the context and launches
    /// nothing, Restore launches what the project remembers. Mapping one onto the other opens
    /// eight windows at somebody who asked to change what they are working on.
    #[test]
    fn set_current_launches_nothing_and_open_launches_only_remembered_apps() {
        assert_eq!(
            project_request("project.enter"),
            Some(ProjectRequest::SetCurrent),
            "Enter must reach SetCurrent, which the service documents as launching nothing"
        );
        assert_eq!(
            project_request("project.restore"),
            Some(ProjectRequest::Open),
            "Restore must reach Open, the only request that launches remembered applications"
        );
        // A tool row is not a project request: routing one through Projects1 would either change
        // the current project as a side effect of opening a terminal, or restore a workspace.
        for tool in [
            "project.open-terminal",
            "project.open-files",
            "project.open-editor",
        ] {
            assert_eq!(project_request(tool), None, "{tool} reached Projects1");
            assert!(tool_launch(tool, "/x").is_some(), "{tool} launches nothing");
        }
        // And the two are disjoint, so no row can be both.
        for row in ["project.enter", "project.restore"] {
            assert!(
                tool_launch(row, "/x").is_none(),
                "{row} also launches a tool"
            );
        }
    }
}
