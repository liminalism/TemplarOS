//! Wiring: one `winit::application::ApplicationHandler` hosting the Home Surface, the
//! trusted-shell protocol connection that declares its layer, and D-Bus subscriptions that keep
//! its content live. See `window.rs`'s module doc for why this crate drives winit
//! itself rather than `pixelkit_shell::run_app`.

use anyhow::Context;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use pixelkit_raster::{Painter, RasterKernel, Rect, WindowBuffer};
use pixelkit_ui::toast::{ToastAction, ToastCard};
use pixelkit_ui::{Theme, Ui};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use templar_client::proxies::{
    FilesIndexProxy, LauncherProxy, LifecycleProxy, ProjectsProxy, SettingsProxy,
};
use templar_field::{FieldDocument, FieldOp, TileKind};
use templar_types::AppId;
use templar_types::action;
use templar_types::provider::ProviderRole;
use templar_window::RevealHome;

use crate::clipboard::Clipboard;
use crate::field_model::{self, LiveState, ProjectInfo};
use crate::file_browser::{BrowseEntry, BrowserSources, DirectoryListing, EntryKind};
use crate::files_surface::FilesLocation;
use crate::home_surface::Drawer;
use crate::icons::{self, SystemIconRoots};
use crate::launcher_model::{LauncherMode, LauncherTarget};
use crate::omnibox::Scope;
use crate::settings::{self, HomeFieldSettings, PreviewMode, RailSettings, TabSettings};
use crate::surface::design;
use crate::surface::field_screen::{FieldAction, FieldData, FieldScreen};
use crate::surface::file_browser::{FileBrowserData, FileRequest, FileVerb};
use crate::surface::fonts;
use crate::surface::notification_popup;
use crate::surface::rail::{RailReveal, RailSide, action_keeps_rail_open};
use crate::surface::rail_session::RailSession;
use crate::surface::window::{
    HostedWindow, key_pressed, modifiers_of, scroll_pixels, translate_button, translate_keys,
};
use crate::wayland_bridge::{PresentOutcome, ShellBridge};

#[path = "launcher_host.rs"]
mod launcher_host;
use launcher_host::LauncherRequest;

/// Worker wakeups and native accessibility requests share the owned winit event loop.
#[derive(Debug)]
pub enum Wake {
    Data,
    Accessibility(accesskit_winit::Event),
}

impl From<accesskit_winit::Event> for Wake {
    fn from(event: accesskit_winit::Event) -> Self {
        Self::Accessibility(event)
    }
}

/// A size preview retains its native scope, pixel size and observed freshness alongside the image.
#[derive(Debug)]
pub struct TreemapPreview {
    pub path: PathBuf,
    pub caption: String,
    pub width: u32,
    pub height: u32,
    pub bitmap: Option<pixelkit_raster::Bitmap>,
    pub status: String,
    pub composition: Vec<(u32, f32)>,
}

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
    /// One atomic journal model for both Home and the popup, including owner invalidation.
    Notifications(crate::surface::notification_feed::NotificationUpdate),
    /// `HomeGarden1.Contexts` by root.
    Garden(HashMap<PathBuf, crate::garden::GardenPlace>),
    WorkingDirectories(HashMap<String, PathBuf>),
    LauncherPreferences {
        epoch: u64,
        preferences: templar_types::launcher::LauncherPreferences,
    },
    FavoriteLaunched {
        id: String,
        target: templar_types::launcher::FavoriteTarget,
        instance: String,
    },
    WindowTransition(u32, u32),
    HomeFieldSettings(HomeFieldSettings),
    /// Wallpaper1's observed presented state, independent of desired enabled/configuration.
    WallpaperApplied(Option<templar_types::wallpaper::WallpaperObservation>),
    /// `shell.rails.*`: gesture rails or the classic bar, and the gate the compositor watches.
    RailSettings(RailSettings),
    /// `shell.rails.lanes`: how the running-apps strip groups windows by kind of program.
    Lanes(crate::taskbar::LaneMode),
    /// `windows.tabs.*`: the committed window tab policy the compositor applies.
    TabSettings(TabSettings),
    /// Who fills an exclusive `Providers1` role the shell acts on: the `terminal` the drop-down
    /// chord toggles, and the file manager for directory activation.
    /// `None` is "nobody", which is a fact worth relaying — the compositor stops looking for a
    /// terminal window it will never find.
    Provider {
        role: &'static str,
        app_id: Option<String>,
    },
    /// `Lifecycle1.LockRequested`: somebody asked for the session to be locked.
    LockRequested,
    /// What `templar-unlock` said about a password: `Ok` unlocks, `Err` is the message shown.
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
    HomeThumbnail {
        path: PathBuf,
        bitmap: Option<pixelkit_raster::Bitmap>,
    },
    DesktopDirectory {
        path: PathBuf,
        listing: Result<super::desktop_dock::NativeListing, String>,
    },
    DockHandlers {
        path: PathBuf,
        handlers: Result<Vec<String>, String>,
    },
    NativeTransferFinished {
        generation: u64,
        success: bool,
    },
    /// One application's icon, resolved and decoded off-thread. `None` when nothing was found —
    /// the attempt is already recorded, so the receiver simply has nothing to file.
    Icon(String, Option<pixelkit_raster::Bitmap>),
    /// The Files object's current directory as a treemap and a composition, rasterized
    /// off-thread.
    Treemap {
        epoch: u64,
        preview: TreemapPreview,
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
        request_generation: u64,
        result: Result<IndexedSearchResults, templar_client::file_index::IndexCallError>,
    },
    SearchPage {
        request_generation: u64,
        cursor: String,
        page: u32,
        result: Result<
            templar_client::file_index::IndexPage,
            templar_client::file_index::IndexCallError,
        >,
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

struct IndexedSearchResults {
    session: std::sync::Arc<templar_client::file_index::SearchSession>,
    entries: Vec<BrowseEntry>,
    pages: crate::surface::search_pages::SearchPages,
    notice: String,
}

/// What one toast-host frame asked the service to do about one notification.
/// Collected during the draw pass, performed after it.
enum ToastUserAction {
    Dismiss,
    Invoke(String),
}

/// Whether a window fills the screen, so the switcher collapses to its hint and the machine
/// cluster is not drawn at all (`bar_screen::Presence`, `App::ensure_cluster`).
///
/// Two things decide it, and nothing else:
///
/// - a window that fills the screen (`ShellToplevel::covers_screen`) — maximized or fullscreen.
///   A fullscreen one sorts *above* `Layer::ShellOverlay` (contract Z3), so the bar would be under
///   it whatever it painted; a maximized one does not, but it is just as much the thing the user
///   is working in, and an island parked on top of it is the taskbar this desktop is not.
/// - Home being revealed, which uncovers everything: the compositor has lifted Home over the
///   windows, so the bar has Home to paint on and the switcher should simply be visible.
///
/// The peek is deliberately *not* here. It is a way into the reveal, not a state of its own, so
/// it goes to `BarData::peeked` and enters `bar_screen`'s own reveal — otherwise a peek ended by
/// a one-pixel overshoot would collapse the island instantly while the same island reached from
/// the hint takes `HINT_CLOSE_MS` to go, which is two feels for one gesture.
///
/// An ordinary floating window over the bottom of the screen counts for nothing. It is not what
/// the screen is *for* at that moment, the island is a small strip in the middle of the edge, and
/// there is no hover route back out from under one — the compositor's dwell fires only under a
/// window that covers the screen.
fn bar_covered(home_revealed: bool, toplevels: &[templar_window::ShellToplevel]) -> bool {
    !home_revealed
        && toplevels
            .iter()
            .any(templar_window::ShellToplevel::covers_screen)
}

/// A rectangle as `set_minimize_target` carries it: x, y, width, height in surface-local
/// logical pixels.
type LogicalRect = (i32, i32, i32, i32);

/// `rect` (buffer pixels) in logical pixels at `scale`, rounded outward so the logical rectangle
/// still covers every pixel of the card. `None` for an empty rectangle.
fn logical_rect(rect: Rect, scale: f64) -> Option<LogicalRect> {
    if rect.w <= 0 || rect.h <= 0 || scale <= 0.0 {
        return None;
    }
    let x0 = (f64::from(rect.x) / scale).floor() as i32;
    let y0 = (f64::from(rect.y) / scale).floor() as i32;
    let x1 = (f64::from(rect.x + rect.w) / scale).ceil() as i32;
    let y1 = (f64::from(rect.y + rect.h) / scale).ceil() as i32;
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// What Home last told the compositor about where each window parks
/// (`ShellBridge::set_minimize_target`), so a frame sends only what changed.
#[derive(Debug, Default)]
struct MinimizeTargets {
    sent: HashMap<templar_window::WindowId, LogicalRect>,
    /// The compositor predates the request. Stays set for the life of this connection.
    unsupported: bool,
}

impl MinimizeTargets {
    /// The requests this frame owes: a target that is new or moved, and a clear for a window
    /// that still exists but no longer has a card. A window that closed is forgotten without a
    /// word — the compositor's own object went with it.
    fn changes(
        &mut self,
        targets: impl Iterator<Item = (templar_window::WindowId, LogicalRect)>,
        toplevels: &[templar_window::ShellToplevel],
    ) -> Vec<(templar_window::WindowId, Option<LogicalRect>)> {
        if self.unsupported {
            return Vec::new();
        }
        self.sent.retain(|window, _| {
            toplevels
                .iter()
                .any(|toplevel| toplevel.window_id == *window)
        });
        let targets: HashMap<_, _> = targets.collect();
        let mut changes: Vec<_> = targets
            .iter()
            .filter(|(window, rect)| self.sent.get(window) != Some(rect))
            .map(|(window, rect)| (*window, Some(*rect)))
            .collect();
        changes.extend(
            self.sent
                .keys()
                .filter(|window| !targets.contains_key(window))
                .map(|window| (*window, None)),
        );
        changes
    }

    /// `window`'s target as the compositor now has it.
    fn record(&mut self, window: templar_window::WindowId, rect: Option<LogicalRect>) {
        match rect {
            Some(rect) => self.sent.insert(window, rect),
            None => self.sent.remove(&window),
        };
    }
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

/// Say which parts of a surface take the pointer, for a frame that did not float.
///
/// The floating path declares this inside `present_masked`; this is the same declaration for the
/// path that could not float, so that *every* present says what it means. Failure is logged and
/// nothing else: a surface whose region could not be declared is a surface with the previous
/// frame's, which is exactly the state this exists to avoid, but it is not a reason to drop the
/// frame as well.
fn declare_input_shapes(
    bridge: Option<&mut ShellBridge>,
    window: &winit::window::Window,
    shapes: &[(i32, i32, i32, i32)],
    what: &str,
) {
    if let Some(bridge) = bridge
        && let Err(error) = bridge.declare_input_shapes(window, shapes)
    {
        tracing::debug!(%error, "could not declare {what}'s input region");
    }
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

/// Desktop has its own ordered native watcher; generic index refreshes must
/// neither discard that listing nor race it with an unordered folder fetch.
fn invalidate_folder_listings(
    listings: &mut HashMap<PathBuf, Result<DirectoryListing, String>>,
    desktop: Option<&Path>,
) -> Vec<PathBuf> {
    let mut refresh = Vec::new();
    listings.retain(|path, _| {
        if desktop == Some(path.as_path()) {
            true
        } else {
            refresh.push(path.clone());
            false
        }
    });
    refresh
}

/// How far one press of a level control moves it.
///
/// The same five percentage points `templar status louder` uses, and deliberately the same number:
/// a person who steps the volume from a tile and from a terminal is doing one thing, and two
/// different step sizes would make the two surfaces disagree about what "one step" means.
const STEP_PERCENT: i32 = 5;

/// The application `settings.open` opens — the id of `apps/templar-settings/org.templaros.Settings.desktop`.
const SETTINGS_APP_ID: &str = "org.templaros.Settings";

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
const SHELL_ROLES: [&str; 4] = [
    "terminal",
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

/// Which of the shell's two surfaces a rail is drawn on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RailHost {
    /// The island: running applications.
    Apps,
    /// The cluster: settings, network, clock, account.
    System,
}

/// Which rail the pointer's direction opens. The compositor only knows sides; which rail sits on
/// which is the person's (`shell.rails.swap`): apps to the left and the machine to the right,
/// unless they asked otherwise.
fn rail_host(side: RailSide, swap: bool) -> RailHost {
    match (side, swap) {
        (RailSide::Left, false) | (RailSide::Right, true) => RailHost::Apps,
        (RailSide::Right, false) | (RailSide::Left, true) => RailHost::System,
    }
}

/// One rail the compositor opened and the shell is drawing.
#[derive(Debug)]
struct OpenRail {
    /// The bridge's opening number, so the shell can tell this rail from the next.
    opening: u64,
    host: RailHost,
    reveal: RailReveal,
    session: RailSession,
}

/// Present a rail host whose rail is closed as nothing at all: one fully transparent frame at
/// the size it is, an *empty* input region, and a request to shrink to 1×1.
///
/// Never the opaque fallback the classic bar takes when it has no shapes: that would be a grey
/// strip across the bottom of the screen taking every click in it. A surface that cannot float
/// here declares its empty region and draws nothing.
///
/// `idle` remembers the buffer size last presented this way, so the machine readings and the
/// window list changing do not re-present an unchanged nothing.
fn present_nothing(
    bridge: Option<&mut ShellBridge>,
    hosted: &mut HostedWindow,
    mask: &mut WindowBuffer,
    wanted: &mut (i32, i32),
    idle: &mut Option<(u32, u32)>,
    what: &str,
) {
    if *wanted != (1, 1) {
        *wanted = (1, 1);
        let _ = hosted.window.request_inner_size(LogicalSize::new(1.0, 1.0));
    }
    if !hosted.prepare() {
        return;
    }
    let size = (hosted.buffer.width, hosted.buffer.height);
    if *idle == Some(size) && !hosted.pending {
        return;
    }
    let Some(bridge) = bridge else {
        return;
    };
    if mask.width != size.0 || mask.height != size.1 {
        *mask = WindowBuffer::new(size.0, size.1);
    }
    mask.pixels.fill(MASK_CLEAR);
    match bridge.present_masked(
        &hosted.window,
        &hosted.buffer.pixels,
        &mask.pixels,
        &[],
        size.0,
        size.1,
    ) {
        Ok(PresentOutcome::Submitted) => {
            hosted.pending = false;
            *idle = Some(size);
        }
        Ok(PresentOutcome::Deferred) => hosted.pending = true,
        Err(error) => {
            tracing::debug!(%error, "{what} could not float; drawing nothing");
            if let Err(error) = bridge.declare_input_shapes(&hosted.window, &[]) {
                tracing::debug!(%error, "clearing {what}'s input region");
            }
        }
    }
}

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
    launcher: Option<HostedWindow>,
    launcher_screen: crate::surface::launcher_screen::LauncherScreen,
    launcher_request: Option<LauncherRequest>,
    launcher_focus_claimed: bool,
    launcher_preferences: templar_types::launcher::LauncherPreferences,
    launcher_mutations: tokio::sync::mpsc::Sender<templar_types::launcher::LauncherMutation>,
    launcher_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    working_directories: HashMap<String, PathBuf>,
    favorite_instances: HashMap<String, String>,
    last_recent_activation: Option<templar_window::WindowId>,
    window_transition: (u32, u32),
    window_transition_relayed: Option<(u32, u32)>,

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
    reduced_motion_sent: Option<bool>,
    field_settings: HomeFieldSettings,
    wallpaper_applied: Option<templar_types::wallpaper::WallpaperObservation>,
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
    desktop_dock: Option<super::desktop_dock::DesktopDock>,
    native_drop_target: Option<(PathBuf, bool)>,
    native_drop_receiver: Option<std::sync::mpsc::Receiver<Result<Vec<PathBuf>, String>>>,
    native_drop_move: bool,
    native_drop_pending: bool,
    native_drop_generation: u64,
    native_drag_origin: Option<PathBuf>,
    native_drop_item: Option<PathBuf>,
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
    preview_requested: HashMap<templar_window::WindowId, Instant>,
    preview_demand: crate::previews::DemandTracker,
    preview_demand_sent: bool,
    /// Each widget's latest content, keyed by `TileKind::System`'s probe.
    widgets: HashMap<String, crate::surface::widgets::WidgetSnapshot>,
    /// What the machine is doing, for the bar's readings chip and its account popover.
    machine: crate::data::MachineReadings,
    appearance: templar_client::appearance::AppearanceObserver,
    /// The Files object's current directory, its treemap and its composition. One directory,
    /// because there is one Files object.
    treemap: Option<TreemapPreview>,
    /// Includes invalidation epoch: an old load cannot republish after a generation change.
    treemap_pending: Option<(PathBuf, u32, u32, u64)>,
    treemap_epoch: u64,
    /// When the last pointer drop happened, for the experiment's undo-within-5-s measure.
    last_drop_at: Option<Instant>,
    started: Instant,

    drawer_feeds: HashMap<Drawer, crate::drawers::DrawerFeed>,
    search_results: Option<(Scope, String, Result<IndexedSearchResults, String>)>,
    search_generation: u64,
    search_tag: String,
    search_request: Option<tokio::task::AbortHandle>,
    search_refresh: Option<crate::surface::search_pages::SearchPages>,
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
    home_thumbnail_pending: HashSet<PathBuf>,
    notification_rows: Vec<crate::surface::widgets::notifications::NotificationRow>,
    /// The bounded bottom-right toast stack: what shows, what waits, what retires when.
    toast_stack: crate::surface::notification_popup::ToastStack,
    /// The single Notification-layer host window. `None` when nothing is showing: no
    /// surface lingers for a mode nobody is in, and lock/fullscreen drop it outright.
    toast_host: Option<HostedWindow>,
    /// The size the toast host was last asked for. The compositor never configures this
    /// surface's extent, so the shell is the only thing that knows it.
    toast_wanted: (i32, i32),
    /// Output the toasts are stuck to until they leave. `None` until the first
    /// presentation and after a full drain.
    toast_output: Option<String>,
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
    /// A calendar popup borrows keyboard focus only while it is visibly presented.
    calendar_focus_claimed: bool,
    /// `shell.rails.*` as last read. Defaults (gesture rails) until the first read lands.
    rail_settings: RailSettings,
    /// `shell.rails.lanes` as last read: the schema default until the first read lands.
    lanes: crate::taskbar::LaneMode,
    /// The application ids on the active lane (`taskbar::active_lane_apps`), refreshed whenever
    /// the catalogue is. Read from the catalogue, never measured from the windows.
    active_lane_apps: std::collections::HashSet<String>,
    /// The gate last relayed to the compositor. Unchanged readings cost no protocol round trip;
    /// `None` until the first relay.
    rail_gate_relayed: Option<RailSettings>,
    /// `windows.tabs.*` as last read. Defaults until the first read lands.
    tab_settings: TabSettings,
    /// The tab policy last relayed to the compositor. `None` until the first relay.
    tab_policy_relayed: Option<TabSettings>,
    /// The mode the bar and cluster surfaces were built for: `true` for gesture rails (edge
    /// start, persistent, 1×1 and transparent while closed), `false` for the classic bar. A
    /// change of mode rebuilds both, because their alignment is fixed at creation.
    hosts_gesture: bool,
    /// The rail open right now, if any.
    rail: Option<OpenRail>,
    /// The opening this shell dismissed itself, until the compositor answers with no rail. Its
    /// late `rail_pointer` events must not reopen it.
    rail_dismissed: Option<u64>,
    /// The bridge's `rail_generation` last acted on: pointer progress along a rail has its own
    /// dirty path, and never redraws Home, the bar in general or Taskfield.
    rail_generation: u64,
    /// The buffer size each rail host last presented as nothing (`present_nothing`).
    bar_idle: Option<(u32, u32)>,
    cluster_idle: Option<(u32, u32)>,
    /// The window overview's labels, on `Layer::Presentation` (manager v8).
    ///
    /// Created when `taskfield_state` says the mode opened and dropped when it says it closed,
    /// rather than kept mapped and hidden: it is the whole output, it exists for as long as a
    /// gesture, and a full-screen surface lying around for a mode nobody is in is a surface that
    /// can only be wrong. The compositor also maps and unmaps it; neither side depends on the
    /// other getting it right.
    taskfield: Option<HostedWindow>,
    /// Frozen at first paint after the mode opens, thrown away when it closes. The compositor
    /// owns the layout; recomputing from the live snapshot every frame is how labels drifted
    /// off the cards they name when a window closed or resized.
    taskfield_sections: Option<Vec<templar_window::taskfield::Section>>,
    taskfield_screen: crate::surface::taskfield_screen::TaskfieldScreen,
    taskfield_mask: WindowBuffer,
    bar_screen: crate::surface::bar_screen::BarScreen,
    /// The size the bar surface was last asked for (`bar_screen::wanted_size`). The compositor
    /// never configures this surface's width, so the shell is the only thing that knows it.
    bar_wanted: (i32, i32),
    /// The bridge generation the bar and Home were last drawn for.
    bridge_generation: u64,
    /// What Home last told the compositor about where each window parks.
    minimize_targets: MinimizeTargets,
    browser_pending: Option<(FilesLocation, bool)>,
    browser_error: Option<(FilesLocation, String)>,
    browser_status: Option<String>,
    icons: HashMap<String, pixelkit_raster::Bitmap>,
    icon_attempted: HashSet<String>,
    /// Application icons asked for before the catalogue had an entry for them, so the lookup ran
    /// without the manifest's `Icon=` and desktop file id. Session-start windows are asked for
    /// before `ListApplications` answers; once it does, these are the attempts worth repeating.
    icon_hintless: HashSet<String>,

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
    // A constructor of initial values, one line per field: splitting it would scatter the
    // defaults it exists to keep in one place.
    #[allow(clippy::too_many_lines)]
    fn new(
        runtime: tokio::runtime::Handle,
        data_tx: Sender<DataUpdate>,
        data_rx: Receiver<DataUpdate>,
        wake_proxy: EventLoopProxy<Wake>,
    ) -> ShellApp {
        let appearance_wake = wake_proxy.clone();
        let appearance = templar_client::appearance::AppearanceObserver::new(move || {
            let _ = appearance_wake.send_event(Wake::Data);
        });
        let launcher_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let launcher_mutations =
            launcher_host::mutation_queue(&runtime, &data_tx, &wake_proxy, launcher_epoch.clone());
        ShellApp {
            runtime,
            data_tx,
            data_rx,
            wake_proxy,
            bridge: None,
            providers: BTreeMap::new(),
            terminal_relayed: None,
            lock_screen: crate::surface::lock_screen::LockScreen::default(),
            lock_input: pixelkit_shell::Input::new(),
            location_query: String::new(),
            home: None,
            launcher: None,
            launcher_screen: crate::surface::launcher_screen::LauncherScreen::default(),
            launcher_request: None,
            launcher_focus_claimed: false,
            launcher_preferences: templar_types::launcher::LauncherPreferences::default(),
            launcher_mutations,
            launcher_epoch,
            working_directories: HashMap::new(),
            favorite_instances: HashMap::new(),
            last_recent_activation: None,
            window_transition: (2, 6),
            window_transition_relayed: None,
            text: pixelkit_text::TextCache::new(fonts::set()),
            kernel: RasterKernel::new(),
            bar_mask: WindowBuffer::new(0, 0),
            bar_centred: false,
            theme: design::theme(),
            field_screen: FieldScreen::with_position_memory(
                crate::work_positions::WorkPositions::load(),
            ),
            field: FieldDocument::new(OUTPUT, 16, 8),
            field_synced: None,
            undo: Vec::new(),
            last_field_change_at: None,
            last_file_operation: None,
            clipboard: None,
            drag_exported: false,
            field_settings: HomeFieldSettings::default(),
            wallpaper_applied: None,
            reduced_motion_sent: None,
            recent: crate::recents::RecentApps::default(),
            field_requested: false,
            apps: Vec::new(),
            apps_available: false,
            projects: Vec::new(),
            garden: HashMap::new(),
            desktop_dock: None,
            native_drop_target: None,
            native_drop_receiver: None,
            native_drop_move: false,
            native_drag_origin: None,
            native_drop_item: None,
            native_drop_pending: false,
            native_drop_generation: 0,
            home_was_revealed: false,
            garden_tended_at_start: false,
            app_variants: HashMap::new(),
            home_dir: crate::drawers::standard_paths::home_dir(),
            folder_listings: HashMap::new(),
            folder_pending: HashSet::new(),
            previews: HashMap::new(),
            preview_requested: HashMap::new(),
            preview_demand: crate::previews::DemandTracker::default(),
            preview_demand_sent: false,
            widgets: HashMap::new(),
            machine: crate::data::MachineReadings::default(),
            appearance,
            treemap: None,
            treemap_pending: None,
            treemap_epoch: 0,
            last_drop_at: None,
            started: Instant::now(),
            drawer_feeds: HashMap::new(),
            search_results: None,
            search_generation: 0,
            search_tag: format!(
                "shell-files-{}",
                templar_client::file_index::observation_token()
            ),
            search_request: None,
            search_refresh: None,
            search_pending: None,
            browser_sources: BrowserSources::default(),
            browser_listing: None,
            file_thumbnails: std::collections::HashMap::new(),
            file_thumbnails_generation: 0,
            file_thumbnails_pending: std::collections::HashSet::new(),
            home_thumbnail_pending: HashSet::new(),
            notification_rows: Vec::new(),
            toast_stack: crate::surface::notification_popup::ToastStack::default(),
            toast_host: None,
            toast_wanted: (0, 0),
            toast_output: None,
            bar: None,
            bar_screen: crate::surface::bar_screen::BarScreen::default(),
            bar_wanted: (0, 0),
            cluster: None,
            cluster_screen: crate::surface::cluster_screen::ClusterScreen::default(),
            cluster_mask: WindowBuffer::new(0, 0),
            cluster_wanted: (0, 0),
            calendar_focus_claimed: false,
            rail_settings: RailSettings::default(),
            lanes: crate::taskbar::LaneMode::default(),
            active_lane_apps: std::collections::HashSet::new(),
            rail_gate_relayed: None,
            tab_settings: TabSettings::default(),
            tab_policy_relayed: None,
            hosts_gesture: false,
            rail: None,
            rail_dismissed: None,
            rail_generation: 0,
            bar_idle: None,
            cluster_idle: None,
            taskfield: None,
            taskfield_sections: None,
            taskfield_screen: crate::surface::taskfield_screen::TaskfieldScreen::default(),
            taskfield_mask: WindowBuffer::new(0, 0),
            bridge_generation: 0,
            minimize_targets: MinimizeTargets::default(),
            browser_pending: None,
            browser_error: None,
            browser_status: None,
            icons: HashMap::new(),
            icon_attempted: HashSet::new(),
            icon_hintless: HashSet::new(),
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
                let (session_widget, settings_widget, machine) = tokio::join!(
                    crate::data::fetch_session_widget(),
                    crate::data::fetch_settings_widget(),
                    crate::data::fetch_machine_readings(),
                );
                let _ = tx.send(DataUpdate::Machine(machine));
                if tx.send(DataUpdate::Drawers(drawers)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::Apps(apps));
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
                let _ = tx.send(DataUpdate::Widget("settings".to_owned(), settings_widget));
                let _ = tx.send(DataUpdate::TreemapsStale);
                let _ = tx.send(DataUpdate::FilesChanged);
                let _ = proxy.send_event(Wake::Data);
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
            let Ok(identity) = templar_client::proxies::AppIdentityProxy::new(&connection).await
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
            let _ = proxy.send_event(Wake::Data);
            while changed.next().await.is_some() {
                if tx
                    .send(DataUpdate::Apps(crate::data::fetch_apps().await))
                    .is_err()
                {
                    return;
                }
                let _ = proxy.send_event(Wake::Data);
            }
        });
    }

    fn spawn_garden_watch(&self) {
        let tx = self.data_tx.clone();
        let wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let Ok(garden) = templar_client::proxies::HomeGardenProxy::new(&connection).await
            else {
                return;
            };
            let Ok(mut changed) = garden.receive_context_changed().await else {
                return;
            };
            // Subscribe before the initial snapshot. A public placement can create a
            // Place without any managed instance, so it emits no ContextChanged.
            // External tending still publishes Tended when its field batch commits.
            let Ok(mut tended) = garden.receive_tended().await else {
                return;
            };
            loop {
                if tx
                    .send(DataUpdate::Garden(crate::garden::fetch_places().await))
                    .is_err()
                {
                    return;
                }
                match garden.working_directories().await {
                    Ok(json) => match crate::garden::decode_working_directories(&json) {
                        Ok(cwds) => {
                            if tx.send(DataUpdate::WorkingDirectories(cwds)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "reading terminal working directories");
                        }
                    },
                    Err(error) => tracing::warn!(%error, "fetching terminal working directories"),
                }
                let _ = wake.send_event(Wake::Data);
                let published = tokio::select! {
                    event = changed.next() => event.is_some(),
                    event = tended.next() => event.is_some(),
                };
                if !published {
                    return;
                }
            }
        });
    }

    fn spawn_desktop_watch(&self) {
        let tx = self.data_tx.clone();
        let wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            match super::desktop_dock::desktop_directory().await {
                Ok(Some(path)) => {
                    let observed = path.clone();
                    super::desktop_dock::watch_directory(path, move |listing| {
                        let _ = tx.send(DataUpdate::DesktopDirectory {
                            path: observed.clone(),
                            listing,
                        });
                        let _ = wake.send_event(Wake::Data);
                    })
                    .await;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(format!("Desktop unavailable: {error}")));
                    let _ = wake.send_event(Wake::Data);
                }
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
            let _ = proxy.send_event(Wake::Data);
        });
    }

    fn spawn_field_fetch_remote(&self, cols: u16, rows: u16) {
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            if let Some(doc) = crate::data::fetch_field(OUTPUT, cols, rows).await {
                let _ = tx.send(DataUpdate::FieldRemote(doc));
                let _ = proxy.send_event(Wake::Data);
            }
        });
    }

    /// Take what another client did to the field (`field_model::reconcile_remote`): a tile
    /// placed elsewhere, a Place the garden resized, a Place removed, hidden or forgotten.
    fn adopt_remote_tiles(&mut self, remote: &FieldDocument) {
        for op in field_model::reconcile_remote(&self.field, remote) {
            match templar_field::apply(&self.field, &op) {
                Ok((next, _)) => self.field = next,
                Err(error) => {
                    tracing::warn!(%error, op = op.label(), "taking a change made elsewhere");
                }
            }
        }
        self.sync_desktop_order();
    }

    fn sync_desktop_order(&mut self) {
        let Some(dock) = self.desktop_dock.as_mut() else {
            return;
        };
        let Some(order) = self.field.tiles.iter().find_map(|tile| match &tile.kind {
            TileKind::DesktopDock { path, order } if *path == dock.path => Some(order),
            _ => None,
        }) else {
            return;
        };
        if dock.order() == *order {
            return;
        }
        match dock
            .apply_saved_order(order.clone())
            .and_then(|_| dock.browse_listing())
        {
            Ok(listing) => {
                self.folder_listings.insert(dock.path.clone(), Ok(listing));
            }
            Err(error) => self
                .field_screen
                .report(error.to_string(), self.started.elapsed().as_millis() as u64),
        }
    }

    fn relay_reduced_motion(&mut self) {
        let enabled = self.field_settings.reduced_motion;
        if self.reduced_motion_sent == Some(enabled) {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        match bridge.set_reduced_motion(enabled) {
            Ok(()) => self.reduced_motion_sent = Some(enabled),
            Err(error) => tracing::warn!(%error, "relaying reduced motion"),
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
            let Ok(proxy) = templar_client::proxies::HomeFieldProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                if tx.send(DataUpdate::FieldChanged).is_err() {
                    return;
                }
                let _ = proxy_wake.send_event(Wake::Data);
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
                let _ = proxy.send_event(Wake::Data);
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
                let _ = proxy.send_event(Wake::Data);
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
    /// last told, so re-sending the same id would be a round trip for nothing.
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

    fn discard_search(&mut self) {
        if let Some((_, _, Ok(batch))) = self.search_results.take() {
            self.runtime.spawn(async move {
                if let Err(error) = batch.session.close().await {
                    tracing::debug!(%error, "closing replaced file search cursor");
                }
            });
        }
    }

    /// Note a question. It is asked once the typing has stopped, in [`Self::flush_search`].
    fn note_search(&mut self, scope: Scope, query: String) {
        if let Some(request) = self.search_request.take() {
            request.abort();
        }
        self.search_refresh = None;
        self.search_generation = self
            .search_generation
            .checked_add(1)
            .expect("search generation exhausted");
        self.discard_search();
        if matches!(scope, Scope::Location(_)) {
            query.trim().clone_into(&mut self.location_query);
        }
        if query.trim().is_empty() {
            self.search_pending = None;
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
        let roots: Vec<PathBuf> = match &scope {
            Scope::Location(location) => location
                .directory()
                .map(|path| vec![path.to_path_buf()])
                .unwrap_or_default(),
            Scope::Applications => Vec::new(),
            Scope::LauncherFiles(path) => path.iter().cloned().collect(),
        };
        if roots.is_empty() && !matches!(scope, Scope::LauncherFiles(_)) {
            return;
        }
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        let request_generation = self.search_generation;
        let tag = self.search_tag.clone();
        let request = self.runtime.spawn(async move {
            let result = run_search(roots, &query, &tag).await;
            let _ = tx.send(DataUpdate::SearchResults {
                scope,
                query,
                request_generation,
                result,
            });
            let _ = proxy.send_event(Wake::Data);
        });
        self.search_request = Some(request.abort_handle());
    }

    fn fetch_search_pages(&mut self) {
        let (visible, offset) = if self.launcher_screen.state().open {
            let visible = self.launcher_screen.visible_range();
            let slots = self.launcher_file_slots();
            let first = slots.get(visible.0).copied().unwrap_or(0);
            let last = slots
                .get(visible.1.saturating_sub(1))
                .map_or(first, |index| index + 1);
            ((first, last), self.launcher_screen.scroll_offset())
        } else {
            (
                self.field_screen.files_view.search_visible,
                self.field_screen.files.entry_scroll.offset(),
            )
        };
        let Some((_, _, Ok(batch))) = self.search_results.as_mut() else {
            return;
        };
        batch.pages.scrolled(offset);
        for page in batch.pages.request(visible) {
            let session = batch.session.clone();
            let tx = self.data_tx.clone();
            let wake = self.wake_proxy.clone();
            let request_generation = self.search_generation;
            self.runtime.spawn(async move {
                let result = session
                    .fetch_page(
                        page * crate::surface::search_pages::PAGE_SIZE,
                        crate::surface::search_pages::PAGE_SIZE,
                    )
                    .await;
                let _ = tx.send(DataUpdate::SearchPage {
                    request_generation,
                    cursor: session.result().cursor.clone(),
                    page,
                    result,
                });
                let _ = wake.send_event(Wake::Data);
            });
        }
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
                let _ = proxy.send_event(Wake::Data);
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
            let _ = proxy.send_event(Wake::Data);
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
            let _ = proxy.send_event(Wake::Data);
        });
    }

    /// Open a folder in whichever application holds [`ProviderRole::FileManager`].
    ///
    /// Not a hardcoded application. TemplarOS's own Files is no longer the file manager — pcmanfm-qt
    /// is — and the role is the only thing that knows which one this session chose, so this takes
    /// the same route `project.open-files` already took (`tool_launch`) rather than naming one.
    fn open_in_file_manager(&mut self, path: &std::path::Path) {
        // `project.open-files` *is* this route's argument shape — the bare directory. Asking
        // [`tool_launch`] for it rather than writing it out a second time is what stops the Place
        // menu's row and a Folder tile from drifting apart, which is the whole of K3.
        let root = path
            .to_str()
            .map_or_else(|| templar_types::uri_list::encode_path(path), str::to_owned);
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
            let _ = proxy.send_event(Wake::Data);
        });
    }

    #[allow(clippy::too_many_lines)]
    fn drain_updates(&mut self) {
        match self.appearance.take_update() {
            Some(Ok(appearance)) => {
                self.field_settings.reduced_motion = appearance.reduced_motion;
                if design::set_palette(appearance.palette) {
                    self.invalidate_treemap();
                    for hosted in [
                        &self.home,
                        &self.bar,
                        &self.cluster,
                        &self.taskfield,
                        &self.launcher,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        hosted.request_redraw();
                    }
                    if let Some(hosted) = &self.toast_host {
                        hosted.request_redraw();
                    }
                }
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "appearance observation unavailable; retaining last palette");
            }
            None => {}
        }
        while let Ok(update) = self.data_rx.try_recv() {
            if let Some(home) = &mut self.home {
                home.invalidate_semantics();
            }
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
                        self.active_lane_apps = crate::taskbar::active_lane_apps(&apps);
                        let wanted: Vec<String> = apps
                            .iter()
                            .filter(|entry| entry.variants > 1 && !entry.logical.is_empty())
                            .map(|entry| entry.logical.clone())
                            .collect();
                        self.apps = apps;
                        // An icon looked up before the catalogue knew the application went without
                        // its hints; now that it does, the next frame asks again with them.
                        let apps = &self.apps;
                        let icons = &self.icons;
                        let retry: Vec<String> = self
                            .icon_hintless
                            .iter()
                            .filter(|key| {
                                !icons.contains_key(*key)
                                    && apps.iter().any(|entry| entry.id.as_str() == key.as_str())
                            })
                            .cloned()
                            .collect();
                        for key in retry {
                            self.icon_hintless.remove(&key);
                            self.icon_attempted.remove(&key);
                        }
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
                                let _ = proxy.send_event(Wake::Data);
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
                DataUpdate::WorkingDirectories(cwds) => {
                    self.working_directories.clone_from(&cwds);
                    self.field_screen.set_terminal_cwds(cwds);
                }
                DataUpdate::LauncherPreferences { epoch, preferences } => {
                    if epoch
                        < self
                            .launcher_epoch
                            .load(std::sync::atomic::Ordering::SeqCst)
                    {
                        continue;
                    }
                    if preferences != self.launcher_preferences {
                        self.launcher_preferences = preferences;
                        self.bar_screen.reset();
                        if let Some(launcher) = &self.launcher {
                            launcher.request_redraw();
                        }
                        if let Some(bar) = &self.bar {
                            bar.request_redraw();
                        }
                    }
                }
                DataUpdate::FavoriteLaunched {
                    id,
                    target,
                    instance,
                } => {
                    if self.favorite_instances.len() >= 512 {
                        let live: HashSet<_> = self
                            .bridge
                            .as_ref()
                            .map(ShellBridge::toplevels)
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|window| {
                                window.instance_id.map(|instance| instance.to_string())
                            })
                            .collect();
                        self.favorite_instances
                            .retain(|_, instance| live.contains(instance));
                        if self.favorite_instances.len() >= 512 {
                            if let Some(key) = self.favorite_instances.keys().next().cloned() {
                                self.favorite_instances.remove(&key);
                            }
                        }
                    }
                    self.favorite_instances
                        .insert(launcher_host::profile_key(&id, &target), instance);
                }
                DataUpdate::WindowTransition(kind, size) => {
                    self.window_transition = (kind, size);
                    self.relay_window_transition();
                }
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
                DataUpdate::WallpaperApplied(applied) => self.wallpaper_applied = applied,
                DataUpdate::HomeFieldSettings(settings) => {
                    self.field_settings = settings;
                    self.field_screen.drag_cfg.throw_enabled =
                        settings.throw && !settings.reduced_motion;
                    self.field_screen
                        .place_styles
                        .set_theme(settings.garden_theme);
                    self.relay_reduced_motion();
                    if settings.reduced_motion
                        && let Some(rail) = &mut self.rail
                    {
                        rail.reveal.show_all();
                    }
                }
                DataUpdate::RailSettings(settings) => {
                    self.rail_settings = settings;
                    self.relay_gesture_gate();
                }
                DataUpdate::Lanes(lanes) => {
                    if self.lanes != lanes {
                        self.lanes = lanes;
                        // The chips change place, and a fan or hover index names a place.
                        self.bar_screen.reset();
                    }
                }
                DataUpdate::TabSettings(settings) => {
                    self.tab_settings = settings;
                    self.relay_tab_policy();
                }
                DataUpdate::Provider { role, app_id } => self.note_provider(role, app_id),
                DataUpdate::LockRequested => {
                    self.dismiss_launcher();
                    self.engage_lock();
                }
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
                DataUpdate::Treemap { epoch, preview } => {
                    let requested = self.treemap_pending.as_ref().is_some_and(
                        |(path, width, height, pending_epoch)| {
                            path == &preview.path
                                && *width == preview.width
                                && *height == preview.height
                                && *pending_epoch == epoch
                        },
                    );
                    if requested {
                        self.treemap_pending = None;
                    }
                    let current_path = self
                        .field_screen
                        .files
                        .location()
                        .directory()
                        .or(self.home_dir.as_deref());
                    if requested
                        && epoch == self.treemap_epoch
                        && current_path == Some(preview.path.as_path())
                    {
                        self.treemap = Some(preview);
                    }
                }
                // A journalled operation finished. This is the one progress-and-error surface
                // for everything the Home file surfaces do: one operation, one report.
                DataUpdate::FileOperation { undo, notice } => {
                    let undo_available = undo.is_some();
                    if let Some(undo) = undo {
                        self.last_file_operation = Some(undo);
                    }
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen
                        .report_operation(notice, undo_available, now_ms);
                }
                DataUpdate::Notice(text) => {
                    if self.launcher_screen.state().open {
                        self.launcher_screen.show_info(text.clone());
                        if let Some(launcher) = &self.launcher {
                            launcher.request_redraw();
                        }
                    }
                    let now_ms = self.started.elapsed().as_millis() as u64;
                    self.field_screen.report(text, now_ms);
                }
                DataUpdate::Icon(key, bitmap) => {
                    if let Some(bitmap) = bitmap {
                        self.icon_hintless.remove(&key);
                        self.icons.insert(key, bitmap);
                    }
                    if let Some(hosted) = &self.toast_host {
                        hosted.request_redraw();
                    }
                }
                DataUpdate::FolderListing { path, result } => {
                    self.folder_pending.remove(&path);
                    if self
                        .desktop_dock
                        .as_ref()
                        .is_some_and(|dock| dock.path == path)
                    {
                        continue;
                    }
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
                DataUpdate::HomeThumbnail { path, bitmap } => {
                    if self.field_settings.previews != PreviewMode::Off
                        && self.field_screen.thumbnail_paths().contains(&path)
                        && self.file_thumbnails.len() < 64
                        && let Some(bitmap) = bitmap
                    {
                        self.home_thumbnail_pending.remove(&path);
                        self.file_thumbnails.insert(path, bitmap);
                    }
                }
                DataUpdate::DesktopDirectory { path, listing } => {
                    if self
                        .desktop_dock
                        .as_ref()
                        .is_none_or(|dock| dock.path != path)
                    {
                        let order = self
                            .field
                            .tiles
                            .iter()
                            .find_map(|tile| match &tile.kind {
                                TileKind::DesktopDock {
                                    path: existing,
                                    order,
                                } if *existing == path => Some(order.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        self.desktop_dock =
                            Some(super::desktop_dock::DesktopDock::new(path.clone(), order));
                    }
                    match listing {
                        Ok(listing) => {
                            let ordered = self.desktop_dock.as_mut().map(|dock| {
                                dock.apply_listing(listing)?;
                                Ok::<_, anyhow::Error>((
                                    dock.browse_listing()?,
                                    dock.order(),
                                    dock.path.clone(),
                                ))
                            });
                            if let Some(result) = ordered {
                                match result {
                                    Ok((listing, order, root)) => {
                                        self.folder_listings.insert(path, Ok(listing));
                                        if let Some(id) = self.field.tiles.iter().find_map(|tile| {
                                            match &tile.kind {
                                                TileKind::DesktopDock { path, order: old }
                                                    if *path == root && *old != order =>
                                                {
                                                    Some(tile.id)
                                                }
                                                _ => None,
                                            }
                                        }) {
                                            self.perform(FieldAction::Apply(
                                                FieldOp::DesktopDockOrder { id, order },
                                            ));
                                        }
                                    }
                                    Err(error) => self.field_screen.report(
                                        error.to_string(),
                                        self.started.elapsed().as_millis() as u64,
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            self.folder_listings.insert(path, Err(error));
                        }
                    }
                }
                DataUpdate::DockHandlers { path, handlers } => match handlers {
                    Ok(handlers) if !handlers.is_empty() => {
                        self.field_screen.set_dock_handlers(path, handlers);
                    }
                    Ok(_) => self.field_screen.report(
                        "No applications can open that file".to_owned(),
                        self.started.elapsed().as_millis() as u64,
                    ),
                    Err(error) => self
                        .field_screen
                        .report(error, self.started.elapsed().as_millis() as u64),
                },
                DataUpdate::NativeTransferFinished {
                    generation,
                    success,
                } => {
                    if !self.native_drop_pending || generation != self.native_drop_generation {
                        continue;
                    }
                    self.native_drop_pending = false;
                    self.native_drop_target = None;
                    self.native_drop_item = None;
                    self.native_drag_origin = None;
                    self.field_screen.set_native_drop_hover(None, false);
                    if let Some(bridge) = self.bridge.as_mut()
                        && let Err(error) = bridge.complete_native_drop(success)
                    {
                        tracing::warn!(%error, "completing native file drop");
                    }
                }
                DataUpdate::Notifications(update) => {
                    if let Some(quiet_through) = update.quiet_through {
                        let baseline: Vec<_> = update
                            .active
                            .iter()
                            .filter(|row| row.sequence <= quiet_through)
                            .cloned()
                            .collect();
                        self.toast_stack.adopt_snapshot(&baseline);
                        self.toast_host = None;
                        self.toast_output = None;
                        self.toast_wanted = (0, 0);
                    }
                    self.toast_stack.note_active(
                        &update.active,
                        Instant::now(),
                        notification_popup::now_ms(),
                    );
                    // Consume locked arrivals without retaining a queue for unlock.
                    if self.bridge.as_ref().is_some_and(ShellBridge::lock_engaged) {
                        self.toast_stack.hide_all();
                    }
                    let rows = update
                        .active
                        .iter()
                        .map(
                            |row| crate::surface::widgets::notifications::NotificationRow {
                                id: row.id,
                                app: row.app_name.clone(),
                                summary: row.summary.clone(),
                                actions: row.actions.clone(),
                            },
                        )
                        .collect();
                    let (snapshot, rows) = crate::data::notification_widget(rows);
                    self.notification_rows = rows;
                    self.widgets.insert("notifications".to_owned(), snapshot);
                    if let Some(hosted) = &self.toast_host {
                        hosted.request_redraw();
                    }
                }
                DataUpdate::TreemapsStale => self.invalidate_treemap(),
                DataUpdate::Field(doc) => {
                    self.field = doc;
                    self.sync_desktop_order();
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
                    request_generation,
                    mut result,
                } => {
                    if request_generation != self.search_generation
                        || !self.search_scope_current(&scope, &query)
                    {
                        if let Ok(batch) = result {
                            self.runtime.spawn(async move {
                                let _ = batch.session.close().await;
                            });
                        }
                        continue;
                    }
                    if matches!(
                        result,
                        Err(templar_client::file_index::IndexCallError::Cancelled)
                    ) {
                        continue;
                    }
                    if let (Ok(batch), Some(old)) = (&mut result, self.search_refresh.take()) {
                        batch.pages.preserve_refresh(&old);
                    }
                    self.discard_search();
                    self.search_results =
                        Some((scope, query, result.map_err(|error| error.to_string())));
                }
                DataUpdate::SearchPage {
                    request_generation,
                    cursor,
                    page,
                    result,
                } => {
                    if request_generation != self.search_generation {
                        continue;
                    }
                    let selected = if self.launcher_screen.state().open {
                        self.launcher_selected_path()
                    } else {
                        self.field_screen.files.selected_paths().into_iter().next()
                    };
                    let Some((scope, query, Ok(batch))) = self.search_results.as_mut() else {
                        continue;
                    };
                    if batch.session.result().cursor != cursor {
                        continue;
                    }
                    use templar_client::file_index::IndexCallError;
                    let result = match result {
                        Ok(result) if result.dropped != 0 => Err(IndexCallError::SnapshotRequired(
                            "matching rows changed".into(),
                        )),
                        result => result,
                    };
                    match result {
                        Ok(result) => match search_entries(&result.hits) {
                            Ok(entries) => {
                                batch.pages.accept(result.generation, page, entries);
                            }
                            Err(error) => {
                                batch.pages.pending.remove(&page);
                                batch.pages.notice = Some(error.to_string());
                            }
                        },
                        Err(IndexCallError::SnapshotRequired(_)) => {
                            if batch.pages.snapshot_required(Instant::now(), selected) {
                                self.search_refresh = Some(batch.pages.clone());
                                batch.pages.paused = true;
                                self.search_generation += 1;
                                self.search_pending = Some((
                                    scope.clone(),
                                    query.clone(),
                                    Instant::now() - Self::SEARCH_DEBOUNCE,
                                ));
                            }
                        }
                        Err(IndexCallError::Cancelled) => {
                            batch.pages.pending.remove(&page);
                        }
                        Err(error) => {
                            batch.pages.pending.remove(&page);
                            batch.pages.notice = Some(error.to_string());
                            batch.pages.paused = true;
                        }
                    }
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
                    if let Err(error) = &result {
                        self.field_screen
                            .report(error.clone(), self.started.elapsed().as_millis() as u64);
                    }
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
                    self.invalidate_treemap();
                    if let Some((scope, query, _)) = &self.search_results {
                        let (scope, query) = (scope.clone(), query.clone());
                        self.note_search(scope, query);
                    }
                    let paths = invalidate_folder_listings(
                        &mut self.folder_listings,
                        self.desktop_dock.as_ref().map(|dock| dock.path.as_path()),
                    );
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
        if entry.is_none() {
            self.icon_hintless.insert(key.to_owned());
        }
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
            if bitmap.is_none() {
                // The one thing a bar chip drawn as an initial cannot tell anybody: which of the
                // key, the `Icon=` value and the theme search came up empty.
                crate::icons::log_icon_miss_once(
                    &key,
                    icon.as_deref(),
                    desktop_file_ids.first().map(String::as_str),
                );
            }
            // A miss is sent too: `icon_attempted` already recorded it, so the receiver has
            // nothing to do, but sending unconditionally keeps the two sides symmetric.
            let _ = tx.send(DataUpdate::Icon(key, bitmap));
            let _ = proxy.send_event(Wake::Data);
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
            let _ = proxy.send_event(Wake::Data);
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
        toplevels: &[templar_window::ShellToplevel],
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
            match templar_field::apply(&self.field, &op) {
                Ok((next, _)) => {
                    self.field = next;
                    sending.push(op);
                }
                Err(error) => tracing::warn!(%error, "resizing the field grid"),
            }
        }
        let live = LiveState {
            projects: &self.projects,
            toplevels,
        };
        sending.extend(field_model::merge(&mut self.field, &live));
        if !sending.is_empty() && self.field_synced == Some(true) {
            self.spawn_field_apply(FieldOp::Batch { ops: sending });
        }
        let keys: Vec<String> = toplevels
            .iter()
            .filter_map(|t| t.icon_app_id().map(str::to_owned))
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
        let query = self.launcher_screen.state().query.clone();
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

    /// The Home-only visible preview policy: capture demand derives from the actual Home card
    /// wells laid out this frame, not from the toplevel list. Only wells with visible pixels
    /// are eligible — offscreen, overflow-only, zoom-hidden, fully occluded, locked and
    /// output-disabled cards demand nothing, and their pacing entries are pruned so no preview
    /// deadline survives for them either. The compositor checks the same demand at admission
    /// and before render; a trusted-shell request alone proves no card visible.
    ///
    /// Demand comes from the same clipped, occlusion-checked picture wells painted
    /// this frame, not whole cards or maximized windows on unrelated outputs.
    fn visible_preview_windows(&self, bridge: &ShellBridge) -> HashSet<templar_window::WindowId> {
        if bridge.lock_locked() || bridge.lock_engaged() || self.home.is_none() {
            return HashSet::new();
        }
        self.field_screen
            .work_wells()
            .unwrap_or(&[])
            .iter()
            .map(|(window, _)| *window)
            .collect()
    }

    fn refresh_previews(&mut self, toplevels: &[templar_window::ShellToplevel]) {
        let Some(bridge) = self.bridge.as_ref() else {
            return;
        };
        let disabled =
            self.field_settings.previews == PreviewMode::Off || !bridge.previews_available();
        let private = bridge.lock_locked() || bridge.lock_engaged();
        let visible = if disabled {
            HashSet::new()
        } else {
            self.visible_preview_windows(bridge)
        };
        if private || disabled {
            self.previews.clear();
        }
        let order: Vec<_> = visible.iter().copied().collect();
        if self.preview_demand.set_visible(&order) || !self.preview_demand_sent {
            self.preview_requested.clear();
            let generation = self.preview_demand.generation();
            if let Some(bridge) = self.bridge.as_mut() {
                let demanded: std::collections::HashSet<String> = order
                    .iter()
                    .filter_map(|window| bridge.identifier_of(*window))
                    .map(str::to_owned)
                    .collect();
                bridge.retire_preview_demand(&demanded);
                if let Err(error) = bridge.send_preview_demand(generation, &order) {
                    self.preview_demand_sent = false;
                    tracing::debug!(%error, "preview demand send skipped");
                } else {
                    self.preview_demand_sent = true;
                }
            }
        }
        self.drain_previews(toplevels);

        let now = Instant::now();
        let pacing = match self.field_settings.previews {
            PreviewMode::Off => crate::previews::DemandPacing::Off,
            PreviewMode::Static => crate::previews::DemandPacing::Static,
            PreviewMode::Live => crate::previews::DemandPacing::Live {
                interval: Duration::from_millis(
                    1000 / u64::from(self.field_settings.preview_rate.max(1)),
                ),
            },
        };
        let order = crate::previews::stalest_first(
            &visible.iter().copied().collect::<Vec<_>>(),
            &self.preview_requested,
            now,
        );
        if let Some(bridge) = self.bridge.as_mut() {
            for window in &order {
                let Some(toplevel) = toplevels.iter().find(|t| t.window_id == *window) else {
                    continue;
                };
                let Some(identifier) = bridge.identifier_of(*window).map(str::to_owned) else {
                    continue;
                };
                let key = field_model::session_identifier(toplevel, None);
                let last = self.preview_requested.get(window).copied();
                let due = !bridge.preview_pending(&identifier)
                    && crate::previews::capture_due(
                        pacing,
                        true,
                        self.previews.contains_key(&key),
                        last,
                        now,
                        false,
                    )
                    && (pacing != crate::previews::DemandPacing::Static || last.is_none());
                if !due {
                    continue;
                }
                match bridge.request_preview(&identifier) {
                    Ok(()) => {
                        self.preview_requested.insert(*window, now);
                    }
                    Err(error) => tracing::debug!(%error, "preview request skipped"),
                }
            }
        }
        // Undemanded windows keep no pacing entry: no preview deadline survives for them, Live
        // re-fires on the first reveal even when the output is otherwise idle, and Static
        // shows its retained last frame instead of re-capturing.
        self.preview_requested.retain(|id, _| visible.contains(id));
        let demanded_keys: std::collections::HashSet<String> = toplevels
            .iter()
            .filter(|toplevel| visible.contains(&toplevel.window_id))
            .map(|toplevel| field_model::session_identifier(toplevel, None))
            .collect();
        crate::previews::evict_over_budget(
            &mut self.previews,
            &demanded_keys,
            crate::previews::AGGREGATE_BUDGET_BYTES,
        );
    }

    fn preview_deadline(&self) -> Option<Instant> {
        if self.field_settings.previews != PreviewMode::Live {
            return None;
        }
        let bridge = self.bridge.as_ref()?;
        if bridge.lock_engaged() || bridge.lock_locked() {
            return None;
        }
        let interval =
            Duration::from_millis(1000 / u64::from(self.field_settings.preview_rate.clamp(1, 4)));
        self.preview_requested
            .iter()
            .filter(|(window, _)| self.preview_demand.is_demanded(**window))
            .filter(|(window, _)| {
                bridge
                    .identifier_of(**window)
                    .is_some_and(|id| !bridge.preview_pending(id))
            })
            .map(|(_, at)| *at + interval)
            .min()
    }

    /// File whatever the compositor has finished capturing and forget the windows that are gone.
    /// Separate from the request policy because a suppressed field still has to accept the frames
    /// already in flight — dropping them on the floor would leave the buffers un-released.
    fn drain_previews(&mut self, toplevels: &[templar_window::ShellToplevel]) {
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
        self.drain_previews(&toplevels);
        let Some(home) = &self.home else { return };
        let scale = home.scale;
        let logical = home
            .window
            .inner_size()
            .to_logical::<f64>(home.window.scale_factor());
        // The bar covers the bottom strip of the output, on Home too; the field ends above it.
        // Rails do not: they are there only while asked for, and the field keeps the whole screen.
        let bar_inset = if self.bar.is_some() && !self.hosts_gesture {
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
        self.fetch_search_pages();

        // A search result is shown only while it still answers the question on screen: the same
        // scope and the same query. The staleness guard is the one it always was, keyed on the
        // scope now that a drawer is not a thing.
        let scope = if self.field_screen.files_expanded {
            Scope::Location(self.field_screen.files.location().clone())
        } else {
            Scope::Applications
        };
        if self.search_results.as_ref().is_some_and(|(owner, _, _)| {
            !matches!(owner, Scope::LauncherFiles(_)) && *owner != scope
        }) {
            self.discard_search();
        }
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
        if let Some((_, _, Ok(batch))) = self.search_results.as_mut() {
            let height = scale.px(crate::surface::file_browser::ENTRY_HEIGHT);
            if batch
                .pages
                .heights
                .first()
                .is_some_and(|old| *old != height)
            {
                batch.pages.heights.fill(height);
            }
        }
        let active_search = self
            .search_results
            .as_ref()
            .filter(|(had, _, _)| *had == scope);
        let search_query = active_search.map(|(_, query, _)| query.as_str());
        let search_entries = active_search
            .and_then(|(_, _, result)| result.as_ref().ok())
            .map(|batch| batch.entries.as_slice());
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
            status: active_search
                .and_then(|(_, _, result)| result.as_ref().ok())
                .map(|batch| batch.pages.notice.as_deref().unwrap_or(&batch.notice))
                .or(self.browser_status.as_deref()),
            search_query,
            search_entries,
            search_pages: active_search
                .and_then(|(_, _, result)| result.as_ref().ok())
                .map(|batch| &batch.pages),
            search_error,
            feeds: &self.drawer_feeds,
            thumbnails: &self.file_thumbnails,
            icons: &self.icons,
            home: self.home_dir.as_deref(),
        };
        self.field_screen.previews_on = self.field_settings.previews != PreviewMode::Off;
        let data = FieldData {
            notifications: &self.notification_rows,
            field: &self.field,
            toplevels: &toplevels,
            providers: &self.providers,
            icons: &self.icons,
            previews: &self.previews,
            drawer_feeds: &self.drawer_feeds,
            treemap: self.treemap.as_ref(),
            browser: &browser_data,
            folder_listings: &self.folder_listings,
            connected: self
                .bridge
                .as_ref()
                .is_some_and(ShellBridge::native_available),
            home_revealed,
            apps: &self.apps,
            widgets: &self.widgets,
            variants: &self.app_variants,
            garden: &self.garden,
            reduced_motion: self.field_settings.reduced_motion,
            now_ms: self.started.elapsed().as_millis() as u64,
        };
        if self.field_screen.animating() {
            home.invalidate_semantics();
        }
        home.begin_semantics("Home");
        let modal = self.field_screen.modal_open();
        if modal {
            home.focus.clear();
        }
        self.field_screen.controls_focused = home.focus.current().is_some();
        self.field_screen.shared_focus = !modal;
        self.field_screen.wallpaper_active = self
            .bridge
            .as_ref()
            .is_some_and(ShellBridge::native_available)
            && wallpaper_for_home(&home.window, self.wallpaper_applied.as_ref());
        let actions = {
            let painter = if self.bridge.is_some() {
                Painter::new_premultiplied(&mut home.buffer)
            } else {
                Painter::new(&mut home.buffer)
            };
            let mut ui = Ui::new(
                painter,
                &mut self.text,
                &mut home.input,
                self.theme,
                scale,
                &mut self.kernel,
            )
            .with_semantics(&mut home.semantics);
            if !modal {
                ui = ui.with_focus(&mut home.focus);
            }
            let area = Rect::new(0, 0, width, height);
            self.field_screen.draw(&mut ui, area, &data)
        };
        if !modal && self.field_screen.controls_focused {
            self.field_screen.focus = None;
        }
        home.update_ime(self.field_screen.ime_caret());
        home.publish_semantics();
        home.input.end_frame();
        if let Some(bridge) = &mut self.bridge {
            // Transparent ground still takes selection/input everywhere, just as opaque Home did.
            let size = home
                .window
                .inner_size()
                .to_logical::<f64>(home.window.scale_factor());
            let regions = [(0, 0, size.width.ceil() as i32, size.height.ceil() as i32)];
            match bridge.present_premultiplied(
                &home.window,
                &home.buffer.pixels,
                &regions,
                home.buffer.width,
                home.buffer.height,
            ) {
                Ok(PresentOutcome::Submitted) => home.pending = false,
                Ok(PresentOutcome::Deferred) => home.pending = true,
                Err(error) => {
                    home.pending = false;
                    tracing::error!(%error, "presenting Home's premultiplied frame");
                }
            }
        } else {
            // Non-Wayland keeps its existing opaque software path; native wallpaper is unavailable.
            home.present();
        }
        if let Some(bar) = &self.bar {
            bar.request_redraw();
        }
        // Before the actions: a press on a card that parks its window should find the card
        // already known to the compositor, so the window shrinks into it.
        self.send_minimize_targets(&toplevels);
        self.refresh_previews(&toplevels);
        if let Some(positions) = self.field_screen.take_position_memory()
            && let Err(error) = positions.save()
        {
            tracing::warn!(%error, "saving soft work-card positions");
        }

        self.home_thumbnail_pending
            .retain(|path| self.field_screen.thumbnail_paths().contains(path));
        for action in actions {
            self.perform(action);
        }
        // The browse grid told us during the draw which cells it can see; hand the bounded
        // thumbnail work to the preview service now, off-thread.
        self.spawn_thumbnails();
        self.export_drag_if_lifted();
    }

    /// Tell the compositor what changed about where each window lives on Home since the last
    /// frame (`ShellBridge::set_minimize_target`), so parking shrinks a window into its card.
    fn send_minimize_targets(&mut self, toplevels: &[templar_window::ShellToplevel]) {
        let (Some(bridge), Some(home)) = (&self.bridge, &self.home) else {
            return;
        };
        let Some(targets) = self.field_screen.work_targets() else {
            return;
        };
        let scale = home.window.scale_factor();
        let changes = self.minimize_targets.changes(
            targets.filter_map(|(window, rect)| Some((window, logical_rect(rect, scale)?))),
            toplevels,
        );
        for (window, rect) in changes {
            match bridge.set_minimize_target(window, &home.window, rect) {
                Ok(true) => self.minimize_targets.record(window, rect),
                Ok(false) => {
                    tracing::info!(
                        "the compositor predates set_minimize_target; parking stays a cut"
                    );
                    self.minimize_targets.unsupported = true;
                    return;
                }
                Err(error) => tracing::debug!(%error, ?window, "set_minimize_target failed"),
            }
        }
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

            FieldAction::LaunchProfile(profile) => {
                self.activate_launch_profile(profile, crate::surface::omnibox_screen::Intent::Open)
            }
            FieldAction::ShowLauncher { mode, scope, query } => {
                self.request_launcher(mode, scope, query)
            }
            FieldAction::RunSearch { scope, query } => self.note_search(scope, query),
            FieldAction::Launch(app_id) => self.activate_launcher_target(
                LauncherTarget::App(app_id),
                crate::surface::omnibox_screen::Intent::Open,
            ),
            // Named on purpose, so it goes straight to `Launcher1.Launch` for that exact copy
            // rather than back through the daemon's default-variant policy.
            FieldAction::LaunchVariant(app_id) => {
                self.record_recent(&app_id);
                let label = self.app_label(&app_id);
                let tx = self.data_tx.clone();
                let proxy_wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    let Ok(connection) = crate::data::session().await else {
                        return;
                    };
                    if let Ok(proxy) = LauncherProxy::new(&connection).await
                        && let Err(error) = proxy.launch(&app_id.to_string(), Vec::new(), "").await
                    {
                        tracing::warn!(%error, %app_id, "launching a chosen copy failed");
                        let _ = tx.send(DataUpdate::Notice(launch_failure_notice(&label, &error)));
                        let _ = proxy_wake.send_event(Wake::Data);
                    }
                });
            }
            FieldAction::Activate(window_id) | FieldAction::Restore(window_id) => {
                self.bridge_call("activate", |bridge| bridge.activate(window_id));
                if let Some(instance) = self
                    .bridge
                    .as_ref()
                    .map(ShellBridge::toplevels)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|top| top.window_id == window_id)
                    .and_then(|top| top.instance_id)
                {
                    let tx = self.data_tx.clone();
                    let wake = self.wake_proxy.clone();
                    self.runtime.spawn(async move {
                        crate::garden::activate_instance(instance).await;
                        let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
                        let _ = wake.send_event(Wake::Data);
                    });
                }
            }
            FieldAction::Park(window_id) => {
                self.bridge_call("shelve", |bridge| bridge.shelve(window_id));
            }
            FieldAction::BrowseLocation {
                location,
                show_hidden,
            } => {
                self.spawn_browse(location, show_hidden, true);
                if let Some(home) = &self.home {
                    home.request_redraw();
                }
            }
            FieldAction::BrowseFolder { path, visited } => self.spawn_folder_browse(path, visited),
            FieldAction::OpenFile(path) => self.spawn_open_file(path),
            FieldAction::OpenInFiles(path) => self.open_in_file_manager(&path),
            FieldAction::RestoreFromTrash(trashed_path) => self.spawn_trash_restore(trashed_path),
            FieldAction::DropFiles(drop) => self.spawn_file_drop(&drop),
            FieldAction::FileVerb(request) => self.perform_file_verb(&request),
            FieldAction::RunAction(id) => self.perform_declared(id),
            FieldAction::Adopt { path, window } => self.perform_adopt(&path, window),
            FieldAction::Taskfield(key) => {
                self.open_taskfield(&templar_window::taskfield::TaskFilter::Unit(key), "");
            }
            FieldAction::SetVolume(percent) => self.spawn_volume_set(percent),
            FieldAction::SetMuted(muted) => self.spawn_volume_muted(muted),
            FieldAction::SetBrightness(percent) => self.spawn_brightness_set(percent),
            FieldAction::Project { id, project_id } => self.perform_project(id, &project_id),
            FieldAction::Place { id, path } => self.perform_place(id, &path),
            FieldAction::ResumeSession { path, session_id } => {
                let tx = self.data_tx.clone();
                let wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    let outcome = crate::garden::resume_session(&path, &session_id).await;
                    if let Err(error) = outcome {
                        let _ = tx.send(DataUpdate::Notice(error));
                    }
                    let _ = wake.send_event(Wake::Data);
                });
            }
            FieldAction::HomeThumbnail(path) => {
                if self.file_thumbnails.len() >= 64 {
                    let obsolete = self
                        .file_thumbnails
                        .keys()
                        .find(|path| !self.field_screen.thumbnail_paths().contains(*path))
                        .cloned();
                    if let Some(path) = obsolete {
                        self.file_thumbnails.remove(&path);
                    }
                }
                if self.field_settings.previews != PreviewMode::Off
                    && !self.file_thumbnails.contains_key(&path)
                    && self.file_thumbnails.len() + self.home_thumbnail_pending.len() < 64
                    && self.field_screen.thumbnail_paths().contains(&path)
                    && self.home_thumbnail_pending.insert(path.clone())
                {
                    let tx = self.data_tx.clone();
                    let wake = self.wake_proxy.clone();
                    self.runtime.spawn(async move {
                        let bitmap = super::desktop_dock::preview(&path, 256)
                            .await
                            .ok()
                            .and_then(|bytes| pixelkit_raster::png::decode(&bytes).ok());
                        let _ = tx.send(DataUpdate::HomeThumbnail { path, bitmap });
                        let _ = wake.send_event(Wake::Data);
                    });
                }
            }
            FieldAction::CancelNativeDrop => {
                self.native_drop_pending = false;
                self.native_drop_target = None;
                self.native_drop_receiver = None;
                self.native_drop_item = None;
                self.native_drag_origin = None;
                self.field_screen.set_native_drop_hover(None, false);
                if let Some(bridge) = self.bridge.as_mut()
                    && let Err(error) = bridge.complete_native_drop(false)
                {
                    tracing::warn!(%error, "cancelling native project drop");
                }
            }
            FieldAction::DragFile(path) => {
                if let Some(payload) = Clipboard::copy(vec![path.clone()])
                    .map(|clipboard| crate::data_device::ClipboardPayload::of(&clipboard))
                    && let Some(home) = self.home.as_ref()
                    && let Some(bridge) = self.bridge.as_mut()
                {
                    match bridge.start_file_drag(&home.window, payload) {
                        Ok(true) => {
                            self.native_drag_origin = Some(path);
                        }
                        Ok(false) => self.field_screen.report(
                            "The file drag has no active pointer press".to_owned(),
                            self.started.elapsed().as_millis() as u64,
                        ),
                        Err(error) => self.field_screen.report(
                            format!("Could not drag file: {error}"),
                            self.started.elapsed().as_millis() as u64,
                        ),
                    }
                }
            }
            FieldAction::DockOpen { path, app } => {
                let tx = self.data_tx.clone();
                let wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    if let Err(error) = super::desktop_dock::open(&path, "", app.as_deref()).await {
                        let _ =
                            tx.send(DataUpdate::Notice(format!("Could not open file: {error}")));
                    }
                    let _ = wake.send_event(Wake::Data);
                });
            }
            FieldAction::DockReorder { path, index } => {
                self.reorder_desktop_entry(&path, index);
            }
            FieldAction::DockOpenWith(path) => {
                let tx = self.data_tx.clone();
                let wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    let handlers = super::desktop_dock::open_with_handlers(&path, "")
                        .await
                        .map_err(|error| error.to_string());
                    let _ = tx.send(DataUpdate::DockHandlers { path, handlers });
                    let _ = wake.send_event(Wake::Data);
                });
            }
            FieldAction::TransferFiles {
                paths,
                destination,
                move_files,
            } => {
                if self.native_drop_pending {
                    let choice = self
                        .bridge
                        .as_mut()
                        .ok_or_else(|| "Native drop transport disappeared".to_owned())
                        .and_then(|bridge| {
                            bridge
                                .choose_native_drop_action(move_files)
                                .map_err(|error| error.to_string())
                        });
                    if let Err(error) = choice {
                        self.perform(FieldAction::CancelNativeDrop);
                        self.field_screen.report(
                            format!("Could not select transfer: {error}"),
                            self.started.elapsed().as_millis() as u64,
                        );
                        return;
                    }
                }
                self.spawn_native_transfer(
                    paths,
                    destination,
                    move_files,
                    self.native_drop_pending
                        .then_some(self.native_drop_generation),
                );
            }
            FieldAction::LaunchProjectApp { path, app_id } => {
                let tx = self.data_tx.clone();
                let wake = self.wake_proxy.clone();
                self.runtime.spawn(async move {
                    if let Err(error) = crate::garden::launch_app_in_place(&path, &app_id).await {
                        let _ = tx.send(DataUpdate::Notice(error));
                    }
                    let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
                    let _ = wake.send_event(Wake::Data);
                });
            }
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
            FieldAction::Apply(op) => match templar_field::apply(&self.field, &op) {
                Ok((next, inverse)) => {
                    if let FieldOp::CreateProject { project, .. } = &op
                        && let templar_field::TileKind::Project { project_id } = &project.kind
                    {
                        self.spawn_project_create(project_id.clone(), project.title.clone());
                    }
                    self.field = next;
                    self.sync_desktop_order();
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
                    self.field = templar_field::FieldDocument::new(OUTPUT, cols, rows);
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
                    match templar_field::apply(&self.field, &inverse) {
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

    /// Open the compositor's window overview, filtered (manager v8).
    ///
    /// Not through [`Self::bridge_call`]: the bridge remembers which windows a `Windows` filter
    /// named, because `taskfield_state` says only *that* the filter is a list and this is the one
    /// side that knows what a place is.
    fn open_taskfield(&mut self, filter: &templar_window::taskfield::TaskFilter, label: &str) {
        if let Some(bridge) = &mut self.bridge
            && let Err(error) = bridge.open_taskfield(filter, label)
        {
            tracing::warn!(%error, "open_taskfield request failed");
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

    /// A Place's tools and its gardening (`@templaros.decision.homegarden-activity-grown-places`).
    /// The tools are the project tools with the folder itself as root; the garden verbs go to
    /// `HomeGarden1`, and `garden.why` answers on the field's notice line.
    /// `place.adopt-window`: tell the garden this window belongs to this place.
    ///
    /// The window is turned into its instance here, where the toplevel list is, so nothing below
    /// this has to know what a window is. A window with no instance is one no launch of ours
    /// brokered — the association would have nothing to key on, and saying so is better than a
    /// press that quietly does nothing.
    fn perform_adopt(&mut self, path: &Path, window: templar_window::WindowId) {
        let instance = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default()
            .into_iter()
            .find(|top| top.window_id == window)
            .and_then(|top| top.instance_id);
        let Some(instance) = instance else {
            let now_ms = self.started.elapsed().as_millis() as u64;
            self.field_screen.report(
                "That window was not started by this desktop, so no place can claim it".to_owned(),
                now_ms,
            );
            return;
        };
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        let path = path.to_path_buf();
        self.runtime.spawn(async move {
            match crate::garden::adopt(&path, instance).await {
                // The place's own line ("N windows active") is what changed, so the places are
                // refetched rather than the whole field retended: nothing about the tile moved.
                Ok(()) => {
                    let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
                }
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(error));
                }
            }
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    fn perform_place(&mut self, id: &'static str, path: &Path) {
        if id == "place.open-terminal" {
            if let Some(app_id) = self.providers.get(&ProviderRole::Terminal).cloned() {
                self.perform(FieldAction::LaunchProjectApp {
                    path: path.to_path_buf(),
                    app_id,
                });
            } else {
                self.field_screen.report(
                    "No terminal is selected — choose one in Settings ▸ Providers".to_owned(),
                    self.started.elapsed().as_millis() as u64,
                );
            }
            return;
        }
        if id == "place.show-windows" {
            // The ids, and the name to put over them. The compositor never learns what a place
            // is: this is the one route in the system that turns "this folder" into "these
            // windows", and it does it here, where both are known.
            let toplevels = self
                .bridge
                .as_ref()
                .map(ShellBridge::toplevels)
                .unwrap_or_default();
            let Some(place) = self.garden.get(path) else {
                return;
            };
            let windows = place.windows(&toplevels);
            if windows.is_empty() {
                return;
            }
            let label = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            self.open_taskfield(
                &templar_window::taskfield::TaskFilter::Windows(windows),
                &label,
            );
            return;
        }
        if let Some(project_id) = id.strip_prefix("place.") {
            // A folder is a folder: this row is the same press as a Folder tile and takes the
            // same route, rather than resolving the `FileManager` role a second time with its own
            // copy of the argument shape. One route means one thing to get right.
            if project_id == "open-files" {
                self.open_in_file_manager(path);
                return;
            }
            let project_id: &'static str = match project_id {
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
                    // Geometry remains frozen until the next reveal; only contents update now.
                    let _ = tx.send(DataUpdate::Garden(crate::garden::fetch_places().await));
                }
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(error));
                }
            }
            let _ = proxy_wake.send_event(Wake::Data);
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
            let _ = proxy_wake.send_event(Wake::Data);
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
                let _ = proxy_wake.send_event(Wake::Data);
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

    fn invalidate_treemap(&mut self) {
        self.treemap = None;
        self.treemap_epoch = self.treemap_epoch.wrapping_add(1);
    }

    /// Load once off the event loop; only absent/incompatible Index1 permits a bounded scan.
    fn spawn_treemap(&mut self, path: PathBuf, width: u32, height: u32) {
        if let Some((pending_path, pending_width, pending_height, pending_epoch)) =
            &self.treemap_pending
        {
            if *pending_epoch == self.treemap_epoch
                && (pending_path != &path || *pending_width != width || *pending_height != height)
            {
                // Coalesce replacements behind the outstanding load, but retire its paint now.
                self.invalidate_treemap();
            }
            return;
        }
        let detail = if self
            .treemap
            .as_ref()
            .is_some_and(|previous| path != previous.path && path.starts_with(&previous.path))
        {
            templar_file_index::snapshot::SnapshotDetail::Materialized
        } else {
            templar_file_index::snapshot::SnapshotDetail::Summary
        };
        let epoch = self.treemap_epoch;
        self.treemap_pending = Some((path.clone(), width, height, epoch));
        let tx = self.data_tx.clone();
        let proxy = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let preview = load_treemap_preview_view(path, width, height, detail).await;
            let _ = tx.send(DataUpdate::Treemap { epoch, preview });
            let _ = proxy.send_event(Wake::Data);
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
                .set(crate::settings::RECENT_APPS_KEY, &json, "templar-shell")
                .await
            {
                tracing::debug!(%error, "persisting recent applications");
            }
        });
    }

    /// Perform one declared action (`templar_types::action::MANIFEST`).
    ///
    /// This is the single place an action id becomes a thing that happens, so every surface —
    /// a widget region, the information row, a future menu — reaches a capability the same way
    /// and none of them needs its own idea of what an action means. An id with no route yet is
    /// logged rather than silently dropped: a region wired to nothing should be noisy.
    fn perform_declared(&mut self, id: &'static str) {
        match id {
            "network.open-control" => self.spawn_network_control(),
            "clock.open-calendar" => self.open_calendar(),
            "clock.open-settings" => self.open_datetime_settings(),
            "launcher.search" | "launcher.show-all" => {
                self.request_launcher(LauncherMode::Apps, None, String::new())
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
                if let Some(preset) = templar_window::ArrangementPreset::from_name(name) {
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
    fn perform_on_window(&self, id: &'static str, window: templar_window::WindowId) {
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
            // group; the compositor owns membership, so Home catches up on the next refresh.
            // The bridge checks the negotiated `templar_shell_v2` feature version and
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
                let _ = proxy_wake.send_event(Wake::Data);
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
            Some(window) => self.perform(FieldAction::Activate(window.window_id)),
            None => self.spawn_launch(app_id),
        }
    }

    /// Launch the selected network provider variant, not a different copy in its logical group.
    /// `--popup` is the provider's supported menu entry point, including session-bus forwarding.
    fn spawn_network_control(&self) {
        let app_id = self.providers.get(&ProviderRole::NetworkStatusUi).cloned();
        let tx = self.data_tx.clone();
        let wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result: anyhow::Result<()> = async {
                let app_id = app_id.ok_or_else(|| anyhow::anyhow!(
                    "No network-status-ui provider is selected — install templaros-nm-tray and choose it in Settings ▸ Providers"
                ))?;
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = LauncherProxy::new(&connection).await?;
                proxy.launch(&app_id.to_string(), vec!["--popup".to_owned()], "").await?;
                Ok(())
            }.await;
            if let Err(error) = result {
                let _ = tx.send(DataUpdate::Notice(format!("Network menu unavailable — {error:#}")));
                let _ = wake.send_event(Wake::Data);
            }
        });
    }

    fn open_datetime_settings(&mut self) {
        self.cluster_screen.reset();
        self.dismiss_rail(true);
        if let Some(cluster) = &self.cluster {
            cluster.request_redraw();
        }
        if let Ok(app_id) = SETTINGS_APP_ID.parse::<AppId>() {
            self.spawn_launch_with_args(app_id, vec!["--page".to_owned(), "date-time".to_owned()]);
        }
    }

    /// Open the native clock popup; the declared action and the chip share this route.
    fn open_calendar(&mut self) {
        let Some(local) = local_datetime(std::time::SystemTime::now()) else {
            tracing::warn!("the OS local date is unavailable");
            return;
        };
        if self
            .rail
            .as_ref()
            .is_some_and(|rail| rail.host != RailHost::System)
        {
            self.dismiss_rail(true);
        }
        self.cluster_screen.show_calendar(local.date());
        if let Some(cluster) = &self.cluster {
            cluster.request_redraw();
        }
    }

    fn sync_calendar_focus(&mut self) {
        let open = self.cluster_screen.calendar_open();
        if open == self.calendar_focus_claimed
            || (open && self.cluster_screen.calendar_frame().is_none())
        {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let window = if open {
            self.cluster.as_ref().map(|cluster| cluster.window.as_ref())
        } else {
            None
        };
        if open && window.is_none() {
            return;
        }
        match bridge.focus_surface(window) {
            Ok(()) => self.calendar_focus_claimed = open,
            Err(error) => tracing::warn!(%error, "borrowing keyboard focus for the calendar"),
        }
    }

    /// Create, resize or drop the single Notification-layer toast host.
    ///
    /// One window holds the whole visible stack, bottom-right of the sticky output, sized
    /// to its measured cards. Lock hides everything and drops the window; ordinary
    /// fullscreen defers ordinary cards (critical ones stay); an empty stack drops the
    /// window so no surface and no deadline linger. The host never requests keyboard
    /// focus and never appears in Alt-Tab, work cards or MRU: `Layer::Notification`
    /// maps to a non-application role compositor-side, and this side claims nothing.
    fn sync_toast_host(
        &mut self,
        event_loop: &ActiveEventLoop,
        toplevels: &[templar_window::ShellToplevel],
    ) {
        let now = Instant::now();
        let now_ms = notification_popup::now_ms();
        if self.bridge.as_ref().is_some_and(ShellBridge::lock_engaged) {
            // Lock hides message content and stops toast painting. History is untouched;
            // the hidden ids are remembered, so unlock replays nothing.
            self.toast_stack.hide_all();
            self.toast_host = None;
            self.toast_wanted = (0, 0);
            self.toast_output = None;
            return;
        }
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let defers = notification_popup::fullscreen_defers(toplevels, home_revealed);
        let visible_before_deferral = self.toast_stack.visible_len();
        if defers {
            self.toast_stack.hide_ordinary_for_fullscreen();
        }
        let deferred = self.toast_stack.visible_len() != visible_before_deferral;
        let retired = self.toast_stack.retire_expired(now, now_ms);
        if self.toast_stack.is_empty() {
            self.toast_host = None;
            self.toast_wanted = (0, 0);
            self.toast_output = None;
            return;
        }
        // Sticky output: toasts stay where they first appeared until they leave.
        // Single-output fallback: this shell knows its Home window's monitor, not the
        // compositor's topology, so the pure choose/sticky/reclamp helpers wait on a
        // topology feed. Until then the monitor name sticks.
        if self.toast_output.is_none() {
            let name = self
                .home
                .as_ref()
                .and_then(|home| home.window.current_monitor())
                .and_then(|monitor| monitor.name())
                .unwrap_or_else(|| "home".to_owned());
            self.toast_output = Some(name);
        }
        let output = self
            .toast_output
            .clone()
            .unwrap_or_else(|| "home".to_owned());
        let visible_before = self.toast_stack.visible_len();
        self.toast_stack.promote(now, now_ms, &output, defers);
        if (deferred || retired || self.toast_stack.visible_len() != visible_before)
            && let Some(hosted) = &self.toast_host
        {
            hosted.request_redraw();
        }
        if self.toast_stack.visible_len() == 0 {
            // Everything deferred: no window, but the sticky output and the queue stay.
            self.toast_host = None;
            self.toast_wanted = (0, 0);
            return;
        }
        let app_ids: Vec<String> = self
            .toast_stack
            .visible()
            .iter()
            .filter_map(|toast| toast.presentation.app_id.clone())
            .collect();
        for id in app_ids {
            self.ensure_icon(&id);
        }
        if self.toast_host.is_some() {
            return;
        }
        let scale = self
            .home
            .as_ref()
            .map_or(pixelkit_shell::Scale::ONE, |home| home.scale);
        let height = self.measure_toast_stack_height(scale).max(1);
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Notifications")
                .with_decorations(false)
                .with_active(false)
                .with_inner_size(LogicalSize::new(
                    f64::from(notification_popup::TOAST_CARD_WIDTH),
                    f64::from(height),
                )),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::warn!(%error, "creating the notification toast window");
                return;
            }
        };
        if let Some(bridge) = &self.bridge
            && let Err(error) = bridge.set_layer(&window, templar_window::Layer::Notification)
        {
            tracing::warn!(%error, "declaring the toast onto Layer::Notification");
        }
        // Deliberately no `focus_surface`: toasts never steal keyboard focus.
        match HostedWindow::new(window) {
            Ok(mut hosted) => {
                hosted.attach_accessibility(event_loop, self.wake_proxy.clone());
                hosted.request_redraw();
                self.toast_host = Some(hosted);
                self.toast_wanted = (notification_popup::TOAST_CARD_WIDTH, height);
            }
            Err(error) => tracing::warn!(%error, "presenting the notification toast"),
        }
    }

    /// Logical height of the visible stack at `scale`, measured with the same PixelKit
    /// layout the draw pass paints. Window sizing only; the draw pass re-measures exactly.
    fn measure_toast_stack_height(&mut self, scale: pixelkit_shell::Scale) -> i32 {
        let mut theme = design::theme();
        theme.body.size *= scale.0;
        theme.heading.size *= scale.0;
        let width = scale.px(notification_popup::TOAST_CARD_WIDTH);
        let mut heights = Vec::new();
        for toast in self.toast_stack.visible() {
            let row = &toast.presentation;
            let actions: Vec<ToastAction<'_>> = row
                .actions
                .iter()
                .map(|(key, label)| ToastAction { key, label })
                .collect();
            let icon = row.app_id.as_ref().and_then(|id| self.icons.get(id));
            let card = ToastCard {
                app: &row.app_name,
                summary: &row.summary,
                body: &row.body,
                icon,
                actions: &actions,
                dismiss_label: "Dismiss",
            };
            heights.push(
                pixelkit_ui::toast::toast_layout(
                    &mut self.text,
                    &theme,
                    scale,
                    (0, 0),
                    width,
                    &card,
                )
                .height,
            );
        }
        let mut physical: i32 = heights.iter().sum();
        physical +=
            scale.px(notification_popup::TOAST_GAP) * heights.len().saturating_sub(1) as i32;
        if self.toast_stack.overflow() > 0 {
            physical += scale.px(notification_popup::TOAST_GAP)
                + scale.px(notification_popup::OVERFLOW_STRIP_HEIGHT);
        }
        ((physical as f32 / scale.0).ceil() as i32).max(1)
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
        self.toast_stack.hide_all();
        self.toast_host = None;
        self.toast_output = None;
        self.toast_wanted = (0, 0);
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
        if bridge.take_lock_confirmation() {
            self.runtime.spawn(crate::session_lock::notify_locked());
        }
        // Unlock-and-destroy is illegal until the compositor acknowledged the coverage barrier.
        if !bridge.lock_locked() {
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

    /// Ask `templar-unlock` whether this password is the person's.
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
            let _ = proxy_wake.send_event(Wake::Data);
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
        toplevels: &[templar_window::ShellToplevel],
    ) {
        if self.bar.is_some() {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        // A rail host starts as nothing: 1×1, transparent, no input, until a rail opens on it.
        let wanted = if self.hosts_gesture {
            (1, 1)
        } else {
            crate::surface::bar_screen::wanted_size(
                crate::surface::bar_screen::Presence::Open,
                None,
                false,
                crate::taskbar::entries(toplevels).len(),
                None,
                self.output_width(),
            )
        };
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
        if let Err(error) = bridge.set_layer(&window, templar_window::Layer::ShellOverlay) {
            tracing::warn!(%error, "declaring the bar onto Layer::ShellOverlay");
        }
        // A zone of 0, and the claim is still made: it is what anchors the surface to the bottom
        // edge (its bottom on the output's bottom, its left end at x=0, its own committed
        // height), and it is the difference between a bar that is at the bottom of the screen and
        // a floating window that happens to be near it. Reserving *nothing* is the point — the
        // output's usable area stays whole, so a maximized window fills the screen and wears its
        // own tags and controls, and the bar paints over Home rather than pushing it up.
        if let Err(error) =
            bridge.set_exclusive_zone(&window, templar_window::geometry::Edge::Bottom, 0)
        {
            tracing::warn!(%error, "anchoring the bar to the bottom edge");
        }
        // Centred along that edge, not at its start. Two reasons, and the second is the one that
        // matters: an island the width of its contents sitting hard against the bottom-left corner
        // is a dock in the corner rather than a dock, and — the corner is Home's now. The
        // compositor suppresses the hot corner while a shell overlay is under the pointer, and the
        // island's collapsed hint tab is 72×6 at exactly that corner, so an island left at the
        // start of its edge would sit on the corner and stop it firing at all.
        //
        // A rail is the exception: it is drawn outward from the output's centre across the whole
        // width, so its surface starts at the edge's start and its x is the output's x.
        let alignment = if self.hosts_gesture {
            templar_window::geometry::Alignment::Start
        } else {
            templar_window::geometry::Alignment::Centre
        };
        self.bar_centred = match bridge.set_edge_alignment(
            &window,
            templar_window::geometry::Edge::Bottom,
            alignment,
        ) {
            Ok(aligned) => aligned && !self.hosts_gesture,
            Err(error) => {
                tracing::warn!(%error, "aligning the bar on the bottom edge");
                false
            }
        };
        match HostedWindow::new(window) {
            Ok(hosted) => {
                hosted.request_redraw();
                self.bar = Some(hosted);
                self.bar_wanted = wanted;
                self.bar_idle = None;
            }
            Err(error) => tracing::warn!(%error, "presenting the bar"),
        }
    }

    /// Paint the bar and act on what was clicked. The window list is the bridge's; the entries
    /// are computed fresh each frame (`crate::taskbar::entries`), which is a few dozen windows at
    /// most.
    #[allow(clippy::too_many_lines)]
    fn redraw_bar(&mut self) {
        let gesture = self.hosts_gesture;
        let rail = self
            .rail
            .as_ref()
            .filter(|rail| rail.host == RailHost::Apps)
            .map(|rail| rail.reveal.view());
        if gesture && rail.is_none() {
            if let Some(bar) = &mut self.bar {
                present_nothing(
                    self.bridge.as_mut(),
                    bar,
                    &mut self.bar_mask,
                    &mut self.bar_wanted,
                    &mut self.bar_idle,
                    "the bar",
                );
            }
            return;
        }
        self.bar_idle = None;
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        let (entries, rows) = self.bar_entries(&toplevels);
        // Each window's own application too: a group's fan cards are different applications.
        let mut keys: Vec<String> = entries
            .iter()
            .flat_map(|entry| {
                entry.icon_id.iter().chain(
                    entry
                        .windows()
                        .iter()
                        .filter_map(|window| window.app_id.as_ref()),
                )
            })
            .cloned()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        for key in keys {
            self.ensure_icon(&key);
        }
        let activated = toplevels.iter().find(|t| t.activated).map(|t| t.window_id);
        let now_ms = self.started.elapsed().as_millis() as u64;
        let home_revealed = self.bridge.as_ref().is_some_and(ShellBridge::home_revealed);
        let output_width = self.output_width();
        // Neither the cover nor the peek means anything to a rail: it is there because the
        // pointer asked for it, over whatever is on the screen.
        let covered = !gesture && bar_covered(home_revealed, &toplevels);
        let peeked = !gesture && self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked);
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
            activated,
            covered,
            peeked,
            home_revealed,
            now_ms,
            rail,
            rows,
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
        //
        // A rail always floats, even with nothing solid on it yet: an opaque rail would be a grey
        // band across the whole width of the screen.
        if self.bar_mask.width != bar.buffer.width || self.bar_mask.height != bar.buffer.height {
            self.bar_mask = WindowBuffer::new(bar.buffer.width, bar.buffer.height);
        }
        let shapes = self.bar_screen.opaque_shapes();
        let floated = self.bridge.is_some() || gesture || !shapes.is_empty();
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
                    Ok(outcome) => {
                        // Deferred still counts as presented: the frame is owed, not failed,
                        // and `about_to_wait` redraws it once a buffer comes back.
                        bar.pending = outcome == PresentOutcome::Deferred;
                        true
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the bar could not float; presenting it opaque");
                        false
                    }
                }
            });
        if !presented {
            // The colour buffer goes up opaque, but the region still has to be said: it is part
            // of presenting, and a frame that leaves the last one in force is a surface taking
            // clicks where it no longer draws.
            declare_input_shapes(self.bridge.as_mut(), &bar.window, &regions, "the bar");
            bar.present();
        }
        // The surface is the output's width, and its height is whatever the presence the draw
        // just settled on asks for: the hint under a window, the bar over Home, plus whatever
        // band an open hover row or the account popover needs. The exclusive zone is 0 through
        // all of it, so no size this surface takes reflows anything.
        let wanted = if gesture {
            crate::surface::bar_screen::rail_wanted_size(
                self.bar_screen.fan_cards(&entries),
                self.bar_screen.tooltip_open(now_ms),
                rows,
                output_width,
            )
        } else {
            crate::surface::bar_screen::wanted_size(
                self.bar_screen.presence(covered),
                self.bar_screen.fan_cards(&entries),
                self.bar_screen.tooltip_open(now_ms),
                entries.len(),
                rows,
                output_width,
            )
        };
        if wanted != self.bar_wanted {
            self.bar_wanted = wanted;
            if let Some(bar) = &self.bar {
                let _ = bar
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
        }
        if gesture {
            self.feed_rail_shapes(RailHost::Apps);
            self.perform_rail(RailHost::Apps, actions);
        } else {
            self.perform_bar(actions);
        }
    }

    /// Create, resize or destroy the cluster's surface.
    ///
    /// Destroyed outright while a window fills the screen — the user's rule, and the honest way to
    /// say it: not hidden, not collapsed to a hint, not there. Its zone is 0 like the island's, so
    /// neither creating nor destroying it moves a window.
    ///
    /// It comes back on exactly the two things that bring the switcher back: revealing Home, and
    /// the pointer resting on the bottom edge (`bar_peeked`). Without the second, the clock, the
    /// network, the battery and the account would be unreachable for as long as anything was
    /// maximized — which is most of the time — and "not there" would have meant "gone".
    fn ensure_cluster(
        &mut self,
        event_loop: &ActiveEventLoop,
        toplevels: &[templar_window::ShellToplevel],
    ) {
        // A rail host is persistent: it is 1×1 and transparent while its rail is closed, and a
        // screen-filling window is exactly where a rail is wanted most, so nothing here applies.
        let wanted_gone = !self.hosts_gesture
            && bar_covered(
                self.bridge.as_ref().is_some_and(ShellBridge::home_revealed),
                toplevels,
            )
            && !self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked);
        if wanted_gone {
            if let (Some(bridge), Some(hosted)) = (&mut self.bridge, self.cluster.take()) {
                bridge.forget_surface(&hosted.window);
            }
            self.cluster_wanted = (0, 0);
            // And with it whatever was open on it. The screen's state does not survive the screen:
            // a popover left open when something went fullscreen would otherwise be waiting when
            // the window closed, over a desktop nobody had asked it about.
            self.cluster_screen = crate::surface::cluster_screen::ClusterScreen::default();
            self.sync_calendar_focus();
            return;
        }
        if self.cluster.is_some() {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let wanted = if self.hosts_gesture {
            (1, 1)
        } else {
            crate::surface::cluster_screen::wanted_size(false, false, self.output_width())
        };
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
        if let Err(error) = bridge.set_layer(&window, templar_window::Layer::ShellOverlay) {
            tracing::warn!(%error, "declaring the cluster onto Layer::ShellOverlay");
        }
        if let Err(error) =
            bridge.set_exclusive_zone(&window, templar_window::geometry::Edge::Bottom, 0)
        {
            tracing::warn!(%error, "anchoring the cluster to the bottom edge");
        }
        // The *end* of the edge, where the island asks for the middle of it — or, as a rail, the
        // start: the rail spans the output and draws outward from its centre.
        let alignment = if self.hosts_gesture {
            templar_window::geometry::Alignment::Start
        } else {
            templar_window::geometry::Alignment::End
        };
        if let Err(error) =
            bridge.set_edge_alignment(&window, templar_window::geometry::Edge::Bottom, alignment)
        {
            tracing::warn!(%error, "aligning the cluster on the bottom edge");
        }
        match HostedWindow::new(window) {
            Ok(hosted) => {
                if self.hosts_gesture {
                    hosted.request_redraw();
                }
                self.cluster = Some(hosted);
                self.cluster_wanted = wanted;
                self.cluster_idle = None;
            }
            Err(error) => tracing::warn!(%error, "hosting the cluster window"),
        }
    }

    /// Paint the cluster and act on what it returns.
    #[allow(clippy::too_many_lines)]
    fn redraw_cluster(&mut self) {
        let gesture = self.hosts_gesture;
        let rail = self
            .rail
            .as_ref()
            .filter(|rail| rail.host == RailHost::System)
            .map(|rail| rail.reveal.view());
        if gesture && rail.is_none() && !self.cluster_screen.calendar_open() {
            self.sync_calendar_focus();
            if let Some(cluster) = &mut self.cluster {
                present_nothing(
                    self.bridge.as_mut(),
                    cluster,
                    &mut self.cluster_mask,
                    &mut self.cluster_wanted,
                    &mut self.cluster_idle,
                    "the cluster",
                );
            }
            return;
        }
        self.cluster_idle = None;
        let network_control_available = self.providers.contains_key(&ProviderRole::NetworkStatusUi);
        let account_label = std::env::var("USER")
            .ok()
            .filter(|user| !user.is_empty())
            .unwrap_or_else(account_uid_label);
        let local_now = local_datetime(std::time::SystemTime::now());
        let clock_label = clock_label_for(local_now);
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
            local_now,
            account_label: &account_label,
            machine: &self.machine,
            now_ms,
            rail,
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
        // A rail always floats — see `redraw_bar`.
        let floated = gesture || !shapes.is_empty();
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
                Ok(outcome) => {
                    cluster.pending = outcome == PresentOutcome::Deferred;
                    true
                }
                Err(error) => {
                    tracing::debug!(%error, "the cluster could not float; presenting it opaque");
                    false
                }
            }
        });
        if !presented {
            declare_input_shapes(
                self.bridge.as_mut(),
                &cluster.window,
                &regions,
                "the cluster",
            );
            cluster.present();
        }
        let wanted = if gesture {
            crate::surface::cluster_screen::rail_wanted_size(
                self.cluster_screen.account_open(),
                self.cluster_screen.calendar_open(),
                self.output_width(),
            )
        } else {
            crate::surface::cluster_screen::wanted_size(
                self.cluster_screen.account_open(),
                self.cluster_screen.calendar_open(),
                self.output_width(),
            )
        };
        if wanted != self.cluster_wanted {
            self.cluster_wanted = wanted;
            if let Some(cluster) = &self.cluster {
                let _ = cluster
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
        }
        if gesture {
            self.feed_rail_shapes(RailHost::System);
            self.perform_rail(RailHost::System, actions);
        } else {
            self.perform_bar(actions);
        }
        self.sync_calendar_focus();
    }

    /// Create the Taskfield surface when the compositor says the overview opened, and destroy it
    /// when it says the overview closed.
    ///
    /// Nothing about the mode is decided here. `taskfield_state` is the whole of what the shell
    /// knows, and this is the whole of what the shell does about it.
    fn sync_taskfield(&mut self, event_loop: &ActiveEventLoop) {
        let active = self
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.taskfield().active || bridge.switcher().active);
        if !active {
            if let (Some(bridge), Some(hosted)) = (&mut self.bridge, self.taskfield.take()) {
                bridge.forget_surface(&hosted.window);
            }
            self.taskfield_sections = None;
            return;
        }
        if self.taskfield.is_some() {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let (width, height) = self.output_size();
        let window = match event_loop.create_window(
            Window::default_attributes()
                .with_title("Taskfield")
                .with_decorations(false)
                .with_transparent(true)
                .with_inner_size(LogicalSize::new(f64::from(width), f64::from(height))),
        ) {
            Ok(window) => std::sync::Arc::new(window),
            Err(error) => {
                tracing::warn!(%error, "creating the Taskfield window");
                return;
            }
        };
        if let Err(error) = bridge.set_layer(&window, templar_window::Layer::Presentation) {
            tracing::warn!(%error, "declaring Taskfield onto Layer::Presentation");
            return;
        }
        match HostedWindow::new(window) {
            Ok(hosted) => {
                hosted.request_redraw();
                self.taskfield = Some(hosted);
            }
            Err(error) => tracing::warn!(%error, "presenting Taskfield"),
        }
    }

    /// Paint the overview's labels. Answers nothing: the mode's input is the compositor's, and
    /// this surface commits an empty input region so every click reaches the window under it.
    fn redraw_taskfield(&mut self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        let status = bridge.taskfield().clone();
        let switcher = bridge.switcher().clone();
        let toplevels = self.prepare_taskfield_toplevels(&switcher);
        let (width, height) = self.output_size();
        // Frozen at first paint: the compositor laid the cards out once on entry, and a live
        // recompute here is how labels drifted when a window closed or resized. The remaining
        // disagreement — this surface invents a zero-origin scale-1 output — is a
        // `taskfield_card` event, not a second layout.
        if self.taskfield_sections.is_none() {
            self.taskfield_sections = Some(templar_window::taskfield::layout(
                &templar_window::task::units(&toplevels),
                &toplevels,
                &crate::surface::taskfield_screen::outputs(width, height),
                &status.filter,
            ));
        }
        let sections = self.taskfield_sections.clone().unwrap_or_default();
        let Some(taskfield) = &mut self.taskfield else {
            return;
        };
        if !taskfield.prepare() {
            return;
        }
        let area = Rect::new(
            0,
            0,
            taskfield.buffer.width as i32,
            taskfield.buffer.height as i32,
        );
        let data = crate::surface::taskfield_screen::TaskfieldData {
            status: &status,
            sections: &sections,
            toplevels: &toplevels,
            icons: &self.icons,
        };
        {
            let mut ui = Ui::new(
                Painter::new(&mut taskfield.buffer),
                &mut self.text,
                &mut taskfield.input,
                self.theme,
                taskfield.scale,
                &mut self.kernel,
            );
            if switcher.active {
                self.taskfield_screen.draw_switcher(
                    &mut ui,
                    area,
                    &switcher,
                    &toplevels,
                    &self.icons,
                );
            } else {
                self.taskfield_screen.draw(&mut ui, area, &data);
            }
        }
        taskfield.input.end_frame();
        if self.taskfield_mask.width != taskfield.buffer.width
            || self.taskfield_mask.height != taskfield.buffer.height
        {
            self.taskfield_mask =
                WindowBuffer::new(taskfield.buffer.width, taskfield.buffer.height);
        }
        let shapes = self.taskfield_screen.opaque_shapes();
        if shapes.is_empty() {
            // Nothing to say about this frame — an overview of no windows, or a mode that closed
            // between the wake and the paint. An opaque present here would be a grey screen, so
            // the frame is skipped; the region is not, or the labels of the frame before it would
            // go on taking the pointer over cards that have moved.
            declare_input_shapes(self.bridge.as_mut(), &taskfield.window, &[], "Taskfield");
            return;
        }
        {
            let mut painter = Painter::new(&mut self.taskfield_mask);
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
        // No input region at all. A label band that took the pointer would swallow the click
        // meant for the window under it, and the mode's input belongs to the compositor.
        if let Some(bridge) = self.bridge.as_mut() {
            match bridge.present_masked(
                &taskfield.window,
                &taskfield.buffer.pixels,
                &self.taskfield_mask.pixels,
                &[],
                taskfield.buffer.width,
                taskfield.buffer.height,
            ) {
                Ok(outcome) => taskfield.pending = outcome == PresentOutcome::Deferred,
                Err(error) => {
                    tracing::debug!(%error, "Taskfield could not float; drawing nothing");
                }
            }
        }
    }

    fn prepare_taskfield_toplevels(
        &mut self,
        switcher: &crate::wayland_bridge::SwitcherStatus,
    ) -> Vec<templar_window::ShellToplevel> {
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        if switcher.active {
            self.drain_previews(&toplevels);
        }
        for key in toplevels.iter().filter_map(|top| top.icon_app_id()) {
            self.ensure_icon(key);
        }
        toplevels
    }

    /// What the island and the cluster ask for. One place for both: they answer with the same
    /// `BarAction`, and a second match over it is a second chance to forget a variant.
    fn perform_bar(&mut self, actions: Vec<crate::surface::bar_screen::BarAction>) {
        use crate::surface::bar_screen::BarAction;
        for action in actions {
            match action {
                BarAction::OpenLauncher => {
                    self.request_launcher(LauncherMode::Apps, None, String::new())
                }
                BarAction::OpenFavorite(id) => self.activate_launcher_target(
                    LauncherTarget::Favorite(id),
                    crate::surface::omnibox_screen::Intent::Open,
                ),
                BarAction::Activate(window) => self.perform(FieldAction::Activate(window)),
                BarAction::Window { id, window } => {
                    self.perform(FieldAction::RunWindowAction { id, window });
                }
                BarAction::Field(action) => self.perform(action),
                // A chip with more windows than its fan can hold hands the question over.
                BarAction::Taskfield(key) => self.perform(FieldAction::Taskfield(key)),
                BarAction::Overview => {
                    self.open_taskfield(&templar_window::taskfield::TaskFilter::All, "");
                }
            }
        }
    }

    /// Act on what a rail host returned, then get out of the way: a rail closes on anything done
    /// from it except the level controls (`rail::action_keeps_rail_open`), which a person
    /// adjusts in several goes while watching.
    fn perform_rail(
        &mut self,
        host: RailHost,
        actions: Vec<crate::surface::bar_screen::BarAction>,
    ) {
        let dismiss = !actions.is_empty()
            && self.rail.as_ref().is_some_and(|rail| rail.host == host)
            && !actions.iter().all(action_keeps_rail_open);
        self.perform_bar(actions);
        if dismiss {
            self.dismiss_rail(true);
        }
    }

    /// Tell the compositor the gate's shape and whether it is on, if that changed since it was
    /// last told. Called on connect and on every settings reading; mode "bar" is `enabled` 0.
    fn relay_gesture_gate(&mut self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        if !bridge.gestures_available()
            || self.rail_gate_relayed.as_ref() == Some(&self.rail_settings)
        {
            return;
        }
        let gate = &self.rail_settings;
        match bridge.set_gesture_gate(
            gate.gesture,
            gate.gate_width,
            gate.gate_height,
            gate.learning,
            &gate.exceptions,
        ) {
            Ok(()) => self.rail_gate_relayed = Some(gate.clone()),
            Err(error) => tracing::warn!(%error, "relaying the gesture gate to the compositor"),
        }
    }

    /// Tell the compositor the committed window tab policy, if that changed since it was
    /// last told. Called on connect and on every settings reading.
    fn relay_tab_policy(&mut self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        if self.tab_policy_relayed.as_ref() == Some(&self.tab_settings) {
            return;
        }
        let policy = self.tab_settings;
        match bridge.send_tab_policy(&policy) {
            Ok(()) => self.tab_policy_relayed = Some(policy),
            Err(error) => tracing::warn!(%error, "relaying the window tab policy"),
        }
    }

    /// Whether the bottom of the screen should be gesture rails right now: the person asked for
    /// them, and the compositor is new enough to have a gate. Against an older one the classic
    /// bar stays, rather than two surfaces waiting for a gesture nothing will ever recognise.
    fn gesture_mode(&self) -> bool {
        self.rail_settings.gesture
            && self
                .bridge
                .as_ref()
                .is_some_and(ShellBridge::gestures_available)
    }

    /// The bar's entries in the order they are drawn, and the two-row shape when the lane mode
    /// and the room ask for one. Every consumer of a chip's index — the draw, the hover, the
    /// deadline — has to read this and not `taskbar::entries`, or the index means another chip.
    fn bar_entries(
        &self,
        toplevels: &[templar_window::ShellToplevel],
    ) -> (
        Vec<crate::rail_model::RailEntry>,
        Option<crate::taskbar::Rows>,
    ) {
        let tasks = crate::taskbar::entries_in(toplevels, self.lanes, &self.active_lane_apps);
        let profiles = self.profile_windows(toplevels);
        let mut entries = crate::rail_model::reconcile(
            tasks,
            &self.launcher_preferences,
            &self.apps,
            &self.app_variants,
            &profiles,
        );
        let width = self.output_width();
        if self.lanes != crate::taskbar::LaneMode::Off {
            entries.sort_by_key(|entry| entry.lane != crate::taskbar::Lane::Active);
        }
        let capacity = if self.hosts_gesture {
            crate::surface::bar_screen::rail_row_capacity(width)
        } else {
            crate::surface::bar_screen::island_row_capacity(width)
        };
        let rows = crate::rail_model::rows_of(&entries, self.lanes, capacity);
        (entries, rows)
    }

    /// Rebuild the bar and the cluster when the mode changes. Their edge alignment is fixed when
    /// they are created, and a rail host and a classic one differ in exactly that, so the old
    /// surfaces go and `ensure_bar`/`ensure_cluster` make new ones on this same pass.
    fn sync_rail_mode(&mut self) {
        let gesture = self.gesture_mode();
        if gesture == self.hosts_gesture {
            return;
        }
        self.hosts_gesture = gesture;
        // Leaving gesture mode relays `enabled` 0, and the compositor closes whatever was open.
        self.rail = None;
        self.rail_dismissed = None;
        for hosted in [self.bar.take(), self.cluster.take()].into_iter().flatten() {
            if let Some(bridge) = &mut self.bridge {
                bridge.forget_surface(&hosted.window);
            }
        }
        self.bar_screen.reset();
        self.cluster_screen = crate::surface::cluster_screen::ClusterScreen::default();
        self.sync_calendar_focus();
        self.bar_wanted = (0, 0);
        self.cluster_wanted = (0, 0);
        self.bar_idle = None;
        self.cluster_idle = None;
    }

    /// Follow the compositor's rail: open one when it says so, reveal it as the pointer travels,
    /// close it when it closes, and dismiss it when the pointer has wandered off for longer than
    /// the grace. Only the rail's own host is ever asked to redraw from here.
    fn sync_rail(&mut self, entries: usize, rows: Option<crate::taskbar::Rows>) {
        let now = Instant::now();
        if !self.hosts_gesture {
            return;
        }
        let Some(bridge) = &self.bridge else {
            return;
        };
        let generation = bridge.rail_generation();
        if generation != self.rail_generation {
            self.rail_generation = generation;
            let opened = bridge.rail().map(|(opening, _, side)| (opening, side));
            let pointer = bridge.rail_pointer();
            match opened {
                // Closed, whoever closed it. Idempotent: a rail this shell dismissed itself is
                // already gone, and this is only the compositor agreeing.
                None => {
                    self.rail_dismissed = None;
                    self.dismiss_rail(false);
                }
                Some((opening, side)) => {
                    let known = self.rail.as_ref().map(|rail| rail.opening) == Some(opening);
                    if !known && self.rail_dismissed != Some(opening) {
                        self.dismiss_rail(false);
                        self.open_rail(opening, side, entries, rows);
                    }
                    if let (Some(rail), Some((x, y))) = (&mut self.rail, pointer) {
                        rail.session.pointer(x, y, now);
                        if rail.reveal.advance(x) {
                            let host = rail.host;
                            self.request_rail_redraw(host);
                        }
                    }
                }
            }
        }
        if self
            .rail
            .as_ref()
            .is_some_and(|rail| rail.session.expired(now))
            && !self.cluster_screen.calendar_open()
        {
            self.dismiss_rail(true);
        }
    }

    /// Start drawing a rail the compositor just opened. Laid out once, here: the reveal's length
    /// is fixed for the opening, and only its clip moves after this.
    fn open_rail(
        &mut self,
        opening: u64,
        side: RailSide,
        entries: usize,
        rows: Option<crate::taskbar::Rows>,
    ) {
        let host = rail_host(side, self.rail_settings.swap);
        let (width, height) = self.output_size();
        let (length, initial, wanted) = match host {
            RailHost::Apps => (
                // Two rows are only as long as the longer one.
                crate::surface::bar_screen::rail_length(
                    rows.map_or(entries, crate::taskbar::Rows::widest),
                    width,
                ),
                crate::surface::bar_screen::rail_initial(),
                crate::surface::bar_screen::rail_wanted_size(None, false, rows, width),
            ),
            RailHost::System => (
                crate::surface::cluster_screen::rail_length(),
                crate::surface::cluster_screen::rail_initial(),
                crate::surface::cluster_screen::rail_wanted_size(false, false, width),
            ),
        };
        let gate = (
            i32::try_from(self.rail_settings.gate_width).unwrap_or(i32::MAX),
            i32::try_from(self.rail_settings.gate_height).unwrap_or(i32::MAX),
        );
        self.rail = Some(OpenRail {
            opening,
            host,
            reveal: RailReveal::new(
                width / 2,
                side,
                length,
                if self.field_settings.reduced_motion {
                    length
                } else {
                    initial
                },
            ),
            session: RailSession::new(side, (width, height), gate),
        });
        let (hosted, wanted_now, idle) = match host {
            RailHost::Apps => {
                self.bar_screen.reset();
                (&self.bar, &mut self.bar_wanted, &mut self.bar_idle)
            }
            RailHost::System => {
                self.cluster_screen.reset();
                (
                    &self.cluster,
                    &mut self.cluster_wanted,
                    &mut self.cluster_idle,
                )
            }
        };
        *idle = None;
        if let Some(hosted) = hosted {
            if *wanted_now != wanted {
                *wanted_now = wanted;
                let _ = hosted
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
            hosted.request_redraw();
        }
    }

    /// Put the open rail away: tell the compositor if this shell is the one deciding, collapse
    /// whatever was open on the screen, and let the host's next frame shrink it back to nothing.
    fn dismiss_rail(&mut self, tell_compositor: bool) {
        let Some(rail) = self.rail.take() else {
            return;
        };
        if tell_compositor {
            self.rail_dismissed = Some(rail.opening);
            self.bridge_call("closing the rail", ShellBridge::close_rail);
        }
        match rail.host {
            RailHost::Apps => self.bar_screen.reset(),
            RailHost::System => self.cluster_screen.reset(),
        }
        self.request_rail_redraw(rail.host);
    }

    fn request_rail_redraw(&self, host: RailHost) {
        let hosted = match host {
            RailHost::Apps => &self.bar,
            RailHost::System => &self.cluster,
        };
        if let Some(hosted) = hosted {
            hosted.request_redraw();
        }
    }

    /// Hand the open rail's session the shapes its screen just drew, in output coordinates, so a
    /// fan of cards or the account popover standing above the row holds the rail open.
    ///
    /// The screens record shapes in buffer pixels; the surface is anchored to the output's
    /// bottom-left corner (edge alignment Start), so x is already the output's and y only needs
    /// the surface's own offset from the top.
    fn feed_rail_shapes(&mut self, host: RailHost) {
        let (_, output_height) = self.output_size();
        let (hosted, shapes) = match host {
            RailHost::Apps => (&self.bar, self.bar_screen.opaque_shapes()),
            RailHost::System => (&self.cluster, self.cluster_screen.opaque_shapes()),
        };
        let Some(hosted) = hosted else {
            return;
        };
        let scale = hosted.window.scale_factor().max(f64::EPSILON);
        let height = hosted.window.inner_size().to_logical::<f64>(scale).height;
        let top = f64::from(output_height) - height;
        let logical = |value: i32| f64::from(value) / scale;
        let shapes: Vec<(i32, i32, i32, i32)> = shapes
            .iter()
            .map(|(rect, _)| {
                (
                    logical(rect.x).round() as i32,
                    (top + logical(rect.y)).round() as i32,
                    logical(rect.w).round() as i32,
                    logical(rect.h).round() as i32,
                )
            })
            .collect();
        if let Some(rail) = self.rail.as_mut().filter(|rail| rail.host == host) {
            rail.session.set_shapes(shapes, Instant::now());
        }
    }

    /// A button went down or up on a rail host. A held button holds the rail: a slider dragged
    /// off its track is still being dragged.
    fn note_rail_button(&mut self, host: RailHost, held: bool) {
        if let Some(rail) = self.rail.as_mut().filter(|rail| rail.host == host) {
            rail.session.set_button_held(held, Instant::now());
        }
    }

    /// The cluster's window events: pointer and redraw, exactly as the bar's.
    fn cluster_event(&mut self, event: &WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw_cluster(),
            WindowEvent::CloseRequested => self.cluster = None,
            _ => {
                self.note_rail_pointer_event(RailHost::System, event);
                let Some(cluster) = &mut self.cluster else {
                    return;
                };
                match event {
                    WindowEvent::KeyboardInput { event, .. } if key_pressed(event.state) => {
                        for key in translate_keys(&event.logical_key, event.text.as_deref()) {
                            cluster.input.key(key);
                        }
                        cluster.request_redraw();
                    }
                    WindowEvent::ModifiersChanged(modifiers) => {
                        cluster.input.modifiers_changed(modifiers_of(*modifiers));
                    }
                    WindowEvent::Focused(false) => {
                        self.cluster_screen.reset();
                        self.calendar_focus_claimed = false;
                        cluster.input.blur();
                        cluster.request_redraw();
                    }
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                            cluster.scale = pixelkit_shell::Scale::new(*scale_factor);
                        }
                        // The stored cursor needs no correction: the compositor re-sends the
                        // pointer's motion whenever the surface under it moves or resizes
                        // (`compositor/templar-comp/src/space_sync.rs`).
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
                self.note_rail_pointer_event(RailHost::Apps, event);
                let Some(bar) = &mut self.bar else {
                    return;
                };
                match event {
                    WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                        if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                            bar.scale = pixelkit_shell::Scale::new(*scale_factor);
                        }
                        // The stored cursor needs no correction: the compositor re-sends the
                        // pointer's motion whenever the surface under it moves or resizes
                        // (`compositor/templar-comp/src/space_sync.rs`).
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

    /// The part of a rail host's pointer events the dismissal needs: whether the left button is
    /// down on it. A leave ends any press, as it does for `pixelkit_shell::Input`.
    fn note_rail_pointer_event(&mut self, host: RailHost, event: &WindowEvent) {
        match event {
            WindowEvent::MouseInput {
                state,
                button: winit::event::MouseButton::Left,
                ..
            } => self.note_rail_button(host, *state == ElementState::Pressed),
            WindowEvent::CursorLeft { .. } => self.note_rail_button(host, false),
            _ => {}
        }
    }

    /// Redraw every masked surface whose last frame was deferred, once the bridge says one of
    /// its buffers came back. The release is a bridge event, so `ShellBridge::spawn_waker` is
    /// what brings the loop here for it.
    fn retry_deferred_presents(&self) {
        let Some(bridge) = &self.bridge else {
            return;
        };
        for hosted in [
            &self.home,
            &self.bar,
            &self.cluster,
            &self.taskfield,
            &self.launcher,
        ]
        .into_iter()
        .flatten()
        .filter(|hosted| hosted.pending)
        {
            if bridge.can_present(&hosted.window) {
                hosted.request_redraw();
            }
        }
    }

    /// Paint the visible toast stack: one PixelKit card per row, oldest first, plus the
    /// overflow strip. Outcomes collected here are performed after the draw pass, and
    /// hover/keyboard focus pauses each card's dwell with its remainder intact.
    #[allow(clippy::too_many_lines)]
    fn redraw_toast(&mut self) {
        // Never paint message content while locked.
        if self.bridge.as_ref().is_some_and(ShellBridge::lock_engaged) {
            return;
        }
        let Some(hosted) = self.toast_host.as_mut() else {
            return;
        };
        if !hosted.prepare() || self.toast_stack.visible_len() == 0 {
            return;
        }
        hosted.begin_semantics("Notifications");
        let buffer_width = hosted.buffer.width as i32;
        let scale = hosted.scale;
        let mut ui = Ui::new(
            Painter::new(&mut hosted.buffer),
            &mut self.text,
            &mut hosted.input,
            design::theme(),
            scale,
            &mut self.kernel,
        )
        .with_focus(&mut hosted.focus)
        .with_semantics(&mut hosted.semantics);
        // Every pixel first: this surface is opaque, and the gaps between cards show the
        // host ground (see `field_screen::draw` for why the clear comes first).
        ui.painter.clear(design::shell_fill());
        let mut rects = Vec::new();
        let mut card_actions = Vec::new();
        let mut control_counts = Vec::new();
        let mut y = 0;
        for toast in self.toast_stack.visible() {
            let row = &toast.presentation;
            let actions: Vec<ToastAction<'_>> = row
                .actions
                .iter()
                .map(|(key, label)| ToastAction { key, label })
                .collect();
            let icon = row.app_id.as_ref().and_then(|id| self.icons.get(id));
            let card = ToastCard {
                app: &row.app_name,
                summary: &row.summary,
                body: &row.body,
                icon,
                actions: &actions,
                dismiss_label: "Dismiss",
            };
            let (outcome, layout) = ui.toast((0, y), buffer_width, &card);
            rects.push(Rect::new(0, y, buffer_width, layout.height));
            control_counts.push(layout.actions.len() + 1);
            if outcome.dismissed {
                card_actions.push((row.id, ToastUserAction::Dismiss));
            }
            if let Some(key) = outcome.action {
                card_actions.push((row.id, ToastUserAction::Invoke(key)));
            }
            y += layout.height + ui.px(notification_popup::TOAST_GAP);
        }
        if self.toast_stack.overflow() > 0 {
            let strip = Rect::new(
                0,
                y,
                buffer_width,
                ui.px(notification_popup::OVERFLOW_STRIP_HEIGHT),
            );
            ui.label_styled(
                strip,
                &format!("+{} more in history", self.toast_stack.overflow()),
                design::state_style(&ui),
                design::text_dim(),
                pixelkit_text::Align::Centre,
            );
            y += strip.h;
        } else {
            y -= ui.px(notification_popup::TOAST_GAP);
        }
        let hovered: Vec<bool> = rects.iter().map(|rect| ui.input.hovering(*rect)).collect();
        drop(ui);
        let focused_control = hosted.focus.current();
        hosted.input.end_frame();
        hosted.publish_semantics();
        hosted.present();
        let stack_height = y;
        self.settle_toast_outcomes(&hovered, &control_counts, focused_control, card_actions);
        // Resize to the measured stack; only card rectangles intercept input, not gaps.
        let logical_height = ((stack_height as f32 / scale.0).ceil() as i32).max(1);
        let wanted = (notification_popup::TOAST_CARD_WIDTH, logical_height);
        if wanted != self.toast_wanted {
            self.toast_wanted = wanted;
            if let Some(hosted) = &self.toast_host {
                let _ = hosted
                    .window
                    .request_inner_size(LogicalSize::new(f64::from(wanted.0), f64::from(wanted.1)));
            }
        }
        if let Some(hosted) = &self.toast_host {
            let regions: Vec<_> = rects
                .iter()
                .map(|rect| {
                    let left = (rect.x as f32 / scale.0).floor() as i32;
                    let top = (rect.y as f32 / scale.0).floor() as i32;
                    let right = (rect.right() as f32 / scale.0).ceil() as i32;
                    let bottom = (rect.bottom() as f32 / scale.0).ceil() as i32;
                    (left, top, right - left, bottom - top)
                })
                .collect();
            declare_input_shapes(
                self.bridge.as_mut(),
                &hosted.window,
                &regions,
                "the toast host",
            );
        }
    }

    /// Pause what the pointer or keyboard focus is on and resume the rest with its
    /// remainder, then perform the card outcomes the draw pass collected. Focus runs
    /// dismiss-then-chips per card, in draw order.
    fn settle_toast_outcomes(
        &mut self,
        hovered: &[bool],
        control_counts: &[usize],
        focused_control: Option<usize>,
        card_actions: Vec<(u32, ToastUserAction)>,
    ) {
        let now = Instant::now();
        let ids: Vec<u32> = self
            .toast_stack
            .visible()
            .iter()
            .map(|toast| toast.presentation.id)
            .collect();
        let mut control_base = 0;
        for (index, id) in ids.into_iter().enumerate() {
            let count = control_counts.get(index).copied().unwrap_or(1);
            let keyboard = focused_control
                .is_some_and(|focused| focused >= control_base && focused < control_base + count);
            control_base += count;
            if hovered.get(index).copied().unwrap_or(false) || keyboard {
                self.toast_stack.pause(id, now);
            } else {
                self.toast_stack.resume(id, now);
            }
        }
        for (id, action) in card_actions {
            match action {
                ToastUserAction::Dismiss => {
                    self.toast_stack.dismiss(id);
                    self.perform(FieldAction::DismissNotification(id));
                }
                ToastUserAction::Invoke(key) => {
                    self.toast_stack.invoke(id);
                    self.perform(FieldAction::InvokeNotification { id, key });
                }
            }
        }
    }

    /// Run a notification command off-thread. The journal watcher alone updates the model.
    fn spawn_notification_call<F>(&self, call: F)
    where
        F: FnOnce(
                templar_client::proxies::NotificationsProxy<'_>,
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
                let proxy = templar_client::proxies::NotificationsProxy::new(&connection).await?;
                call(proxy).await.map_err(anyhow::Error::from)
            }
            .await;
            if let Err(error) = result {
                let _ = tx.send(DataUpdate::Notice(format!(
                    "Notification action failed — {error:#}"
                )));
            }
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    fn spawn_clear_notifications(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = templar_client::proxies::NotificationsProxy::new(&connection).await?;
                proxy.clear_all().await.map_err(anyhow::Error::from)
            }
            .await;
            match result {
                Ok(count) => {
                    let _ = tx.send(DataUpdate::Notice(format!(
                        "Cleared {count} notification{}",
                        if count == 1 { "" } else { "s" }
                    )));
                }
                Err(error) => {
                    let _ = tx.send(DataUpdate::Notice(format!(
                        "Could not clear notifications — {error:#}"
                    )));
                }
            }
            let _ = proxy_wake.send_event(Wake::Data);
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
        F: FnOnce(templar_client::proxies::FilesTrashProxy<'static>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = zbus::Result<String>> + Send,
    {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = templar_client::proxies::FilesTrashProxy::new(&connection).await?;
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
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    /// Subscribe before snapshot; one owner-bound journal drives both notification hosts.
    fn spawn_notifications_watch(&self) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let Ok(connection) = crate::data::session().await else {
                return;
            };
            let result = crate::surface::notification_feed::observe(&connection, |update| {
                tx.send(DataUpdate::Notifications(update)).is_ok()
                    && proxy_wake.send_event(Wake::Data).is_ok()
            })
            .await;
            if let Err(error) = result {
                tracing::warn!(%error, "notification observation stopped");
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
                let _ = proxy_wake.send_event(Wake::Data);
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
            let Ok(proxy) = templar_client::proxies::ProvidersProxy::new(&connection).await else {
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
                let _ = proxy_wake.send_event(Wake::Data);
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
                let _ = proxy_wake.send_event(Wake::Data);
                tokio::select! {
                    event = changed.next() => if event.is_none() { return; },
                    event = current_changed.next() => if event.is_none() { return; },
                }
            }
        });
    }

    /// Settings changes drive Home's representation policy, the persisted launcher recents,
    /// the rails, the lanes and the window tab policy. The signal is installed before the
    /// startup reads.
    fn spawn_settings_watch(&self) {
        let launcher_epoch = self.launcher_epoch.clone();
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
                let epoch = launcher_epoch.load(std::sync::atomic::Ordering::SeqCst);
                let (field, recents, rails, lanes, tabs, launcher, transition) = tokio::join!(
                    fetch_home_field_settings(),
                    crate::settings::load_recent_apps(&settings),
                    crate::settings::rails(&settings),
                    crate::settings::lanes(&settings),
                    crate::settings::tabs(&settings),
                    launcher_host::load_preferences(),
                    launcher_host::load_transition(),
                );
                if tx.send(DataUpdate::HomeFieldSettings(field)).is_err() {
                    return;
                }
                let _ = tx.send(DataUpdate::RecentApps(recents));
                let _ = tx.send(DataUpdate::RailSettings(rails));
                let _ = tx.send(DataUpdate::Lanes(lanes));
                let _ = tx.send(DataUpdate::TabSettings(tabs));
                if let Some(preferences) = launcher {
                    let _ = tx.send(DataUpdate::LauncherPreferences { epoch, preferences });
                }
                let _ = tx.send(DataUpdate::WindowTransition(transition.0, transition.1));
                let _ = proxy_wake.send_event(Wake::Data);
                if changed.next().await.is_none() {
                    return;
                }
            }
        });
    }

    /// Observe actual native presentation. Subscribe before Inspect; owner changes invalidate
    /// stale active state immediately, and re-read the same authority when it returns.
    fn spawn_wallpaper_watch(&self) {
        let tx = self.data_tx.clone();
        let wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let observe = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = templar_client::proxies::WallpaperProxy::new(&connection).await?;
                let mut changed = proxy.receive_changed().await?;
                let bus = zbus::fdo::DBusProxy::new(&connection).await?;
                let mut owners = bus.receive_name_owner_changed().await?;
                let publish = |json: Result<String, String>| {
                    let applied = match json.and_then(|json| wallpaper_applied(&json)) {
                        Ok(applied) => applied,
                        Err(error) => {
                            tracing::warn!(%error, "observing native wallpaper; keeping solid Home ground");
                            None
                        }
                    };
                    let _ = tx.send(DataUpdate::WallpaperApplied(applied));
                    let _ = wake.send_event(Wake::Data);
                };
                publish(proxy.inspect().await.map_err(|error| error.to_string()));
                loop {
                    tokio::select! {
                        event = changed.next() => {
                            let Some(event) = event else { break Err::<(), _>(anyhow::anyhow!("Wallpaper1 subscription ended")); };
                            publish(event.args().map(|args| args.json().to_string())
                                .map_err(|error| error.to_string()));
                        }
                        event = owners.next() => {
                            let Some(event) = event else { break Err(anyhow::anyhow!("D-Bus owner observation ended")); };
                            let args = event.args()?;
                            if args.name().as_str() != "org.templaros.Session" { continue; }
                            publish(proxy.inspect().await.map_err(|error| error.to_string()));
                        }
                    }
                }
            };
            if let Err(error) = observe.await {
                tracing::warn!(%error, "native wallpaper observation stopped");
                let _ = tx.send(DataUpdate::WallpaperApplied(None));
                let _ = wake.send_event(Wake::Data);
            }
        });
    }

    /// Listen for `Lifecycle1.LockRequested` and lock when it arrives.
    ///
    /// A raw match rule rather than a generated proxy signal: `templar-client`'s `LifecycleProxy`
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
                .interface("org.templaros.Session.Lifecycle1")
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
                let _ = proxy_wake.send_event(Wake::Data);
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
            let Ok(proxy) = templar_client::proxies::FilesTrashProxy::new(&connection).await else {
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
                let _ = proxy_wake.send_event(Wake::Data);
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
            // One read for both readers. The Settings tile wanted this refetch already; the
            // cluster needs it too, and without the second send its own numbers sat at whatever
            // the last reconciliation left them — a slider that moved and a reading that did not.
            let machine = crate::data::fetch_machine_readings().await;
            let _ = tx.send(DataUpdate::Widget(
                "settings".to_owned(),
                crate::data::settings_widget_from(&machine),
            ));
            let _ = tx.send(DataUpdate::Machine(machine));
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    fn spawn_volume_step(&self, delta: i32) {
        self.spawn_level_step("Volume", move |connection| async move {
            let proxy = templar_client::proxies::AudioProxy::new(&connection).await?;
            proxy.adjust_volume(delta).await?;
            Ok(())
        });
    }

    /// Put the volume exactly here — what the cluster's slider says, rather than a step.
    fn spawn_volume_set(&self, percent: u32) {
        self.spawn_level_step("Volume", move |connection| async move {
            let proxy = templar_client::proxies::AudioProxy::new(&connection).await?;
            proxy.set_volume(percent).await?;
            Ok(())
        });
    }

    /// Mute or unmute, said outright rather than flipped: a drag into and out of the slider's
    /// mute zone must be able to repeat itself without toggling back.
    fn spawn_volume_muted(&self, muted: bool) {
        self.spawn_level_step("Mute", move |connection| async move {
            let proxy = templar_client::proxies::AudioProxy::new(&connection).await?;
            proxy.set_muted(muted).await?;
            Ok(())
        });
    }

    fn spawn_brightness_set(&self, percent: u32) {
        self.spawn_level_step("Brightness", move |connection| async move {
            let proxy = templar_client::proxies::BacklightProxy::new(&connection).await?;
            proxy.set_brightness(percent).await?;
            Ok(())
        });
    }

    fn spawn_volume_mute(&self) {
        self.spawn_level_step("Mute", |connection| async move {
            let proxy = templar_client::proxies::AudioProxy::new(&connection).await?;
            proxy.toggle_muted().await?;
            Ok(())
        });
    }

    fn spawn_brightness_step(&self, delta: i32) {
        self.spawn_level_step("Brightness", move |connection| async move {
            let proxy = templar_client::proxies::BacklightProxy::new(&connection).await?;
            proxy.adjust_brightness(delta).await?;
            Ok(())
        });
    }

    /// Perform one ordinary file verb from a Home file surface.
    ///
    /// The whole point of this function is that it is the *only* one: keyboard and pointer both
    /// arrive here, and each verb goes to the same `templar-filed` interface the Files app uses, so
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
            FileVerb::SearchHere => self.request_launcher(
                LauncherMode::Files,
                request.directory.clone(),
                request.query.clone().unwrap_or_default(),
            ),
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
                let proxy = templar_client::proxies::FilesOperationsProxy::new(&connection).await?;
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
            let _ = proxy_wake.send_event(Wake::Data);
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

    fn spawn_native_transfer(
        &self,
        paths: Vec<PathBuf>,
        destination: PathBuf,
        move_files: bool,
        native_generation: Option<u64>,
    ) {
        let tx = self.data_tx.clone();
        let wake = self.wake_proxy.clone();
        let summary = format!(
            "{} {} items to {}",
            if move_files { "Move" } else { "Copy" },
            paths.len(),
            destination.display()
        );
        self.runtime.spawn(async move {
            let mode = if move_files {
                super::desktop_dock::TransferMode::Move
            } else {
                super::desktop_dock::TransferMode::Copy
            };
            let result = super::desktop_dock::transfer(
                paths,
                destination,
                mode,
                templar_client::files::ConflictPolicy::Fail,
                |id| {
                    let _ = tx.send(DataUpdate::Notice(format!("{summary} · operation {id}")));
                    let _ = wake.send_event(Wake::Data);
                },
            )
            .await;
            let (update, success) = match result {
                Ok(record) => {
                    let success = record.state == templar_client::files::OperationState::Committed
                        && record.skipped.is_empty();
                    let can_undo = async {
                        let connection = crate::data::session().await.ok()?;
                        let operations =
                            templar_client::proxies::FilesOperationsProxy::new(&connection)
                                .await
                                .ok()?;
                        operations
                            .can_undo(&record.id)
                            .await
                            .ok()
                            .map(|(allowed, _)| allowed)
                    }
                    .await
                    .unwrap_or(false);
                    let notice = crate::data::describe_operation(&summary, &record);
                    (
                        DataUpdate::FileOperation {
                            undo: can_undo.then(|| FileOperationUndo {
                                op_id: record.id,
                                summary,
                                at: Instant::now(),
                            }),
                            notice,
                        },
                        success,
                    )
                }
                Err(error) => (
                    DataUpdate::FileOperation {
                        undo: None,
                        notice: format!("{summary} — {error}"),
                    },
                    false,
                ),
            };
            let _ = tx.send(update);
            if let Some(generation) = native_generation {
                let _ = tx.send(DataUpdate::NativeTransferFinished {
                    generation,
                    success,
                });
            }
            let _ = wake.send_event(Wake::Data);
        });
    }

    #[allow(clippy::too_many_lines)]
    fn pump_native_file_drops(&mut self) {
        let events = self
            .bridge
            .as_mut()
            .map(ShellBridge::take_native_drag_events)
            .unwrap_or_default();
        let scale = self
            .home
            .as_ref()
            .map_or(1.0, |home| home.window.scale_factor());
        let locked = self
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.lock_engaged() || bridge.lock_locked());
        let on_home = self
            .bridge
            .as_ref()
            .zip(self.home.as_ref())
            .is_some_and(|(bridge, home)| bridge.native_drag_on_window(&home.window));
        if !self.native_drop_pending
            && !self
                .bridge
                .as_ref()
                .is_some_and(ShellBridge::file_drag_live)
        {
            self.native_drag_origin = None;
        }
        for event in events {
            match event {
                crate::wayland_bridge::NativeDragEvent::Enter { x, y }
                | crate::wayland_bridge::NativeDragEvent::Motion { x, y } => {
                    if self.native_drop_pending {
                        continue;
                    }
                    let target = if locked || !on_home {
                        None
                    } else {
                        self.field_screen.native_drop_destination(
                            (x * scale).round() as i32,
                            (y * scale).round() as i32,
                        )
                    };
                    let shift = self
                        .home
                        .as_ref()
                        .is_some_and(|home| home.input.modifiers().shift);
                    if let Some(bridge) = self.bridge.as_mut() {
                        let result = if target.as_ref().is_some_and(|(_, dock)| !dock) {
                            bridge.set_native_project_drag_target(true, shift)
                        } else {
                            bridge.set_native_drag_target(target.is_some(), shift)
                        };
                        match result {
                            Ok(true) => {
                                self.native_drop_target = target;
                            }
                            Ok(false) => {
                                self.native_drop_target = None;
                            }
                            Err(error) => {
                                self.native_drop_target = None;
                                tracing::warn!(%error, "negotiating native file drop");
                            }
                        }
                    }
                    self.field_screen
                        .set_native_drop_hover(self.native_drop_target.clone(), shift);
                    if let Some(home) = &self.home {
                        home.request_redraw();
                    }
                }
                crate::wayland_bridge::NativeDragEvent::Leave => {
                    self.field_screen.set_native_drop_hover(None, false);
                    if !self.native_drop_pending {
                        self.native_drop_target = None;
                    }
                    if let Some(home) = &self.home {
                        home.request_redraw();
                    }
                }
                crate::wayland_bridge::NativeDragEvent::Drop {
                    x,
                    y,
                    move_requested,
                } => {
                    if self.native_drop_pending {
                        continue;
                    }
                    self.field_screen.set_native_drop_hover(None, false);
                    let target = self.field_screen.native_drop_destination(
                        (x * scale).round() as i32,
                        (y * scale).round() as i32,
                    );
                    if locked || target.is_none() || self.native_drop_target.is_none() {
                        if let Some(bridge) = self.bridge.as_mut() {
                            let _ = bridge.complete_native_drop(false);
                        }
                        continue;
                    }
                    self.native_drop_target = target;
                    self.native_drop_item = self
                        .field_screen
                        .dock_file_at((x * scale).round() as i32, (y * scale).round() as i32)
                        .map(Path::to_path_buf);
                    self.native_drop_move = move_requested;
                    let wake = self.wake_proxy.clone();
                    if let Some(bridge) = self.bridge.as_mut() {
                        match bridge.receive_native_drop(move || {
                            let _ = wake.send_event(Wake::Data);
                        }) {
                            Ok(receiver) => {
                                self.native_drop_receiver = Some(receiver);
                                self.native_drop_generation =
                                    self.native_drop_generation.wrapping_add(1);
                                self.native_drop_pending = true;
                            }
                            Err(error) => {
                                self.native_drop_target = None;
                                self.native_drop_item = None;
                                self.field_screen.report(
                                    format!("Could not receive files: {error}"),
                                    self.started.elapsed().as_millis() as u64,
                                );
                                let _ = bridge.complete_native_drop(false);
                            }
                        }
                    }
                }
            }
        }
        let result =
            self.native_drop_receiver
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Some(Err("Native file drop receiver disconnected".to_owned()))
                    }
                });
        if let Some(result) = result {
            self.native_drop_receiver = None;
            match result {
                Ok(paths) => {
                    if let Some((destination, dock)) = self.native_drop_target.clone() {
                        if dock {
                            if !self.reorder_native_dock_drop(&paths, &destination) {
                                self.spawn_native_transfer(
                                    paths,
                                    destination,
                                    self.native_drop_move,
                                    Some(self.native_drop_generation),
                                );
                            }
                        } else {
                            let (copy_allowed, move_allowed) = self
                                .bridge
                                .as_ref()
                                .map_or((false, false), ShellBridge::native_drop_actions);
                            if copy_allowed || move_allowed {
                                self.field_screen.offer_project_transfer(
                                    paths,
                                    destination,
                                    copy_allowed,
                                    move_allowed,
                                );
                            } else {
                                self.perform(FieldAction::CancelNativeDrop);
                                self.field_screen.report(
                                    "The source offered no supported file transfer".to_owned(),
                                    self.started.elapsed().as_millis() as u64,
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    self.field_screen
                        .report(error, self.started.elapsed().as_millis() as u64);
                    self.native_drop_pending = false;
                    self.native_drop_target = None;
                    self.native_drop_item = None;
                    self.field_screen.set_native_drop_hover(None, false);
                    if let Some(bridge) = self.bridge.as_mut() {
                        let _ = bridge.complete_native_drop(false);
                    }
                }
            }
            if let Some(home) = self.home.as_ref() {
                home.request_redraw();
            }
        }
    }

    fn reorder_desktop_entry(&mut self, path: &Path, index: usize) {
        let Some(destination) = path.parent() else {
            return;
        };
        let Some(dock) = self
            .desktop_dock
            .as_mut()
            .filter(|dock| dock.path == destination)
        else {
            return;
        };
        if !dock.reorder(path, index) {
            return;
        }
        let order = dock.order();
        if let Ok(listing) = dock.browse_listing() {
            self.folder_listings
                .insert(destination.to_path_buf(), Ok(listing));
        }
        if let Some(id) = self.field.tiles.iter().find_map(|tile| match &tile.kind {
            TileKind::DesktopDock { path, .. } if path == destination => Some(tile.id),
            _ => None,
        }) {
            self.perform(FieldAction::Apply(FieldOp::DesktopDockOrder { id, order }));
        }
    }

    fn reorder_native_dock_drop(&mut self, paths: &[PathBuf], destination: &Path) -> bool {
        if paths.len() != 1
            || self.native_drag_origin.as_ref() != paths.first()
            || paths[0].parent() != Some(destination)
            || !self
                .bridge
                .as_ref()
                .is_some_and(ShellBridge::file_drag_live)
        {
            return false;
        }
        let Some(dock) = self
            .desktop_dock
            .as_ref()
            .filter(|dock| dock.path == destination)
        else {
            return false;
        };
        let index = self
            .native_drop_item
            .as_ref()
            .and_then(|path| {
                dock.entries
                    .iter()
                    .position(|entry| entry.path().ok().as_ref() == Some(path))
            })
            .unwrap_or_else(|| dock.entries.len().saturating_sub(1));
        self.reorder_desktop_entry(&paths[0], index);
        if let Some(bridge) = self.bridge.as_mut()
            && let Err(error) = bridge.complete_native_drop(true)
        {
            tracing::warn!(%error, "completing native dock reorder");
        }
        self.native_drop_pending = false;
        self.native_drop_target = None;
        self.native_drag_origin = None;
        true
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
                let proxy = templar_client::proxies::FilesBrowseProxy::new(&connection).await?;
                let name = proxy
                    .create_directory(
                        directory
                            .to_str()
                            .context("This directory API requires a UTF-8 path")?,
                        "New folder",
                    )
                    .await?;
                Ok::<_, anyhow::Error>(name)
            }
            .await;
            let notice = match result {
                Ok(name) => format!("Created {name}"),
                Err(error) => format!("Could not create a folder — {error:#}"),
            };
            let _ = tx.send(DataUpdate::Notice(notice));
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    /// Properties, from `Files.Browse1.Inspect` — the same record the Files app's pane shows.
    fn spawn_properties(&self, path: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let proxy = templar_client::proxies::FilesBrowseProxy::new(&connection).await?;
                let json = proxy.inspect(path.to_str().context("Properties requires a UTF-8 path; native URI/path rendering remains available")?).await?;
                let info: templar_client::files::FileInfo = serde_json::from_str(&json)?;
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
            let _ = proxy_wake.send_event(Wake::Data);
        });
    }

    /// `Copy path`, rendered by `Files.Actions1` so every surface copies the identical text.
    ///
    /// The compositor shell publishes path text through `wl-copy` as an ordinary Wayland client;
    /// standalone Files instead uses PixelKit's native clipboard worker. The shell's own data
    /// source only ever offers a path set
    /// (`crate::data_device`), and a path as *text* is for pasting into a terminal or a dialog,
    /// which wants `text/plain` and nothing else.
    fn spawn_copy_path(&self, path: PathBuf) {
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
        self.runtime.spawn(async move {
            let result = async {
                let connection = crate::data::session().await.map_err(anyhow::Error::msg)?;
                let rendered =
                    templar_client::file_index::render_native_path(&connection, &path, "canonical")
                        .await?;
                copy_text_to_clipboard(&rendered).await?;
                Ok::<_, anyhow::Error>(rendered)
            }
            .await;
            let notice = match result {
                Ok(rendered) => format!("Copied path: {rendered}"),
                Err(error) => format!("Could not copy the path — {error:#}"),
            };
            let _ = tx.send(DataUpdate::Notice(notice));
            let _ = proxy_wake.send_event(Wake::Data);
        });
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
                let actions = templar_client::proxies::FilesActionsProxy::new(&connection).await?;
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
                let _ = proxy_wake.send_event(Wake::Data);
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
                let proxy = templar_client::proxies::FilesOperationsProxy::new(&connection).await?;
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
            // the record either way, and `templar files undo` can reach it.
            let _ = tx.send(DataUpdate::FileOperation { undo: None, notice });
            let _ = proxy_wake.send_event(Wake::Data);
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
        let label = self.app_label(&app_id);
        let tx = self.data_tx.clone();
        let proxy_wake = self.wake_proxy.clone();
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
                        let _ = tx.send(DataUpdate::Notice(launch_failure_notice(&label, &error)));
                        let _ = proxy_wake.send_event(Wake::Data);
                    }
                }
                Err(error) => tracing::warn!(%error, "Launcher1 unavailable"),
            }
        });
    }

    /// The name the launcher showed for this application, falling back to the id so a
    /// failure notice never names something the person never saw.
    fn app_label(&self, app_id: &AppId) -> String {
        self.apps
            .iter()
            .find(|entry| entry.id == *app_id)
            .map_or_else(|| app_id.to_string(), |entry| entry.label().to_owned())
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

/// What the field reports when the daemon refuses a launch: the name the launcher
/// showed and the daemon's reason, so a refused launch reads as refused rather than
/// as an unresponsive click. (An application that *starts* and then dies is the
/// daemon's watchdog's to report, over notifications — the shell never sees it.)
fn launch_failure_notice(app_label: &str, reason: impl std::fmt::Display) -> String {
    format!("Could not launch {app_label} — {reason}")
}

/// The account chip's label when `USER` is unset: the numeric uid, which is at least true.
#[allow(unsafe_code)]
fn account_uid_label() -> String {
    // SAFETY: `getuid` takes no arguments, touches no memory and cannot fail.
    format!("uid {}", unsafe { libc::getuid() })
}

/// The OS remains the authority for local time; Chrono validates and handles calendar arithmetic.
#[allow(unsafe_code)]
fn local_datetime(now: std::time::SystemTime) -> Option<chrono::NaiveDateTime> {
    let seconds = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let seconds = libc::time_t::try_from(seconds).ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers are valid; localtime_r retains neither and initializes local on success.
    if unsafe { libc::localtime_r(&raw const seconds, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: the successful localtime_r call initialized the complete tm.
    let local = unsafe { local.assume_init() };
    let date = chrono::NaiveDate::from_ymd_opt(
        local.tm_year.checked_add(1900)?,
        u32::try_from(local.tm_mon.checked_add(1)?).ok()?,
        u32::try_from(local.tm_mday).ok()?,
    )?;
    // Both clock views have minute resolution, so leap seconds need no special UI policy.
    date.and_hms_opt(
        u32::try_from(local.tm_hour).ok()?,
        u32::try_from(local.tm_min).ok()?,
        0,
    )
}

/// The information row's clock, as of now. Shared with `surface::offscreen`, which draws the same
/// field without a running shell.
pub fn clock_label_now() -> String {
    local_clock_label(std::time::SystemTime::now())
}

fn local_clock_label(now: std::time::SystemTime) -> String {
    clock_label_for(local_datetime(now))
}

fn clock_label_for(local: Option<chrono::NaiveDateTime>) -> String {
    use chrono::{Datelike, Timelike};
    local.map_or_else(
        || "TIME UNAVAILABLE".to_owned(),
        |local| {
            format_clock_fields(
                local.weekday().num_days_from_sunday() as i32,
                local.day() as i32,
                local.month0() as i32,
                local.hour() as i32,
                local.minute() as i32,
            )
        },
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

/// Live and service-backed offscreen Home share the typed size source and off-thread raster.
pub(super) async fn load_treemap_preview(path: PathBuf, width: u32, height: u32) -> TreemapPreview {
    load_treemap_preview_view(
        path,
        width,
        height,
        templar_file_index::snapshot::SnapshotDetail::Summary,
    )
    .await
}

async fn load_treemap_preview_view(
    path: PathBuf,
    width: u32,
    height: u32,
    detail: templar_file_index::snapshot::SnapshotDetail,
) -> TreemapPreview {
    let result = load_treemap_entries(&path, detail).await;
    let mut preview = TreemapPreview {
        caption: format!("Disk use · {}", path.display()),
        path,
        width,
        height,
        bitmap: None,
        status: String::new(),
        composition: Vec::new(),
    };
    match result {
        Ok(load) => {
            preview.status = match (load.generation, load.snapshot_seq) {
                (Some(generation), Some(sequence)) => format!(
                    "{} — generation {generation}, snapshot {sequence}",
                    if load.complete {
                        "Measured indexed sizes"
                    } else {
                        "Partial indexed sizes"
                    }
                ),
                _ => format!(
                    "Bounded direct scan — {}",
                    if load.complete { "complete" } else { "partial" }
                ),
            };
            if let Some(reason) = load.reason {
                preview.status.push_str(": ");
                preview.status.push_str(&reason);
            }
            match tokio::task::spawn_blocking(move || {
                rasterize_entries(&load.entries, width, height)
            })
            .await
            {
                Ok((bitmap, composition)) => {
                    preview.bitmap = Some(bitmap);
                    preview.composition = composition;
                }
                Err(error) => preview.status = format!("Size rendering unavailable: {error}"),
            }
        }
        Err(error) => preview.status = format!("Size index unavailable: {error}"),
    }
    preview
}

/// A stale, partial or unreadable live index is never replaced by an event-loop disk walk.
async fn load_treemap_entries(
    path: &std::path::Path,
    detail: templar_file_index::snapshot::SnapshotDetail,
) -> Result<templar_client::tree_source::TreeLoad, templar_client::tree_source::TreeSourceError> {
    use templar_client::tree_source::{DirectScanSource, TreeSource, TreeSourceError};
    let connection = crate::data::session()
        .await
        .map_err(|error| TreeSourceError::Call(format!("session bus unavailable: {error}")))?;
    match templar_client::tree_source::load_snapshot_tree_view(&connection, path, detail, Some(3))
        .await
    {
        Ok(load) => Ok(load),
        Err(error) if error.allows_fallback() => {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || DirectScanSource::default().load_tree(&path))
                .await
                .map_err(|error| TreeSourceError::Call(error.to_string()))?
        }
        Err(error) => Err(error),
    }
}

/// Lays out and rasterizes an already-loaded directory's contents. An unreadable or empty
/// directory yields a flat, dim bitmap and no band, same as it always has: `entries` is simply
/// short (the root's own entry, or nothing at all).
fn rasterize_entries(
    entries: &[templar_treemap::RawEntry],
    width: u32,
    height: u32,
) -> (pixelkit_raster::Bitmap, Vec<(u32, f32)>) {
    use templar_treemap::{ColorSettings, CushionConfig, LayoutConfig};
    let (width, height) = (width.max(1), height.max(1));
    let tree = templar_treemap::build_tree(entries);
    let colours = ColorSettings::default();
    let layout = templar_treemap::compute_layout_in_rect(
        &tree,
        tree.root,
        0.0,
        0.0,
        width as f32,
        height as f32,
        &LayoutConfig::default(),
    );
    let pixels = templar_treemap::rasterize_argb(
        width,
        height,
        &layout.rects,
        &tree,
        &CushionConfig::default(),
        &colours,
    );
    let weights = &tree.category_weights[tree.root.index()];
    let total: f64 = weights.iter().map(|weight| *weight as f64).sum();
    let composition = if total == 0.0 {
        Vec::new()
    } else {
        templar_treemap::colors::top_categories(weights, templar_treemap::CATEGORY_COUNT)
            .into_iter()
            .map(|(category, weight)| {
                (
                    templar_treemap::colors::category_color(category).to_rgb_u32(),
                    (weight as f64 / total) as f32,
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

/// Append one JSON line to the throw experiment's log, `$XDG_STATE_HOME/templar-shell/throw-spike.jsonl`
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
    let dir = state_home.join("templar-shell");
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

fn search_entries(
    hits: &[templar_client::file_index::IndexHit],
) -> Result<Vec<Option<BrowseEntry>>, templar_client::file_index::IndexCallError> {
    hits.iter()
        .map(|hit| {
            if !hit.present {
                return Ok(None);
            }
            let row = &hit.row;
            let path = row.path.native_path()?;
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            Ok(Some(BrowseEntry {
                path,
                name,
                kind: if row.kind == 1 {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                hidden: row.hidden,
                is_symlink: row.kind == 2,
                size_bytes: row.size_bytes,
                mtime_ms: row.mtime_ns / 1_000_000,
            }))
        })
        .collect()
}

async fn run_search(
    roots: Vec<PathBuf>,
    text: &str,
    tag: &str,
) -> Result<IndexedSearchResults, templar_client::file_index::IndexCallError> {
    use templar_file_index::{NativePath, Query};
    let connection = crate::data::session().await.map_err(|error| {
        templar_client::file_index::IndexCallError::Other(anyhow::anyhow!(error))
    })?;
    let query = Query {
        text: text.to_owned(),
        within: roots
            .iter()
            .map(|path| NativePath::from_path(path))
            .collect(),
        page_size: crate::surface::search_pages::PAGE_SIZE,
        client_tag: Some(tag.to_owned()),
        require_metadata: true,
        ..Query::default()
    };
    let session = templar_client::file_index::SearchSession::search(&connection, &query).await?;
    let result = session.result();
    let first = search_entries(&result.hits)?;
    let entries = first.iter().flatten().cloned().collect::<Vec<_>>();
    let pages = crate::surface::search_pages::SearchPages::new(
        result.generation,
        session.retained(),
        first,
    );
    let total = result
        .total
        .map_or_else(|| "unknown".to_owned(), |total| total.to_string());
    let notice = format!(
        "{} retained of {total} matches · generation {}{}{}",
        session.retained(),
        result.generation,
        if result.current() {
            ""
        } else {
            " · names or metadata not current; see Settings Files for the reason"
        },
        if result.truncated {
            " · retained search truncated; refine the query"
        } else {
            ""
        }
    );
    let truth = result
        .roots
        .iter()
        .filter(|root| !root.current() || root.metadata != "verified")
        .map(|root| {
            format!(
                "{}: {} {} · metadata {}",
                root.display,
                root.state,
                root.reason.as_deref().unwrap_or(""),
                root.metadata
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let notice = if truth.is_empty() {
        notice
    } else {
        format!("{notice} · {truth}")
    };
    Ok(IndexedSearchResults {
        session: std::sync::Arc::new(session),
        entries,
        pages,
        notice,
    })
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
                .with_transparent(
                    winit::platform::wayland::ActiveEventLoopExtWayland::is_wayland(event_loop),
                )
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
                tracing::info!(
                    native = bridge.native_available(),
                    "connected Home's alpha-capable Wayland presenter"
                );
                match bridge.set_layer(&home_window, templar_window::Layer::HomeSurface) {
                    Ok(()) => tracing::info!("Home Surface declared onto Layer::HomeSurface"),
                    Err(error) => tracing::warn!(%error, "declaring the Home Surface layer"),
                }
                let proxy = self.wake_proxy.clone();
                if let Err(error) = bridge.spawn_waker(move || {
                    let _ = proxy.send_event(Wake::Data);
                }) {
                    tracing::warn!(%error, "the compositor's events will wait for the next input");
                }
                self.bridge = Some(bridge);
                self.minimize_targets = MinimizeTargets::default();
                // The gate as the defaults have it until the settings reading lands; it is
                // relayed again then if the person chose otherwise.
                self.relay_gesture_gate();
                self.relay_tab_policy();
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "templar_shell_v2 unavailable; the Window Shelf will show no live windows until \
                     this process is connected to the compositor's trusted-shell socket"
                );
            }
        }

        self.home = match if self.bridge.is_some() {
            Ok(HostedWindow::new_wayland(home_window))
        } else {
            HostedWindow::new(home_window)
        } {
            Ok(mut hosted) => {
                hosted.attach_accessibility(event_loop, self.wake_proxy.clone());
                let clipboard_wake = self.wake_proxy.clone();
                hosted
                    .input
                    .attach_clipboard(pixelkit_shell::clipboard::ClipboardWorker::new(move || {
                        let _ = clipboard_wake.send_event(Wake::Data);
                    }));
                Some(hosted)
            }
            Err(error) => {
                tracing::error!(%error, "creating the Home Surface presenter");
                None
            }
        };
        // Every watcher installs its match rule before taking its startup snapshot. The slow
        // reconciliation task starts last and exists only as a continuity backstop.
        self.spawn_applications_watch();
        self.spawn_field_watch();
        self.spawn_garden_watch();
        self.spawn_desktop_watch();
        self.spawn_files_watch();
        self.spawn_notifications_watch();
        self.spawn_providers_watch();
        self.spawn_projects_watch();
        self.spawn_settings_watch();
        self.spawn_wallpaper_watch();
        self.spawn_trash_watch();
        self.spawn_lock_watch();
        self.spawn_reconciliation_loop();
        if let Some(home) = &self.home {
            home.request_redraw();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, wake: Wake) {
        if let Wake::Accessibility(event) = wake {
            self.drain_updates();
            if let Some(home) = &mut self.home
                && home.window.id() == event.window_id
            {
                home.accessibility_event(event.window_event);
            } else if let Some(hosted) = &mut self.launcher
                && hosted.window.id() == event.window_id
            {
                hosted.accessibility_event(event.window_event);
            } else if let Some(hosted) = &mut self.toast_host
                && hosted.window.id() == event.window_id
            {
                hosted.accessibility_event(event.window_event);
            }
            return;
        }
        if let Some(home) = &mut self.home {
            home.invalidate_semantics();
        }
        self.drain_updates();
        if let Some(launcher) = &self.launcher {
            launcher.request_redraw();
        }
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        self.sync_toast_host(event_loop, &toplevels);
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
        if self
            .launcher
            .as_ref()
            .is_some_and(|hosted| hosted.window.id() == window_id)
        {
            self.launcher_event(&event);
            return;
        }
        let is_toast = self
            .toast_host
            .as_ref()
            .is_some_and(|hosted| hosted.window.id() == window_id);
        if is_toast {
            if let Some(hosted) = &mut self.toast_host {
                hosted.process_accessibility_event(&event);
            }
            // The toast host routes its own input to its PixelKit cards. It never claims
            // keyboard focus: keys are only ever routed when the compositor sent them.
            match event {
                WindowEvent::RedrawRequested => self.redraw_toast(),
                WindowEvent::CloseRequested => self.toast_host = None,
                WindowEvent::Focused(false) => {
                    if let Some(hosted) = &mut self.toast_host {
                        hosted.input.blur();
                        hosted.request_redraw();
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    if let Some(hosted) = &mut self.toast_host {
                        hosted
                            .input
                            .cursor_moved(position.x as f32, position.y as f32);
                        hosted.request_redraw();
                    }
                }
                WindowEvent::CursorLeft { .. } => {
                    if let Some(hosted) = &mut self.toast_host {
                        hosted.input.cursor_left();
                        hosted.request_redraw();
                    }
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    if let Some(hosted) = &mut self.toast_host
                        && let Some(pressed) = translate_button(button)
                    {
                        hosted.input.mouse(pressed, state == ElementState::Pressed);
                        hosted.request_redraw();
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    if let Some(hosted) = &mut self.toast_host {
                        let (dx, dy) = scroll_pixels(delta);
                        hosted.input.scrolled(dx, dy);
                        hosted.request_redraw();
                    }
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    if !key_pressed(event.state) {
                        return;
                    }
                    let keys = translate_keys(&event.logical_key, event.text.as_deref());
                    if !keys.is_empty()
                        && let Some(hosted) = &mut self.toast_host
                    {
                        for key in keys {
                            if key == pixelkit_shell::KeyInput::Escape {
                                hosted.focus.clear();
                            }
                            hosted.input.key(key);
                        }
                        hosted.request_redraw();
                    }
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    if let Some(hosted) = &mut self.toast_host {
                        hosted.input.modifiers_changed(modifiers_of(modifiers));
                    }
                }
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                    if let Some(hosted) = &mut self.toast_host {
                        if let WindowEvent::ScaleFactorChanged { scale_factor, .. } = event {
                            hosted.scale = pixelkit_shell::Scale::new(scale_factor);
                        }
                        hosted.request_redraw();
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
        let is_taskfield = self
            .taskfield
            .as_ref()
            .is_some_and(|taskfield| taskfield.window.id() == window_id);
        if is_taskfield {
            if matches!(event, WindowEvent::RedrawRequested) {
                self.redraw_taskfield();
            }
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
        if let Some(home) = &mut self.home {
            home.process_accessibility_event(&event);
            if matches!(
                event,
                WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
                    | WindowEvent::KeyboardInput { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
                    | WindowEvent::Ime(_)
                    | WindowEvent::Focused(_)
            ) {
                home.invalidate_semantics();
            }
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
            WindowEvent::Focused(false) => {
                if self.native_drop_pending && self.field_screen.modal_open() {
                    self.native_drop_pending = false;
                    self.native_drop_target = None;
                    if let Some(bridge) = self.bridge.as_mut() {
                        let _ = bridge.complete_native_drop(false);
                    }
                }
                self.field_screen.pointer_blurred();
                hosted.input.blur();
                hosted.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Preedit(text, _)) => {
                hosted.input.set_composition(&text);
                hosted.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Commit(text)) => {
                hosted.input.set_composition("");
                for character in text.chars() {
                    hosted
                        .input
                        .key(pixelkit_shell::KeyInput::Character(character));
                }
                hosted.request_redraw();
            }
            WindowEvent::Ime(winit::event::Ime::Disabled) => hosted.input.set_composition(""),
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
                        if key == pixelkit_shell::KeyInput::Escape {
                            hosted.focus.clear();
                        }
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
                    if button == pixelkit_shell::MouseButton::Left && state == ElementState::Pressed
                    {
                        hosted.focus.clear();
                    }
                    hosted.input.mouse(button, state == ElementState::Pressed);
                    if button == pixelkit_shell::MouseButton::Left
                        && state == ElementState::Released
                    {
                        self.field_screen.pointer_released(hosted.input.cursor());
                    }
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
            self.dismiss_launcher();
            self.bridge = None;
            self.window_transition_relayed = None;
            self.minimize_targets = MinimizeTargets::default();
            self.previews.clear();
            self.preview_requested.clear();
            self.preview_demand = crate::previews::DemandTracker::default();
            self.preview_demand_sent = false;
        }
        if self
            .bridge
            .as_ref()
            .is_some_and(|bridge| bridge.lock_engaged() || bridge.lock_locked())
        {
            self.refresh_previews(&[]);
        }
        // The lock screen has no winit window, so nothing else would ever redraw it: its keys
        // arrive on the bridge's own queue and its frames are committed straight to the
        // compositor.
        if self.drain_lock_keys() {
            self.paint_lock();
        }
        // Super+Space opens the independent popup without revealing Home. The compositor
        // retains physical keys until this overlay has committed and borrowed focus.
        if self
            .bridge
            .as_mut()
            .is_some_and(|bridge| bridge.take_launcher_focus() > 0)
        {
            self.request_launcher(LauncherMode::Apps, None, String::new());
        }
        // Answered here rather than on the next paint: the terminal chord is a keypress a person
        // is waiting on, and Home is only redrawn when something asks it to be.
        if self.drain_terminal_requests()
            && let Some(home) = &self.home
        {
            home.request_redraw();
        }
        if let Some(bridge) = &mut self.bridge {
            for request in bridge.take_launcher_requests() {
                self.launcher_request = Some(LauncherRequest {
                    mode: request.mode,
                    scope: request.scope,
                    query: request.query,
                });
            }
        }
        self.drain_updates();
        self.ensure_launcher(event_loop);
        self.flush_search();
        self.fetch_search_pages();
        self.pump_native_file_drops();
        // winit's same-scale wl_surface.enter/leave has no WindowEvent. The already-awake
        // event loop still sees its updated current_monitor; redraw once if validity changed.
        if let Some(home) = &self.home {
            let active = self
                .bridge
                .as_ref()
                .is_some_and(ShellBridge::native_available)
                && wallpaper_for_home(&home.window, self.wallpaper_applied.as_ref());
            if active != self.field_screen.wallpaper_active {
                home.request_redraw();
            }
        }
        let toplevels = self
            .bridge
            .as_ref()
            .map(ShellBridge::toplevels)
            .unwrap_or_default();
        self.sync_toast_host(event_loop, &toplevels);
        self.sync_rail_mode();
        self.ensure_bar(event_loop, &toplevels);
        self.ensure_cluster(event_loop, &toplevels);
        self.sync_taskfield(event_loop);
        self.relay_reduced_motion();
        self.relay_window_transition();
        self.record_activation_recency(&toplevels);
        let (entries, rows) = self.bar_entries(&toplevels);
        self.sync_rail(entries.len(), rows);
        // A masked frame deferred because the compositor still held every buffer is owed, and
        // the pointer that asked for it may have stopped: the buffer coming back is the wake.
        self.retry_deferred_presents();
        let rail_open = |host: RailHost| {
            !self.hosts_gesture || self.rail.as_ref().is_some_and(|rail| rail.host == host)
        };
        let bar_live = rail_open(RailHost::Apps);
        let cluster_live = rail_open(RailHost::System);
        // The window list moved: Home's session tiles and the bar both show it, so both redraw
        // now rather than on the next slow sweep.
        let generation = self.bridge.as_ref().map_or(0, ShellBridge::generation);
        if generation != self.bridge_generation {
            self.bridge_generation = generation;
            if let Some(launcher) = &self.launcher {
                launcher.request_redraw();
            }
            if let Some(home) = &self.home {
                home.request_redraw();
            }
            // A closed rail shows no window list, so it has nothing to redraw for.
            if let Some(bar) = self.bar.as_ref().filter(|_| bar_live) {
                bar.request_redraw();
            }
            // Every `taskfield_state` bumps the generation too: the query, the selection and the
            // filter all arrive that way, and every one of them changes what this draws.
            if let Some(taskfield) = &self.taskfield {
                taskfield.request_redraw();
            }
        }
        // Toasts retire on their own dwell; the stack owns the deadline. Retirement is
        // presentation only: the service row and its history entry are untouched.
        if self
            .toast_stack
            .retire_expired(Instant::now(), notification_popup::now_ms())
            && let Some(hosted) = &self.toast_host
        {
            hosted.request_redraw();
        }
        if self.toast_stack.is_empty() && self.toast_host.is_some() {
            self.toast_host = None;
            self.toast_wanted = (0, 0);
            self.toast_output = None;
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
        // ghost follows the pointer and a throw's velocity is sampled evenly. Otherwise the next
        // wake is the earliest of the toast stack's deadline and the bar's hover timers.
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
            .toast_stack
            .next_deadline(Instant::now(), notification_popup::now_ms());
        // The bar's own timers: a hover row's dwell and grace, and — while a window covers the
        // bottom strip — the dwell that reveals the bar out from under it and the grace that
        // collapses it again. `covered` has to be recomputed here because it decides which of
        // those two pairs is running.
        let bar_covered = !self.hosts_gesture
            && bar_covered(
                self.bridge.as_ref().is_some_and(ShellBridge::home_revealed),
                &toplevels,
            )
            && !self.bridge.as_ref().is_some_and(ShellBridge::bar_peeked);
        let bar_deadline = self.bar.as_ref().filter(|_| bar_live).and_then(|bar| {
            let entries = self.bar_entries(&toplevels).0;
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
        let cluster_deadline = self
            .cluster
            .as_ref()
            .filter(|_| cluster_live)
            .map(|cluster| {
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
        // An open rail's grace: the pointer is away from it, and it goes when that runs out.
        let rail_deadline = self.rail.as_ref().and_then(|rail| rail.session.deadline());
        let preview_deadline = self.preview_deadline();
        if preview_deadline.is_some_and(|deadline| deadline <= Instant::now())
            && let Some(home) = &self.home
        {
            home.request_redraw();
        }
        let deadline = [
            self.search_pending
                .as_ref()
                .map(|(_, _, at)| *at + Self::SEARCH_DEBOUNCE),
            toast_deadline,
            bar_deadline,
            cluster_deadline,
            rail_deadline,
            preview_deadline,
        ]
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

/// Retain the actual per-output observation, not desired enabled/configuration.
fn wallpaper_applied(
    json: &str,
) -> Result<Option<templar_types::wallpaper::WallpaperObservation>, String> {
    let status: templar_types::wallpaper::WallpaperStatus =
        serde_json::from_str(json).map_err(|error| error.to_string())?;
    Ok(status.applied)
}

fn wallpaper_for_home(
    window: &Window,
    applied: Option<&templar_types::wallpaper::WallpaperObservation>,
) -> bool {
    if applied.is_none() {
        return false;
    }
    let output = window.current_monitor().and_then(|monitor| monitor.name());
    wallpaper_on_output(applied, output.as_deref())
}

/// Current native backends advertise stable DisplayId as wl_output.name.
/// An unknown/unentered output stays solid; another output's success cannot validate Home.
fn wallpaper_on_output(
    applied: Option<&templar_types::wallpaper::WallpaperObservation>,
    name: Option<&str>,
) -> bool {
    let (Some(applied), Some(name)) = (applied, name) else {
        return false;
    };
    applied
        .outputs
        .iter()
        .any(|output| output.output.0 == name && output.presented.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_refresh_preserves_desktop_watcher_listing_and_refreshes_other_folders() {
        let desktop = PathBuf::from("/home/u/Desktop");
        let project = PathBuf::from("/home/u/Work/project");
        let mut listings = HashMap::from([
            (desktop.clone(), Err("native watcher status".to_owned())),
            (project.clone(), Err("old generic status".to_owned())),
        ]);
        assert_eq!(
            invalidate_folder_listings(&mut listings, Some(&desktop)),
            vec![project]
        );
        assert_eq!(listings.len(), 1);
        assert_eq!(
            listings[&desktop].as_ref().unwrap_err(),
            "native watcher status"
        );
        assert_eq!(
            invalidate_folder_listings(&mut listings, None),
            vec![desktop]
        );
        assert!(listings.is_empty());
    }

    #[test]
    fn wallpaper_transparency_requires_actual_presented_state_not_desired_enable() {
        use templar_types::wallpaper::{
            WallpaperConfig, WallpaperObservation, WallpaperOutput, WallpaperStatus,
        };
        let mut status = WallpaperStatus {
            revision: Default::default(),
            enabled: true,
            config: WallpaperConfig::default(),
            paused: false,
            source: None,
            pending: true,
            applied: None,
            error: None,
        };
        let active = |status: &WallpaperStatus| {
            let applied = wallpaper_applied(&serde_json::to_string(status).unwrap()).unwrap();
            wallpaper_on_output(applied.as_ref(), Some("test"))
        };
        assert!(!active(&status));
        let mut output = WallpaperOutput {
            output: templar_types::display::DisplayId("test".into()),
            accepted: Some(Default::default()),
            presented: None,
            source_bytes: 4,
            texture_bytes: 4,
            context_replicas: 1,
            error: None,
        };
        status.applied = Some(WallpaperObservation {
            revision: Default::default(),
            outputs: vec![output.clone()],
            indexed_supported: false,
        });
        assert!(
            !active(&status),
            "accepted but not presented is not a valid backdrop"
        );
        output.presented = output.accepted;
        status.applied.as_mut().unwrap().outputs = vec![output];
        status.enabled = false;
        status.error = Some("replacement decode failed".into());
        assert!(
            active(&status),
            "a still-presented texture remains real during reconciliation/error"
        );
        assert!(
            !wallpaper_on_output(status.applied.as_ref(), Some("other-output")),
            "a different output's successful texture must not make Home transparent"
        );
        assert!(
            !wallpaper_on_output(status.applied.as_ref(), None),
            "unknown Home output remains solid until wl_surface.enter identifies it"
        );
        let mut failed_output = status.applied.as_ref().unwrap().outputs[0].clone();
        failed_output.output.0 = "other-output".into();
        failed_output.presented = None;
        failed_output.error = Some("native import failed on this GPU".into());
        status.applied.as_mut().unwrap().outputs.push(failed_output);
        assert!(
            !wallpaper_on_output(status.applied.as_ref(), Some("other-output")),
            "matching but failed Home output stays solid despite another output's success"
        );
        // Same-scale enter/leave transitions use the current surface output, not cached success.
        for (entered, expected) in [
            (None, false),
            (Some("test"), true),
            (Some("other-output"), false),
            (Some("test"), true),
            (None, false),
        ] {
            assert_eq!(
                wallpaper_on_output(status.applied.as_ref(), entered),
                expected,
                "Home entered-output transition {entered:?}"
            );
        }
        status.applied.as_mut().unwrap().outputs.clear();
        assert!(
            !active(&status),
            "cleared native output restores solid Home ground"
        );
        assert!(wallpaper_applied("not valid JSON").is_err());
    }

    fn window(
        id: u64,
        fullscreen: bool,
        maximized: bool,
        shelved: bool,
    ) -> templar_window::ShellToplevel {
        templar_window::ShellToplevel {
            window_id: templar_window::WindowId(id),
            canonical_app_id: None,
            claimed_app_id: None,
            instance_id: None,
            title: "kitty".to_owned(),
            verified_identity: false,
            activated: true,
            mru_rank: 0,
            fullscreen,
            shelved,
            maximized,
            geometry: templar_window::Rect::new(0, 0, 1920, 1080),
            layer: templar_window::Layer::Normal,
            group: None,
            tab_count: None,
        }
    }

    #[test]
    fn card_rects_become_logical_rounding_outward() {
        assert_eq!(
            logical_rect(Rect::new(10, 20, 30, 40), 1.0),
            Some((10, 20, 30, 40))
        );
        // 1.5: 3..34 × 4..35 covers 2.0..22.67 × 2.67..23.33 logical, so 2..23 × 2..24.
        assert_eq!(
            logical_rect(Rect::new(3, 4, 31, 31), 1.5),
            Some((2, 2, 21, 22))
        );
        assert_eq!(logical_rect(Rect::new(0, 0, 0, 10), 2.0), None);
    }

    #[test]
    fn minimize_targets_send_only_changes_and_forget_closed_windows() {
        use templar_window::WindowId;
        fn sorted(
            targets: &mut MinimizeTargets,
            now: &[(u64, LogicalRect)],
            live: &[templar_window::ShellToplevel],
        ) -> Vec<(WindowId, Option<LogicalRect>)> {
            let mut changes =
                targets.changes(now.iter().map(|(id, rect)| (WindowId(*id), *rect)), live);
            changes.sort_by_key(|(window, _)| window.0);
            changes
        }
        let mut targets = MinimizeTargets::default();
        let live = vec![
            window(1, false, false, false),
            window(2, false, false, true),
        ];
        let first = sorted(
            &mut targets,
            &[(1, (0, 0, 10, 10)), (2, (20, 0, 10, 10))],
            &live,
        );
        assert_eq!(
            first,
            vec![
                (WindowId(1), Some((0, 0, 10, 10))),
                (WindowId(2), Some((20, 0, 10, 10)))
            ]
        );
        for (window, rect) in first {
            targets.record(window, rect);
        }
        // Nothing moved: nothing to send.
        assert!(
            sorted(
                &mut targets,
                &[(1, (0, 0, 10, 10)), (2, (20, 0, 10, 10))],
                &live
            )
            .is_empty()
        );
        // Window 2 lost its card but still exists: clear it. Window 1 moved: resend.
        let next = sorted(&mut targets, &[(1, (5, 0, 10, 10))], &live);
        assert_eq!(
            next,
            vec![(WindowId(1), Some((5, 0, 10, 10))), (WindowId(2), None)]
        );
        for (window, rect) in next {
            targets.record(window, rect);
        }
        // Window 1 closed: forgotten without a request.
        assert!(sorted(&mut targets, &[], &live[1..]).is_empty());
        assert!(targets.sent.is_empty());
        // An old compositor is asked nothing more.
        targets.unsupported = true;
        assert!(sorted(&mut targets, &[(2, (0, 0, 1, 1))], &live).is_empty());
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
    fn the_switcher_hides_only_under_a_window_that_fills_the_screen() {
        assert!(!bar_covered(false, &[]), "a bare desktop covers nothing");

        // Maximized and fullscreen alike: the window fills the screen, and the switcher is the
        // one thing on the bottom edge that is not part of it.
        for filling in [window(1, false, true, false), window(1, true, false, false)] {
            assert!(bar_covered(false, std::slice::from_ref(&filling)));
            // Home revealed: the compositor lifted Home over the windows, so the bar has Home to
            // paint on and the switcher is simply visible.
            assert!(!bar_covered(true, std::slice::from_ref(&filling)));
        }

        // Parked is not on screen, so it covers nothing.
        assert!(!bar_covered(false, &[window(1, false, true, true)]));

        // An ordinary floating window over the bottom of the screen is not what the screen is
        // for at that moment, and there is no hover route back out from under one — the
        // compositor's dwell fires only under a window that covers the screen.
        let mut floating = window(2, false, false, false);
        floating.geometry = templar_window::Rect::new(0, 0, 1920, 1080);
        assert!(!bar_covered(false, &[floating]));
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
            tool_launch("project.open-files", "/home/dk/src/templaros"),
            Some((
                ProviderRole::FileManager,
                vec!["/home/dk/src/templaros".to_owned()]
            )),
            "a project's own tool row still resolves the role"
        );
        // And what it resolves is exactly what the one route passes: the bare directory, which
        // is what pcmanfm-qt (and every file manager for thirty years) takes.
        let (role, arguments) =
            tool_launch("project.open-files", "/home/dk/src/templaros").expect("the row exists");
        assert_eq!(role, ProviderRole::FileManager);
        assert_eq!(arguments, vec!["/home/dk/src/templaros".to_owned()]);
    }

    /// project's real root in the form it takes. Getting the argument wrong opens the editor on
    /// the person's home directory, which looks like it worked.
    #[test]
    fn each_project_tool_resolves_its_role_and_opens_at_the_root() {
        assert_eq!(
            tool_launch("project.open-terminal", "/home/dk/src/templaros"),
            Some((
                ProviderRole::Terminal,
                vec![
                    "--directory".to_owned(),
                    "/home/dk/src/templaros".to_owned()
                ]
            ))
        );
        assert_eq!(
            tool_launch("project.open-files", "/home/dk/src/templaros"),
            Some((
                ProviderRole::FileManager,
                vec!["/home/dk/src/templaros".to_owned()]
            ))
        );
        assert_eq!(
            tool_launch("project.open-editor", "/home/dk/src/templaros"),
            Some((
                ProviderRole::CodeEditor,
                vec!["/home/dk/src/templaros".to_owned()]
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

    /// A refused launch names the application the launcher showed and the daemon's
    /// reason: the notice that keeps a refusal from reading as an unresponsive click.
    #[test]
    fn a_refused_launch_reports_the_name_and_the_reason() {
        assert_eq!(
            launch_failure_notice("Proton VPN", "no manifest for this application"),
            "Could not launch Proton VPN — no manifest for this application"
        );
    }
}
