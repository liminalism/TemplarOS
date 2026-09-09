//! The Home Field screen: the document from `crate::field_model` drawn as tiles, with a search
//! field above it, the drawers' full bodies a click away, and every action reachable by pointer
//! *and* by keyboard. Like every screen in this crate, a pure `draw(ui, area, data) -> actions`
//! over a `WindowBuffer`: no D-Bus, no Wayland, so it is tested with a synthetic `Input`.
//!
//! What this draws is decided elsewhere — `templar_field` owns where tiles are and what a drop
//! means; `crate::field_model` owns which tiles exist. This module owns only pixels and the
//! translation of pointer/keyboard gestures into `FieldAction`s. The visual grammar is
//! `@templaros.decision.home-field-stable-bounded-semantic-grammar`: flat, calm tile shells; type expressed inside the rectangle, never by
//! its silhouette; a raised shadow only while a tile is lifted; one activity border for a live
//! window; the pending operation spelled out in text before release.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use pixelkit_raster::{Bitmap, Rect, ScaleFilter};
use pixelkit_shell::KeyInput;
use pixelkit_text::{Align, TextStyle};
use pixelkit_ui::{TextFieldState, Ui};

use templar_field::drag::{Drag, DragConfig, DragSample, GrabOffset, Release};
use templar_field::focus::Direction;
use templar_field::{
    Cell, FieldDocument, FieldOp, Landing, Operation, PixelRect, Placement, Representation,
    Selection, Span, SpanRange, Target, Tile, TileId, TileKind, apply, cell_at, tile_rect,
};
use templar_types::AppId;
use templar_window::{ShellToplevel, WindowId};

use crate::field_model::{self, GAP};

/// A folder window's row height and icon side, logical pixels. Tight on purpose: a window three
/// cells tall has to be able to show more than four things.
const FOLDER_ROW_HEIGHT: i32 = 20;

const FOLDER_ICON: i32 = 14;

/// What a folder tray opens to when it is pressed, clamped to the kind's ceiling. A tray is a
/// door; this is the size at which the room behind it is worth looking at.
const FOLDER_OPEN_SPAN: Span = Span { cols: 4, rows: 4 };
use crate::file_browser::{DirectoryListing, EntryKind};
use crate::home_surface::Drawer;
use crate::surface::context_menu::{Chosen, ContextMenu, MenuTarget};
use crate::surface::design::{self, Elevation};
use crate::surface::file_browser::FilesBrowseView;
use crate::surface::file_browser::{FileBrowserData, theme_icon_key};
use crate::surface::place_forms;
use crate::surface::widgets::{self, WidgetSnapshot};

/// One typed file drop, ready for the service. The shell never decides what a conflict, a
/// permission failure or a device boundary means — it names the sources and the destination and
/// lets `templar-filed` answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDrop {
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    /// Copy rather than move, as the grammar announced before release.
    pub copy: bool,
}

/// What the screen asks its caller to do. Everything that touches a service or the compositor
/// comes out here; nothing in this module performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldAction {
    /// Ask the file service this question, for this scope. The host debounces it — this is the
    /// question, not the round trip.
    RunSearch {
        scope: crate::omnibox::Scope,
        query: String,
    },
    Launch(AppId),
    /// Keep this application on Home for good, as a tile of its own. The one route to an
    /// application tile: Home no longer grows one per running window.
    PinApp(AppId),
    /// Start exactly this copy of an application, rather than whichever one the daemon would
    /// choose. The only route that names a variant on purpose.
    LaunchVariant(AppId),
    Activate(WindowId),
    Park(WindowId),
    Restore(WindowId),
    /// Fetch what the Files surface's current location needs. Same staleness guards as before,
    /// keyed on the location rather than on a drawer that no longer exists.
    BrowseLocation {
        location: crate::files_surface::FilesLocation,
        show_hidden: bool,
    },
    OpenFile(PathBuf),
    /// Fetch one folder window's directory. Separate from [`FieldAction::BrowseLocation`] because
    /// there is one Files object but many folder windows: this one is keyed by path and cached
    /// per path, so several windows on the field each list their own directory.
    ///
    /// `visited` is whether this is somewhere the person actually went. A folder window is
    /// repainted every frame, so its own anchor arrives here constantly and for no reason of the
    /// user's; counting that as a `directory-opened` is how the garden came to feed on Home's own
    /// chrome — the Desktop tray earned an act of work every session simply by being drawn. Only
    /// a directory reached *inside* the window is a visit.
    BrowseFolder {
        path: PathBuf,
        visited: bool,
    },
    /// Open this folder in whichever application holds `ProviderRole::FileManager`.
    ///
    /// The one route from Home to a folder. Every press that opens one arrives here — the Files
    /// object's ground, a Folder tile, a row or a crumb of a folder window, a Files-tile row, a
    /// typed path in the search, and the Place menu's `place.open-files`. There used to be an
    /// `OpenFolderWindow` beside it that did nothing but forward here, and a second copy of the
    /// role lookup in `perform_place`; a route that exists twice is a route that behaves
    /// differently twice.
    OpenInFiles(PathBuf),
    /// Perform one declared action (`templar_types::action::MANIFEST`). This is how every widget
    /// region, and every menu row, reaches a capability: the tile names an action id and the
    /// host decides what performing it means, so a new control never needs a new variant here.
    RunAction(&'static str),
    /// The same, for an action about a project: `project.enter`, `project.restore` and the three
    /// tools. The project id is the one the menu was opened over, carried from there rather than
    /// looked up again — the field reflows while a menu is open, and "which project" must not be
    /// able to change under it.
    Project {
        id: &'static str,
        project_id: String,
    },
    /// The same, for a Place: `place.open-*` and the `garden.*` verbs, over the folder the menu
    /// or the tile's own roles row named.
    Place {
        id: &'static str,
        path: PathBuf,
    },
    /// The same, for an action that needs to name *which* window it applies to — everything the
    /// context menu offers over a session tile. One declaration, two routes: this is the pointer
    /// half of what the compositor's reserved chords do from the keyboard.
    RunWindowAction {
        id: &'static str,
        window: WindowId,
    },
    /// Put one window in another's window group (`window.group`, and on the wire
    /// `templar_shell_toplevel_v1.join_group`): the drop of one session tile on another, or on a
    /// group's tile.
    ///
    /// Not a `RunWindowAction`: that names one window, and grouping is the one window verb that
    /// needs two. The action id is still the manifest's, so the menu, the cheat sheet and this
    /// cannot call the capability three different things.
    JoinWindowGroup {
        window: WindowId,
        other: WindowId,
    },
    /// Name one window group (`window.rename-group`, `templar_shell_toplevel_v1.set_group_name`),
    /// addressed through any member. An empty name unnames it.
    RenameWindowGroup {
        window: WindowId,
        name: String,
    },
    /// Dismiss exactly one notification (`Notifications1.Dismiss`); the bulk clear stays the
    /// declared `notifications.clear` action.
    DismissNotification(u32),
    /// Invoke one notification's declared action (`Notifications1.Invoke`), which emits the
    /// freedesktop `ActionInvoked` for the owning application.
    InvokeNotification {
        id: u32,
        key: String,
    },
    /// Put one trashed item back, named by its path under a trash's `files/`.
    ///
    /// Not a `RunAction`: a declared action carries no payload, and restoring is always *this*
    /// item — the row the pointer was on. The host hands it to `Files.Trash1.Restore`, which
    /// journals it like any other file operation, so it keeps its own undo.
    RestoreFromTrash(String),
    /// A drop the grammar named as a real file operation: copy or move these sources into that
    /// directory, as one journalled `Files.Operations1` operation with one undo.
    DropFiles(FileDrop),
    /// An ordinary file verb performed in an expanded Home file surface — copy, cut, paste,
    /// trash, rename, new folder, properties, copy path. The host performs it through the same
    /// `templar-filed` interfaces the Files app uses.
    FileVerb(crate::surface::file_browser::FileRequest),
    RevealHome,
    /// Discard the stored layout and take the current default instead. Not an `Apply`: the
    /// document and its undo log go together, so there is no inverse to keep and the host has to
    /// ask the service rather than edit the copy it holds.
    ResetField,
    /// A field op the user performed; the caller applies it and keeps the inverse for undo.
    Apply(FieldOp),
    Undo,
    /// The Files object is large enough to show its directory as a treemap and has none at this
    /// size: the caller scans and rasterizes one of `width`×`height` pixels off-thread.
    NeedTreemap {
        path: PathBuf,
        width: u32,
        height: u32,
    },
    /// One pointer drop happened — the throw experiment's measurements
    /// (`docs/research/throw-spike.md`). Emitted before the `Apply` it led to.
    DropMetrics(DropMetrics),
}

/// What one drop measured. Logged by the app only while the experiment is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropMetrics {
    pub thrown: bool,
    /// Lift to release: the time-to-target.
    pub lifted_for_ms: u64,
    /// The drop named a semantic operation (group, add to project…) rather than placing.
    pub semantic: bool,
    /// The same tile was dropped less than three seconds ago: a retry.
    pub relift: bool,
    /// The operation's label, for the log.
    pub op: &'static str,
}

/// Everything the screen reads but does not own.
#[derive(Debug)]
pub struct FieldData<'a> {
    pub field: &'a FieldDocument,
    pub toplevels: &'a [ShellToplevel],
    /// App icons by canonical app id.
    pub icons: &'a HashMap<String, Bitmap>,
    /// Window previews by session identifier (phase 6; empty until then).
    pub previews: &'a HashMap<String, Bitmap>,
    pub drawer_feeds: &'a HashMap<Drawer, crate::drawers::DrawerFeed>,
    /// The rasterized cushion treemap (`templar_treemap`) of the Files object's current directory,
    /// at whatever size was last requested, and that directory's composition: `(0x00RRGGBB,
    /// share)` per file category, largest first.
    ///
    /// One directory, not eight: there is one Files object now, and what it is showing is the
    /// only directory the field has an opinion about.
    pub treemap: Option<&'a Bitmap>,
    pub composition: &'a [(u32, f32)],
    pub browser: &'a FileBrowserData<'a>,
    /// Each open folder window's directory listing, keyed by the directory it lists. One entry per
    /// window, unlike `browser.listing`, which is the single Files object's one location.
    pub folder_listings: &'a HashMap<PathBuf, Result<DirectoryListing, String>>,
    /// The notifications widget's structured rows (`widgets::notifications`).
    pub notifications: &'a [crate::surface::widgets::notifications::NotificationRow],
    pub connected: bool,
    pub home_revealed: bool,
    /// Every installed application (`AppIdentity1.ListApplications`): one of the omnibox's
    /// sources. Empty is ambiguous on its own, hence `apps_available`.
    pub apps: &'a [crate::app_search::AppEntry],
    /// The copies of each logical application that has more than one, keyed by logical id. Empty
    /// for the overwhelming majority: one application, one copy, no choice to offer.
    pub variants: &'a HashMap<String, Vec<(String, String, String)>>,
    /// Every project `Projects1` knows, and every declared action — the omnibox's other two
    /// in-memory sources.
    pub projects: &'a [field_model::ProjectInfo],
    /// Homegarden's places by root: the roles row and branch body a grown folder draws.
    pub garden: &'a HashMap<PathBuf, crate::garden::GardenPlace>,
    pub actions: &'static [templar_types::action::ActionDecl],
    /// The last file search that completed, with the query it answered. The omnibox compares
    /// that query against the one on screen and withholds a stale answer rather than showing it.
    pub file_search: Option<crate::omnibox::FileResults<'a>>,
    /// Whether the application registry answered. A launcher that cannot reach it says so rather
    /// than showing an empty list, which reads as "nothing is installed".
    pub apps_available: bool,
    /// Recently launched applications, newest first, already resolved against `apps`. Empty on a
    /// cold session; the strip under search is then simply not drawn.
    pub recents: &'a [&'a crate::app_search::AppEntry],
    /// The same list as ids, newest first, straight from [`crate::recents::RecentApps::ids`] —
    /// what the launcher ranks with. Separate from `recents` because that one is resolved against
    /// the registry and this one is not: an application that was uninstalled still says something
    /// about the order you reach for the ones that are left.
    pub recent_ids: &'a [String],
    /// Each widget's current content, keyed by `TileKind::System`'s probe. Absent means the
    /// first fetch has not landed; see `crate::surface::widgets` for why a widget that cannot
    /// reach its service reports that rather than showing a zero.
    pub widgets: &'a HashMap<String, WidgetSnapshot>,
    /// Preformatted local date/time supplied by the shell host.
    /// Preserve state marks but suppress the lifted tile's expansion/projection motion.
    pub reduced_motion: bool,
    /// Monotonic milliseconds, for the drag's velocity.
    pub now_ms: u64,
}

/// The ghost shown while a tile is lifted: where it would land, and who would move.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ghost {
    placement: Placement,
    moved: Vec<(TileId, Placement)>,
    label: Option<&'static str>,
    label_at: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    text: String,
    expires_at_ms: u64,
}

#[derive(Debug)]
pub struct FieldScreen {
    pub drag: Drag,
    pub drag_cfg: DragConfig,
    /// After Escape, the held pointer must be released before another gesture can start.
    pointer_cancelled: bool,
    /// The tile a drag out of the search results would create — an application, or a folder for
    /// a typed path. Held here and nowhere else until the drop: nothing is placed until release,
    /// so a cancelled drag has nothing to undo.
    pending: Option<Tile>,
    /// The result row pressed and not yet released. A row's click is delivered on release, not on
    /// the press `pixelkit_shell::Input` registers it at, so that a press can become a drag.
    result_press: Option<usize>,
    pub focus: Option<TileId>,
    /// Tiles affected by the next bulk operation. Focus remains a separate keyboard cursor.
    pub selection: Selection<TileId>,
    /// A background drag selects every tile intersecting this pixel rectangle.
    marquee: Option<Marquee>,
    /// Space on a focused tile: it is lifted by keyboard, and the arrows move this landing.
    pub keyboard_move: Option<(TileId, Cell)>,
    /// A pointer resize in progress: the tile, and the span the pointer is currently asking
    /// for. What the field would actually do with that request is not stored — it is dry-run
    /// through `templar_field::apply` every frame, so the preview cannot drift from the drop.
    pub resize: Option<(TileId, Span)>,
    /// `r` on a focused tile: resized by keyboard, arrows growing and shrinking the request.
    pub keyboard_resize: Option<(TileId, Span)>,
    /// Whether the Files object is expanded over the field. Where it is expanded *to* is
    /// `files`, which outlives every collapse — that is what makes reopening it continuous.
    pub files_expanded: bool,
    /// The expansion in flight, if the object is between its tile and the field right now.
    pub expansion: Option<Expansion>,
    /// The reveal state the last frame was drawn under, so a bare Super press — a change of it
    /// in either direction — can close the expanded Files object.
    last_home_revealed: Option<bool>,
    /// An expansion asked for mid-frame, started once this frame's layout is known — the tile it
    /// grows out of is only knowable after the field has been laid out.
    pending_expand: Option<u64>,
    /// The Files object's one state, at every rung.
    pub files: crate::files_surface::FilesSurfaceState,
    /// A project zoomed to fill the field: its interior grid is what is drawn and edited, with
    /// a breadcrumb back. The document is untouched by zoom — it is a view, never a hidden
    /// desktop (`@templaros.decision.home-field-stable-bounded-semantic-grammar`).
    pub zoomed: Option<TileId>,
    pub files_view: FilesBrowseView,
    pub search: TextFieldState,
    pub search_focused: bool,
    /// The omnibox dropdown's cursor, and what this frame's search bar decided it should show.
    /// The rows are computed with the bar and painted after the tiles, so the list sits over the
    /// field rather than under it.
    pub omnibox: crate::surface::omnibox_screen::OmniboxState,
    omnibox_rows: Vec<crate::omnibox::Row>,
    omnibox_anchor: Rect,
    omnibox_files: crate::omnibox::FileState,
    omnibox_committed: Option<usize>,
    /// The query the file service was last asked for, so a redraw does not ask again.
    asked_for: String,
    /// Manifest-driven Home help. F1 and the visible top-row affordance both toggle it.
    pub help_open: bool,
    help_scroll: pixelkit_ui::ScrollState,
    /// The field's pixel rectangle from the last frame, for tests and hit-testing.
    pub bounds: Rect,
    /// The last pointer drop, for the experiment's retry measure.
    last_drop: Option<(TileId, u64)>,
    /// The released frame: one plain-language confirmation with its undo route.
    notice: Option<Notice>,
    /// A destructive widget region awaiting its second press. Cleared whenever the pointer
    /// leaves the field or Escape is pressed, so an armed Shut down never survives to catch a
    /// later, unrelated click.
    confirming: Option<&'static str>,
    /// The secondary-click menu (`crate::surface::context_menu`): the pointer route to the
    /// window and field actions that otherwise had only a chord.
    pub menu: ContextMenu,
    /// The open "name this group" question, if there is one.
    rename_group: Option<GroupRename>,
    /// Where each folder window has navigated to, keyed by its tile. Absent means the tile is
    /// showing the directory it was opened at — the tile's own path is the window's home, and
    /// navigating never rewrites the document, so a restart reopens where the window was placed
    /// rather than wherever it was last browsed to.
    folder_at: HashMap<TileId, PathBuf>,
    /// Each folder window's scroll position, keyed by its tile.
    folder_scroll: HashMap<TileId, pixelkit_ui::ScrollState>,
}

/// Naming a window group: which group (by one of its members, which is how every group request is
/// addressed) and what has been typed so far.
///
/// A small panel rather than an inline edit on the tile, for the same reason the Files object's
/// rename is one: the tile is a live thing whose title the compositor may change under the caret,
/// and a refusal needs somewhere to be said.
#[derive(Debug)]
struct GroupRename {
    window: WindowId,
    field: TextFieldState,
}

impl Default for FieldScreen {
    fn default() -> Self {
        Self {
            drag: Drag::Idle,
            drag_cfg: DragConfig::default(),
            pointer_cancelled: false,
            pending: None,
            result_press: None,
            focus: None,
            selection: Selection::default(),
            marquee: None,
            keyboard_move: None,
            resize: None,
            keyboard_resize: None,
            files_expanded: false,
            expansion: None,
            last_home_revealed: None,
            pending_expand: None,
            files: crate::files_surface::FilesSurfaceState::default(),
            zoomed: None,
            files_view: FilesBrowseView::default(),
            search: TextFieldState::new(),
            search_focused: false,
            omnibox: crate::surface::omnibox_screen::OmniboxState::default(),
            omnibox_rows: Vec::new(),
            omnibox_anchor: Rect::new(0, 0, 0, 0),
            omnibox_files: crate::omnibox::FileState::Idle,
            omnibox_committed: None,
            asked_for: String::new(),
            help_open: false,
            help_scroll: pixelkit_ui::ScrollState::default(),
            bounds: Rect::new(0, 0, 0, 0),
            last_drop: None,
            notice: None,
            confirming: None,
            menu: ContextMenu::default(),
            rename_group: None,
            folder_at: HashMap::new(),
            folder_scroll: HashMap::new(),
        }
    }
}

const CRUMB_HEIGHT: i32 = 30;
/// How long the Files object takes to grow into the field, or to fall back into its tile.
///
/// Short enough that nobody waits for it and long enough that the eye follows the object rather
/// than being handed a new screen. `reduced_motion` sets it to zero, which is not a shorter
/// animation but no animation at all: the surface is simply there.
const EXPAND_MS: u64 = 140;

/// The Files object on its way between its tile and the whole field.
///
/// Expansion is spatial: the same object gets larger, in the place it already was, rather than a
/// different surface appearing over the top of it. That is the whole of why this exists — a tile
/// that vanishes and a pane that appears are two objects, and the field has one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expansion {
    /// Where the object started: its tile, or the whole field when it is on the way back.
    pub from: Rect,
    pub started_ms: u64,
    pub collapsing: bool,
}

impl Expansion {
    /// How far along, from 0 at the first frame to 1 when it is over. Eased with a smoothstep,
    /// which is monotone on [0, 1] — the object never doubles back on itself.
    pub fn progress(self, now_ms: u64, duration_ms: u64) -> f32 {
        if duration_ms == 0 {
            return 1.0;
        }
        let elapsed = now_ms.saturating_sub(self.started_ms);
        if elapsed >= duration_ms {
            return 1.0;
        }
        // `elapsed < duration_ms` here, so the ratio is in [0, 1).
        let linear = elapsed as f32 / duration_ms as f32;
        linear * linear * (3.0 - 2.0 * linear)
    }

    pub fn finished(self, now_ms: u64, duration_ms: u64) -> bool {
        duration_ms == 0 || now_ms.saturating_sub(self.started_ms) >= duration_ms
    }

    /// Where the object is right now, between [`Self::from`] and `to`. Exact at both ends: the
    /// first frame is the tile it left and the last is the rectangle it was going to, with no
    /// rounding drift to leave a one-pixel jump at the end.
    pub fn rect(self, to: Rect, now_ms: u64, duration_ms: u64) -> Rect {
        let t = self.progress(now_ms, duration_ms);
        if t <= 0.0 {
            return self.from;
        }
        if t >= 1.0 {
            return to;
        }
        let lerp = |a: i32, b: i32| a + ((f64::from(b - a)) * f64::from(t)).round() as i32;
        Rect::new(
            lerp(self.from.x, to.x),
            lerp(self.from.y, to.y),
            lerp(self.from.w, to.w),
            lerp(self.from.h, to.h),
        )
    }
}

const NOTICE_LIFETIME_MS: u64 = 4_000;
/// A failure stays up longer than a confirmation: it is the only account of what happened, and a
/// person who looks up a moment late has missed it for good.
const FAILURE_NOTICE_LIFETIME_MS: u64 = 8_000;
/// Logical-pixel side of the resize grip's square.
const GRIP_SIZE: i32 = 18;

impl FieldScreen {
    /// Whether the caller should keep redrawing on a timer: only while something is lifted.
    pub fn animating(&self) -> bool {
        self.drag.is_lifted() || self.expansion.is_some()
    }

    /// Confirm one applied operation. The caller invokes this only after `templar_field::apply`
    /// succeeds, so the notice never promises a change that was refused.
    pub fn confirm(&mut self, operation: &FieldOp, now_ms: u64) {
        self.notice = Some(Notice {
            text: operation.label().to_owned(),
            expires_at_ms: now_ms.saturating_add(NOTICE_LIFETIME_MS),
        });
    }

    /// Report that something the user asked for did not happen.
    ///
    /// The same strip as [`Self::confirm`], and deliberately so — a person watches one place to
    /// learn what came of a click. It stays up longer than a confirmation because a failure is
    /// worth reading twice, and unlike a confirmation it is the *only* evidence: the alternative,
    /// which is what the shell did until now, is a `tracing::error!` in a journal nobody is
    /// tailing and a button that appears to do nothing at all.
    pub fn report(&mut self, text: impl Into<String>, now_ms: u64) {
        self.notice = Some(Notice {
            text: text.into(),
            expires_at_ms: now_ms.saturating_add(FAILURE_NOTICE_LIFETIME_MS),
        });
    }

    /// The pixel rectangle of `tile` as last drawn: on the top level, its own rect (a child
    /// inside a project is drawn nested within it, `templar_field::tile_rect`); zoomed into a
    /// project, only that project's children have rects, laid out on the interior grid.
    pub fn tile_rect(&self, doc: &FieldDocument, tile: TileId, ui: &Ui<'_>) -> Option<Rect> {
        self.placed_rect(doc, tile, ui)
            .map(|rect| nudge_inside(rect, self.bounds))
    }

    fn placed_rect(&self, doc: &FieldDocument, tile: TileId, ui: &Ui<'_>) -> Option<Rect> {
        match self.zoomed {
            None => tile_rect(doc, to_pixel(self.bounds), tile, ui.px(GAP)).map(from_pixel),
            Some(project) => {
                let located = doc.location(tile)?;
                if located.container != Some(project) {
                    return None;
                }
                let (cols, rows) = doc.grid_size(Some(project))?;
                Some(from_pixel(templar_field::cell_rect(
                    cols,
                    rows,
                    to_pixel(self.bounds),
                    located.placement.cell,
                    located.placement.span,
                    ui.px(GAP),
                )))
            }
        }
    }

    /// The grid being drawn and edited: the field, or the zoomed project's interior.
    fn grid_dims(&self, doc: &FieldDocument) -> (u16, u16) {
        doc.grid_size(self.zoomed).unwrap_or((doc.cols, doc.rows))
    }

    /// Every tile on the current grid with its rect, in reading order.
    fn current_rects(&self, doc: &FieldDocument, ui: &Ui<'_>) -> Vec<(TileId, Rect)> {
        let mut rects: Vec<(TileId, Rect)> = doc
            .placements_in(self.zoomed)
            .into_iter()
            .filter_map(|(id, _)| self.tile_rect(doc, id, ui).map(|rect| (id, rect)))
            .collect();
        rects.sort_by_key(|(_, rect)| (rect.y, rect.x));
        rects
    }

    /// The grab band at the top of a tile's rect.
    /// The resize grip: the tile's bottom-right corner. Growth is anchored at the top-left, so
    /// this is the one corner that can express the whole gesture — and it is the corner furthest
    /// from [`band`](Self::band), the move strip, so the two can never be grabbed by accident.
    pub fn grip(rect: Rect, ui: &Ui<'_>) -> Rect {
        let size = ui
            .px(GRIP_SIZE)
            .min((rect.w / 3).max(1))
            .min((rect.h / 3).max(1));
        Rect::new(
            rect.right() - size,
            rect.bottom() - size,
            size.max(1),
            size.max(1),
        )
    }

    /// Whether this tile can be resized at all. A tile whose floor is its ceiling has no grip:
    /// showing one that refuses every drag would be a lie.
    fn resizable(tile: &Tile) -> bool {
        tile.spans.min != tile.spans.max
    }

    /// Every tile drags by its band; an object-like tile (a file, a folder, a pane, an app —
    /// the things every desktop taught people to drag from anywhere) drags by its whole body
    /// too. Instrument tiles (widgets, drawers, parked sessions, projects) keep the band-only
    /// grab so a press on their controls can never lift the tile out from under its own button.
    /// A press that never crosses the lift threshold is still delivered as the click it was, so
    /// opening things is unchanged.
    fn body_draggable(doc: &FieldDocument, id: TileId) -> bool {
        doc.tile(id).is_some_and(|tile| {
            matches!(
                tile.kind,
                TileKind::File { .. }
                    | TileKind::Folder { .. }
                    | TileKind::Tray { .. }
                    | TileKind::App { .. }
            )
        })
    }

    /// The span the pointer at `(x, y)` is asking a tile anchored at its current cell to become,
    /// clamped to the tile's own declared range. `None` when the pointer is off the grid.
    fn span_under_pointer(
        &self,
        doc: &FieldDocument,
        ui: &Ui<'_>,
        tile: TileId,
        x: i32,
        y: i32,
    ) -> Option<Span> {
        let located = doc.location(tile)?;
        let spans = doc.tile(tile)?.spans;
        let (cols, rows) = self.grid_dims(doc);
        let cell = cell_at(cols, rows, to_pixel(self.bounds), x, y, ui.px(GAP))?;
        let origin = located.placement.cell;
        let wanted = Span::new(
            cell.col.saturating_sub(origin.col).saturating_add(1),
            cell.row.saturating_sub(origin.row).saturating_add(1),
        );
        Some(wanted.clamp(spans.min, spans.max))
    }

    /// The document as it would be if the pending resize were released now.
    ///
    /// This runs the *real* op rather than reimplementing the rules, so what the ghost promises
    /// and what the drop delivers cannot disagree — including the clamp, which shows up here as
    /// a preview that simply stops growing while the pointer keeps moving.
    fn resize_preview(doc: &FieldDocument, tile: TileId, span: Span) -> Option<FieldDocument> {
        apply(doc, &FieldOp::Resize { id: tile, span })
            .ok()
            .map(|(next, _)| next)
    }

    /// The screen rectangle of an arbitrary placement on the grid currently being edited.
    fn rect_of(&self, doc: &FieldDocument, placement: Placement, ui: &Ui<'_>) -> Rect {
        let (cols, rows) = self.grid_dims(doc);
        from_pixel(templar_field::cell_rect(
            cols,
            rows,
            to_pixel(self.bounds),
            placement.cell,
            placement.span,
            ui.px(GAP),
        ))
    }

    /// How a tile is grabbed, and what that costs its body — one answer, consulted by the
    /// painter and by the pointer, so the strip a person aims at and the strip that reacts can
    /// never be two different rectangles.
    ///
    /// Above the compact rungs a tile has a heading: its name and the controls that act on the
    /// whole of it, in a band the body does not get. At Micro and Small it has none. A tile that
    /// small spending a third of itself on a band containing its own name was the worst trade on
    /// the field; instead the top edge stays grabbable without being *taken*, the name is drawn
    /// as the first line of the body, and the controls appear only when the tile is hovered or
    /// focused. Identity is never hidden — only manipulation.
    pub fn grab_strip(rect: Rect, rung: Representation, ui: &Ui<'_>) -> GrabStrip {
        // Never into the bottom third, whatever the tile's height: that is where the resize grip
        // lives, and two gestures sharing a pixel is the one thing neither of them survives.
        let ceiling = (rect.h * 2 / 3).max(0);
        if rung >= Representation::Wide {
            let (heading, body) = rect.split_top(ui.px(design::HEADING_HEIGHT).min(ceiling));
            return GrabStrip {
                grab: heading,
                heading: Some(heading),
                body,
            };
        }
        let grab = rect
            .split_top(ui.px(design::EDGE_GRAB_HEIGHT).min(ceiling))
            .0;
        GrabStrip {
            grab,
            heading: None,
            body: rect,
        }
    }

    /// Where a tile's name and its controls go, given how it is grabbed.
    ///
    /// The companion to [`Self::grab_strip`], and split out for the same reason: the painter, the
    /// hit-test and a test all need to agree about where a control is, and geometry computed
    /// inline inside a draw call can only be checked by drawing. With a heading the name and the
    /// controls share it. Without one — Micro and Small — the name is the body's first line and
    /// the controls wait for a hover or a keyboard focus, then appear in a strip over the top of
    /// the body. Identity is never hidden; only manipulation is.
    pub fn control_rects(
        strip: GrabStrip,
        rect: Rect,
        controls_width: i32,
        revealed: bool,
        ui: &Ui<'_>,
    ) -> TileChrome {
        if let Some(heading) = strip.heading {
            // `split_right` hands back the right strip first.
            let (controls, title) = heading.split_right(ui.px(controls_width).min(heading.w / 2));
            return TileChrome {
                reveal: None,
                controls,
                title,
                content: strip.body,
            };
        }
        // The name is the first line of the body, not a band over it: the tile keeps saying what
        // it is at every size, and pays one line rather than a third of itself for the privilege.
        let line = ui.px(design::COMPACT_TITLE_HEIGHT).min(strip.body.h);
        let (title, content) = strip.body.split_top(line + ui.px(4));
        if !revealed || controls_width == 0 {
            return TileChrome {
                reveal: None,
                controls: Rect::new(rect.right(), rect.y, 0, 0),
                title,
                content,
            };
        }
        let reveal = Rect::new(
            rect.x,
            rect.y,
            rect.w,
            ui.px(design::REVEAL_HEIGHT).min(rect.h),
        );
        let controls = reveal
            .split_right(ui.px(controls_width).min(reveal.w / 2))
            .0;
        TileChrome {
            reveal: Some(reveal),
            controls,
            // The name gives up the width the controls took rather than being drawn under them.
            // Identity is never hidden, so what a revealed tile shows is a shorter name, not a
            // name with three buttons on top of it.
            title: Rect::new(title.x, title.y, (controls.x - title.x).max(0), title.h),
            content,
        }
    }

    /// The same, for a tile the document has placed.
    pub fn strip_of(doc: &FieldDocument, tile: &Tile, rect: Rect, ui: &Ui<'_>) -> GrabStrip {
        Self::grab_strip(rect, representation_of(doc, tile), ui)
    }

    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect, data: &FieldData<'_>) -> Vec<FieldAction> {
        // The whole buffer, not just `area`. Home's buffer is never cleared between frames
        // (`HostedWindow::prepare` keeps it, and the window's size does not change), and `area`
        // is the output *minus* the bar's strip — but only once the bar exists. The shell draws
        // several frames before that, at the full height of the output, with tiles laid out down
        // to the bottom edge; from then on nothing ever wrote to those last 48 rows again, so
        // they kept the bottoms of those tiles — a row of widget controls, body, region strip and
        // border, sitting under the clock for the rest of the session. That is the second half of
        // "the bottom left is a mess", and it is the same failure as the bar's own: a surface that
        // paints only part of itself keeps whatever was there before.
        ui.painter.clear(ui.theme.background);
        let mut actions = Vec::new();
        if self.handle_help_mode(ui, area) {
            return actions;
        }

        self.expire_notice(data.now_ms);

        let inset = area.inset(ui.px(design::FIELD_INSET));
        let recents_visible = self.recents_visible(data);
        let chrome_h = ui.px(design::TOP_BAR_HEIGHT)
            + if recents_visible {
                ui.px(design::TOP_BAR_GAP) + ui.px(design::RECENTS_HEIGHT)
            } else {
                0
            };
        let (chrome, below_bar) = inset.split_top(chrome_h.min(inset.h));
        self.draw_search_bar(ui, chrome, recents_visible, data, &mut actions);
        if self.help_open {
            self.draw_help_overlay(ui, area);
            return actions;
        }
        let gap = ui.px(design::TOP_BAR_GAP).min(below_bar.h);
        let rest = Rect::new(
            below_bar.x,
            below_bar.y + gap,
            below_bar.w,
            (below_bar.h - gap).max(0),
        );

        self.bounds = rest;
        // A bare Super press is "show me Home" or "give me my windows back", and the expanded
        // Files object is neither: it stays put through a reveal in either direction, which on
        // real hardware read as Super hiding every window except the file manager. So a change
        // of reveal state closes the object; where it was is kept, and opening it again lands
        // there.
        if self
            .last_home_revealed
            .is_some_and(|was| was != data.home_revealed)
        {
            self.collapse_files(data.now_ms);
        }
        self.last_home_revealed = Some(data.home_revealed);
        // An expansion asked for mid-frame starts now, when the field's layout — and so the
        // tile it grows out of — is finally known.
        if let Some(at) = self.pending_expand.take() {
            self.expand_files(data.field, ui, at);
        }
        if self.files_expanded {
            let duration = Self::expand_ms(data);
            match self.expansion {
                // Still on the way. The object is drawn as itself at the size it currently is —
                // the rung ladder does the cross-fade, so a small rectangle gets the Wide body
                // and a large one the Large body, exactly as a tile of that size would.
                Some(expansion) if !expansion.finished(data.now_ms, duration) => {
                    let to = if expansion.collapsing {
                        self.collapse_target(data.field, expansion.from, ui)
                    } else {
                        files_window_rect(rest, ui)
                    };
                    Self::draw_growing(ui, expansion.rect(to, data.now_ms, duration), data);
                    self.draw_notice(ui, rest);
                    return actions;
                }
                Some(expansion) => {
                    self.expansion = None;
                    if expansion.collapsing {
                        self.files_expanded = false;
                    }
                }
                None => {}
            }
        }
        if self.files_expanded {
            let window = files_window_rect(rest, ui);
            // The ground around the window stays the field, dimmed: the object is a window over
            // Home, not a mode Home enters. Pressing that ground puts it away, which is the same
            // gesture as clicking off any other window and is why it is dimmed rather than hidden.
            if window != rest {
                ui.painter.blend_rect(rest, design::ground(), 150);
                if ui
                    .input
                    .cursor()
                    .is_some_and(|(x, y)| !window.contains(x, y))
                    && ui.input.take_click(rest)
                {
                    self.collapse_files(data.now_ms);
                }
                design::shell(
                    ui,
                    window,
                    design::shell_fill(),
                    design::edge_strong(),
                    Elevation::Lifted,
                );
            }
            self.draw_expanded(ui, window, data, &mut actions);
            self.draw_notice(ui, rest);
            self.draw_omnibox(ui, data, &mut actions);
            return actions;
        }

        let doc = data.field;
        if self.zoomed.is_some_and(|id| doc.container(id).is_none()) {
            self.zoomed = None;
        }
        let rest = if let Some(project) = self.zoomed {
            let (crumb, body) = rest.split_top(ui.px(CRUMB_HEIGHT));
            let name = doc
                .tile(project)
                .map_or("project", |tile| tile.title.as_str());
            let style = TextStyle::new(ui.theme.body.face, 13.0);
            ui.label_styled(
                crumb.inset(ui.px(6)),
                &format!("Home  ▸  {name}"),
                style,
                ui.theme.text_dim,
                Align::Left,
            );
            ui.painter.stroke_rect(body, ui.px(2), ui.theme.accent_soft);
            body
        } else {
            rest
        };
        self.bounds = rest;
        let visible: Vec<TileId> = self
            .current_rects(doc, ui)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        self.selection.retain(|id| visible.contains(id));
        if self
            .focus
            .is_some_and(|id| doc.location(id).is_none_or(|l| l.container != self.zoomed))
        {
            self.focus = None;
        }
        let asking = self.update_rename_group(ui, rest, &mut actions);
        if !asking && !self.handle_menu(ui, doc, data, rest, &mut actions) {
            self.handle_keys(ui, doc, data, &mut actions);
            self.handle_pointer(ui, doc, data, &mut actions);
            self.open_menu_on_secondary_click(ui, doc, data, rest);
        }

        let ghost = self.ghost(doc, ui);
        let lifted = self.drag.tile().filter(|_| self.drag.is_lifted());
        let rects = self.current_rects(doc, ui);
        for (id, rect) in &rects {
            if Some(*id) == lifted {
                continue;
            }
            let Some(tile) = doc.tile(*id) else { continue };
            let visual = TileVisual {
                hovered: ui.input.hovering(*rect),
                focused: self.focus == Some(*id),
                selected: self.selection.contains(id),
                lifted: false,
                displaced: ghost
                    .as_ref()
                    .and_then(|g| g.moved.iter().find(|(m, _)| m == id))
                    .map(|(_, to)| to.cell),
            };
            self.paint_tile(ui, *rect, tile, visual, data, &mut actions);
        }

        self.draw_overlays(ui, doc, data, ghost.as_ref(), lifted, &mut actions);
        if let Some(marquee) = self.marquee {
            design::dashed_rect(ui, marquee.rect(), design::accent());
        }
        self.draw_resize_preview(ui, doc);
        self.draw_notice(ui, rest);
        self.menu.paint(ui, rest);
        self.paint_rename_group(ui, rest);

        if rects.is_empty() {
            self.draw_empty_state(ui, rest, data.connected);
        }
        self.draw_omnibox(ui, data, &mut actions);
        Self::occlude_windows(ui, area, data);
        actions
    }

    /// Paint flat ground where a window is, so nothing of Home shows through a translucent one.
    ///
    /// The Home Field is the backdrop, so a client with an alpha channel — a terminal with
    /// `background_opacity` under 1 is the ordinary case — composites over whatever the field
    /// last drew there. What came through was tiles, text and live window previews, and a preview
    /// of the very terminal you were looking through it was the worst of them (the fifth hands-on
    /// run). Behind a window there should be the windows behind it and then the ground, and
    /// nothing else: the compositor still composites those, because this fills only the field's
    /// own surface, not theirs.
    ///
    /// Nothing is hidden that anyone can see: these rectangles are exactly the ones a window is
    /// covering. Home in the gaps between windows is untouched, and a revealed Home is in front
    /// of every window, so it is not occluded at all.
    ///
    /// Window geometry is in the compositor's one global logical space and the field's surface is
    /// one output, so this is right for the single-output case and conservative for any other: a
    /// rectangle that does not intersect the surface fills nothing.
    fn occlude_windows(ui: &mut Ui<'_>, area: Rect, data: &FieldData<'_>) {
        if data.home_revealed {
            return;
        }
        for window in data.toplevels {
            if window.shelved || window.layer != templar_window::Layer::Normal {
                continue;
            }
            let geometry = window.geometry;
            if geometry.width <= 0 || geometry.height <= 0 {
                continue;
            }
            let rect = Rect::new(geometry.x, geometry.y, geometry.width, geometry.height);
            let (left, top) = (rect.x.max(area.x), rect.y.max(area.y));
            let (right, bottom) = (
                rect.right().min(area.right()),
                rect.bottom().min(area.bottom()),
            );
            if right > left && bottom > top {
                ui.painter.fill_rect(
                    Rect::new(left, top, right - left, bottom - top),
                    design::ground(),
                );
            }
        }
    }

    fn draw_empty_state(&self, ui: &mut Ui<'_>, area: Rect, connected: bool) {
        let message = if self.zoomed.is_some() {
            "Empty project — drop tiles onto its tile on the field to fill it"
        } else if connected {
            "Search to launch, or open Files"
        } else {
            "Home is not connected to the compositor"
        };
        ui.label(
            area.inset(ui.px(12)),
            message,
            ui.theme.text_dim,
            Align::Left,
        );
    }

    fn expire_notice(&mut self, now_ms: u64) {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| now_ms >= notice.expires_at_ms)
        {
            self.notice = None;
        }
    }

    /// The pending resize, dry-run and drawn: where the tile would end up, and which neighbours
    /// would give up how much to let it.
    ///
    /// The clamp needs no special case here. When the request outruns what the neighbours can
    /// give, `templar_field::apply` returns the largest span that works, the ghost stops growing,
    /// and the pointer visibly runs ahead of it — which is the whole feedback the user needs to
    /// know they have hit the limit rather than that the drag has broken.
    fn draw_resize_preview(&self, ui: &mut Ui<'_>, doc: &FieldDocument) {
        let Some((tile, span)) = self.resize.or(self.keyboard_resize) else {
            return;
        };
        let Some(preview) = Self::resize_preview(doc, tile, span) else {
            return;
        };
        for (id, was) in &doc.located {
            let Some(now) = preview.location(*id) else {
                continue;
            };
            if now == *was || was.container != self.zoomed {
                continue;
            }
            let rect = self.rect_of(doc, now.placement, ui);
            if *id == tile {
                ui.painter.blend_rect(rect, design::accent(), 30);
                design::dashed_rect(ui, rect, design::accent());
            } else {
                // A yielding neighbour: shown at the size it would become, so the cost of the
                // growth is visible before it is paid for.
                ui.painter.blend_rect(rect, design::ground(), 100);
                design::dashed_rect(ui, rect, design::edge_strong());
            }
        }
    }

    fn draw_notice(&self, ui: &mut Ui<'_>, area: Rect) {
        let Some(notice) = &self.notice else {
            return;
        };
        let width = ui.px(430).min((area.w - ui.px(24)).max(0));
        if width <= 0 {
            return;
        }
        let rect = Rect::new(
            area.x + (area.w - width) / 2,
            area.bottom() - ui.px(54),
            width,
            ui.px(42),
        );
        ui.painter
            .shadow_rect(rect, ui.px(4), ui.px(6), design::text(), 80);
        ui.painter
            .fill_rounded_rect(rect, ui.px(design::WELL_RADIUS), design::text());
        let (undo, message) = rect.split_right(ui.px(82));
        ui.label_styled(
            message.inset(ui.px(12)),
            &notice.text,
            design::title_style(ui),
            0x00f2_f4f6,
            Align::Left,
        );
        ui.label_styled(
            undo.inset(ui.px(8)),
            "CTRL+Z",
            TextStyle::tracked(ui.theme.body.face, 11.0, 0.12),
            0x009a_a4ad,
            Align::Centre,
        );
    }

    /// The ghost (where a lifted tile would land, who would move, the pending operation's
    /// label) and the lifted tile itself, drawn over everything else.
    fn draw_overlays(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        ghost: Option<&Ghost>,
        lifted: Option<TileId>,
        actions: &mut Vec<FieldAction>,
    ) {
        let (cols, rows) = self.grid_dims(doc);
        if let Some(id) = lifted
            && let Some(origin) = self.tile_rect(doc, id, ui)
        {
            ui.painter
                .fill_rounded_rect(origin, ui.px(design::TILE_RADIUS), design::recessed());
            design::dashed_rect(ui, origin, design::recessed_edge());
        }
        if let Some(ghost) = ghost {
            let ghost_rect = from_pixel(templar_field::cell_rect(
                cols,
                rows,
                to_pixel(self.bounds),
                ghost.placement.cell,
                ghost.placement.span,
                ui.px(GAP),
            ));
            ui.painter.blend_rect(ghost_rect, design::accent(), 24);
            design::dashed_rect(ui, ghost_rect, design::accent());
            for (id, to) in &ghost.moved {
                let Some(located) = doc.location(*id) else {
                    continue;
                };
                let preview = from_pixel(templar_field::cell_rect(
                    cols,
                    rows,
                    to_pixel(self.bounds),
                    to.cell,
                    located.placement.span,
                    ui.px(GAP),
                ));
                ui.painter.blend_rect(preview, design::ground(), 110);
                design::dashed_rect(ui, preview, design::edge_strong());
            }
            if let (Some(label), Some(at)) = (ghost.label, ghost.label_at) {
                ui.painter.fill_rect(at, design::accent());
                ui.label_styled(
                    at.inset(ui.px(8)),
                    label,
                    TextStyle::tracked(ui.theme.body.face, 11.0, 0.12),
                    design::strong_ink(),
                    Align::Left,
                );
            }
        }

        let pending = self.pending.clone();
        if let (Some(tile), Some((x, y))) = (pending, self.drag.tile_position())
            && self.drag.is_lifted()
        {
            let cell = from_pixel(templar_field::cell_rect(
                cols,
                rows,
                to_pixel(self.bounds),
                Cell::new(0, 0),
                tile.spans.preferred,
                ui.px(GAP),
            ));
            let raised = Rect::new(x as i32, y as i32, cell.w, cell.h);
            let visual = TileVisual {
                hovered: false,
                focused: false,
                selected: false,
                lifted: true,
                displaced: None,
            };
            self.paint_tile(ui, raised, &tile, visual, data, actions);
        }
        if let (Some(id), Some((x, y))) = (lifted, self.drag.tile_position())
            && let (Some(tile), Some(rect)) = (doc.tile(id), self.tile_rect(doc, id, ui))
        {
            let swell = if data.reduced_motion { 0 } else { ui.px(2) };
            let raised = Rect::new(
                x as i32 - swell,
                y as i32 - swell,
                rect.w + swell * 2,
                rect.h + swell * 2,
            );
            let visual = TileVisual {
                hovered: false,
                focused: false,
                selected: self.selection.contains(&id),
                lifted: true,
                displaced: None,
            };
            self.paint_tile(ui, raised, tile, visual, data, actions);
        }
    }

    fn recents_visible(&self, data: &FieldData<'_>) -> bool {
        self.search.text().trim().is_empty() && !self.files_expanded && !data.recents.is_empty()
    }

    #[allow(clippy::too_many_lines)]
    fn draw_search_bar(
        &mut self,
        ui: &mut Ui<'_>,
        chrome: Rect,
        recents_visible: bool,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let gap = ui.px(design::TOP_BAR_GAP);
        let bar = if recents_visible {
            chrome.split_top(ui.px(design::TOP_BAR_HEIGHT)).0
        } else {
            chrome
        };
        // No Home plate and no Help button. A plate labelled "Home" *on* Home said the word for
        // the surface it was already standing on, and the route it offered — reveal the field
        // from under the windows — is the bar's Home chip, which is visible from everywhere and
        // not only from the place it takes you to. Help is a secondary-click row and F1
        // (`context_menu.rs`, `MenuScope::Field`); a permanent button in the corner of every
        // session is furniture for something a person reaches for twice.
        // Centred in the bar, and nothing else is in it. The live/parked summary that used to
        // sit on the right went with the window tiles it counted: the bar's taskbar lists every
        // running window by name, which is more than a number ever said.
        let search_w = ui.px(design::SEARCH_WIDTH).min(bar.w);
        let search_area = Rect::new(bar.x + (bar.w - search_w) / 2, bar.y, search_w, bar.h);
        if ui.input.take_click(search_area) {
            self.search_focused = true;
        }
        // Escape leaves the search and empties it, so the next letter typed starts a new question
        // rather than continuing one asked minutes ago. Read *here*: the focused `text_field`
        // below consumes every key of the frame, so `handle_keys` never sees one while the search
        // has focus — which is why Escape used to do nothing at all.
        if self.search_focused && ui.input.keys().contains(&KeyInput::Escape) {
            self.search_focused = false;
            self.search.set_text("");
        }
        // Scope is derived from where the surface is, never chosen. There is no mode to be in,
        // so there is no mode to get stuck in: on bare Home the field is the application
        // launcher, and inside the expanded Files object it searches that location.
        let scope = self.scope();
        let query = self.search.text().to_owned();
        let rows = crate::omnibox::federate(
            crate::omnibox::Sources {
                apps: data.apps,
                recents: data.recent_ids,
                files: data.file_search,
                home: data.browser.home,
            },
            &query,
            &scope,
            crate::omnibox::TOTAL_CAP,
        );
        // Read before `text_field` consumes the frame's keys: a focused field takes every key,
        // including the ones the dropdown's cursor lives on.
        let committed = self
            .search_focused
            .then(|| self.omnibox.handle_keys(ui, &query, rows.len()))
            .flatten();
        let placeholder = match &scope {
            crate::omnibox::Scope::Location(location) => format!("Search {}", location.label()),
            // It says what to do, because there is now something to do without touching the
            // pointer at all: any letter typed on Home lands here.
            crate::omnibox::Scope::Applications => "Type to open an application".to_owned(),
        };
        let previous_theme = ui.theme;
        ui.theme.background = design::shell_fill();
        ui.theme.padding = 14;
        ui.text_field(
            &mut self.search,
            search_area,
            &placeholder,
            self.search_focused,
            false,
        );
        ui.theme = previous_theme;

        if recents_visible {
            let recents_row = Rect::new(
                search_area.x,
                chrome.y + ui.px(design::TOP_BAR_HEIGHT) + gap,
                search_area.w,
                ui.px(design::RECENTS_HEIGHT).min(
                    (chrome.bottom() - (chrome.y + ui.px(design::TOP_BAR_HEIGHT) + gap)).max(0),
                ),
            );
            Self::draw_recents(ui, recents_row, data, actions);
        }

        // Inside a location, whatever is typed goes to the file service once the typing has
        // stopped. The debounce itself lives in the host, which has the clock; this only says
        // what to ask. The launcher on bare Home has its answer in hand and asks nothing.
        if query.trim() != self.asked_for.trim() {
            self.asked_for.clone_from(&query);
            if matches!(scope, crate::omnibox::Scope::Location(_)) {
                actions.push(FieldAction::RunSearch {
                    scope: scope.clone(),
                    query: query.clone(),
                });
            }
        }

        self.omnibox_rows = rows;
        self.omnibox_anchor = search_area;
        // Only a location has files to be searching for. The launcher had its whole answer in
        // hand and still printed "still searching files…" under it, which is the general search
        // this control is explicitly not.
        self.omnibox_files = match scope {
            crate::omnibox::Scope::Applications => crate::omnibox::FileState::Idle,
            crate::omnibox::Scope::Location(_) => {
                crate::omnibox::file_state(data.file_search, &query)
            }
        };
        self.omnibox_committed = committed;
    }

    /// Recently launched applications, as a row of chips under the search field. The COSMIC
    /// app-library recent list, drawn where the empty search actually is.
    fn draw_recents(
        ui: &mut Ui<'_>,
        area: Rect,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let chip_w = ui.px(design::RECENT_CHIP_WIDTH);
        if chip_w <= 0 || area.h <= 0 {
            return;
        }
        let max = usize::try_from((area.w / chip_w).max(0)).unwrap_or(0);
        let shown: Vec<&crate::app_search::AppEntry> =
            data.recents.iter().copied().take(max).collect();
        if shown.is_empty() {
            return;
        }
        let total = chip_w * i32::try_from(shown.len()).unwrap_or(0);
        let start_x = area.x + (area.w - total) / 2;
        let icon_size = ui
            .px(36)
            .min(chip_w - ui.px(12))
            .min(area.h - ui.px(22))
            .max(8);
        for (index, app) in shown.iter().enumerate() {
            let chip = Rect::new(
                start_x + chip_w * i32::try_from(index).unwrap_or(0),
                area.y,
                chip_w,
                area.h,
            );
            let hovered = ui.input.hovering(chip);
            if hovered {
                ui.painter.fill_rounded_rect(
                    chip.inset(ui.px(2)),
                    ui.px(design::CHIP_RADIUS),
                    design::accent_soft(),
                );
            }
            let icon = Rect::new(
                chip.x + (chip.w - icon_size) / 2,
                chip.y + ui.px(6),
                icon_size,
                icon_size,
            );
            if let Some(bitmap) = data.icons.get(&app.id.to_string()) {
                ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
            } else {
                ui.painter.fill_rounded_rect_aa(
                    ui.kernel,
                    icon,
                    ui.px(design::CHIP_RADIUS) as f32,
                    design::accent_soft(),
                    255,
                );
                let initial = app
                    .label()
                    .chars()
                    .next()
                    .unwrap_or('?')
                    .to_uppercase()
                    .to_string();
                ui.label(icon, &initial, design::accent(), Align::Centre);
            }
            let name = Rect::new(
                chip.x + ui.px(4),
                icon.bottom() + ui.px(2),
                (chip.w - ui.px(8)).max(0),
                ui.px(16).min((chip.bottom() - icon.bottom()).max(0)),
            );
            if name.h > 0 {
                ui.label_styled(
                    name,
                    app.label(),
                    design::state_style(ui),
                    if hovered {
                        design::accent()
                    } else {
                        design::text()
                    },
                    Align::Centre,
                );
            }
            if ui.input.take_click(chip) {
                actions.push(FieldAction::Launch(app.id.clone()));
            }
        }
    }

    /// The dropdown, painted after the field so it is over the tiles rather than under them, and
    /// hit-tested before them — [`Self::omnibox_area`] is what keeps a tile from stealing a click
    /// aimed at a row. The same two-step the context menu uses.
    fn draw_omnibox(
        &mut self,
        ui: &mut Ui<'_>,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        if !self.search_focused {
            return;
        }
        let rows = std::mem::take(&mut self.omnibox_rows);
        let outcome = crate::surface::omnibox_screen::draw(
            ui,
            self.omnibox_anchor,
            self.bounds,
            &rows,
            &mut self.omnibox,
            data.icons,
            self.omnibox_files,
        );
        let chosen = match outcome {
            crate::surface::omnibox_screen::OmniboxOutcome::None => self
                .omnibox_committed
                .and_then(|index| rows.get(index))
                .map(crate::surface::omnibox_screen::commit),
            chose @ crate::surface::omnibox_screen::OmniboxOutcome::Chose(_) => Some(chose),
        };
        if let Some(crate::surface::omnibox_screen::OmniboxOutcome::Chose(hit)) = chosen {
            self.choose(hit, actions);
        }
    }

    /// The rectangle the dropdown occupies right now, if it is open.
    fn omnibox_area(&self, ui: &Ui<'_>) -> Option<Rect> {
        if !self.search_focused {
            return None;
        }
        crate::surface::omnibox_screen::area(
            ui,
            self.omnibox_anchor,
            self.bounds,
            &self.omnibox_rows,
            self.omnibox_files,
        )
    }

    /// Where a collapsing object is heading: the Files tile's rectangle *now*.
    ///
    /// Recomputed every frame rather than remembered at the start of the collapse, because the
    /// field reflows while the object is open — a tile that moved would otherwise be somewhere
    /// the object falls past. `fallback` is where it grew out of, used only when the tile is no
    /// longer placed at all.
    fn collapse_target(&self, doc: &FieldDocument, fallback: Rect, ui: &Ui<'_>) -> Rect {
        self.tile_rect(doc, field_model::files_tile_id(), ui)
            .unwrap_or(fallback)
    }

    /// The Files object mid-flight: its own shell and its own body, at whatever size it has
    /// reached, clipped so nothing spills past the rectangle it currently occupies.
    ///
    /// The rung is asked of the ladder rather than chosen here — the interpolated rectangle is
    /// converted back to a span and put through `Representation::for_span`, so the cross-fade
    /// between the Wide body and the Large one happens at exactly the size it happens at when a
    /// person drags the tile to that size by hand. There is one answer to "how much does this
    /// object show", and growth is not a special case of it.
    fn draw_growing(ui: &mut Ui<'_>, rect: Rect, data: &FieldData<'_>) {
        let cell = ui.px(field_model::TILE_SIZE + GAP).max(1);
        let span = Span::new(
            u16::try_from((rect.w / cell).max(1)).unwrap_or(u16::MAX),
            u16::try_from((rect.h / cell).max(1)).unwrap_or(u16::MAX),
        );
        let kind = TileKind::Widget {
            widget: field_model::FILES_WIDGET.to_owned(),
            scope: None,
        };
        let rung = Representation::for_span(&kind, span);
        design::shell(
            ui,
            rect,
            design::shell_fill(),
            design::edge_strong(),
            Elevation::Lifted,
        );
        ui.painter.push_clip(rect);
        let strip = Self::grab_strip(rect, rung, ui);
        if let Some(heading) = strip.heading {
            ui.label_styled(
                Rect::new(heading.x + ui.px(12), heading.y, heading.w, heading.h),
                "Files",
                design::system_label_style(ui),
                design::text(),
                Align::Left,
            );
        }
        crate::surface::files_screen::draw(
            ui,
            strip.body,
            rung,
            crate::surface::files_screen::FilesData {
                feeds: data.drawer_feeds,
                sources: data.browser.sources,
            },
        );
        ui.painter.pop_clip();
    }

    /// Open the Files object over the field, growing out of wherever its tile is.
    ///
    /// The object does not appear; it gets bigger. If its tile is not on screen — stashed, or on
    /// a field that has not been laid out yet — there is nothing to grow out of and it is simply
    /// there, which is the honest degradation rather than a slide from an invented corner.
    fn expand_files(&mut self, doc: &FieldDocument, ui: &Ui<'_>, now_ms: u64) {
        if self.files_expanded {
            return;
        }
        self.files_expanded = true;
        self.expansion = self
            .tile_rect(doc, field_model::files_tile_id(), ui)
            .map(|from| Expansion {
                from,
                started_ms: now_ms,
                collapsing: false,
            });
    }

    /// Put it back. The same motion in reverse, and the surface stays drawn until it has
    /// finished falling into the tile — a pane that blinks out is a pane that went somewhere
    /// nobody saw.
    fn collapse_files(&mut self, now_ms: u64) {
        if !self.files_expanded {
            return;
        }
        self.expansion = Some(Expansion {
            from: self.bounds,
            started_ms: now_ms,
            collapsing: true,
        });
    }

    /// How long an expansion takes here. `reduced_motion` makes it nothing at all, which is not
    /// a faster animation but no animation: the surface is simply where it was going.
    fn expand_ms(data: &FieldData<'_>) -> u64 {
        if data.reduced_motion { 0 } else { EXPAND_MS }
    }

    /// Where the question is being asked from. Derived, never chosen.
    fn scope(&self) -> crate::omnibox::Scope {
        if self.files_expanded {
            crate::omnibox::Scope::Location(self.files.location().clone())
        } else {
            crate::omnibox::Scope::Applications
        }
    }

    /// Act on one omnibox row.
    fn choose(&mut self, hit: crate::omnibox::Hit, actions: &mut Vec<FieldAction>) {
        use crate::omnibox::Hit;
        self.search_focused = false;
        match hit {
            Hit::App { id } => {
                actions.push(FieldAction::Launch(id));
                self.search.set_text("");
            }
            Hit::File { path } => actions.push(FieldAction::OpenFile(path)),
            // Inside the Files object a folder is somewhere to go; on bare Home it is the file
            // manager opening there, exactly as a Folder tile is. Same row, same hit — the scope
            // decides, exactly as it does for the query itself, so there is still no mode to be in.
            Hit::Directory { path } => {
                if self.files_expanded {
                    self.files
                        .go_to(crate::files_surface::FilesLocation::Directory(path));
                } else {
                    actions.push(FieldAction::OpenInFiles(path));
                }
            }
        }
    }

    /// Toggle/dismiss Help and, while it is open, claim the entire surface for it.
    fn handle_help_mode(&mut self, ui: &mut Ui<'_>, area: Rect) -> bool {
        let f1 = ui.input.keys().contains(&KeyInput::Function(1));
        if f1 {
            self.help_open = !self.help_open;
            let _ = ui.input.take_keys();
        } else if self.help_open && ui.input.keys().contains(&KeyInput::Escape) {
            self.help_open = false;
            let _ = ui.input.take_keys();
        }
        if !self.help_open {
            return false;
        }
        self.menu.close();
        self.draw_help_overlay(ui, area);
        true
    }

    /// The six things Home is for, in the order a person sitting down reaches for them.
    ///
    /// The cheat sheet below this is the complete action list. This is the first-run surface:
    /// what this screen is, and where each first request goes, without having to scan every
    /// declared verb.
    fn draw_help_primer(ui: &mut Ui<'_>, area: Rect) {
        // The last row is a gesture, not an action, which is why it lives here rather than in the
        // manifest below: grouping has no chord of its own on Home, and dropping one window's
        // title bar on another is the only way to learn it. The chords that come *after* a group
        // exists — Alt+Tab inside it, Super+Tab across groups, Super+G and Super+Shift+G — are
        // declared actions and appear in the cheat sheet under Navigation and Window.
        const ROWS: [(&str, &str); 8] = [
            ("Super", "Come back here from any window"),
            ("Apps", "Type an application's name to open it"),
            ("Recents", "Applications you launched, under Apps"),
            ("Files", "Every file on this machine"),
            ("Session", "Lock, log out, or shut down"),
            ("Settings", "Volume, brightness, and the rest"),
            ("Right-click", "More for a tile or the field"),
            ("Drop a title bar", "On another, to group two windows"),
        ];
        design::shell(
            ui,
            area,
            design::recessed(),
            design::edge(),
            Elevation::Rest,
        );
        let inner = area.inset(ui.px(12));
        let heading_h = ui.px(22);
        ui.label_styled(
            Rect::new(inner.x, inner.y, inner.w, heading_h),
            "How Home works",
            TextStyle::tracked(ui.theme.body.face, 11.0, 0.12),
            design::text_faint(),
            Align::Left,
        );
        let body = Rect::new(
            inner.x,
            inner.y + heading_h,
            inner.w,
            (inner.h - heading_h).max(0),
        );
        let row_h = (body.h / ROWS.len() as i32).min(ui.px(20)).max(1);
        let key_w = ui.px(96).min(body.w / 3);
        for (index, (key, intent)) in ROWS.iter().enumerate() {
            let row = Rect::new(body.x, body.y + index as i32 * row_h, body.w, row_h);
            let (key_rect, rest) = row.split_left(key_w);
            ui.label_styled(
                key_rect,
                key,
                design::state_style(ui),
                design::text(),
                Align::Left,
            );
            ui.label_styled(
                rest,
                intent,
                design::state_style(ui),
                design::text_dim(),
                Align::Left,
            );
        }
    }

    /// The action manifest rendered as product help, rather than copied into another list that
    /// can drift. Actions without an accelerator remain visible as deliberate on-screen routes.
    fn draw_help_overlay(&mut self, ui: &mut Ui<'_>, area: Rect) {
        ui.painter.blend_rect(area, design::text(), 105);
        let panel = area.inset(ui.px(24));
        ui.painter
            .shadow_rect(panel, ui.px(5), ui.px(9), design::text(), 90);
        ui.painter
            .fill_rounded_rect(panel, ui.px(design::WELL_RADIUS), design::shell_fill());
        ui.painter
            .stroke_rect(panel, ui.scale.hairline(), design::edge_strong());

        let (header, content) = panel.inset(ui.px(18)).split_top(ui.px(52));
        let (close, title) = header.split_right(ui.px(116).min(header.w / 3));
        ui.label_styled(
            title,
            "Help",
            TextStyle::tracked(ui.theme.heading.face, 20.0, 0.06),
            design::text(),
            Align::Left,
        );
        design::shell(
            ui,
            close,
            design::recessed(),
            design::edge(),
            Elevation::Rest,
        );
        if ui.input.hovering(close) {
            ui.painter.fill_rect(close.inset(1), design::accent_soft());
        }
        ui.label_styled(
            close.inset(ui.px(6)),
            "Close  ·  F1",
            design::state_style(ui),
            design::text_dim(),
            Align::Centre,
        );
        if ui.input.take_click(close) {
            self.help_open = false;
            return;
        }

        // Keep text at a readable size. The old grid divided the available height by
        // every action, reducing busy categories to overlapping single-pixel rows.
        let columns = if content.w >= ui.px(880) {
            3
        } else if content.w >= ui.px(600) {
            2
        } else {
            1
        };
        let categories = templar_types::action::ActionCategory::ALL;
        let gap = ui.px(12);
        let row_height = ui.px(28);
        let mut heights = vec![ui.px(224)];
        for group in categories.chunks(columns) {
            let count = group
                .iter()
                .map(|category| templar_types::action::in_category(*category).count())
                .max()
                .unwrap_or(0);
            heights.push(ui.px(44) + count as i32 * row_height + gap);
        }
        let total: i32 = heights.iter().sum();
        for key in ui.input.take_keys() {
            let delta = match key {
                KeyInput::Down => row_height,
                KeyInput::Up => -row_height,
                KeyInput::PageDown => content.h,
                KeyInput::PageUp => -content.h,
                KeyInput::Home => -total,
                KeyInput::End => total,
                _ => 0,
            };
            self.help_scroll.scroll_by(delta, content.h, total);
        }
        ui.scroll_list(
            &mut self.help_scroll,
            content,
            &heights,
            |ui, index, row| {
                if index == 0 {
                    Self::draw_help_primer(ui, Rect::new(row.x, row.y, row.w, row.h - gap));
                    return;
                }
                let cell_w = (row.w - gap * (columns as i32 - 1)).max(1) / columns as i32;
                for (col, category) in categories
                    .chunks(columns)
                    .nth(index - 1)
                    .unwrap()
                    .iter()
                    .enumerate()
                {
                    let cell = Rect::new(
                        row.x + col as i32 * (cell_w + gap),
                        row.y,
                        cell_w,
                        row.h - gap,
                    );
                    Self::draw_help_category(ui, cell, *category);
                }
            },
        );
    }

    fn draw_help_category(
        ui: &mut Ui<'_>,
        cell: Rect,
        category: templar_types::action::ActionCategory,
    ) {
        design::shell(
            ui,
            cell,
            design::recessed(),
            design::edge(),
            Elevation::Rest,
        );
        let inner = cell.inset(ui.px(10));
        let (heading, body) = inner.split_top(ui.px(24));
        ui.label_styled(
            heading,
            category.title(),
            TextStyle::tracked(ui.theme.body.face, 11.0, 0.12),
            design::text_faint(),
            Align::Left,
        );
        for (index, declaration) in templar_types::action::in_category(category).enumerate() {
            let row = Rect::new(body.x, body.y + index as i32 * ui.px(28), body.w, ui.px(28));
            let (accelerator, label) = row.split_right(ui.px(116).min(row.w / 2));
            ui.label_styled(
                label,
                declaration.label,
                design::state_style(ui),
                design::text(),
                Align::Left,
            );
            let route = declaration
                .primary_chord()
                .map_or_else(|| "On screen".to_owned(), |chord| chord.to_string());
            ui.label_styled(
                accelerator,
                &route,
                TextStyle::tracked(ui.theme.body.face, 10.0, 0.08),
                design::text_faint(),
                Align::Right,
            );
        }
    }

    /// The Files object over the whole field: one breadcrumb band and the object's own body.
    ///
    /// One nesting level, where there used to be three — a breadcrumb band, then a surface with
    /// its own rail and its own second search field, then a browser with its own places sidebar
    /// and its own toolbar breadcrumb. The rail is the pinned list inside the body now, and the
    /// second search field is the one at the top of the screen.
    fn draw_expanded(
        &mut self,
        ui: &mut Ui<'_>,
        rest: Rect,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let (crumb, body) = rest.split_top(ui.px(CRUMB_HEIGHT));
        let style = TextStyle::new(ui.theme.body.face, 13.0);
        // The crumb says which object is open over Home. A directory's name is not repeated
        // here: the toolbar's breadcrumb underneath already spells the whole path, and for the
        // home folder the old form read "Home ▸ Home". A feed or a search has no breadcrumb of
        // its own, so its name is the crumb's last step.
        let crumb_text = match self.files.location() {
            crate::files_surface::FilesLocation::Directory(_) => "Home  ▸  Files".to_owned(),
            other => format!("Home  ▸  Files  ▸  {}", other.label()),
        };
        // The object's own way out, at the band's right end. Escape closes it and so does a
        // press on the dimmed field around it, but neither is visible: the Home plate used to be
        // the visible one and Home has no plate any more.
        let (close, label) = crumb.split_right(ui.px(32).min(crumb.w));
        ui.label_styled(
            label.inset(ui.px(6)),
            &crumb_text,
            style,
            ui.theme.text_dim,
            Align::Left,
        );
        if ui.input.hovering(close) {
            ui.painter
                .fill_rounded_rect(close.inset(ui.px(4)), ui.px(4), design::accent_soft());
        }
        ui.label_styled(close, "⌄", style, ui.theme.text_dim, Align::Centre);
        if ui.input.take_click(close)
            || (!self.search_focused && ui.input.keys().contains(&KeyInput::Escape))
        {
            self.collapse_files(data.now_ms);
            return;
        }
        match self.files_view.draw(
            ui,
            body,
            &mut self.files,
            data.browser,
            !self.search_focused,
        ) {
            Some(crate::surface::file_browser::FileBrowserAction::Load {
                location,
                show_hidden,
            }) => actions.push(FieldAction::BrowseLocation {
                location,
                show_hidden,
            }),
            Some(crate::surface::file_browser::FileBrowserAction::Open(path)) => {
                actions.push(FieldAction::OpenFile(path));
            }
            Some(crate::surface::file_browser::FileBrowserAction::OpenInFiles(path)) => {
                actions.push(FieldAction::OpenInFiles(path));
            }
            Some(crate::surface::file_browser::FileBrowserAction::File(request)) => {
                actions.push(FieldAction::FileVerb(request));
            }
            None => {}
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_keys(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        // A focused search owns the keyboard, and `draw_search_bar` has already had this frame's
        // keys (Escape included) before the field consumed them.
        if self.search_focused {
            return;
        }
        let keys: Vec<KeyInput> = ui.input.keys().to_vec();
        let modifiers = ui.input.modifiers();
        let (cols, rows) = self.grid_dims(doc);
        let rects = self.current_rects(doc, ui);
        for key in keys {
            if let Some((tile, cell)) = self.keyboard_move {
                self.handle_move_key(&key, tile, cell, cols, rows, actions);
                continue;
            }
            if let Some((tile, span)) = self.keyboard_resize {
                self.handle_resize_key(&key, doc, tile, span, actions);
                continue;
            }
            // Type to search. A letter on bare Home is somebody starting to name an application:
            // it opens the search and goes into it, rather than falling on the floor while the
            // field waits to be clicked. Only when the keyboard is not already driving the field
            // — a focused or selected tile keeps `r` (resize), space (move) and Enter — and only
            // for unmodified printable keys. Renames and menus are handled by the caller, which
            // does not reach here while either is open.
            if let KeyInput::Character(character) = key
                && !character.is_control()
                && !character.is_whitespace()
                && !modifiers.control
                && !modifiers.alt
                && self.focus.is_none()
                && self.selection.is_empty()
                && self.confirming.is_none()
                && self.zoomed.is_none()
            {
                self.search_focused = true;
                self.search
                    .set_text(format!("{}{character}", self.search.text()));
                continue;
            }
            match key {
                KeyInput::Left | KeyInput::Right | KeyInput::Up | KeyInput::Down => {
                    let direction = match key {
                        KeyInput::Left => Direction::Left,
                        KeyInput::Right => Direction::Right,
                        KeyInput::Up => Direction::Up,
                        _ => Direction::Down,
                    };
                    self.navigate_selection(&rects, direction, modifiers.shift);
                }
                KeyInput::Tab => {
                    // Tab visits every object in reading order, wrapping in either direction.
                    let index = self
                        .focus
                        .and_then(|id| rects.iter().position(|(tile, _)| *tile == id));
                    if !rects.is_empty() {
                        let next = match (index, modifiers.shift) {
                            (Some(index), true) => (index + rects.len() - 1) % rects.len(),
                            (Some(index), false) => (index + 1) % rects.len(),
                            (None, true) => rects.len() - 1,
                            (None, false) => 0,
                        };
                        self.focus = Some(rects[next].0);
                        self.selection.replace(rects[next].0);
                    }
                }
                KeyInput::Backspace => {
                    // Inside a project: move the focused tile back out onto the field, next to
                    // the project.
                    if let Some(project) = self.zoomed
                        && let Some(id) = self.focus
                        && let Some(home) = doc.location(project)
                    {
                        actions.push(FieldAction::Apply(Self::move_out_op(
                            doc,
                            id,
                            Landing {
                                container: None,
                                cell: home.placement.cell,
                            },
                        )));
                    }
                }
                KeyInput::Enter => {
                    if let Some(id) = self.focus
                        && let Some(tile) = doc.tile(id)
                    {
                        self.primary_action(tile, data, actions);
                    }
                }
                KeyInput::Character(' ') => {
                    if let Some(id) = self.focus
                        && let Some(located) = doc.location(id)
                        && located.container == self.zoomed
                    {
                        self.keyboard_move = Some((id, located.placement.cell));
                    }
                }
                KeyInput::Character('z' | 'Z') if modifiers.control => {
                    actions.push(FieldAction::Undo);
                }
                KeyInput::Character('a' | 'A') if modifiers.control => {
                    self.select_all(&rects);
                }
                KeyInput::Character('r' | 'R') if modifiers.control && modifiers.shift => {
                    if let Some(op) = field_model::restore_shelved(doc) {
                        actions.push(FieldAction::Apply(op));
                    }
                }
                // The pointer's grip, by keyboard: `field.resize` in the action manifest. Every
                // field gesture has to be reachable both ways, so resize cannot be the one that
                // is pointer-only.
                KeyInput::Character('r' | 'R') if !modifiers.control => {
                    if let Some(id) = self.focus
                        && let Some(located) = doc.location(id)
                        && located.container == self.zoomed
                        && doc.tile(id).is_some_and(Self::resizable)
                    {
                        self.keyboard_resize = Some((id, located.placement.span));
                    }
                }
                KeyInput::Delete => {
                    if let Some(op) = self.stash_selection() {
                        actions.push(FieldAction::Apply(op));
                    }
                }
                KeyInput::Escape if self.confirming.is_some() => self.confirming = None,
                KeyInput::Escape if self.zoomed.is_some() => self.zoomed = None,
                KeyInput::Escape => {
                    self.focus = None;
                    self.selection.clear();
                }
                _ => {}
            }
        }
    }

    fn navigate_selection(&mut self, rects: &[(TileId, Rect)], direction: Direction, extend: bool) {
        let previous = self.focus;
        let next = match previous {
            None => first_in(rects),
            Some(from) => neighbour_in(rects, from, direction).or(Some(from)),
        };
        self.focus = next;
        let Some(next) = next else { return };
        let order: Vec<TileId> = rects.iter().map(|(id, _)| *id).collect();
        if extend {
            if self.selection.is_empty()
                && let Some(previous) = previous
            {
                self.selection.replace(previous);
            }
            self.selection.extend_to(&order, next);
        } else {
            self.selection.replace(next);
        }
    }

    fn stash_selection(&self) -> Option<FieldOp> {
        if self.selection.len() > 1 {
            Some(FieldOp::StashMany {
                ids: self.selection.to_vec(),
            })
        } else {
            self.focus.map(|id| FieldOp::Stash { id })
        }
    }

    fn select_all(&mut self, rects: &[(TileId, Rect)]) {
        let order: Vec<TileId> = rects.iter().map(|(id, _)| *id).collect();
        self.selection.select_all(&order);
        self.focus = order.first().copied();
    }

    /// Arrow keys move a keyboard-lifted tile's landing cell; Enter drops it on the current
    /// grid; Escape puts it back.
    fn handle_move_key(
        &mut self,
        key: &KeyInput,
        tile: TileId,
        cell: Cell,
        cols: u16,
        rows: u16,
        actions: &mut Vec<FieldAction>,
    ) {
        match *key {
            KeyInput::Left | KeyInput::Right | KeyInput::Up | KeyInput::Down => {
                let cell = match key {
                    KeyInput::Left => Cell::new(cell.col.saturating_sub(1), cell.row),
                    KeyInput::Right => {
                        Cell::new((cell.col + 1).min(cols.saturating_sub(1)), cell.row)
                    }
                    KeyInput::Up => Cell::new(cell.col, cell.row.saturating_sub(1)),
                    _ => Cell::new(cell.col, (cell.row + 1).min(rows.saturating_sub(1))),
                };
                self.keyboard_move = Some((tile, cell));
            }
            KeyInput::Enter => {
                self.keyboard_move = None;
                let landing = Landing {
                    container: self.zoomed,
                    cell,
                };
                let op = if self.selection.len() > 1 && self.selection.contains(&tile) {
                    FieldOp::MoveMany {
                        ids: self.selection.to_vec(),
                        anchor: tile,
                        landing,
                    }
                } else {
                    FieldOp::Move { id: tile, landing }
                };
                actions.push(FieldAction::Apply(op));
            }
            KeyInput::Escape => self.keyboard_move = None,
            _ => {}
        }
    }

    /// Arrows grow and shrink a keyboard-resized tile's request; Enter asks for it, Escape
    /// abandons it. The request is clamped to the tile's own range here, and to what the field
    /// can actually give when it is applied.
    fn handle_resize_key(
        &mut self,
        key: &KeyInput,
        doc: &FieldDocument,
        tile: TileId,
        span: Span,
        actions: &mut Vec<FieldAction>,
    ) {
        let range = doc.tile(tile).map(|tile| tile.spans);
        match *key {
            KeyInput::Left | KeyInput::Right | KeyInput::Up | KeyInput::Down => {
                let wanted = match key {
                    KeyInput::Left => Span::new(span.cols.saturating_sub(1), span.rows),
                    KeyInput::Right => Span::new(span.cols.saturating_add(1), span.rows),
                    KeyInput::Up => Span::new(span.cols, span.rows.saturating_sub(1)),
                    _ => Span::new(span.cols, span.rows.saturating_add(1)),
                };
                let wanted = range.map_or(wanted, |range| wanted.clamp(range.min, range.max));
                self.keyboard_resize = Some((tile, wanted));
            }
            KeyInput::Enter => {
                self.keyboard_resize = None;
                actions.push(FieldAction::Apply(FieldOp::Resize { id: tile, span }));
            }
            KeyInput::Escape => self.keyboard_resize = None,
            _ => {}
        }
    }

    #[allow(clippy::too_many_lines)]
    fn handle_pointer(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let whole = ui.painter.bounds();
        let held = ui.input.pressing(whole);
        if ui.input.keys().contains(&KeyInput::Escape)
            && (self.drag != Drag::Idle || self.resize.is_some() || self.marquee.is_some())
        {
            self.drag = Drag::Idle;
            self.resize = None;
            self.marquee = None;
            self.pending = None;
            self.result_press = None;
            self.pointer_cancelled = held;
            return;
        }
        if self.pointer_cancelled {
            self.pointer_cancelled = held;
            return;
        }
        // The dropdown is painted after the tiles but claims its clicks before them: a press
        // inside it is the list's, not whatever tile happens to be underneath.
        if let Some(area) = self.omnibox_area(ui)
            && let Some((x, y)) = ui.input.cursor()
            && area.contains(x, y)
        {
            self.press_on_result(ui, doc, data, actions, area, held, x, y);
            return;
        }
        let Some((cx, cy)) = ui.input.cursor() else {
            if self.drag != Drag::Idle {
                self.finish_drag(ui, doc, data, actions);
            }
            self.finish_resize(ui, doc, actions);
            return;
        };

        if let Some(mut marquee) = self.marquee {
            if held {
                marquee.current = (cx, cy);
                self.marquee = Some(marquee);
                let area = marquee.rect();
                let selected = self
                    .current_rects(doc, ui)
                    .into_iter()
                    .filter_map(|(id, rect)| intersects(area, rect).then_some(id));
                self.selection.replace_many(selected);
                self.focus = self.selection.iter().next().copied();
            } else {
                self.marquee = None;
            }
            return;
        }

        // A resize owns the pointer for as long as it is held: it is started from a corner the
        // move band cannot reach, so there is never a question of which gesture this is.
        if let Some((tile, _)) = self.resize {
            if held {
                if let Some(span) = self.span_under_pointer(doc, ui, tile, cx, cy) {
                    self.resize = Some((tile, span));
                }
            } else {
                self.finish_resize(ui, doc, actions);
            }
            return;
        }

        let sample = DragSample {
            x: cx as f32,
            y: cy as f32,
            t_ms: data.now_ms,
        };
        match (self.drag, held) {
            (Drag::Idle, true) => {
                // Topmost tile whose grab band is under the pointer: later in reading order
                // draws later, so search from the end. The grip is checked first for the same
                // reason, so a tile drawn later cannot have its corner stolen by one beneath it.
                // One pass over the rects, walked twice: laying them out means a `tile_rect` for
                // every tile and a sort, and this arm runs on every press.
                let rects = self.current_rects(doc, ui);
                for (id, rect) in rects.iter().rev() {
                    let resizable = doc.tile(*id).is_some_and(Self::resizable);
                    if resizable && Self::grip(*rect, ui).contains(cx, cy) {
                        let span = doc
                            .location(*id)
                            .map_or_else(|| Span::new(1, 1), |located| located.placement.span);
                        self.resize = Some((*id, span));
                        self.focus = Some(*id);
                        self.keyboard_move = None;
                        self.keyboard_resize = None;
                        return;
                    }
                }
                let mut hit = None;
                for (id, rect) in rects.iter().rev() {
                    let grabbed = if Self::body_draggable(doc, *id) {
                        rect.contains(cx, cy)
                    } else {
                        doc.tile(*id).is_some_and(|tile| {
                            Self::strip_of(doc, tile, *rect, ui).grab.contains(cx, cy)
                        })
                    };
                    if grabbed {
                        hit = Some((*id, *rect));
                        break;
                    }
                }
                if let Some((id, rect)) = hit {
                    self.drag.press(
                        id,
                        GrabOffset {
                            dx: (cx - rect.x) as f32,
                            dy: (cy - rect.y) as f32,
                        },
                        sample,
                    );
                    self.focus = Some(id);
                    if !self.selection.contains(&id) && !ui.input.modifiers().shift {
                        self.selection.replace(id);
                    }
                } else if !rects.iter().any(|(_, rect)| rect.contains(cx, cy))
                    && ui.input.take_click(self.bounds)
                {
                    self.selection.clear();
                    self.focus = None;
                    self.marquee = Some(Marquee {
                        origin: (cx, cy),
                        current: (cx, cy),
                    });
                }
            }
            (Drag::Idle, false) => {}
            (_, true) => {
                if self.drag.sample(sample, &self.drag_cfg)
                    == templar_field::drag::DragEvent::Lifted
                    && self.search_focused
                {
                    // A lift with the results open is a drag out of them: the list gives way so
                    // the field it is being carried to is visible.
                    self.search_focused = false;
                    self.search.set_text("");
                    self.result_press = None;
                }
            }
            (_, false) => self.finish_drag(ui, doc, data, actions),
        }
    }

    /// A press on a result row, which may become a drag onto the field or stay a click. The
    /// press claims the click (`pixelkit_shell::Input` registers one at the press) and arms a drag
    /// with the tile the row stands for; a release still inside the list is the click, delivered
    /// then. The lift itself happens in the ordinary drag arm once the pointer has left the list —
    /// which is the gesture. A press and release within one frame reaches `draw_omnibox` unchanged.
    #[allow(clippy::too_many_arguments)]
    fn press_on_result(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
        area: Rect,
        held: bool,
        x: i32,
        y: i32,
    ) {
        if !held {
            if let Some(index) = self.result_press.take() {
                self.drag = Drag::Idle;
                self.pending = None;
                if let Some(row) = self.omnibox_rows.get(index) {
                    let hit = row.hit.clone();
                    self.choose(hit, actions);
                }
            }
            return;
        }
        if self.result_press.is_some() || self.drag != Drag::Idle {
            return;
        }
        let Some(index) = crate::surface::omnibox_screen::row_at(
            ui,
            self.omnibox_anchor,
            self.bounds,
            &self.omnibox_rows,
            self.omnibox_files,
            x,
            y,
        ) else {
            return;
        };
        let _ = ui.input.take_click(area);
        self.result_press = Some(index);
        let Some((tile, exists)) = self
            .omnibox_rows
            .get(index)
            .and_then(|row| Self::pending_tile(doc, &row.hit))
        else {
            return;
        };
        // Grabbed by its middle: `landing_cell` adds half a cell to the tile's corner, so
        // the cell under the pointer is the one it lands on.
        let (cols, rows) = self.grid_dims(doc);
        let cell = from_pixel(templar_field::cell_rect(
            cols,
            rows,
            to_pixel(self.bounds),
            Cell::new(0, 0),
            Span::new(1, 1),
            ui.px(GAP),
        ));
        let id = tile.id;
        self.pending = (!exists).then_some(tile);
        self.drag.press(
            id,
            GrabOffset {
                dx: (cell.w / 2) as f32,
                dy: (cell.h / 2) as f32,
            },
            DragSample {
                x: x as f32,
                y: y as f32,
                t_ms: data.now_ms,
            },
        );
    }

    /// The tile a result row stands for, and whether the field already has it — in which case the
    /// drag moves the tile that is there rather than making a second one.
    fn pending_tile(doc: &FieldDocument, hit: &crate::omnibox::Hit) -> Option<(Tile, bool)> {
        use crate::omnibox::Hit;
        let tile = match hit {
            Hit::App { id } => field_model::app_tile(id),
            Hit::Directory { path } => templar_field::folder_tile(
                field_model::folder_tile_id(path),
                path,
                doc.cols,
                doc.rows,
            ),
            // A loose file is never a Home tile (the placeable-objects decision).
            Hit::File { .. } => return None,
        };
        let exists = doc.tile(tile.id).is_some();
        Some((tile, exists))
    }

    /// What kind of thing `id` is: the pending tile's kind when the drag is carrying one that is
    /// not in the document yet, the document's answer otherwise.
    fn source_kind<'a>(&'a self, doc: &'a FieldDocument, id: TileId) -> Option<&'a TileKind> {
        match &self.pending {
            Some(tile) if tile.id == id => Some(&tile.kind),
            _ => doc.kind_of(id),
        }
    }

    /// A drop of a tile that is not in the document yet places it rather than moving it. `Move`
    /// becomes `Place` at the same landing (a container landing included); grouping it with the
    /// tile it was dropped on places it first and groups in the same batch, so the field never
    /// shows the interim state.
    fn as_new_tile(&self, op: FieldOp) -> FieldOp {
        let Some(tile) = &self.pending else {
            return op;
        };
        match op {
            FieldOp::Move { id, landing } if id == tile.id => FieldOp::Place {
                tile: tile.clone(),
                landing,
            },
            FieldOp::CreateProject {
                project,
                landing,
                absorb,
            } if absorb.contains(&tile.id) => FieldOp::Batch {
                ops: vec![
                    FieldOp::Place {
                        tile: tile.clone(),
                        landing: Landing {
                            container: None,
                            cell: Cell::new(0, 0),
                        },
                    },
                    FieldOp::CreateProject {
                        project,
                        landing,
                        absorb,
                    },
                ],
            },
            other => other,
        }
    }

    /// Where the "name this group" panel goes, and the three rectangles inside it that take
    /// input. Computed from the field's own area alone, so the input pass can place it before
    /// anything is painted and the paint pass can find exactly the same rectangles again.
    fn rename_group_rects(ui: &Ui<'_>, area: Rect) -> (Rect, Rect, Rect, Rect, Rect) {
        let width = ui.px(380).min(area.w);
        let height = ui.px(118).min(area.h);
        let panel = Rect::new(
            area.x + (area.w - width) / 2,
            area.y + ui.px(64).min((area.h - height).max(0)),
            width,
            height,
        );
        let inner = panel.inset(ui.px(12));
        let (title, rest) = inner.split_top(ui.px(22));
        let (field, buttons) = rest.split_top(ui.px(34));
        let (cancel, rest) = buttons.split_right(ui.px(96));
        let (accept, _) = rest.split_right(ui.px(96));
        (panel, title, field, accept, cancel)
    }

    /// Claim this frame's input for the "name this group" question, and report whether it owns
    /// the frame.
    ///
    /// Split from the painting for the reason the context menu is: `pixelkit_shell::Input` gives
    /// a press to whoever claims it first, so a question drawn *over* the tiles has to take the
    /// input *before* they are painted or the tile underneath answers it. While it is up it owns
    /// the keyboard and the pointer both, including the ground outside it — a question that let
    /// the field keep acting behind it would fire two things per press.
    ///
    /// This is the pointer route for `window.rename-group`, whose declared menu row opens it.
    fn update_rename_group(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        actions: &mut Vec<FieldAction>,
    ) -> bool {
        let Some(rename) = self.rename_group.as_mut() else {
            return false;
        };
        let (_, _, field, accept, cancel) = Self::rename_group_rects(ui, area);
        let keys = ui.input.take_keys();
        if keys.contains(&KeyInput::Escape) {
            self.rename_group = None;
            return true;
        }
        let confirmed = keys.contains(&KeyInput::Enter);
        rename.field.apply(&keys);
        // An empty name is not a refusal: it is how a group goes back to being called by how many
        // windows it holds, which is what an unnamed group is called.
        let typed = rename.field.text().trim().to_owned();
        let window = rename.window;
        let cancelled = ui.input.take_click(cancel);
        let accepted = ui.input.take_click(accept);
        let _ = ui.input.take_click(field);
        // Everything else on the field, so no tile answers a click aimed at this.
        let _ = ui.input.take_click(area);
        if cancelled {
            self.rename_group = None;
        } else if accepted || confirmed {
            actions.push(FieldAction::RenameWindowGroup {
                window,
                name: typed,
            });
            self.rename_group = None;
        }
        true
    }

    /// Draw the "name this group" question, over everything the field painted.
    fn paint_rename_group(&mut self, ui: &mut Ui<'_>, area: Rect) {
        let Some(rename) = self.rename_group.as_mut() else {
            return;
        };
        let (panel, title, field, accept, cancel) = Self::rename_group_rects(ui, area);
        ui.painter.fill_rect(panel, ui.theme.panel);
        ui.painter
            .stroke_rect(panel, ui.scale.hairline(), ui.theme.panel_edge);
        let label = templar_types::action::action("window.rename-group")
            .map_or("Rename group", |declaration| declaration.label);
        ui.label(title, label, ui.theme.text, Align::Left);
        ui.text_field(
            &mut rename.field,
            field.inset(ui.px(3)),
            "Name",
            true,
            false,
        );
        // The clicks were claimed in the input pass; these paint, and their answers are already
        // spoken for.
        let _ = ui.button(cancel.inset(ui.px(4)), "Cancel");
        let _ = ui.button(accept.inset(ui.px(4)), "Rename");
    }

    /// Give this frame's input to the context menu, and report whether it owns the frame.
    ///
    /// The menu is drawn over the tiles, so it has to take the input *before* they are painted:
    /// `pixelkit_shell::Input` gives a press to whoever claims it first, and the tile underneath
    /// would otherwise get every click aimed at a menu row. While the menu is up it owns the
    /// keyboard and the pointer both — a menu that let the field keep acting behind it would fire
    /// two things per press.
    fn handle_menu(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        area: Rect,
        actions: &mut Vec<FieldAction>,
    ) -> bool {
        if let Some(chosen) = self.menu.update(ui, area) {
            self.run_menu_choice(chosen, doc, data, actions);
        }
        self.menu.is_open()
    }

    /// Secondary-click anywhere on the field opens the context menu
    /// (`crate::surface::context_menu`). Over a tile it is that tile's menu — and, when the tile
    /// stands for a live window, that window's; over bare ground it is the field's own, which is
    /// still the only pointer route to `field.undo` and `window.show-all`.
    fn open_menu_on_secondary_click(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        area: Rect,
    ) {
        if !ui.input.take_context_click(area) {
            return;
        }
        let Some((cx, cy)) = ui.input.cursor() else {
            return;
        };
        // Topmost first, matching every other hit test on this screen.
        let tile = self
            .current_rects(doc, ui)
            .into_iter()
            .rev()
            .find(|(_, rect)| rect.contains(cx, cy))
            .map(|(id, _)| id);
        // A group's tile has no window of its own, so it borrows one of its members: every group
        // request on the wire is sent to a member and applies to the set.
        let window_group =
            tile.is_some_and(|id| matches!(doc.kind_of(id), Some(TileKind::WindowGroup { .. })));
        let toplevel = tile.and_then(|id| {
            if window_group {
                field_model::group_window(data.field, id, data.toplevels)
            } else {
                field_model::window_for_tile(data.field, id, data.toplevels)
            }
        });
        let in_window_group = toplevel.is_some_and(|toplevel| toplevel.group.is_some());
        let window = toplevel.map(|toplevel| toplevel.window_id);
        if let Some(id) = tile {
            self.focus = Some(id);
            if !self.selection.contains(&id) {
                self.selection.replace(id);
            }
        }
        // An application installed more than once offers the copies it has. Everything else gets
        // the declared rows and nothing more, which is nearly everything.
        let choices = tile
            .and_then(|id| doc.tile(id))
            .and_then(|tile| match &tile.kind {
                TileKind::App { app_id } => data
                    .apps
                    .iter()
                    .find(|entry| entry.id == *app_id)
                    .filter(|entry| entry.variants > 1)
                    .map(|entry| entry.logical.as_str()),
                _ => None,
            })
            .and_then(|logical| data.variants.get(logical))
            .map_or_else(Vec::new, |variants| {
                variants
                    .iter()
                    .map(
                        |(id, name, origin)| crate::surface::context_menu::MenuChoice {
                            label: name.clone(),
                            detail: origin.clone(),
                            id: id.clone(),
                        },
                    )
                    .collect()
            });
        let place = tile.and_then(|id| match doc.kind_of(id) {
            Some(TileKind::Folder { path }) => Some(path.clone()),
            _ => None,
        });
        let place_state = place.as_ref().and_then(|path| data.garden.get(path));
        self.menu.open_at_with(
            (cx, cy),
            MenuTarget {
                tile,
                window,
                shelved: doc.shelved().next().is_some(),
                window_group,
                in_window_group,
                project: tile.and_then(|id| match doc.kind_of(id) {
                    Some(TileKind::Project { project_id }) => Some(project_id.clone()),
                    _ => None,
                }),
                place: place.clone(),
                place_pinned: place_state.is_some_and(|p| p.pinned),
                place_kept: place_state.is_some_and(|p| p.kept),
            },
            choices,
        );
    }

    /// Turn one menu row into the same thing its chord does.
    ///
    /// Every arm here is the pointer half of a route the keyboard already had — except
    /// `window.close`, which had neither. The four navigation actions are computed from the
    /// shell's own toplevel list rather than asked of the compositor: `templar_shell_v1` has no
    /// request for them, deliberately, because they are reserved chords that must keep working
    /// with this process dead (`compositor/templar-comp/src/keybindings.rs`). Activating the window
    /// the same rule picks reaches the same place by the route the shell does have.
    #[allow(clippy::too_many_lines)]
    fn run_menu_choice(
        &mut self,
        chosen: Chosen,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let window = chosen.target.window;
        let tile = chosen.target.tile;
        // A copy-of-an-application row carries which copy; every declared row carries none.
        if let Some(variant) = chosen.choice
            && chosen.action == crate::surface::context_menu::LAUNCH_VARIANT
        {
            if let Ok(app_id) = variant.parse() {
                actions.push(FieldAction::LaunchVariant(app_id));
            }
            return;
        }
        match chosen.action {
            "field.help" => self.help_open = true,
            "field.undo" => actions.push(FieldAction::Undo),
            "field.stash" => {
                if let Some(op) = self.stash_selection() {
                    actions.push(FieldAction::Apply(op));
                }
            }
            "field.restore" => {
                if let Some(op) = field_model::restore_shelved(doc) {
                    actions.push(FieldAction::Apply(op));
                }
            }
            "field.reset" => actions.push(FieldAction::ResetField),
            "field.move" => {
                if let Some(id) = tile
                    && let Some(located) = doc.location(id)
                    && located.container == self.zoomed
                {
                    self.keyboard_move = Some((id, located.placement.cell));
                }
            }
            "field.resize" => {
                if let Some(id) = tile
                    && let Some(located) = doc.location(id)
                    && located.container == self.zoomed
                    && doc.tile(id).is_some_and(Self::resizable)
                {
                    self.keyboard_resize = Some((id, located.placement.span));
                }
            }
            "home.reveal" => actions.push(FieldAction::RevealHome),
            id @ ("project.enter"
            | "project.restore"
            | "project.open-terminal"
            | "project.open-files"
            | "project.open-editor") => {
                if let Some(project_id) = chosen.target.project.clone() {
                    actions.push(FieldAction::Project { id, project_id });
                }
            }
            id if id.starts_with("place.") || id.starts_with("garden.") => {
                if id == "garden.tend" {
                    actions.push(FieldAction::RunAction(id));
                } else if let Some(path) = chosen.target.place.clone() {
                    actions.push(FieldAction::Place { id, path });
                }
            }
            "window.park" => {
                if let Some(window) = window {
                    actions.push(FieldAction::Park(window));
                }
            }
            "window.cycle-forward" | "window.cycle-backward" => {
                if let Some(next) = cycle_from(data.toplevels, window, chosen.action) {
                    actions.push(FieldAction::Activate(next));
                }
            }
            "window.step-right" | "window.step-left" => {
                if let Some(from) = window
                    && let Some(next) = group_step(data.toplevels, from, chosen.action)
                {
                    actions.push(FieldAction::Activate(next));
                }
            }
            "home.pin-app" => {
                // The window's own application, not the tile's title: the tile may be a session
                // tile, and what is being pinned is the application behind it.
                if let Some(app_id) = window
                    .and_then(|window| {
                        data.toplevels
                            .iter()
                            .find(|toplevel| toplevel.window_id == window)
                    })
                    .and_then(|toplevel| toplevel.canonical_app_id.clone())
                {
                    actions.push(FieldAction::PinApp(app_id));
                }
            }
            // Naming a group needs a name, and a declared row carries no payload — so the row
            // opens the one place a name can be typed, and the request goes when it is.
            "window.rename-group" => {
                if let Some((tile, window)) = tile.zip(window) {
                    let name = doc
                        .tile(tile)
                        .map_or_else(String::new, |tile| tile.title.clone());
                    self.rename_group = Some(GroupRename {
                        window,
                        field: TextFieldState::with_text(name),
                    });
                }
            }
            "window.escape-fullscreen"
            | "window.close"
            | "window.ungroup"
            | "window.activate-group"
            | "window.shelve-group"
            | "window.unshelve-group" => {
                if let Some(window) = window {
                    actions.push(FieldAction::RunWindowAction {
                        id: chosen.action,
                        window,
                    });
                }
            }
            id => actions.push(FieldAction::RunAction(id)),
        }
    }

    /// End a pointer resize, asking for the span the pointer last named. The field clamps it;
    /// an op that changes nothing is simply not emitted, so releasing without having moved
    /// leaves no undo entry behind.
    fn finish_resize(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        actions: &mut Vec<FieldAction>,
    ) {
        let Some((tile, span)) = self.resize.take() else {
            return;
        };
        // The press that started the grip drag is still pending as a click; swallow it so the
        // release does not also open the tile (`paint_tile`'s content click).
        let _ = ui.input.take_click(ui.painter.bounds());
        let unchanged = doc
            .location(tile)
            .is_some_and(|located| located.placement.span == span);
        if !unchanged {
            actions.push(FieldAction::Apply(FieldOp::Resize { id: tile, span }));
        }
    }

    fn finish_drag(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        self.release_drag(ui, doc, data, actions);
        // Placed, or let go: either way the drag is no longer carrying it.
        self.pending = None;
    }

    fn release_drag(
        &mut self,
        ui: &mut Ui<'_>,
        doc: &FieldDocument,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let bounds = self.bounds;
        let pixel_bounds = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        match self.drag.release(&self.drag_cfg, pixel_bounds) {
            None => {}
            Some(Release::Click { tile }) => {
                // A click on the band selects. The click itself is left for the band's own
                // controls (a session's "park" button) to claim when the tile is painted.
                if self.pending.is_none() {
                    self.focus = Some(tile);
                }
            }
            Some(Release::Drop {
                tile,
                tile_at,
                thrown,
                lifted_for_ms,
            }) => {
                let _ = ui.input.take_click(ui.painter.bounds());
                if let Some(drop) = self.file_drop(doc, tile, tile_at, ui) {
                    self.last_drop = Some((tile, data.now_ms));
                    actions.push(FieldAction::DropFiles(drop));
                    // `release` has already cleared the drag; the field document is deliberately
                    // untouched, because the tile has not moved — the file has.
                    return;
                }
                // Grouping and ungrouping go to the compositor before the placement is applied,
                // so the request is out even if the field refuses the move.
                if let Some(gesture) = self.group_gesture(doc, tile, tile_at, ui, data) {
                    actions.push(gesture);
                }
                let group = self.selection.len() > 1 && self.selection.contains(&tile);
                let op = if group {
                    self.landing_cell(doc, tile, tile_at, ui)
                        .map(|cell| FieldOp::MoveMany {
                            ids: self.selection.to_vec(),
                            anchor: tile,
                            landing: Landing {
                                container: self.zoomed,
                                cell,
                            },
                        })
                } else {
                    self.drop_op(doc, tile, tile_at, ui, data)
                };
                if let Some(op) = op {
                    let relift = self.last_drop.is_some_and(|(last, at)| {
                        last == tile && data.now_ms.saturating_sub(at) < 3000
                    });
                    self.last_drop = Some((tile, data.now_ms));
                    actions.push(FieldAction::DropMetrics(DropMetrics {
                        thrown,
                        lifted_for_ms,
                        semantic: !matches!(op, FieldOp::Move { .. } | FieldOp::MoveMany { .. }),
                        relift,
                        op: op.label(),
                    }));
                    actions.push(FieldAction::Apply(op));
                }
            }
        }
    }

    /// The cell a tile dropped with its top-left at `tile_at` lands on: the cell under the
    /// tile's first cell's centre.
    fn landing_cell(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
    ) -> Option<Cell> {
        let bounds = to_pixel(self.bounds);
        let gap = ui.px(GAP);
        let (cols, rows) = self.grid_dims(doc);
        let one =
            templar_field::cell_rect(cols, rows, bounds, Cell::new(0, 0), Span::new(1, 1), gap);
        let _ = tile;
        cell_at(
            cols,
            rows,
            bounds,
            tile_at.0 as i32 + one.w / 2,
            tile_at.1 as i32 + one.h / 2,
            gap,
        )
    }

    /// The target under the pointer for the grammar, and the op a drop there performs. Inside a
    /// zoomed project every drop is placement on its grid: containers are one level deep, so
    /// there is nothing else a drop there could mean.
    fn target_at(doc: &FieldDocument, grid: Option<TileId>, source: TileId, cell: Cell) -> Target {
        if grid.is_some() {
            return Target::Empty {
                container: grid,
                cell,
            };
        }
        match doc.tile_at(None, cell).filter(|id| *id != source) {
            Some(id) => match doc.kind_of(id) {
                Some(TileKind::Project { .. }) => Target::Project { id },
                Some(TileKind::WindowGroup { .. }) => Target::WindowGroup { id },
                Some(kind) => Target::Tile {
                    id,
                    kind: kind.clone(),
                    accepts_open: false,
                },
                None => Target::Empty {
                    container: None,
                    cell,
                },
            },
            None => Target::Empty {
                container: None,
                cell,
            },
        }
    }

    /// Where the lifted tile is coming from, for the grammar. The only thing it asks is whether
    /// the tile is currently inside a window group's tile — which is what makes a drag onto the
    /// bare field mean "take this window out of its group" rather than "put it there".
    fn drop_origin(doc: &FieldDocument, source: TileId) -> templar_field::DropOrigin {
        let in_window_group = doc
            .location(source)
            .and_then(|located| located.container)
            .and_then(|container| doc.kind_of(container))
            .is_some_and(|kind| matches!(kind, TileKind::WindowGroup { .. }));
        templar_field::DropOrigin { in_window_group }
    }

    /// Move `id` to `landing`, dissolving the app group it leaves when it was that group's last
    /// member: a group of favourites with nothing in it is a box with nothing to say. A project is
    /// never dissolved this way — it has a directory behind it and is emptied on purpose.
    fn move_out_op(doc: &FieldDocument, id: TileId, landing: Landing) -> FieldOp {
        let op = FieldOp::Move { id, landing };
        let Some(from) = doc.location(id).and_then(|located| located.container) else {
            return op;
        };
        if Some(from) == landing.container {
            return op;
        }
        let group = doc
            .tile(from)
            .is_some_and(|tile| matches!(tile.kind, TileKind::AppGroup { .. }));
        if group && doc.count_in(Some(from)) == 1 {
            FieldOp::Batch {
                ops: vec![op, FieldOp::Remove { id: from }],
            }
        } else {
            op
        }
    }

    fn op_for(doc: &FieldDocument, source: TileId, operation: &Operation) -> Option<FieldOp> {
        Some(match operation {
            Operation::Relocate { container, to } => Self::move_out_op(
                doc,
                source,
                Landing {
                    container: *container,
                    cell: *to,
                },
            ),
            Operation::Stash => FieldOp::Stash { id: source },
            Operation::GroupIntoAppGroup { with } => {
                let target_cell = doc.location(*with)?.placement.cell;
                let serial = doc.next_tile_id().0;
                let kind = TileKind::AppGroup {
                    group_id: format!("group-{serial}"),
                };
                let group = Tile {
                    id: field_model::tile_id_for(&format!("app-group:{serial}")),
                    spans: SpanRange::for_kind(&kind, doc.cols, doc.rows),
                    kind,
                    title: "Apps".to_owned(),
                    accent: None,
                };
                FieldOp::CreateProject {
                    project: group,
                    landing: Landing {
                        container: None,
                        cell: target_cell,
                    },
                    absorb: vec![*with, source],
                }
            }
            Operation::GroupIntoNewProject { with } => {
                let target_cell = doc.location(*with)?.placement.cell;
                let project_id = format!("field-{}", doc.next_tile_id().0);
                let project = Tile {
                    id: field_model::project_tile_id(&project_id),
                    kind: TileKind::Project { project_id },
                    title: "New project".to_owned(),
                    spans: SpanRange::for_kind(
                        &TileKind::Project {
                            project_id: String::new(),
                        },
                        doc.cols,
                        doc.rows,
                    ),
                    accent: None,
                };
                FieldOp::CreateProject {
                    project,
                    landing: Landing {
                        container: None,
                        cell: target_cell,
                    },
                    absorb: vec![*with, source],
                }
            }
            Operation::PinTool { project }
            | Operation::ParkSession { project }
            | Operation::ScopeWidget { project }
            | Operation::AddToProject { project }
            | Operation::AddToAppGroup { group: project } => FieldOp::Move {
                id: source,
                landing: Landing {
                    container: Some(*project),
                    cell: Cell::new(0, 0),
                },
            },
            // None of these is a field op.
            //
            // Opening a file with an app and adding a reference to a tray need services this
            // screen does not reach yet (trays and file tiles arrive with persistence); until
            // then they are placement only, which is exactly what an unlabelled drop means.
            //
            // Copy and move are a different kind of "not a field op": they move a person's data,
            // so they go to `Files.Operations1` as one journalled operation with its own undo
            // (see `file_drop`) and the field document is deliberately left exactly as it was.
            //
            // Joining and leaving a group are not field ops either, and for the strongest of the
            // three reasons: `templar_window` owns membership. The drop asks the compositor, and the
            // tile moves on the next merge when the compositor's answer comes back — so the field
            // and the workspace can never disagree about what is grouped, not even for a frame.
            // Meanwhile the caller falls back to plain placement, which is what a drop always
            // does when nothing else claims it.
            Operation::OpenWith { .. }
            | Operation::AddReference { .. }
            | Operation::JoinWindowGroup { .. }
            | Operation::LeaveWindowGroup
            | Operation::CopyInto { .. }
            | Operation::MoveInto { .. } => return None,
        })
    }

    /// What a drag *out* of Home offers: the lifted files as a `text/uri-list`.
    ///
    /// The export half of typed drag and drop, and the same encoding the clipboard uses. A drag
    /// carrying no file — a window, a widget, a project — offers nothing rather than an empty
    /// payload, because an empty uri-list is a promise of files that turns out to be a lie.
    ///
    /// **Where the Wayland wiring stops:** handing this to another application means starting a
    /// `wl_data_device` drag and answering `send` for the offered type, and the shell's
    /// compositor connection is the private `templar_shell_v1` protocol with no
    /// `wl_data_device_manager` bound. The payload is exact and tested; the handover is not
    /// built.
    pub fn dragged_uri_list(&self, doc: &FieldDocument) -> Option<String> {
        let paths = self.dragged_file_paths(doc);
        (!paths.is_empty()).then(|| templar_types::uri_list::encode(&paths))
    }

    /// The files a live drag is carrying, in selection order. Empty when nothing is lifted or
    /// what is lifted is not a file.
    pub fn dragged_file_paths(&self, doc: &FieldDocument) -> Vec<PathBuf> {
        let Some(tile) = self.drag.tile() else {
            return Vec::new();
        };
        let carried: Vec<TileId> = if self.selection.len() > 1 && self.selection.contains(&tile) {
            self.selection.to_vec()
        } else {
            vec![tile]
        };
        carried
            .iter()
            .filter_map(|id| match doc.kind_of(*id)? {
                TileKind::File { path } | TileKind::Folder { path } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether the drag has actually been lifted, as opposed to a press that has not moved far
    /// enough to be one yet.
    ///
    /// A `wl_data_device` drag must not start on a press: a click on a file tile would then
    /// become a drag the compositor is grabbing for, and the click would never arrive.
    pub fn drag_is_lifted(&self) -> bool {
        self.drag.is_lifted()
    }

    /// A drop the grammar named as a real file operation, as one request for the whole
    /// selection.
    ///
    /// One request and not one per file: a person dragging forty photos into a folder performed
    /// one action and must get one progress bar, one error if it goes wrong and one undo — which
    /// is what `Files.Operations1` gives for a single operation over many sources and cannot give
    /// for forty operations over one each.
    fn file_drop(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
    ) -> Option<FileDrop> {
        let kind = doc.kind_of(tile)?;
        let cell = self.landing_cell(doc, tile, tile_at, ui)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        let modifiers = templar_field::DropModifiers {
            copy: ui.input.modifiers().control,
        };
        let operation = templar_field::operation_for(
            tile,
            kind,
            &target,
            Self::drop_origin(doc, tile),
            modifiers,
        )?;
        let (folder, copy) = match operation {
            Operation::CopyInto { folder } => (folder, true),
            Operation::MoveInto { folder } => (folder, false),
            _ => return None,
        };
        let destination = match doc.kind_of(folder)? {
            TileKind::Folder { path } => path.clone(),
            _ => return None,
        };
        // Everything the drag was carrying: a multi-selection drags as one, so it lands as one.
        let dragged: Vec<TileId> = if self.selection.len() > 1 && self.selection.contains(&tile) {
            self.selection.to_vec()
        } else {
            vec![tile]
        };
        let sources: Vec<PathBuf> = dragged
            .iter()
            .filter_map(|id| match doc.kind_of(*id)? {
                TileKind::File { path } | TileKind::Folder { path } => Some(path.clone()),
                _ => None,
            })
            .filter(|path| path != &destination)
            .collect();
        if sources.is_empty() {
            return None;
        }
        Some(FileDrop {
            sources,
            destination,
            copy,
        })
    }

    /// The compositor request a drop asks for, when the grammar named one that changes group
    /// membership rather than the document.
    ///
    /// Group membership belongs to `templar_window`, so a drop here asks and the tile follows on the
    /// next merge; the field op the drop also produces is plain placement, as it is for every
    /// other drop the grammar leaves to a service.
    fn group_gesture(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
        data: &FieldData<'_>,
    ) -> Option<FieldAction> {
        let kind = doc.kind_of(tile)?;
        let cell = self.landing_cell(doc, tile, tile_at, ui)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        let modifiers = templar_field::DropModifiers {
            copy: ui.input.modifiers().control,
        };
        let operation = templar_field::operation_for(
            tile,
            kind,
            &target,
            Self::drop_origin(doc, tile),
            modifiers,
        )?;
        let window = field_model::window_for_tile(doc, tile, data.toplevels)?.window_id;
        match operation {
            Operation::JoinWindowGroup { with } => {
                // The target is either another window's tile or a group's; a group's tile is
                // addressed through any of its members, because that is how the wire addresses
                // a group at all.
                let other = field_model::window_for_tile(doc, with, data.toplevels)
                    .or_else(|| field_model::group_window(doc, with, data.toplevels))?;
                (other.window_id != window).then_some(FieldAction::JoinWindowGroup {
                    window,
                    other: other.window_id,
                })
            }
            Operation::LeaveWindowGroup => Some(FieldAction::RunWindowAction {
                id: "window.ungroup",
                window,
            }),
            _ => None,
        }
    }

    fn drop_op(
        &self,
        doc: &FieldDocument,
        tile: TileId,
        tile_at: (f32, f32),
        ui: &Ui<'_>,
        _data: &FieldData<'_>,
    ) -> Option<FieldOp> {
        let kind = self.source_kind(doc, tile)?;
        let cell = self.landing_cell(doc, tile, tile_at, ui)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        // The modifier the pointer is holding right now: the only place the grammar consults
        // one, and the difference between copying a file into a folder and moving it there.
        let modifiers = templar_field::DropModifiers {
            copy: ui.input.modifiers().control,
        };
        let operation = templar_field::operation_for(
            tile,
            kind,
            &target,
            Self::drop_origin(doc, tile),
            modifiers,
        )?;
        let op = Self::op_for(doc, tile, &operation).unwrap_or(FieldOp::Move {
            id: tile,
            landing: Landing {
                container: self.zoomed,
                cell,
            },
        });
        Some(self.as_new_tile(op))
    }

    /// The ghost for the current lift (pointer or keyboard): a dry run of the drop.
    fn ghost(&self, doc: &FieldDocument, ui: &Ui<'_>) -> Option<Ghost> {
        let (tile, cell) = if let Some((tile, cell)) = self.keyboard_move {
            (tile, cell)
        } else if self.drag.is_lifted() {
            let tile = self.drag.tile()?;
            let pixel_bounds = (
                self.bounds.x as f32,
                self.bounds.y as f32,
                self.bounds.w as f32,
                self.bounds.h as f32,
            );
            let at = self.drag.projected_position(&self.drag_cfg, pixel_bounds)?;
            let cell = self.landing_cell(doc, tile, at, ui)?;
            (tile, cell)
        } else {
            return None;
        };
        let kind = self.source_kind(doc, tile)?;
        let target = Self::target_at(doc, self.zoomed, tile, cell);
        // The modifier the pointer is holding right now: the only place the grammar consults
        // one, and the difference between copying a file into a folder and moving it there.
        let modifiers = templar_field::DropModifiers {
            copy: ui.input.modifiers().control,
        };
        let operation = templar_field::operation_for(
            tile,
            kind,
            &target,
            Self::drop_origin(doc, tile),
            modifiers,
        )?;
        let label = operation.label();
        let label_at = match &target {
            Target::Tile { id, .. } | Target::Project { id } => self
                .tile_rect(doc, *id, ui)
                .map(|rect| Rect::new(rect.x, rect.y, rect.w, ui.px(26).min(rect.h))),
            _ => None,
        };
        let op = self.as_new_tile(
            Self::op_for(doc, tile, &operation).unwrap_or(FieldOp::Move {
                id: tile,
                landing: Landing {
                    container: self.zoomed,
                    cell,
                },
            }),
        );
        let (next, _) = apply(doc, &op).ok()?;
        let created = Self::created_container(&op);
        let placement = match created {
            Some(container) => next.location(container)?.placement,
            None => next.location(tile)?.placement,
        };
        let moved = doc
            .located
            .iter()
            .filter(|(id, _)| *id != tile)
            .filter_map(|(id, before)| {
                let after = next.location(*id)?;
                (after.container == self.zoomed && after.placement != before.placement)
                    .then_some((*id, after.placement))
            })
            .collect();
        Some(Ghost {
            placement,
            moved,
            label: match created_op(&op) {
                Some(create) => Some(create.label()),
                None => label,
            },
            label_at,
        })
    }

    /// The container a drop creates, when it creates one — at the top of the op or at the end of
    /// the batch a pending tile's placement is folded into.
    fn created_container(op: &FieldOp) -> Option<TileId> {
        match created_op(op)? {
            FieldOp::CreateProject { project, .. } => Some(project.id),
            _ => None,
        }
    }

    fn primary_action(
        &mut self,
        tile: &Tile,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        match &tile.kind {
            TileKind::App { app_id } => actions.push(FieldAction::Launch(app_id.clone())),
            TileKind::ParkedSession { .. } => {
                if let Some(window) =
                    field_model::window_for_tile(data.field, tile.id, data.toplevels)
                {
                    actions.push(if window.shelved {
                        FieldAction::Restore(window.window_id)
                    } else {
                        FieldAction::Activate(window.window_id)
                    });
                }
            }
            // The Files object opens the file manager on home, exactly as a Folder tile opens it
            // on its own path. It used to launch `org.templaros.Files` by name, which is an
            // application this desktop no longer ships — every press logged "no manifest for
            // org.templaros.Files" and did nothing. Which file manager runs is the provider role's
            // answer, never a name written here.
            TileKind::Widget { widget, .. } => {
                if widget == field_model::FILES_WIDGET
                    && let Some(home) = crate::drawers::standard_paths::home_dir()
                {
                    actions.push(FieldAction::OpenInFiles(home));
                }
            }
            // A group's body opens it, exactly as a project's does — Full is the zoom, and the
            // ladder's other four rungs are what the tile shows in place. Its *title* is the
            // other half and does something else entirely (raise the whole group); `paint_tile`
            // claims that click before the body ever sees it.
            TileKind::Project { .. } | TileKind::WindowGroup { .. } | TileKind::AppGroup { .. } => {
                self.zoomed = Some(tile.id);
                self.focus = None;
                self.keyboard_move = None;
            }
            TileKind::Folder { path } => actions.push(FieldAction::OpenInFiles(path.clone())),
            TileKind::Tray { .. } | TileKind::File { .. } | TileKind::System { .. } => {}
        }
    }

    /// A window group's body, one rung at a time (`@templaros.decision.home-field-stable-bounded-semantic-grammar`,
    /// and §3's WG4 list).
    ///
    /// Micro is a count badge — at one cell the only thing worth saying is how many. Small adds
    /// how many in words under the name the band already shows. Wide is the member titles in a
    /// row, because at three columns the useful question stops being "how many" and becomes
    /// "which". Large is their previews, drawn through the same path a parked session's preview
    /// takes, so a group of two windows shows the two windows. Full is the zoom and never reaches
    /// here: a zoomed container draws its own interior grid instead.
    fn paint_group_body(
        ui: &mut Ui<'_>,
        content: Rect,
        rung: Representation,
        members: &[&templar_window::ShellToplevel],
        accent: u32,
        data: &FieldData<'_>,
    ) {
        let inner = content.inset(ui.px(8));
        if members.is_empty() {
            // Only ever seen for the frame between a group dissolving and the merge sweeping its
            // tile. Saying so beats an empty rectangle.
            ui.label_styled(
                inner,
                "no windows",
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            );
            return;
        }
        let count = members.len();
        if rung == Representation::Micro {
            let size = ui.px(22).min(inner.w).min(inner.h).max(4);
            let badge = Rect::new(
                inner.x + (inner.w - size) / 2,
                inner.y + (inner.h - size) / 2,
                size,
                size,
            );
            ui.badge(
                badge,
                &count.to_string(),
                design::text(),
                design::shell_fill(),
                accent,
            );
            return;
        }
        if rung == Representation::Small {
            let parked = members.iter().filter(|member| member.shelved).count();
            let line = if parked == count {
                format!("{count} windows · parked")
            } else {
                format!("{count} windows")
            };
            ui.label_styled(
                inner,
                &line,
                design::state_style(ui),
                design::text_dim(),
                Align::Left,
            );
            return;
        }
        Self::paint_group_members_row(ui, inner, rung, members, data);
    }

    /// Wide and Large lay the members out as one row of equal cells; what goes in a cell is the
    /// difference between a title and a picture.
    fn paint_group_members_row(
        ui: &mut Ui<'_>,
        inner: Rect,
        rung: Representation,
        members: &[&templar_window::ShellToplevel],
        data: &FieldData<'_>,
    ) {
        let count = members.len();
        let gap = ui.px(4);
        let cells = i32::try_from(count).unwrap_or(i32::MAX).max(1);
        let width = ((inner.w - gap * (cells - 1)).max(1) / cells).max(1);
        for (index, member) in members.iter().enumerate() {
            let offset = i32::try_from(index).unwrap_or(i32::MAX);
            let cell = Rect::new(inner.x + offset * (width + gap), inner.y, width, inner.h);
            if cell.right() > inner.right() + 1 {
                break;
            }
            let title = if member.title.is_empty() {
                member
                    .canonical_app_id
                    .as_ref()
                    .map_or_else(|| "(untitled)".to_owned(), field_model::short_name)
            } else {
                member.title.clone()
            };
            if rung == Representation::Wide {
                ui.painter
                    .stroke_rect(cell, ui.scale.hairline(), design::edge());
                ui.label_styled(
                    cell.inset(ui.px(4)),
                    &title,
                    design::state_style(ui),
                    if member.shelved {
                        design::text_dim()
                    } else {
                        design::text()
                    },
                    Align::Left,
                );
                continue;
            }
            // Large: the preview, filed under exactly the identifier the member's own session
            // tile uses, so the two never show different pictures of one window.
            let well = cell;
            ui.painter.fill_rounded_rect_aa(
                ui.kernel,
                well,
                ui.px(design::WELL_RADIUS) as f32,
                design::well_fill(),
                255,
            );
            ui.painter
                .stroke_rect(well, ui.scale.hairline(), design::edge());
            let identifier = field_model::session_identifier(member, None);
            if let Some(preview) = data.previews.get(&identifier) {
                ui.painter
                    .blit(preview, well.inset(ui.px(1)), ScaleFilter::Bilinear);
            } else if let Some(bitmap) = member
                .canonical_app_id
                .as_ref()
                .and_then(|id| data.icons.get(&id.to_string()))
            {
                let size = ui.px(28).min(well.w).min(well.h).max(4);
                let icon = Rect::new(
                    well.x + (well.w - size) / 2,
                    well.y + (well.h - size) / 2,
                    size,
                    size,
                );
                ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
            }
            let caption = Rect::new(well.x, well.bottom() - ui.px(16), well.w, ui.px(16));
            ui.label_styled(
                caption.inset(ui.px(4)),
                &title,
                design::state_style(ui),
                design::text_dim(),
                Align::Left,
            );
        }
    }

    /// The one control that grows and shrinks a folder window, in the top right of its body.
    ///
    /// It has to be here rather than on the tile's ground, because a window listing a full
    /// directory has no ground left to press. It carries no name: the tile's own band above it
    /// already says which folder this is, and saying it twice is the thing the fifth hands-on run
    /// objected to.
    fn paint_folder_toggle(
        ui: &mut Ui<'_>,
        toggle: Rect,
        id: TileId,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let collapsed = data.field.tile(id).map(folder_collapsed_span);
        let expanded = data
            .field
            .location(id)
            .is_some_and(|at| Some(at.placement.span) != collapsed);
        if ui.input.hovering(toggle) {
            ui.painter.fill_rect(toggle, ui.theme.hover);
        }
        ui.label_styled(
            toggle,
            if expanded { "\u{2013}" } else { "\u{2b}" },
            design::state_style(ui),
            design::text_dim(),
            Align::Centre,
        );
        if ui.input.take_click(toggle)
            && let Some(op) = folder_toggle_op(data.field, id)
        {
            actions.push(FieldAction::Apply(op));
        }
    }

    /// The step back up, drawn above a folder window's list when it has navigated below the
    /// directory it was opened at. At the anchor there is nothing to say that the tile's own
    /// title does not already say, so the whole body is the listing and this draws nothing.
    ///
    /// Returns the rectangle the list gets.
    fn paint_folder_crumb(
        &mut self,
        ui: &mut Ui<'_>,
        crumb: Rect,
        id: TileId,
        anchor: &Path,
        here: &Path,
        body: Rect,
    ) -> Rect {
        if here == anchor {
            return body;
        }
        if ui.input.hovering(crumb) {
            ui.painter.fill_rect(crumb, ui.theme.hover);
        }
        ui.label_styled(
            crumb,
            &format!("↑  {}", here.to_string_lossy()),
            design::state_style(ui),
            design::text_dim(),
            Align::Left,
        );
        if ui.input.take_click(crumb) {
            self.folder_scroll.remove(&id);
            match here.parent() {
                Some(parent) if parent.starts_with(anchor) => {
                    self.folder_at.insert(id, parent.to_path_buf());
                }
                // Above the anchor there is nowhere further up to go inside this window: it goes
                // home rather than wandering out of the folder it was opened for.
                _ => {
                    self.folder_at.remove(&id);
                }
            }
        }
        body
    }

    fn folder_listing<'a>(
        ui: &mut Ui<'_>,
        list: Rect,
        row_height: i32,
        result: Option<&'a Result<DirectoryListing, String>>,
    ) -> Option<&'a DirectoryListing> {
        let Some(listing) = result else {
            ui.label_styled(
                list,
                "reading…",
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            );
            return None;
        };
        let listing = match listing {
            Ok(listing) => listing,
            Err(error) => {
                let (heading, detail) = list.split_top(row_height);
                ui.label_styled(
                    heading,
                    "Could not read folder",
                    design::state_style(ui),
                    design::danger(),
                    Align::Left,
                );
                ui.label_styled(
                    detail.split_top((row_height * 2).min(detail.h)).0,
                    error,
                    design::state_style(ui),
                    design::text_dim(),
                    Align::Left,
                );
                return None;
            }
        };
        if listing.entries.is_empty() {
            ui.label_styled(
                list,
                "empty",
                design::state_style(ui),
                design::text_faint(),
                Align::Left,
            );
            return None;
        }
        Some(listing)
    }

    /// A folder window's body: what is actually in the directory, listed.
    ///
    /// This is the whole of what makes a folder tile a window rather than a label. It used to
    /// draw the bound path in dim text and nothing else, so the largest object on Home was also
    /// the emptiest (the fifth hands-on run's "a lot of empty space"). Now it lists the directory
    /// it is showing, a row per entry with the icon the installed theme gives that kind of thing;
    /// a folder row goes into that folder *in this window*, a file row opens.
    ///
    /// Navigation is view state, not document state: the tile keeps the path it was placed at, so
    /// closing and reopening the session puts the window back where it was put, not wherever it
    /// was last browsed to.
    /// A grown Place: the form its own use has earned, drawn inside its allocation
    /// (`@templaros.decision.a-place-is-a-drawn-structure-not-a-richer-rectangle`).
    ///
    /// The tile's chrome has already drawn the name, so the body is one state line and then the
    /// form: a branch for a place worked through, a shelf for one looked at, nothing at all for
    /// a place that has earned neither — a plain place large enough to browse falls back to the
    /// ordinary folder listing, which is what a folder with no story is.
    ///
    /// There is no roles row. The three `place.open-*` actions are rows of the Place context
    /// menu and reach the same providers by the same route; drawn into the body they made every
    /// place look like a collage of the applications used to manipulate it, which is the one
    /// thing a project must not look like.
    #[allow(clippy::too_many_arguments)]
    fn paint_place(
        &mut self,
        ui: &mut Ui<'_>,
        content: Rect,
        tile: &Tile,
        rung: Representation,
        path: &Path,
        place: &crate::garden::GardenPlace,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let inner = content.inset(ui.px(10));
        if inner.w <= 0 || inner.h <= 0 {
            return;
        }
        let line = ui.px(FOLDER_ROW_HEIGHT);
        let (state_area, body) = inner.split_top(line);
        ui.label_styled(
            state_area,
            &place.state_line(),
            design::state_style(ui),
            if place.pinned {
                design::accent()
            } else {
                design::text_dim()
            },
            Align::Left,
        );
        if rung < Representation::Wide || body.h < line {
            return;
        }
        match place.form.as_str() {
            "branch" => place_forms::branch(ui, body, path, place, actions),
            "shelf" => place_forms::shelf(ui, body, path, place, actions),
            // Plain, and every form this build has not learned: a place with nothing earned is
            // quiet, and a place with room to be browsed is browsable.
            _ if rung >= Representation::Large => {
                self.paint_folder_window(ui, body, tile.id, path, data, actions);
            }
            _ => {}
        }
    }

    fn paint_folder_window(
        &mut self,
        ui: &mut Ui<'_>,
        content: Rect,
        id: TileId,
        anchor: &Path,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let here = self
            .folder_at
            .get(&id)
            .cloned()
            .unwrap_or_else(|| anchor.to_path_buf());
        let inner = content.inset(ui.px(8));
        if inner.w <= 0 || inner.h <= 0 {
            return;
        }
        let row_height = ui.px(FOLDER_ROW_HEIGHT);
        let (header, body) = inner.split_top(row_height);
        let (toggle, crumb_area) = header.split_right(row_height);
        Self::paint_folder_toggle(ui, toggle, id, data, actions);
        let list = self.paint_folder_crumb(ui, crumb_area, id, anchor, &here, body);
        actions.push(FieldAction::BrowseFolder {
            visited: here != anchor,
            path: here.clone(),
        });
        let Some(listing) =
            Self::folder_listing(ui, list, row_height, data.folder_listings.get(&here))
        else {
            return;
        };
        let (list, footer) = if listing.truncated {
            let (list, footer) = list.split_bottom(row_height);
            (list, Some(footer))
        } else {
            (list, None)
        };
        let heights = vec![row_height; listing.entries.len()];
        let scroll = self.folder_scroll.entry(id).or_default();
        let mut chosen = None;
        let icons = data.icons;
        ui.scroll_list(scroll, list, &heights, |ui, index, row| {
            let entry = &listing.entries[index];
            if ui.input.hovering(row) {
                ui.painter.fill_rect(row, ui.theme.hover);
            }
            let side = ui.px(FOLDER_ICON);
            let (icon_box, text) = row.split_left(side + ui.px(4));
            let name = match entry.kind {
                EntryKind::Directory => crate::icons::directory_icon_name(&entry.path, None),
                _ => "text-x-generic",
            };
            if let Some(bitmap) = icons.get(&theme_icon_key(name)) {
                let square = Rect::new(icon_box.x, icon_box.y + (row.h - side) / 2, side, side);
                ui.painter.blit(bitmap, square, ScaleFilter::Bilinear);
            }
            ui.label_styled(
                text,
                &entry.name,
                design::state_style(ui),
                if entry.hidden {
                    design::text_dim()
                } else {
                    design::text()
                },
                Align::Left,
            );
            if ui.input.take_click(row) {
                chosen = Some(index);
            }
        });
        if let Some(index) = chosen {
            let entry = &listing.entries[index];
            if entry.is_dir() {
                actions.push(FieldAction::OpenInFiles(entry.path.clone()));
            } else {
                actions.push(FieldAction::OpenFile(entry.path.clone()));
            }
        }
        if let Some(foot) = footer {
            // Saying so beats a list that silently stops: the cap is the service's, and the way
            // past it is the standalone manager.
            ui.label_styled(
                foot,
                &format!("first {} shown", listing.limit),
                design::state_style(ui),
                design::text_faint(),
                Align::Right,
            );
        }
    }

    #[allow(clippy::too_many_lines)]
    fn paint_tile(
        &mut self,
        ui: &mut Ui<'_>,
        rect: Rect,
        tile: &Tile,
        visual: TileVisual,
        data: &FieldData<'_>,
        actions: &mut Vec<FieldAction>,
    ) {
        let window = match tile.kind {
            TileKind::ParkedSession { .. } => {
                field_model::window_for_tile(data.field, tile.id, data.toplevels)
            }
            _ => None,
        };
        let running = match &tile.kind {
            TileKind::App { app_id } => data
                .toplevels
                .iter()
                .filter(|t| t.canonical_app_id.as_ref() == Some(app_id))
                .count(),
            _ => 0,
        };
        // Every member of a group, its outline on the workspace and its tile here take the same
        // colour, so a group looks like one thing everywhere it appears (§2 rule 7). The tile
        // wears it as a hairline edge and nothing else — no tint, no second fill: a container
        // whose whole job is to be recognised at a glance does not need a card to say so, and the
        // theme direction (§6) is thin explicit geometry rather than more surface.
        let group_accent = tile.accent.unwrap_or(design::accent());
        let (fill, edge) = match &tile.kind {
            TileKind::Project { .. } => (design::accent_soft(), design::project_edge()),
            TileKind::WindowGroup { .. } => (design::shell_fill(), group_accent),
            TileKind::ParkedSession { .. } => (design::shell_fill(), design::edge_strong()),
            TileKind::System { .. } => (design::system_shell(), design::edge_strong()),
            TileKind::Folder { .. } => (design::folder_shell(), design::folder_edge()),
            TileKind::Tray { .. } => (design::tray_shell(), design::tray_edge()),
            TileKind::App { .. }
            | TileKind::AppGroup { .. }
            | TileKind::Widget { .. }
            | TileKind::File { .. } => (design::shell_fill(), design::edge()),
        };
        let elevation = if visual.lifted {
            Elevation::Lifted
        } else if visual.hovered {
            Elevation::Hover
        } else {
            Elevation::Rest
        };
        design::shell(ui, rect, fill, edge, elevation);
        if visual.selected && !visual.lifted {
            ui.painter
                .stroke_rect(rect.inset(ui.px(2)), ui.px(2).max(1), design::accent());
        }

        let widget_composition = match &tile.kind {
            TileKind::Widget { widget, .. } if widget == field_model::FILES_WIDGET => {
                Some(data.composition).filter(|composition| !composition.is_empty())
            }
            _ => None,
        };
        let tray_composition = match &tile.kind {
            TileKind::Tray { tray_id } => data
                .field
                .tray(*tray_id)
                .map(|tray| composition_for_paths(&tray.entries)),
            _ => None,
        };
        if let Some(composition) = widget_composition.or(tray_composition.as_deref()) {
            draw_composition_band(ui, rect, composition);
        }

        let strip = Self::strip_of(data.field, tile, rect, ui);
        let child_count = data.field.count_in(Some(tile.id));
        // Every member of a group tile, and which of them are parked. Read once: the ladder's
        // four in-place rungs and the band's Park control all want the same answer.
        let members = match &tile.kind {
            TileKind::WindowGroup { .. } => {
                field_model::group_members(data.field, tile.id, data.toplevels)
            }
            _ => Vec::new(),
        };
        let all_parked = !members.is_empty() && members.iter().all(|member| member.shelved);
        let controls_width = match &tile.kind {
            TileKind::ParkedSession { .. } if window.is_some_and(|window| !window.shelved) => 64,
            TileKind::App { .. } if running > 0 => 32,
            TileKind::Project { .. } if child_count > 0 => 64,
            TileKind::WindowGroup { .. } if !members.is_empty() => 64,
            _ => 0,
        };
        let display_title = tile.title.clone();
        let title_style = match &tile.kind {
            TileKind::Project { .. }
            | TileKind::WindowGroup { .. }
            | TileKind::AppGroup { .. }
            | TileKind::System { .. } => design::system_label_style(ui),
            TileKind::Widget { widget, .. } if widget == field_model::FILES_WIDGET => {
                design::system_label_style(ui)
            }
            _ => design::title_style(ui),
        };
        let left_mark = match &tile.kind {
            TileKind::File { .. } => design::FILE_SPINE_WIDTH,
            TileKind::ParkedSession { .. } if window.is_some_and(|window| !window.shelved) => {
                design::ACTIVITY_EDGE_WIDTH
            }
            _ => 0,
        };
        let title_colour = match &tile.kind {
            TileKind::Project { .. } => design::accent(),
            TileKind::WindowGroup { .. } => group_accent,
            _ => design::text(),
        };

        // Where the name goes and where the controls go — one answer, so the strip a person aims
        // at and the strip that reacts can never be two different rectangles.
        let revealed = visual.hovered || visual.focused || visual.lifted;
        let chrome = Self::control_rects(strip, rect, controls_width, revealed, ui);
        let (band_controls, title_area, content) = (chrome.controls, chrome.title, chrome.content);
        if let Some(reveal) = chrome.reveal {
            // Manipulation, and only manipulation, waits for attention: the strip fades in over
            // the top of the body carrying what a heading would have held. Focus counts as
            // attention, so this is reachable without a pointer.
            ui.painter.blend_rect(reveal, design::shell_low(), 225);
        }
        let title_area = Rect::new(
            title_area.x + ui.px(left_mark + 12),
            title_area.y,
            (title_area.w - ui.px(left_mark + 18)).max(0),
            title_area.h,
        );
        ui.label_styled(
            title_area,
            &display_title,
            title_style,
            title_colour,
            Align::Left,
        );
        // Pressing a group's name raises every member together, the last active one on top —
        // `activate_group`, taken here so the body's own click (which opens the group) never sees
        // it. The two are deliberately different: the title reaches the windows, the body
        // reaches the group.
        if matches!(&tile.kind, TileKind::WindowGroup { .. })
            && let Some(member) = members
                .iter()
                .find(|member| member.activated)
                .or(members.first())
            && ui.input.take_click(title_area)
        {
            actions.push(FieldAction::RunWindowAction {
                id: "window.activate-group",
                window: member.window_id,
            });
        }
        if band_controls.w > 0 {
            match &tile.kind {
                // Park the whole group, or bring the whole group back: one press for the set,
                // which is what makes a group a unit of work rather than a label.
                TileKind::WindowGroup { .. } if !members.is_empty() => {
                    let member = members
                        .iter()
                        .find(|member| member.activated)
                        .or(members.first());
                    let (id, fallback) = if all_parked {
                        ("window.unshelve-group", "Return")
                    } else {
                        ("window.shelve-group", "Park")
                    };
                    let control = band_controls.inset(ui.px(6));
                    let hovered = ui.input.hovering(control);
                    ui.painter.fill_rounded_rect(
                        control,
                        ui.px(design::CHIP_RADIUS),
                        if hovered {
                            design::accent_soft()
                        } else {
                            design::shell_fill()
                        },
                    );
                    ui.painter
                        .stroke_rect(control, ui.scale.hairline(), group_accent);
                    let label = templar_types::action::action(id)
                        .map_or(fallback, |declaration| declaration.control_label());
                    ui.label_styled(
                        control,
                        label,
                        TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
                        design::text_dim(),
                        Align::Centre,
                    );
                    if let Some(member) = member
                        && ui.input.take_click(control)
                    {
                        actions.push(FieldAction::RunWindowAction {
                            id,
                            window: member.window_id,
                        });
                    }
                }
                TileKind::App { .. } if running > 0 => {
                    ui.badge(
                        band_controls.inset(ui.px(4)),
                        &running.to_string(),
                        design::text(),
                        design::activity_soft(),
                        design::activity(),
                    );
                }
                TileKind::Project { .. } if child_count > 0 => {
                    ui.label_styled(
                        band_controls.inset(ui.px(6)),
                        &format!("{child_count} tiles"),
                        design::state_style(ui),
                        design::accent(),
                        Align::Right,
                    );
                }
                _ => {}
            }
        }

        match &tile.kind {
            TileKind::App { app_id } => {
                let size = ui
                    .px(72)
                    .min(content.w - ui.px(16))
                    .min(content.h - ui.px(16))
                    .max(8);
                let icon = Rect::new(
                    content.x + (content.w - size) / 2,
                    content.y + (content.h - size) / 2,
                    size,
                    size,
                );
                paint_app_icon(ui, icon, app_id, &tile.title, data.icons);
            }
            TileKind::AppGroup { .. } => {
                // The favourites it holds as a grid of icons, each one a launch, laid out in
                // reading order of the interior rather than at their interior cells — four apps
                // in a 2×2 tile want four large icons, not four dots in a 4×4 grid. A click on an
                // icon is claimed here, before the body's own click below, so an icon launches
                // and the ground between them opens the group.
                let inner = content.inset(ui.px(8));
                let children: Vec<(AppId, String)> = data
                    .field
                    .placements_in(Some(tile.id))
                    .into_iter()
                    .filter_map(|(child, _)| data.field.tile(child))
                    .filter_map(|child| match &child.kind {
                        TileKind::App { app_id } => Some((app_id.clone(), child.title.clone())),
                        _ => None,
                    })
                    .collect();
                if children.is_empty() {
                    ui.label_styled(
                        inner,
                        "empty — drop apps here",
                        design::state_style(ui),
                        design::text_dim(),
                        Align::Left,
                    );
                }
                let count = children.len().max(1) as i32;
                let per_row = f64::from(count).sqrt().ceil().max(1.0) as i32;
                let lines = (count + per_row - 1) / per_row;
                let cell = (inner.w / per_row).min(inner.h / lines).max(8);
                for (index, (app_id, title)) in children.iter().enumerate() {
                    let index = index as i32;
                    let slot = Rect::new(
                        inner.x + (index % per_row) * cell,
                        inner.y + (index / per_row) * cell,
                        cell,
                        cell,
                    );
                    let icon = slot.inset((cell / 8).max(2));
                    if ui.input.hovering(slot) && !visual.lifted {
                        ui.painter.fill_rounded_rect(
                            slot,
                            ui.px(design::CHIP_RADIUS),
                            design::accent_soft(),
                        );
                    }
                    paint_app_icon(ui, icon, app_id, title, data.icons);
                    if !visual.lifted && ui.input.take_click(slot) {
                        actions.push(FieldAction::Launch(app_id.clone()));
                    }
                }
            }
            TileKind::ParkedSession { app_id, identifier } => {
                let inner = content.inset(ui.px(12));
                ui.painter.fill_rounded_rect_aa(
                    ui.kernel,
                    inner,
                    ui.px(design::WELL_RADIUS) as f32,
                    design::well_fill(),
                    255,
                );
                ui.painter
                    .stroke_rect(inner, ui.scale.hairline(), design::edge());
                if let Some(preview) = data.previews.get(identifier) {
                    ui.painter
                        .blit(preview, inner.inset(ui.px(1)), ScaleFilter::Bilinear);
                } else if let Some(bitmap) = app_id
                    .as_ref()
                    .and_then(|id| data.icons.get(&id.to_string()))
                {
                    let size = ui.px(32).min(inner.w).min(inner.h).max(4);
                    let icon = Rect::new(
                        inner.x + ui.px(4),
                        inner.y + (inner.h - size) / 2,
                        size,
                        size,
                    );
                    ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
                }
                let state = match window {
                    Some(w) if w.shelved => "parked",
                    Some(w) if w.maximized => "open · maximized",
                    Some(_) => "open",
                    None => "closed",
                };
                let small = design::state_style(ui);
                let state_area = Rect::new(inner.x, inner.bottom() - ui.px(16), inner.w, ui.px(16));
                ui.label_styled(
                    state_area.inset(ui.px(4)),
                    state,
                    small,
                    design::text_dim(),
                    Align::Right,
                );
                if let Some(w) = window
                    && !w.shelved
                    && band_controls.w >= ui.px(30)
                {
                    let park = band_controls.inset(ui.px(6));
                    let hovered = ui.input.hovering(park);
                    ui.painter.fill_rounded_rect(
                        park,
                        ui.px(design::CHIP_RADIUS),
                        if hovered {
                            design::accent_soft()
                        } else {
                            design::shell_fill()
                        },
                    );
                    ui.painter
                        .stroke_rect(park, ui.scale.hairline(), design::edge_strong());
                    let park_label = templar_types::action::action("window.park")
                        .map_or("Park", |declaration| declaration.control_label());
                    ui.label_styled(
                        park,
                        park_label,
                        TextStyle::tracked(ui.theme.body.face, 11.0, 0.10),
                        design::text_dim(),
                        Align::Centre,
                    );
                    if ui.input.take_click(park) {
                        actions.push(FieldAction::Park(w.window_id));
                    }
                }
            }
            TileKind::Widget { widget, .. } => {
                let inner = content.inset(ui.px(4));
                if widget == field_model::FILES_WIDGET {
                    let files = crate::surface::files_screen::FilesData {
                        feeds: data.drawer_feeds,
                        sources: data.browser.sources,
                    };
                    match crate::surface::files_screen::draw(
                        ui,
                        inner,
                        representation_of(data.field, tile),
                        files,
                    ) {
                        // A folder is a Files window of its own, so the window switcher lists
                        // it; the views only the shell has (Recent, Trash, Drives…) still open
                        // in place.
                        crate::surface::files_screen::FilesAction::Open(
                            crate::files_surface::FilesLocation::Directory(path),
                        ) => actions.push(FieldAction::OpenInFiles(path)),
                        crate::surface::files_screen::FilesAction::Open(location) => {
                            self.files.go_to(location);
                            self.pending_expand = Some(data.now_ms);
                        }
                        crate::surface::files_screen::FilesAction::Run(id) => {
                            actions.push(FieldAction::RunAction(id));
                        }
                        crate::surface::files_screen::FilesAction::None => {}
                    }
                } else {
                    // A widget kind this build does not know. Saying so is the whole of the
                    // behaviour: a migration is how a retired widget leaves a saved field, and
                    // guessing at a body for one would hide the fact that it needs one.
                    ui.label_styled(
                        inner.inset(ui.px(8)),
                        "no such widget",
                        design::state_style(ui),
                        design::text_faint(),
                        Align::Left,
                    );
                }
            }
            TileKind::WindowGroup { .. } => {
                Self::paint_group_body(
                    ui,
                    content,
                    representation_of(data.field, tile),
                    &members,
                    group_accent,
                    data,
                );
            }
            TileKind::Project { .. } => {
                // Miniature of the interior: each child as a small chip with its title, in the
                // child grid's own proportions, so the tile reads as what it contains.
                let small = design::state_style(ui);
                let inner = content.inset(ui.px(12));
                if let Some(container) = data.field.container(tile.id) {
                    let children = data.field.placements_in(Some(tile.id));
                    ui.painter.push_clip(inner);
                    for (child, placement) in &children {
                        let chip = from_pixel(templar_field::cell_rect(
                            container.cols,
                            container.rows,
                            to_pixel(inner),
                            placement.cell,
                            placement.span,
                            ui.px(3),
                        ));
                        ui.painter.fill_rounded_rect(
                            chip,
                            ui.px(design::CHIP_RADIUS),
                            design::well_fill(),
                        );
                        ui.painter
                            .stroke_rect(chip, ui.scale.hairline(), design::project_edge());
                        if let Some(child_tile) = data.field.tile(*child) {
                            ui.label_styled(
                                chip.inset(ui.px(3)),
                                &child_tile.title,
                                small,
                                design::text(),
                                Align::Left,
                            );
                        }
                    }
                    ui.painter.pop_clip();
                    if children.is_empty() {
                        ui.label_styled(
                            inner,
                            "empty — drop tiles here",
                            small,
                            design::text_dim(),
                            Align::Left,
                        );
                    }
                }
            }
            TileKind::Folder { path } => {
                let rung = representation_of(data.field, tile);
                match data.garden.get(path) {
                    Some(place) if rung < Representation::Full => {
                        self.paint_place(ui, content, tile, rung, path, place, data, actions);
                    }
                    _ => self.paint_folder_window(ui, content, tile.id, path, data, actions),
                }
            }
            TileKind::Tray { tray_id } => {
                let inner = content.inset(ui.px(12));
                let entries = data
                    .field
                    .tray(*tray_id)
                    .map_or(&[][..], |tray| tray.entries.as_slice());
                let line_height = ui.px(16);
                ui.painter.push_clip(inner);
                for (index, path) in entries
                    .iter()
                    .take((inner.h / line_height).max(0) as usize)
                    .enumerate()
                {
                    let row = Rect::new(
                        inner.x,
                        inner.y + index as i32 * line_height,
                        inner.w,
                        line_height,
                    );
                    let name = path
                        .file_name()
                        .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy());
                    ui.label_styled(
                        row,
                        &name,
                        design::measurement_style(ui),
                        design::text_dim(),
                        Align::Left,
                    );
                }
                ui.painter.pop_clip();
                if entries.is_empty() {
                    ui.label_styled(
                        inner,
                        "empty tray — add references here",
                        design::state_style(ui),
                        design::text_dim(),
                        Align::Left,
                    );
                }
            }
            TileKind::File { path } => {
                let inner = content.inset(ui.px(12));
                // The extension names the kind once; a file without one gets no chip at all —
                // an all-caps FILE placeholder only repeats what the tile already is.
                let extension = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map_or_else(String::new, str::to_uppercase);
                let chip_width = if extension.is_empty() {
                    0
                } else {
                    ui.px(48).min(inner.w)
                };
                let (chip, detail) = inner.split_right(chip_width);
                ui.label_styled(
                    detail,
                    &path
                        .parent()
                        .map_or_else(String::new, |parent| parent.to_string_lossy().into_owned()),
                    design::state_style(ui),
                    design::text_dim(),
                    Align::Left,
                );
                if !extension.is_empty() {
                    ui.painter.fill_rounded_rect(
                        chip.inset(ui.px(3)),
                        ui.px(design::CHIP_RADIUS),
                        design::shell_low(),
                    );
                    ui.painter.stroke_rect(
                        chip.inset(ui.px(3)),
                        ui.scale.hairline(),
                        design::edge(),
                    );
                    ui.label_styled(
                        chip.inset(ui.px(5)),
                        &extension,
                        design::state_style(ui),
                        design::text_dim(),
                        Align::Centre,
                    );
                }
                ui.painter.fill_rect(
                    Rect::new(
                        rect.x,
                        rect.y,
                        ui.px(design::FILE_SPINE_WIDTH).min(rect.w),
                        rect.h,
                    ),
                    file_category_colour(path),
                );
            }
            TileKind::System { probe } => {
                // Every system tile draws through the widget contract, so a control cannot exist
                // on a tile without also existing in the action manifest, the cheat sheet and
                // `templar`. An unknown probe says so rather than rendering an empty shell.
                if let Some(widget) = widgets::descriptor(probe) {
                    let rung = representation_of(data.field, tile);
                    // Notifications draw their own rows; every other widget goes through the
                    // one painter.
                    let chosen = if widget.id == "notifications" {
                        let snapshot = data.widgets.get(probe.as_str());
                        let notifications = widgets::notifications::NotificationsData {
                            rows: data.notifications,
                            headline: snapshot.map_or("…", |snapshot| snapshot.headline.as_str()),
                            available: snapshot.is_none_or(|snapshot| snapshot.available),
                        };
                        match widgets::notifications::draw(
                            ui,
                            content,
                            widget,
                            &notifications,
                            rung,
                            self.confirming,
                        ) {
                            widgets::notifications::NotificationsOutcome::None => None,
                            widgets::notifications::NotificationsOutcome::Dismiss(id) => {
                                self.confirming = None;
                                actions.push(FieldAction::DismissNotification(id));
                                None
                            }
                            widgets::notifications::NotificationsOutcome::Invoke { id, key } => {
                                self.confirming = None;
                                actions.push(FieldAction::InvokeNotification { id, key });
                                None
                            }
                            widgets::notifications::NotificationsOutcome::Action(declaration) => {
                                Some(declaration)
                            }
                        }
                    } else if widget.id == "settings" {
                        // Three readings are its headline, not a detail list the floor rung hides
                        // — see `widgets::settings`. Regions and rungs are still the contract's.
                        widgets::settings::draw(
                            ui,
                            content,
                            widget,
                            data.widgets.get(probe.as_str()),
                            rung,
                            self.confirming,
                        )
                    } else {
                        widgets::draw(
                            ui,
                            content,
                            widget,
                            data.widgets.get(probe.as_str()),
                            rung,
                            self.confirming,
                        )
                    };
                    if let Some(chosen) = chosen {
                        // A destructive action arms on the first press and fires on the
                        // second; anything else goes straight through.
                        if chosen.confirm && self.confirming != Some(chosen.id) {
                            self.confirming = Some(chosen.id);
                        } else {
                            self.confirming = None;
                            actions.push(FieldAction::RunAction(chosen.id));
                        }
                    }
                } else {
                    ui.label_styled(
                        content.inset(ui.px(12)),
                        &format!("{probe} — no such widget"),
                        design::state_style(ui),
                        design::text_dim(),
                        Align::Left,
                    );
                }
            }
        }

        if window.is_some_and(|window| !window.shelved) {
            ui.painter.fill_rect(
                Rect::new(
                    rect.x,
                    rect.y,
                    ui.px(design::ACTIVITY_EDGE_WIDTH).min(rect.w),
                    rect.h,
                ),
                design::activity(),
            );
        }
        if visual.displaced.is_some() {
            ui.painter.blend_rect(rect, design::ground(), 115);
        }
        if visual.focused && !visual.lifted {
            design::focus_ring(ui, rect);
        }

        // The grip, drawn only once the tile is under the pointer or focused: a corner mark on
        // every tile all the time would be the kind of permanent chrome this field does without.
        let grip = Self::grip(rect, ui);
        let resizing = self.resize.is_some_and(|(id, _)| id == tile.id)
            || self.keyboard_resize.is_some_and(|(id, _)| id == tile.id);
        let on_grip =
            !visual.lifted && Self::resizable(tile) && (ui.input.hovering(grip) || resizing);
        if !visual.lifted && Self::resizable(tile) && (visual.hovered || visual.focused || resizing)
        {
            let thickness = ui.px(2).max(1);
            let colour = if on_grip {
                design::accent()
            } else {
                design::edge_strong()
            };
            ui.painter.fill_rect(
                Rect::new(grip.x, grip.bottom() - thickness, grip.w, thickness),
                colour,
            );
            ui.painter.fill_rect(
                Rect::new(grip.right() - thickness, grip.y, thickness, grip.h),
                colour,
            );
        }

        // A press on the grip is a resize, never an open.
        if !visual.lifted && !on_grip && ui.input.take_click(content) {
            self.focus = Some(tile.id);
            let modifiers = ui.input.modifiers();
            if modifiers.control {
                self.selection.toggle(tile.id);
            } else if modifiers.shift {
                let order: Vec<TileId> = self
                    .current_rects(data.field, ui)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                self.selection.extend_to(&order, tile.id);
            } else {
                self.selection.replace(tile.id);
                self.primary_action(tile, data, actions);
            }
        }
    }
}

/// `window.cycle-forward` / `window.cycle-backward` from the shell's side.
///
/// The compositor's own cycle walks its stacking order, which this process is never told; the
/// shell walks its toplevel list in `window_id` order instead. Both step through the same set of
/// live windows in a stable, reversible order, which is all the action promises — nobody can see
/// two different orders at once, because only one of the two routes is being used at a time.
/// Parked windows are skipped: they are reached through the shelf, not the switcher.
fn cycle_from(
    toplevels: &[ShellToplevel],
    current: Option<WindowId>,
    action: &str,
) -> Option<WindowId> {
    let mut order: Vec<WindowId> = toplevels
        .iter()
        .filter(|window| !window.shelved)
        .map(|window| window.window_id)
        .collect();
    order.sort_unstable();
    if order.is_empty() {
        return None;
    }
    let Some(at) = current.and_then(|id| order.iter().position(|candidate| *candidate == id))
    else {
        return Some(order[0]);
    };
    let len = order.len();
    let next = if action == "window.cycle-forward" {
        (at + 1) % len
    } else {
        (at + len - 1) % len
    };
    Some(order[next])
}

/// `window.step-right` / `window.step-left` from the shell's side: the same spatial rule the
/// compositor's chords use (`templar_window::geometry::contiguous_window`, two hops), run over the
/// geometry the shell already receives on every toplevel. One implementation of the rule, two
/// callers — the shell does not get its own idea of what a group is.
fn group_step(toplevels: &[ShellToplevel], from: WindowId, action: &str) -> Option<WindowId> {
    /// Kept in step with `compositor/templar-comp/src/keybindings.rs`'s `STEP_HOPS`.
    const STEP_HOPS: u8 = 2;
    let windows: Vec<(WindowId, templar_window::Rect)> = toplevels
        .iter()
        .filter(|window| !window.shelved)
        .map(|window| (window.window_id, window.geometry))
        .collect();
    let direction = if action == "window.step-right" {
        templar_window::geometry::Direction::Right
    } else {
        templar_window::geometry::Direction::Left
    };
    templar_window::geometry::contiguous_window(&windows, from, direction, STEP_HOPS)
}

/// A folder tray's expand/collapse, as one field operation.
///
/// A tray is a door; the expanded size is the one at which the room behind it is worth looking at,
/// clamped to the kind's ceiling. Both directions go through the field's own resize, so both undo
/// and both yield to their neighbours like any other resize.
/// What a folder window collapses to: the tile's own floor, but never one row.
///
/// The Folder class floor is a *line* — the shape a barely used garden Place takes, where the
/// name and a state line are the whole of the drawing. A folder *window* at one row has nowhere
/// to put the listing that this toggle exists to show, so it is not somewhere the toggle may
/// send it.
fn folder_collapsed_span(tile: &Tile) -> Span {
    Span::new(tile.spans.min.cols, tile.spans.min.rows.max(2))
}

fn folder_toggle_op(doc: &FieldDocument, id: TileId) -> Option<FieldOp> {
    let tile = doc.tile(id)?;
    let collapsed = folder_collapsed_span(tile);
    let expanded = Span::new(
        tile.spans
            .max
            .cols
            .min(FOLDER_OPEN_SPAN.cols)
            .max(collapsed.cols),
        tile.spans
            .max
            .rows
            .min(FOLDER_OPEN_SPAN.rows)
            .max(collapsed.rows),
    );
    let now = doc.location(id).map(|at| at.placement.span)?;
    let span = if now == collapsed {
        expanded
    } else {
        collapsed
    };
    (now != span).then_some(FieldOp::Resize { id, span })
}

/// Where the Files object goes when it opens: a window over the field, not the field itself.
///
/// Most of Home stays visible around it — the whole point of the object is that it is one thing
/// on a desktop rather than the desktop becoming a file manager (the fifth hands-on run: "it
/// takes up the entire screen, as if only one window can be opened at a time"). On a field too
/// small to hold a window and still be a field, it is the field: a 400-pixel-wide pane inside a
/// 500-pixel-wide screen would be neither.
fn files_window_rect(field: Rect, ui: &Ui<'_>) -> Rect {
    const MIN_FIELD: i32 = 900;
    if field.w < ui.px(MIN_FIELD) {
        return field;
    }
    let w = (field.w * 3 / 4).max(ui.px(720));
    let h = (field.h * 5 / 6).max(ui.px(400)).min(field.h);
    Rect::new(
        field.x + (field.w - w) / 2,
        field.y + (field.h - h) / 2,
        w.min(field.w),
        h,
    )
}

fn representation_of(doc: &FieldDocument, tile: &Tile) -> Representation {
    let span = doc
        .location(tile.id)
        .map_or(tile.spans.preferred, |located| located.placement.span);
    Representation::for_span(&tile.kind, span)
}

fn file_category_colour(path: &std::path::Path) -> u32 {
    let category = templar_treemap::classify_path(path).category;
    templar_treemap::colors::category_color(category).to_rgb_u32()
}

fn composition_for_paths(paths: &[std::path::PathBuf]) -> Vec<(u32, f32)> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut counts = [0_u32; templar_treemap::CATEGORY_COUNT];
    for path in paths {
        let category = templar_treemap::classify_path(path).category;
        counts[templar_treemap::tree::extensions::category_index(category)] += 1;
    }
    let mut categories: Vec<_> = templar_treemap::tree::extensions::CATEGORY_ORDER
        .into_iter()
        .filter_map(|category| {
            let count = counts[templar_treemap::tree::extensions::category_index(category)];
            (count > 0).then_some((category, count))
        })
        .collect();
    categories.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let total = paths.len() as f32;
    categories
        .into_iter()
        .map(|(category, count)| {
            (
                templar_treemap::colors::category_color(category).to_rgb_u32(),
                count as f32 / total,
            )
        })
        .collect()
}

fn draw_composition_band(ui: &mut Ui<'_>, rect: Rect, composition: &[(u32, f32)]) {
    let height = ui.px(design::COMPOSITION_BAND_HEIGHT).min(rect.h);
    if height <= 0 || composition.is_empty() {
        return;
    }
    let mut x = rect.x;
    for (index, (colour, share)) in composition.iter().enumerate() {
        let remaining = (rect.right() - x).max(0);
        let width = if index + 1 == composition.len() {
            remaining
        } else {
            ((rect.w as f32 * share).round() as i32).clamp(0, remaining)
        };
        if width > 0 {
            ui.painter
                .fill_rect(Rect::new(x, rect.y, width, height), *colour);
            x += width;
        }
    }
}

/// Whether a cached treemap is near enough to `wanted` to blit rather than rasterize again:
/// within a quarter in each dimension, so a drag's every pixel of resize does not re-scan.
// No caller while the Files object has no directory of its own to show; the expansion work that
// gives it one is what asks for a treemap again, and the rule is the same rule.
#[allow(dead_code)]
fn close_enough(bitmap: &Bitmap, wanted: (u32, u32)) -> bool {
    let near = |have: u32, want: u32| have * 4 >= want * 3 && have * 3 <= want * 4;
    near(bitmap.width, wanted.0) && near(bitmap.height, wanted.1)
}

/// How one tile is grabbed and where its body starts. See [`FieldScreen::grab_strip`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GrabStrip {
    /// Where a move gesture starts.
    pub grab: Rect,
    /// The heading, when the tile is large enough to have one. `None` at the compact rungs.
    pub heading: Option<Rect>,
    /// What is left for the tile's content — the whole rect when there is no heading.
    pub body: Rect,
}

/// Where a tile's name, its manipulation controls and its remaining content go. See
/// [`FieldScreen::control_rects`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileChrome {
    /// The strip that fades in over a compact tile's body to carry its controls. `None` for a
    /// tile with a heading (the heading is already that strip) and for one nothing has revealed.
    pub reveal: Option<Rect>,
    /// Where the controls are drawn and where a press on one lands. Zero-width when the tile has
    /// no controls, or has them but has not been hovered or focused.
    pub controls: Rect,
    /// Where the tile says what it is. Never covered by [`Self::controls`].
    pub title: Rect,
    /// What is left for the tile's content once the name has taken its line.
    pub content: Rect,
}

#[derive(Clone, Copy, Debug)]
struct TileVisual {
    hovered: bool,
    focused: bool,
    selected: bool,
    lifted: bool,
    /// Drawn dimmed: a ghost predicts it moving to this cell.
    displaced: Option<Cell>,
}

#[derive(Clone, Copy, Debug)]
struct Marquee {
    origin: (i32, i32),
    current: (i32, i32),
}

impl Marquee {
    fn rect(self) -> Rect {
        let x = self.origin.0.min(self.current.0);
        let y = self.origin.1.min(self.current.1);
        Rect::new(
            x,
            y,
            (self.origin.0 - self.current.0).abs().max(1),
            (self.origin.1 - self.current.1).abs().max(1),
        )
    }
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.x < b.right() && a.right() > b.x && a.y < b.bottom() && a.bottom() > b.y
}

/// The first tile in reading order among `rects`.
fn first_in(rects: &[(TileId, Rect)]) -> Option<TileId> {
    rects.first().map(|(id, _)| *id)
}

/// The nearest tile in `direction` from `from`, by centre, weighting the primary axis — the
/// same rule as `templar_field::focus::neighbour`, over whichever grid is on screen.
fn neighbour_in(rects: &[(TileId, Rect)], from: TileId, direction: Direction) -> Option<TileId> {
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = (origin.x + origin.w / 2, origin.y + origin.h / 2);
    let mut best: Option<(i64, TileId)> = None;
    for (id, rect) in rects {
        if *id == from {
            continue;
        }
        let (cx, cy) = (rect.x + rect.w / 2, rect.y + rect.h / 2);
        let (ahead, aside) = match direction {
            Direction::Left => (ox - cx, (cy - oy).abs()),
            Direction::Right => (cx - ox, (cy - oy).abs()),
            Direction::Up => (oy - cy, (cx - ox).abs()),
            Direction::Down => (cy - oy, (cx - ox).abs()),
        };
        if ahead <= 0 {
            continue;
        }
        let score = i64::from(ahead) + 2 * i64::from(aside);
        if best.is_none_or(|(s, b)| (score, *id) < (s, b)) {
            best = Some((score, *id));
        }
    }
    best.map(|(_, id)| id)
}

/// One application's icon, or its initial on an accent plate when no icon resolved.
fn paint_app_icon(
    ui: &mut Ui<'_>,
    icon: Rect,
    app_id: &AppId,
    title: &str,
    icons: &HashMap<String, Bitmap>,
) {
    if let Some(bitmap) = icons.get(&app_id.to_string()) {
        ui.painter.blit(bitmap, icon, ScaleFilter::Bilinear);
    } else {
        ui.painter.fill_rounded_rect_aa(
            ui.kernel,
            icon,
            ui.px(design::TILE_RADIUS) as f32,
            ui.theme.accent_soft,
            255,
        );
        let initial = title
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string();
        ui.label(icon, &initial, ui.theme.accent, Align::Centre);
    }
}

/// The `CreateProject` an op performs, if any: at the top, or last in the batch a pending tile's
/// placement is folded into.
fn created_op(op: &FieldOp) -> Option<&FieldOp> {
    match op {
        FieldOp::CreateProject { .. } => Some(op),
        FieldOp::Batch { ops } => match ops.last() {
            Some(create @ FieldOp::CreateProject { .. }) => Some(create),
            _ => None,
        },
        _ => None,
    }
}

/// Keep a tile's rectangle inside the field.
///
/// A defence, not a fix for anything known to happen: `templar_field::cell_rect` extrapolates rather
/// than clamping — `edge()` is linear in the cell index with no bound at `count` — so a placement
/// outside the document's own grid would be handed a rectangle outside `bounds`, and below the
/// field is the bar's strip. Every `FieldOp` that places a tile goes through
/// `FieldDocument::is_free`, which does check `Placement::fits`, so nothing should create one;
/// nothing in the type forbids it either, and the consequence would land on the bar.
///
/// The paint and the pointer both go through `tile_rect`, so this moves the rectangle a person
/// aims at and the rectangle that reacts together.
fn nudge_inside(rect: Rect, bounds: Rect) -> Rect {
    if bounds.w <= 0 || bounds.h <= 0 {
        return rect;
    }
    let w = rect.w.min(bounds.w);
    let h = rect.h.min(bounds.h);
    Rect::new(
        rect.x.clamp(bounds.x, bounds.right() - w),
        rect.y.clamp(bounds.y, bounds.bottom() - h),
        w,
        h,
    )
}

fn to_pixel(rect: Rect) -> PixelRect {
    PixelRect::new(rect.x, rect.y, rect.w, rect.h)
}

fn from_pixel(rect: PixelRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.w, rect.h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field_model::{LiveState, merge, tile_id_for};
    use pixelkit_raster::{Painter, RasterKernel, WindowBuffer};
    use pixelkit_shell::{Input, Modifiers, MouseButton, Scale};

    const W: i32 = 1200;
    const H: i32 = 800;

    fn toplevel(id: u64, app: &str, shelved: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: format!("win {id}"),
            verified_identity: true,
            activated: false,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            shelved,
            maximized: false,
            geometry: templar_window::Rect::new(0, 0, 640, 480),
            layer: templar_window::Layer::Normal,
            group: None,
        }
    }

    fn app_entry(id: &str, name: &str) -> crate::app_search::AppEntry {
        crate::app_search::AppEntry {
            id: id.parse().unwrap(),
            name: name.to_owned(),
            desktop_file_id: String::new(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
            logical: String::new(),
            variants: 1,
        }
    }

    struct Harness {
        screen: FieldScreen,
        doc: FieldDocument,
        toplevels: Vec<ShellToplevel>,
        apps: Vec<crate::app_search::AppEntry>,
        recents: Vec<crate::app_search::AppEntry>,
        recent_ids: Vec<String>,
        variants: HashMap<String, Vec<(String, String, String)>>,
        text: pixelkit_text::TextCache,
        kernel: RasterKernel,
        buffer: WindowBuffer,
        input: Input,
        now: u64,
        /// The rectangle the host hands `draw`. The session's is the output minus the bar's
        /// strip; before the bar exists it is the whole output, which is what leaves pixels in
        /// the strip if the field does not clear the whole buffer.
        area: Rect,
    }

    impl Harness {
        fn new() -> Self {
            let (cols, rows) = field_model::grid_for(W, H, field_model::TILE_SIZE);
            let mut doc = FieldDocument::new("test", cols, rows);
            let toplevels = vec![
                toplevel(1, "org.templaros.Files", false),
                toplevel(2, "org.example.Editor", true),
            ];
            merge(
                &mut doc,
                &LiveState {
                    toplevels: &toplevels,
                    projects: &[],
                },
            );
            // Home no longer grows an application tile per running window, so the two these
            // tests aim at are pinned the way a person would pin them.
            for app in ["org.templaros.Files", "org.example.Editor"] {
                field_model::pin_app(&mut doc, &app.parse().unwrap());
            }

            Harness {
                screen: FieldScreen::default(),
                doc,
                toplevels,
                apps: vec![
                    app_entry("org.templaros.Files", "Files"),
                    app_entry(
                        "desktop-import.gimp.desktop",
                        "GNU Image Manipulation Program",
                    ),
                ],
                recents: Vec::new(),
                recent_ids: Vec::new(),
                variants: HashMap::new(),
                text: pixelkit_text::TextCache::new(crate::surface::fonts::set()),
                kernel: RasterKernel::new(),
                buffer: WindowBuffer::new(W as u32, H as u32),
                input: Input::new(),
                now: 0,
                area: Rect::new(0, 0, W, H),
            }
        }

        /// Seed the field with a tile per running window, and a container per group.
        ///
        /// Home places none of these any more — a running window is the bar's business — but the
        /// field still understands them and `FieldScreen` still draws and acts on them, which is
        /// what the tests calling this cover.
        fn with_window_tiles(mut self) -> Self {
            field_model::place_window_tiles(&mut self.doc, &self.toplevels);
            self
        }

        fn frame(&mut self) -> Vec<FieldAction> {
            self.frame_raw()
                .into_iter()
                .filter(|action| {
                    !matches!(
                        action,
                        FieldAction::NeedTreemap { .. } | FieldAction::DropMetrics(_)
                    )
                })
                .collect()
        }

        /// Every action, background requests and experiment bookkeeping included.
        fn frame_raw(&mut self) -> Vec<FieldAction> {
            self.now += 16;
            let icons = HashMap::new();
            let previews = HashMap::new();
            let drawer_feeds = HashMap::new();
            let widgets = HashMap::new();
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
                feeds: &drawer_feeds,
                thumbnails: &empty_thumbnails,
                icons: &HashMap::new(),
                home: None,
            };
            let recents: Vec<&crate::app_search::AppEntry> = self.recents.iter().collect();
            let data = FieldData {
                notifications: &[],
                field: &self.doc,
                toplevels: &self.toplevels,
                icons: &icons,
                previews: &previews,
                drawer_feeds: &drawer_feeds,
                treemap: None,
                composition: &[],
                browser: &browser,
                folder_listings: &HashMap::new(),
                connected: true,
                home_revealed: false,
                apps: &self.apps,
                apps_available: true,
                widgets: &widgets,
                variants: &self.variants,
                projects: &[],
                garden: &HashMap::new(),
                actions: templar_types::action::MANIFEST,
                file_search: None,
                recents: &recents,
                recent_ids: &self.recent_ids,
                reduced_motion: false,
                now_ms: self.now,
            };
            let mut ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            let actions = self.screen.draw(&mut ui, self.area, &data);
            self.input.end_frame();
            actions
        }

        fn rect_of(&mut self, tile: TileId) -> Rect {
            // A frame to establish bounds, then the rect.
            self.frame();
            let ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            self.screen
                .tile_rect(&self.doc, tile, &ui)
                .expect("tile is placed")
        }

        fn click(&mut self, x: i32, y: i32) -> Vec<FieldAction> {
            self.input.cursor_moved(x as f32, y as f32);
            self.input.mouse(MouseButton::Left, true);
            self.input.mouse(MouseButton::Left, false);
            self.frame()
        }

        fn key(&mut self, key: KeyInput) -> Vec<FieldAction> {
            self.input.key(key);
            self.frame()
        }

        fn apply(&mut self, actions: &[FieldAction]) {
            for action in actions {
                if let FieldAction::Apply(op) = action {
                    self.doc = apply(&self.doc, op).expect("op applies").0;
                }
            }
        }
    }

    #[test]
    fn field_chrome_uses_the_design_system_geometry() {
        let mut h = Harness::new();
        h.frame();

        assert_eq!(
            h.screen.bounds,
            Rect::new(
                design::FIELD_INSET,
                design::FIELD_INSET + design::TOP_BAR_HEIGHT + design::TOP_BAR_GAP,
                W - design::FIELD_INSET * 2,
                H - design::FIELD_INSET * 2 - design::TOP_BAR_HEIGHT - design::TOP_BAR_GAP,
            )
        );

        let files = tile_id_for("app:org.templaros.Files");
        let rect = h.rect_of(files);
        let ui = Ui::new(
            Painter::new(&mut h.buffer),
            &mut h.text,
            &mut h.input,
            design::theme(),
            Scale::ONE,
            &mut h.kernel,
        );
        assert_eq!(
            FieldScreen::grab_strip(rect, Representation::Large, &ui)
                .heading
                .map(|heading| heading.h),
            Some(design::HEADING_HEIGHT)
        );
    }

    #[test]
    fn successful_field_operations_get_a_bounded_undo_notice() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        h.screen.confirm(&FieldOp::Stash { id: files }, h.now);

        let notice = h.screen.notice.as_ref().expect("confirmation is visible");
        assert_eq!(notice.expires_at_ms, NOTICE_LIFETIME_MS);
        assert_eq!(notice.text, "STASH ON SHELF");

        h.now = NOTICE_LIFETIME_MS - 17;
        h.frame();
        assert!(
            h.screen.notice.is_some(),
            "notice remains before four seconds"
        );
        h.frame();
        assert!(
            h.screen.notice.is_none(),
            "notice expires after four seconds"
        );
    }

    #[test]
    fn clicking_an_app_tile_launches_it_and_a_session_tile_restores_or_activates() {
        let mut h = Harness::new().with_window_tiles();
        let files = tile_id_for("app:org.templaros.Files");
        let rect = h.rect_of(files);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(
            actions,
            vec![FieldAction::Launch("org.templaros.Files".parse().unwrap())]
        );

        let parked = field_model::session_tile_for(&h.doc, &h.toplevels[1]).unwrap();
        let rect = h.rect_of(parked);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(actions, vec![FieldAction::Restore(WindowId(2))]);

        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(actions, vec![FieldAction::Activate(WindowId(1))]);
    }

    /// The `y` of the omnibox dropdown's `index`-th row, from the same constants the painter
    /// uses: the top bar, the 4px gap under it, the panel's own 4px inset, and half a row.
    fn omnibox_row_y(index: i32) -> i32 {
        design::FIELD_INSET
            + design::TOP_BAR_HEIGHT
            + 4
            + 4
            + crate::surface::omnibox_screen::ROW_HEIGHT * index
            + crate::surface::omnibox_screen::ROW_HEIGHT / 2
    }

    /// Recents sit under the centred search field. An empty query is not a dropdown of every
    /// application — that is what typing is for — and a chip launches the application it names.
    #[test]
    fn a_recent_chip_under_search_launches_that_application() {
        let mut h = Harness::new();
        h.recents = vec![app_entry("org.templaros.Files", "Files")];
        h.frame();
        let search = h.screen.omnibox_anchor;
        let chip_w = design::RECENT_CHIP_WIDTH;
        let chip_x = search.x + (search.w - chip_w) / 2;
        let recents_y = search.bottom() + design::TOP_BAR_GAP + design::RECENTS_HEIGHT / 2;
        let actions = h.click(chip_x + chip_w / 2, recents_y);
        assert_eq!(
            actions,
            vec![FieldAction::Launch("org.templaros.Files".parse().unwrap())],
        );
    }

    /// One route to the file manager. A Folder tile and a path typed into the search are the two
    /// presses that used to take different ways out of Home — a tile went straight to
    /// `OpenInFiles`, a typed path went through an `OpenFolderWindow` that did nothing but
    /// forward there, and the Place menu's row resolved the role a second time in `app.rs`. All
    /// three are `OpenInFiles(path)` now, which is what `open_in_file_manager` answers.
    #[test]
    fn every_folder_press_on_home_opens_the_file_manager_on_that_folder() {
        let mut h = Harness::new();
        let path = std::path::PathBuf::from("/home/a/work");
        h.with_only(vec![(
            tile_id_for("folder:/home/a/work"),
            TileKind::Folder { path: path.clone() },
            Cell::new(0, 0),
        )]);
        let rect = h.rect_of(tile_id_for("folder:/home/a/work"));
        let pressed = h.click(rect.x + rect.w / 2, rect.bottom() - 8);
        assert!(
            pressed.contains(&FieldAction::OpenInFiles(path.clone())),
            "the Folder tile: {pressed:?}"
        );

        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("/etc");
        h.frame();
        let typed = h.key(KeyInput::Enter);
        assert!(
            typed.contains(&FieldAction::OpenInFiles("/etc".into())),
            "a typed path: {typed:?}"
        );
    }

    /// The garden must not feed on Home drawing itself.
    ///
    /// A folder window asks for its listing on every paint, so its own anchor arrives at the host
    /// constantly and for no reason of the user's. Counting that as a `directory-opened` is how
    /// the permanent Desktop tray earned an act of work every session simply by being on the
    /// field: after a real day of work the ledger held exactly one place, `~/Desktop`, grown from
    /// nothing but Home's own chrome.
    #[test]
    fn a_folder_window_drawn_at_its_own_anchor_is_not_a_visit_the_garden_hears_about() {
        let mut h = Harness::new();
        let anchor = std::path::PathBuf::from("/home/a/Desktop");
        let id = tile_id_for("folder:/home/a/Desktop");
        h.with_only(vec![(
            id,
            TileKind::Folder {
                path: anchor.clone(),
            },
            Cell::new(0, 0),
        )]);

        let browses = |actions: Vec<FieldAction>| -> Vec<FieldAction> {
            actions
                .into_iter()
                .filter(|action| matches!(action, FieldAction::BrowseFolder { .. }))
                .collect()
        };

        // Repainted, and repainted again: the listing is still asked for, and nothing is a visit.
        for _ in 0..3 {
            assert_eq!(
                browses(h.frame_raw()),
                vec![FieldAction::BrowseFolder {
                    path: anchor.clone(),
                    visited: false,
                }],
            );
        }

        // Somewhere the person actually went inside the window is.
        let inside = anchor.join("notes");
        h.screen.folder_at.insert(id, inside.clone());
        assert_eq!(
            browses(h.frame_raw()),
            vec![FieldAction::BrowseFolder {
                path: inside,
                visited: true,
            }],
        );
    }

    /// Typing on bare Home is how the search opens. A letter is somebody starting to name an
    /// application, and it used to fall on the floor until the bar was clicked with the pointer.
    #[test]
    fn a_letter_typed_on_bare_home_opens_the_search_with_that_letter_in_it() {
        let mut h = Harness::new();
        assert!(!h.screen.search_focused);
        h.key(KeyInput::Character('p'));
        assert!(
            h.screen.search_focused,
            "the letter did not open the search"
        );
        assert_eq!(h.screen.search.text(), "p");
        // And the field itself takes the next one, so a word arrives whole.
        h.key(KeyInput::Character('c'));
        assert_eq!(h.screen.search.text(), "pc");
    }

    /// A chord is not typing. Home's own keys keep working with a modifier held.
    #[test]
    fn a_modified_letter_is_a_chord_and_never_opens_the_search() {
        let mut h = Harness::new();
        h.input.modifiers_changed(pixelkit_shell::Modifiers {
            shift: false,
            control: true,
            alt: false,
        });
        h.key(KeyInput::Character('c'));
        assert!(!h.screen.search_focused);
        assert_eq!(h.screen.search.text(), "");
    }

    /// Escape leaves the search and empties it: the next letter typed starts a new question
    /// rather than continuing one asked minutes ago.
    #[test]
    fn escape_clears_the_query_as_well_as_unfocusing_it() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("image");
        h.frame();
        h.key(KeyInput::Escape);
        assert!(!h.screen.search_focused);
        assert_eq!(h.screen.search.text(), "");
    }

    /// One highlight, wherever it came from. The pointer moves the same cursor the arrow keys
    /// move, so Enter always opens the row that looks chosen.
    #[test]
    fn hovering_a_row_moves_the_one_highlight_and_enter_opens_that_row() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        // Both of the harness's applications match: "GNU **I**mage…" by a word, "F**i**les" by a
        // substring, in that order.
        h.screen.search.set_text("i");
        h.frame();
        assert_eq!(
            h.screen.omnibox.highlighted, 0,
            "the best match starts chosen"
        );

        h.input
            .cursor_moved((W / 2) as f32, omnibox_row_y(1) as f32);
        h.frame();
        assert_eq!(
            h.screen.omnibox.highlighted, 1,
            "the pointer did not move the highlight"
        );
        let actions = h.key(KeyInput::Enter);
        assert_eq!(
            actions,
            vec![FieldAction::Launch("org.templaros.Files".parse().unwrap())],
            "Enter opened a row other than the one that looked chosen"
        );
    }

    /// Typing narrows the same list, and Enter commits the highlighted row — which with a fresh
    /// query is the best match.
    #[test]
    fn typing_narrows_the_omnibox_and_enter_commits_the_best_match() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("image");
        h.frame();

        let gimp: AppId = "desktop-import.gimp.desktop".parse().unwrap();
        let actions = h.click(W / 2, omnibox_row_y(0));
        assert_eq!(actions, vec![FieldAction::Launch(gimp.clone())]);

        h.screen.search.set_text("image");
        h.screen.search_focused = true;
        let actions = h.key(KeyInput::Enter);
        assert!(
            actions.contains(&FieldAction::Launch(gimp)),
            "Enter did not commit the best match: {actions:?}"
        );
        assert!(
            h.screen.search.text().is_empty(),
            "a launched query is cleared, so the next keystroke starts a new search"
        );
    }

    /// A query nothing matches launches nothing. It still *asks* — the file service has not
    /// answered yet and may well have something — but it commits to nothing on its own.
    #[test]
    fn enter_with_a_query_that_matches_nothing_launches_nothing() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("no such application");
        let actions = h.key(KeyInput::Enter);
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, FieldAction::Launch(_))),
            "{actions:?}"
        );
        assert_eq!(h.screen.search.text(), "no such application");
    }

    /// Every keystroke is not a search. The question is asked once per change of query, and the
    /// host is what waits for the typing to stop.
    #[test]
    fn the_omnibox_asks_the_file_service_once_per_query_not_once_per_frame() {
        let mut h = Harness::new();
        h.screen.files_expanded = true;
        h.screen.search_focused = true;
        h.screen.search.set_text("report");
        let first = h.frame();
        assert!(
            first.iter().any(|action| matches!(
                action,
                FieldAction::RunSearch { query, .. } if query == "report"
            )),
            "{first:?}"
        );
        let again = h.frame();
        assert!(
            !again
                .iter()
                .any(|action| matches!(action, FieldAction::RunSearch { .. })),
            "the same question was asked twice: {again:?}"
        );
    }

    /// The field on bare Home is the application launcher: typing there never asks the file
    /// service, so a browser's name cannot come back as a list of unrelated files.
    #[test]
    fn typing_on_bare_home_never_asks_the_file_service() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("firefox");
        let actions = h.frame();
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, FieldAction::RunSearch { .. })),
            "{actions:?}"
        );
        assert_eq!(h.screen.scope(), crate::omnibox::Scope::Applications);
    }

    #[test]
    fn field_selection_emits_one_bulk_stash_and_one_keyboard_group_move() {
        let mut h = Harness::new();
        h.frame();
        h.input.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        assert!(h.key(KeyInput::Character('a')).is_empty());
        let selected = h.screen.selection.to_vec();
        assert!(selected.len() > 1);
        h.input.modifiers_changed(Modifiers::default());
        assert_eq!(
            h.key(KeyInput::Delete),
            vec![FieldAction::Apply(FieldOp::StashMany {
                ids: selected.clone(),
            })]
        );

        let files = tile_id_for("app:org.templaros.Files");
        let editor = tile_id_for("app:org.example.Editor");
        h.screen.selection.replace_many([files, editor]);
        h.screen.focus = Some(files);
        let before = h.doc.location(files).unwrap().placement.cell;
        h.key(KeyInput::Character(' '));
        h.key(KeyInput::Down);
        let actions = h.key(KeyInput::Enter);
        let [
            FieldAction::Apply(FieldOp::MoveMany {
                ids,
                anchor,
                landing,
            }),
        ] = actions.as_slice()
        else {
            panic!("expected one MoveMany, got {actions:?}");
        };
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&files) && ids.contains(&editor));
        assert_eq!(*anchor, files);
        assert_eq!(landing.cell, Cell::new(before.col, before.row + 1));
    }

    #[test]
    fn control_click_toggles_a_tile_without_activating_it() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let rect = h.rect_of(files);
        h.input.modifiers_changed(Modifiers {
            control: true,
            ..Modifiers::default()
        });
        assert!(h.click(rect.x + rect.w / 2, rect.bottom() - 10).is_empty());
        assert!(h.screen.selection.contains(&files));
        assert!(h.click(rect.x + rect.w / 2, rect.bottom() - 10).is_empty());
        assert!(!h.screen.selection.contains(&files));
    }

    #[test]
    fn dragging_from_bare_ground_marquee_selects_intersecting_tiles() {
        let mut h = Harness::new();
        h.frame();
        let (rects, bounds) = {
            let ui = Ui::new(
                Painter::new(&mut h.buffer),
                &mut h.text,
                &mut h.input,
                design::theme(),
                Scale::ONE,
                &mut h.kernel,
            );
            (h.screen.current_rects(&h.doc, &ui), h.screen.bounds)
        };
        let start = (bounds.y..bounds.bottom())
            .step_by(4)
            .flat_map(|y| (bounds.x..bounds.right()).step_by(4).map(move |x| (x, y)))
            .find(|(x, y)| !rects.iter().any(|(_, rect)| rect.contains(*x, *y)))
            .expect("the field has bare ground between its tiles");
        let (target, target_rect) = rects[0];

        h.input.cursor_moved(start.0 as f32, start.1 as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        h.input.cursor_moved(
            (target_rect.x + target_rect.w / 2) as f32,
            (target_rect.y + target_rect.h / 2) as f32,
        );
        h.frame();
        assert!(h.screen.selection.contains(&target));
        h.input.mouse(MouseButton::Left, false);
        h.frame();
        assert!(h.screen.marquee.is_none());
    }

    #[test]
    fn help_scrolls_by_keyboard_and_wheel_without_losing_its_close_route() {
        let mut h = Harness::new();
        h.key(KeyInput::Function(1));
        assert_eq!(h.screen.help_scroll.offset(), 0);
        h.key(KeyInput::End);
        assert!(h.screen.help_scroll.offset() > 0);
        h.key(KeyInput::Home);
        assert_eq!(h.screen.help_scroll.offset(), 0);
        h.input.cursor_moved(300.0, 300.0);
        h.input.scrolled(0.0, 300.0);
        h.frame();
        assert!(h.screen.help_scroll.offset() > 0);
        h.key(KeyInput::Escape);
        assert!(!h.screen.help_open);
    }

    #[test]
    fn f1_opens_manifest_help_and_f1_or_escape_closes_it() {
        let mut h = Harness::new();
        assert_eq!(h.key(KeyInput::Function(1)), Vec::new());
        assert!(h.screen.help_open);
        assert_eq!(
            templar_types::action::action("field.help")
                .and_then(templar_types::action::ActionDecl::primary_chord),
            Some(templar_types::action::Chord::bare(
                templar_types::action::Key::Function(1)
            ))
        );

        assert_eq!(h.key(KeyInput::Escape), Vec::new());
        assert!(!h.screen.help_open);
        h.key(KeyInput::Function(1));
        h.key(KeyInput::Function(1));
        assert!(!h.screen.help_open);
    }

    #[test]
    fn ctrl_shift_r_restores_the_shelf_as_one_undoable_field_operation() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        h.doc = apply(&h.doc, &FieldOp::Stash { id: files })
            .expect("stash applies")
            .0;
        assert!(h.doc.location(files).is_none());

        h.input.modifiers_changed(Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        });
        let actions = h.key(KeyInput::Character('r'));
        let [FieldAction::Apply(op)] = actions.as_slice() else {
            panic!("expected one restore operation, got {actions:?}");
        };
        let (restored, inverse) = apply(&h.doc, op).expect("restore applies");
        assert!(restored.location(files).is_some());
        assert_eq!(
            apply(&restored, &inverse).expect("inverse applies").0,
            h.doc
        );
    }

    /// A harness whose two windows are in one group, so the field carries a group tile.
    fn grouped_harness() -> Harness {
        let mut h = Harness::new();
        for toplevel in &mut h.toplevels {
            toplevel.group = Some(templar_window::GroupInfo {
                group_id: 5,
                name: "Writing".to_owned(),
                accent: 0x0033_88ff,
            });
            toplevel.shelved = false;
        }
        h.with_window_tiles()
    }

    #[test]
    fn dropping_one_window_on_another_asks_the_compositor_to_group_them() {
        let mut h = Harness::new().with_window_tiles();
        // Two loose windows, both on the field.
        let one = field_model::session_tile_for(&h.doc, &h.toplevels[0]).expect("a tile");
        let two = field_model::session_tile_for(&h.doc, &h.toplevels[1]).expect("a tile");
        let onto = h.rect_of(two);
        let from = h.rect_of(one);
        let grab = (from.x + 10, from.y + 8);
        h.input.cursor_moved(grab.0 as f32, grab.1 as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        let target = (onto.x + onto.w / 2, onto.y + onto.h / 2);
        for step in 1..=10 {
            h.input.cursor_moved(
                (grab.0 + (target.0 - grab.0) * step / 10) as f32,
                (grab.1 + (target.1 - grab.1) * step / 10) as f32,
            );
            h.now += 100;
            h.frame();
        }
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let joined = actions.iter().find_map(|action| match action {
            FieldAction::JoinWindowGroup { window, other } => Some((*window, *other)),
            _ => None,
        });
        assert_eq!(
            joined,
            Some((WindowId(1), WindowId(2))),
            "dropping one window's tile on another's groups them: {actions:?}"
        );
    }

    #[test]
    fn a_group_tile_offers_the_verbs_that_act_on_the_whole_set() {
        let mut h = grouped_harness();
        let group = field_model::group_tile_id(5);
        let tile = h.doc.tile(group).expect("the group has a tile");
        assert_eq!(tile.title, "Writing");
        assert_eq!(tile.accent, Some(0x0033_88ff));

        // Pressing the name raises the whole group.
        let rect = h.rect_of(group);
        let actions = h.click(rect.x + rect.w / 4, rect.y + 10);
        assert!(
            actions.iter().any(|action| matches!(
                action,
                FieldAction::RunWindowAction {
                    id: "window.activate-group",
                    ..
                }
            )),
            "a press on a group's name raises it: {actions:?}"
        );
    }

    #[test]
    fn the_group_menu_row_for_renaming_opens_the_question_and_sends_the_name() {
        let mut h = grouped_harness();
        let group = field_model::group_tile_id(5);
        let rect = h.rect_of(group);
        h.input
            .cursor_moved((rect.x + 6) as f32, (rect.y + 6) as f32);
        h.input.mouse(MouseButton::Right, true);
        h.input.mouse(MouseButton::Right, false);
        h.frame();
        let rows: Vec<&'static str> = h
            .screen
            .menu
            .target()
            .expect("the menu opened")
            .rows()
            .iter()
            .map(|item| item.action)
            .collect();
        assert!(rows.contains(&"window.rename-group"), "rows were {rows:?}");

        // Choosing it opens the question, and Enter sends what is in it.
        h.screen.rename_group = Some(GroupRename {
            window: WindowId(1),
            field: TextFieldState::with_text("Reading".to_owned()),
        });
        h.screen.menu.close();
        let actions = h.key(KeyInput::Enter);
        let named = actions.iter().find_map(|action| match action {
            FieldAction::RenameWindowGroup { window, name } => Some((*window, name.clone())),
            _ => None,
        });
        assert_eq!(named, Some((WindowId(1), "Reading".to_owned())));
        assert!(h.screen.rename_group.is_none(), "the question closed");
    }

    #[test]
    fn dragging_a_member_out_of_its_group_asks_the_compositor_to_ungroup_it() {
        let h = grouped_harness();
        let group = field_model::group_tile_id(5);
        let member = field_model::session_tile_for(&h.doc, &h.toplevels[0]).expect("a tile");
        assert_eq!(
            h.doc.location(member).and_then(|at| at.container),
            Some(group),
            "the member starts inside its group"
        );
        // Inside the group tile the member has no rect of its own on the field, so the drag is
        // exercised through the grammar the drop consults, which is what the pointer path calls.
        let operation = templar_field::operation_for(
            member,
            h.doc.kind_of(member).expect("a kind"),
            &Target::Empty {
                container: None,
                cell: Cell::new(0, 0),
            },
            FieldScreen::drop_origin(&h.doc, member),
            templar_field::DropModifiers::MOVE,
        );
        assert_eq!(operation, Some(Operation::LeaveWindowGroup));
    }

    #[test]
    fn dragging_a_tile_by_its_band_moves_it_and_shows_a_ghost_first() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let rect = h.rect_of(files);
        let grab = (rect.x + 10, rect.y + 8);
        h.input.cursor_moved(grab.0 as f32, grab.1 as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(matches!(h.screen.drag, Drag::Pressed { .. }));
        // Drag three cells right, in steps so velocity stays low.
        let (cols, rows) = (h.doc.cols, h.doc.rows);
        let one = templar_field::cell_rect(
            cols,
            rows,
            to_pixel(h.screen.bounds),
            Cell::new(0, 0),
            Span::new(1, 1),
            GAP,
        );
        let dx = (one.w + GAP) * 3;
        for step in 1..=10 {
            h.input
                .cursor_moved((grab.0 + dx * step / 10) as f32, grab.1 as f32);
            h.now += 100;
            h.frame();
        }
        assert!(h.screen.drag.is_lifted());
        assert!(h.screen.animating());
        let before = h.doc.location(files).unwrap().placement.cell;
        h.input.mouse(MouseButton::Left, false);
        let raw: Vec<FieldAction> = h
            .frame_raw()
            .into_iter()
            .filter(|a| !matches!(a, FieldAction::NeedTreemap { .. }))
            .collect();
        let [
            FieldAction::DropMetrics(metrics),
            FieldAction::Apply(FieldOp::Move { id, landing }),
        ] = raw.as_slice()
        else {
            panic!("expected the drop's metrics then one Move, got {raw:?}");
        };
        assert!(!metrics.thrown, "throwing is off by default");
        assert!(!metrics.semantic, "a move is placement only");
        assert!(!metrics.relift);
        assert!(
            metrics.lifted_for_ms >= 900,
            "ten 100 ms steps in the air: {metrics:?}"
        );
        assert_eq!(*id, files);
        assert_eq!(landing.cell, Cell::new(before.col + 3, before.row));
        assert_eq!(h.screen.drag, Drag::Idle);
    }

    /// An object-like tile (here an app) lifts from anywhere in its body, not just the band —
    /// the gesture every desktop taught — while an instrument tile (a drawer widget) still
    /// grabs only by its band, so a press on its controls can never lift it.
    #[test]
    fn object_tiles_drag_by_their_whole_body_and_instruments_do_not() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let rect = h.rect_of(files);
        // Press well below the band, near the tile's bottom edge (clear of the corner grip).
        h.input
            .cursor_moved((rect.x + 6) as f32, (rect.bottom() - 6) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(
            matches!(h.screen.drag, Drag::Pressed { .. }),
            "a body press on an app tile arms the drag: {:?}",
            h.screen.drag
        );
        h.input.mouse(MouseButton::Left, false);
        h.frame();
        assert_eq!(h.screen.drag, Drag::Idle, "an unmoved press stays a click");

        let files_object = field_model::files_tile_id();
        let rect = h.rect_of(files_object);
        h.input
            .cursor_moved((rect.x + 6) as f32, (rect.bottom() - 6) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert_eq!(
            h.screen.drag,
            Drag::Idle,
            "a widget's body press is its own, not a grab"
        );
        h.input.mouse(MouseButton::Left, false);
        h.frame();
    }

    /// The Files object is the widget a person actually grabs, so the grip is tested on it.
    fn downloads_tile() -> TileId {
        field_model::files_tile_id()
    }

    /// A field of widgets placed exactly where the test wants them, so a resize test is about
    /// the gesture rather than about whichever drawers happen to be adjacent today.
    fn doc_with(cols: u16, rows: u16, tiles: &[(u64, Cell, SpanRange)]) -> FieldDocument {
        let mut doc = FieldDocument::new("test", cols, rows);
        for (id, cell, spans) in tiles {
            let tile = Tile {
                id: TileId(*id),
                kind: TileKind::Widget {
                    widget: "probe".into(),
                    scope: None,
                },
                title: format!("t{id}"),
                spans: *spans,
                accent: None,
            };
            doc = apply(
                &doc,
                &FieldOp::Place {
                    tile,
                    landing: Landing {
                        container: None,
                        cell: *cell,
                    },
                },
            )
            .expect("the test's own placement fits")
            .0;
        }
        doc
    }

    /// The pixel centre of one grid cell, for aiming a resize drag at an exact span.
    fn cell_centre(h: &FieldScreen, doc: &FieldDocument, cell: Cell) -> (i32, i32) {
        let rect = from_pixel(templar_field::cell_rect(
            doc.cols,
            doc.rows,
            to_pixel(h.bounds),
            cell,
            Span::new(1, 1),
            GAP,
        ));
        (rect.x + rect.w / 2, rect.y + rect.h / 2)
    }

    fn grip_of(h: &mut Harness, tile: TileId) -> Rect {
        let rect = h.rect_of(tile);
        let ui = Ui::new(
            Painter::new(&mut h.buffer),
            &mut h.text,
            &mut h.input,
            design::theme(),
            Scale::ONE,
            &mut h.kernel,
        );
        FieldScreen::grip(rect, &ui)
    }

    #[test]
    fn dragging_a_tiles_corner_resizes_it_without_moving_it() {
        let mut h = Harness::new();
        h.frame();
        let range = SpanRange::new(Span::new(1, 1), Span::new(2, 2), Span::new(4, 4));
        h.doc = doc_with(
            h.doc.cols,
            h.doc.rows,
            &[(1, Cell::new(0, 0), range), (2, Cell::new(6, 0), range)],
        );
        let subject = TileId(1);
        let rect = h.rect_of(subject);
        let before = h.doc.location(subject).unwrap().placement;

        let grip = grip_of(&mut h, subject);
        assert!(
            grip.contains(rect.right() - 2, rect.bottom() - 2),
            "the grip is the bottom-right corner"
        );

        h.input
            .cursor_moved((grip.x + 2) as f32, (grip.y + 2) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert_eq!(
            h.screen.resize.map(|(id, _)| id),
            Some(subject),
            "pressing the corner starts a resize, not a move"
        );
        assert_eq!(
            h.screen.drag,
            Drag::Idle,
            "the move gesture stays out of it"
        );

        // Aim at cell (2,2): a tile anchored at (0,0) reaching that cell is 3x3.
        let (x, y) = cell_centre(&h.screen, &h.doc, Cell::new(2, 2));
        h.input.cursor_moved(x as f32, y as f32);
        h.frame();
        assert_eq!(h.screen.resize, Some((subject, Span::new(3, 3))));

        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let [FieldAction::Apply(FieldOp::Resize { id, span })] = actions.as_slice() else {
            panic!("expected one Resize, got {actions:?}");
        };
        assert_eq!((*id, *span), (subject, Span::new(3, 3)));
        assert!(h.screen.resize.is_none());

        h.apply(&actions);
        let after = h.doc.location(subject).unwrap().placement;
        assert_eq!(after.cell, before.cell, "a resize never moves the tile");
        assert_eq!(after.span, Span::new(3, 3), "there was room, so it grew");
    }

    #[test]
    fn growing_into_a_neighbour_narrows_it_against_its_far_edge() {
        let mut h = Harness::new();
        h.frame();
        let range = SpanRange::new(Span::new(1, 1), Span::new(2, 2), Span::new(4, 4));
        h.doc = doc_with(
            h.doc.cols,
            h.doc.rows,
            &[(1, Cell::new(0, 0), range), (2, Cell::new(2, 0), range)],
        );
        let (subject, neighbour) = (TileId(1), TileId(2));
        let before = h.doc.location(neighbour).unwrap().placement;
        assert_eq!(before.right(), 4);

        let grip = grip_of(&mut h, subject);
        h.input
            .cursor_moved((grip.x + 2) as f32, (grip.y + 2) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        let (x, y) = cell_centre(&h.screen, &h.doc, Cell::new(2, 1));
        h.input.cursor_moved(x as f32, y as f32);
        h.frame();
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        h.apply(&actions);

        let after = h.doc.location(neighbour).unwrap().placement;
        assert_eq!(
            after.right(),
            before.right(),
            "the neighbour kept its far edge"
        );
        assert_eq!(after.span.cols, before.span.cols - 1, "it gave up a column");
        assert_eq!(
            h.doc.location(subject).unwrap().placement.span,
            Span::new(3, 2)
        );
    }

    #[test]
    fn a_block_of_minimum_size_widgets_clamps_instead_of_evicting_anyone() {
        // A field packed with widgets already at their floor, so none of them has anything to
        // give. Growing one must stop, not shove a neighbour across the field — and it must not
        // leave a no-op undo entry behind either.
        let mut h = Harness::new();
        h.frame();
        let fixed = SpanRange::new(Span::new(2, 2), Span::new(2, 2), Span::new(4, 4));
        let subject = TileId(1);
        let neighbour = TileId(2);
        h.doc = doc_with(
            h.doc.cols,
            h.doc.rows,
            &[
                (1, Cell::new(0, 0), fixed),
                (2, Cell::new(2, 0), fixed),
                (3, Cell::new(0, 2), fixed),
                (4, Cell::new(2, 2), fixed),
            ],
        );
        let before_neighbour = h.doc.location(neighbour).map(|l| l.placement);

        let grip = grip_of(&mut h, subject);
        h.input
            .cursor_moved((grip.x + 2) as f32, (grip.y + 2) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        let (x, y) = cell_centre(&h.screen, &h.doc, Cell::new(3, 3));
        h.input.cursor_moved(x as f32, y as f32);
        h.frame();
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let before = h.doc.location(subject).unwrap().placement;
        h.apply(&actions);

        assert_eq!(
            h.doc.location(subject).unwrap().placement,
            before,
            "packed at their floor, the widgets cannot make room"
        );
        assert_eq!(
            h.doc.location(neighbour).map(|l| l.placement),
            before_neighbour,
            "and nobody was evicted to pretend otherwise"
        );
    }

    #[test]
    fn releasing_the_grip_without_moving_changes_nothing() {
        let mut h = Harness::new();
        let downloads = downloads_tile();
        let grip = grip_of(&mut h, downloads);

        h.input
            .cursor_moved((grip.x + 2) as f32, (grip.y + 2) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        assert!(
            actions.is_empty(),
            "a resize to the size it already is leaves no undo entry: {actions:?}"
        );
    }

    #[test]
    fn pressing_the_grip_does_not_also_open_the_tile() {
        // The corner sits inside the tile body, which is the click target that expands a drawer.
        let mut h = Harness::new();
        let downloads = downloads_tile();
        let grip = grip_of(&mut h, downloads);
        h.click(grip.x + 2, grip.y + 2);
        assert!(
            !h.screen.files_expanded,
            "the drawer must not have opened from a grip press"
        );
    }

    /// An application installed twice offers both copies from its own tile's menu, and choosing
    /// one starts *that* copy rather than whichever the daemon would have picked.
    #[test]
    fn an_application_with_two_copies_offers_both_from_its_tile() {
        let mut h = Harness::new();
        let firefox: AppId = "desktop-import.firefox.desktop".parse().unwrap();
        h.apps.push(crate::app_search::AppEntry {
            id: firefox.clone(),
            name: "Firefox".to_owned(),
            desktop_file_id: "firefox.desktop".to_owned(),
            icon: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            no_display: false,
            logical: "logical.firefox".to_owned(),
            variants: 2,
        });
        h.variants.insert(
            "logical.firefox".to_owned(),
            vec![
                (
                    "desktop-import.firefox.desktop".to_owned(),
                    "Firefox".to_owned(),
                    "DistroPackage 128".to_owned(),
                ),
                (
                    "flatpak.org.mozilla.firefox".to_owned(),
                    "Firefox".to_owned(),
                    "Flatpak 129".to_owned(),
                ),
            ],
        );
        h.doc = apply(
            &h.doc,
            &FieldOp::Stash {
                id: field_model::files_tile_id(),
            },
        )
        .expect("stash applies")
        .0;
        field_model::pin_app(&mut h.doc, &firefox);
        let rect = h.rect_of(field_model::app_tile_id(&firefox));

        h.secondary_click(rect.x + rect.w / 2, rect.y + rect.h / 2);
        // The Files tile is on the shelf, so the field's restore row is offered too.
        let declared = MenuTarget {
            tile: Some(field_model::app_tile_id(&firefox)),
            shelved: true,
            ..MenuTarget::default()
        }
        .rows()
        .len();
        // The second copy: the row after every declared one, and after the first copy.
        let row = h.menu_row(declared + 1);
        let actions = h.click(row.x + row.w / 2, row.y + row.h / 2);
        assert_eq!(
            actions,
            vec![FieldAction::LaunchVariant(
                "flatpak.org.mozilla.firefox".parse().unwrap()
            )],
            "choosing a copy did not start that copy"
        );
    }

    /// A `Ui` at scale 1, for the pure geometry functions that need one to answer in pixels.
    fn with_ui<R>(h: &mut Harness, f: impl FnOnce(&Ui<'_>) -> R) -> R {
        let ui = Ui::new(
            Painter::new(&mut h.buffer),
            &mut h.text,
            &mut h.input,
            design::theme(),
            Scale::ONE,
            &mut h.kernel,
        );
        f(&ui)
    }

    /// Micro and Small have no heading, so manipulation waits for attention — and when it
    /// arrives it takes width from the name rather than being drawn on top of it. Focus counts
    /// as attention, which is what makes a compact tile reachable without a pointer.
    #[test]
    fn the_bar_strip_never_keeps_pixels_from_a_taller_frame() {
        use crate::surface::bar_screen::BAR_HEIGHT;
        const SENTINEL: u32 = 0x00ff_00ff;
        let mut h = Harness::new();
        h.buffer = WindowBuffer::new(W as u32, (H + BAR_HEIGHT) as u32);
        h.buffer.pixels.fill(SENTINEL);

        // The frames the shell draws before the bar exists: `app.rs::redraw_home` insets by
        // BAR_HEIGHT only while `self.bar.is_some()`, and the bar is created after the bridge
        // connects. The field runs the full height of the output and lays tiles down to it.
        h.area = Rect::new(0, 0, W, H + BAR_HEIGHT);
        h.frame();
        // Then the bar arrives and the field is one strip shorter, for the rest of the session.
        h.area = Rect::new(0, 0, W, H);
        h.frame();

        let background = design::theme().background;
        let stride = h.buffer.width as usize;
        let mut offenders: Vec<(i32, i32, u32)> = Vec::new();
        for y in H..H + BAR_HEIGHT {
            for x in 0..W {
                let pixel = h.buffer.pixels[y as usize * stride + x as usize];
                if pixel != background {
                    offenders.push((x, y, pixel));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the bar's strip kept {} pixel(s) from the taller frame, first at {:?}",
            offenders.len(),
            offenders.first()
        );
    }

    /// A defence, not the cause: `templar_field::cell_rect` extrapolates rather than clamping, so a
    /// placement outside the document's own grid — which nothing should create, but which no type
    /// forbids either — would be handed a rectangle outside the field, and below the field is the
    /// bar. `tile_rect` shoves it back inside instead.
    #[test]
    fn a_tile_placed_off_the_grid_is_drawn_back_inside_the_field() {
        let mut h = Harness::new();
        let stray = h.doc.located[0].0;
        let rows = h.doc.rows;
        h.doc.located[0].1.placement.cell = templar_field::Cell::new(2, rows);
        h.frame();
        let bounds = h.screen.bounds;
        let ui = Ui::new(
            Painter::new(&mut h.buffer),
            &mut h.text,
            &mut h.input,
            design::theme(),
            Scale::ONE,
            &mut h.kernel,
        );
        let rect = h
            .screen
            .tile_rect(&h.doc, stray, &ui)
            .expect("the stray tile still has a rectangle");
        assert!(
            rect.bottom() <= bounds.bottom() && rect.y >= bounds.y,
            "a tile placed off the grid was drawn outside the field: {rect:?} in {bounds:?}"
        );
    }

    #[test]
    fn compact_rungs_reveal_controls_only_when_hovered_or_focused_and_never_cover_the_title_line() {
        let mut h = Harness::new();
        h.frame();
        let rect = Rect::new(40, 80, 96, 96);
        let (resting, revealed) = with_ui(&mut h, |ui| {
            let strip = FieldScreen::grab_strip(rect, Representation::Small, ui);
            assert!(strip.heading.is_none(), "a compact tile has no heading");
            (
                FieldScreen::control_rects(strip, rect, 64, false, ui),
                FieldScreen::control_rects(strip, rect, 64, true, ui),
            )
        });

        assert!(resting.reveal.is_none(), "nothing revealed it");
        assert_eq!(resting.controls.w, 0, "and so it has no controls to hit");
        assert!(resting.title.w > 0, "but it still says what it is");
        assert!(resting.content.y >= resting.title.bottom() - 4);

        let reveal = revealed.reveal.expect("hover or focus reveals the strip");
        assert_eq!(reveal.x, rect.x);
        assert_eq!(reveal.w, rect.w);
        assert!(
            revealed.controls.w > 0,
            "and the controls are there to press"
        );
        assert_eq!(
            revealed.controls.right(),
            reveal.right(),
            "packed against the tile's right edge, where a heading would have put them"
        );
        assert!(
            revealed.title.right() <= revealed.controls.x,
            "the name was drawn under the controls: {:?} vs {:?}",
            revealed.title,
            revealed.controls
        );
        assert!(
            revealed.title.w > 0,
            "identity is never hidden, only shortened"
        );
        assert_eq!(
            revealed.content, resting.content,
            "revealing controls must not move the body: the strip floats over it"
        );
    }

    /// Above the compact rungs the heading is permanent, and the controls live in it — so
    /// nothing appears or disappears under the pointer, and the body never changes size.
    #[test]
    fn wide_and_up_keep_a_persistent_heading() {
        let mut h = Harness::new();
        h.frame();
        let rect = Rect::new(40, 80, 300, 220);
        let (resting, hovered) = with_ui(&mut h, |ui| {
            let strip = FieldScreen::grab_strip(rect, Representation::Wide, ui);
            let heading = strip.heading.expect("Wide and up have a heading");
            assert_eq!(heading.y, rect.y);
            assert_eq!(strip.grab, heading, "the heading is what a move grabs");
            (
                FieldScreen::control_rects(strip, rect, 64, false, ui),
                FieldScreen::control_rects(strip, rect, 64, true, ui),
            )
        });

        assert!(resting.reveal.is_none(), "a heading is not a reveal strip");
        assert_eq!(
            resting, hovered,
            "a persistent heading does not change under the pointer"
        );
        assert!(resting.controls.w > 0, "the controls are always there");
        assert!(
            resting.title.right() <= resting.controls.x,
            "the name and the controls share the heading rather than overlapping"
        );
        assert!(
            resting.content.y >= resting.title.bottom(),
            "the heading is taken from the tile, not drawn over the body"
        );
    }

    /// Reduced motion is not a shorter animation, it is none: every frame is the destination,
    /// so there is no intermediate state for a person to see at all.
    #[test]
    fn reduced_motion_expansion_has_no_intermediate_frames() {
        let from = Rect::new(20, 60, 96, 90);
        let to = Rect::new(10, 62, 1260, 760);
        let expansion = Expansion {
            from,
            started_ms: 7,
            collapsing: false,
        };
        for step in 0..200 {
            let now = 7 + step;
            assert_eq!(
                expansion.rect(to, now, 0),
                to,
                "an intermediate frame at {step}ms"
            );
            assert!(expansion.finished(now, 0));
            let progress = expansion.progress(now, 0);
            assert!(
                (progress - 1.0).abs() < f32::EPSILON,
                "progress {progress} at {step}ms is neither begun nor arrived"
            );
        }
        // And the same on the way back: reduced motion is symmetric or it is a jump one way.
        let collapsing = Expansion {
            from: to,
            started_ms: 7,
            collapsing: true,
        };
        assert_eq!(collapsing.rect(from, 7, 0), from);
    }

    /// A tile that reflowed while the object was open is still where the object lands: the
    /// collapse target is recomputed every frame rather than remembered when the collapse began.
    #[test]
    fn collapse_target_follows_a_moved_tile() {
        let mut h = Harness::new();
        h.frame();
        let files = field_model::files_tile_id();
        let before = h.rect_of(files);

        // Somewhere it certainly was not: two cells down and to the right.
        let placement = h
            .doc
            .placements_in(None)
            .into_iter()
            .find_map(|(id, placement)| (id == files).then_some(placement))
            .expect("the Files tile is placed");
        let landing = templar_field::Landing {
            container: None,
            cell: Cell::new(placement.cell.col + 2, placement.cell.row + 2),
        };
        let (moved, _) = templar_field::ops::apply(&h.doc, &FieldOp::Move { id: files, landing })
            .expect("moving the Files tile");
        h.doc = moved;
        let after = h.rect_of(files);
        assert_ne!(before, after, "the tile did not actually move");

        // The laid-out screen, borrowed out of the harness so `with_ui` can have the rest of it.
        let fallback = Rect::new(-1, -1, 1, 1);
        let doc = h.doc.clone();
        let screen = std::mem::take(&mut h.screen);
        let target = with_ui(&mut h, |ui| screen.collapse_target(&doc, fallback, ui));
        assert_eq!(
            target, after,
            "the collapse headed for where the tile used to be"
        );

        // With no Files tile at all there is nowhere to fall back to but where it grew from.
        let empty = FieldDocument::new("empty", 8, 6);
        let orphan = with_ui(&mut h, |ui| screen.collapse_target(&empty, fallback, ui));
        assert_eq!(orphan, fallback);
    }

    /// The expansion's interpolation: exact at both ends, monotone in between, and finished when
    /// it says it is. A motion that overshoots or that never quite arrives is a motion a person
    /// waits for, which is the one thing this must never be.
    #[test]
    fn expansion_is_endpoint_exact_monotone_and_terminates() {
        let from = Rect::new(20, 60, 96, 90);
        let to = Rect::new(10, 62, 1260, 760);
        let expansion = Expansion {
            from,
            started_ms: 1_000,
            collapsing: false,
        };

        assert_eq!(
            expansion.rect(to, 1_000, EXPAND_MS),
            from,
            "the first frame"
        );
        assert_eq!(
            expansion.rect(to, 1_000 + EXPAND_MS, EXPAND_MS),
            to,
            "the last frame"
        );
        assert_eq!(
            expansion.rect(to, 9_999_999, EXPAND_MS),
            to,
            "and it stays arrived"
        );
        assert!(expansion.finished(1_000 + EXPAND_MS, EXPAND_MS));
        assert!(!expansion.finished(1_000 + EXPAND_MS - 1, EXPAND_MS));

        // Monotone in every axis: the object never doubles back on itself.
        let mut previous = expansion.rect(to, 1_000, EXPAND_MS);
        let mut progress = 0.0;
        for step in 0..=EXPAND_MS {
            let now = 1_000 + step;
            let next_progress = expansion.progress(now, EXPAND_MS);
            assert!(
                next_progress >= progress,
                "progress went backwards at {step}ms"
            );
            progress = next_progress;
            let rect = expansion.rect(to, now, EXPAND_MS);
            assert!(rect.x <= previous.x, "x at {step}ms");
            assert!(rect.w >= previous.w, "w at {step}ms");
            assert!(rect.h >= previous.h, "h at {step}ms");
            previous = rect;
        }
        assert!((progress - 1.0).abs() < f32::EPSILON);
    }

    /// Reduced motion is not a shorter animation, it is none: the surface is simply where it was
    /// going, on the first frame.
    #[test]
    fn reduced_motion_arrives_immediately_rather_than_quickly() {
        let from = Rect::new(20, 60, 96, 90);
        let to = Rect::new(10, 62, 1260, 760);
        let expansion = Expansion {
            from,
            started_ms: 7,
            collapsing: false,
        };
        assert!((expansion.progress(7, 0) - 1.0).abs() < f32::EPSILON);
        assert!(expansion.finished(7, 0));
        assert_eq!(expansion.rect(to, 7, 0), to);
    }

    /// Opening the Files object grows it out of its own tile, and closing it puts it back —
    /// the surface stays drawn until it has finished falling in, because a pane that blinks out
    /// is a pane that went somewhere nobody saw.
    #[test]
    fn expanding_grows_out_of_the_tile_and_collapsing_falls_back_into_it() {
        let mut h = Harness::new();
        let files = field_model::files_tile_id();
        let rect = h.rect_of(files);

        // The object's own ground opens the home directory in the file manager — a window the
        // switcher lists — rather than growing the shell into a file manager.
        let launched = h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert!(
            launched
                .iter()
                .any(|a| matches!(a, FieldAction::OpenInFiles(_))),
            "{launched:?}"
        );
        assert!(!h.screen.files_expanded);
        // The shell-only views (Recent, Trash, Drives) still expand in place; the widget queues
        // that for the frame after the press, once the tile it grows out of is laid out.
        h.screen.pending_expand = Some(h.now);
        h.frame();
        assert!(h.screen.files_expanded);
        let expansion = h.screen.expansion.expect("it grew out of something");
        assert!(!expansion.collapsing);
        assert_eq!(expansion.from, rect, "it grew out of somewhere else");

        // Once it has arrived the expansion is spent and the real surface takes over.
        h.now += EXPAND_MS + 1;
        h.frame();
        assert!(h.screen.expansion.is_none());
        assert!(h.screen.files_expanded);

        h.key(KeyInput::Escape);
        let collapsing = h.screen.expansion.expect("it is on its way back");
        assert!(collapsing.collapsing);
        assert!(
            h.screen.files_expanded,
            "the surface vanished before it had finished going anywhere"
        );
        h.now += EXPAND_MS + 1;
        h.frame();
        assert!(!h.screen.files_expanded);
        assert!(h.screen.expansion.is_none());
    }

    #[test]
    fn escape_cancels_a_pointer_resize_until_release() {
        let mut h = Harness::new();
        let id = field_model::files_tile_id();
        let rect = h.rect_of(id);
        h.input
            .cursor_moved((rect.right() - 2) as f32, (rect.bottom() - 2) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(h.screen.resize.is_some());
        let actions = h.key(KeyInput::Escape);
        assert!(!actions.iter().any(|a| matches!(a, FieldAction::Apply(_))));
        assert!(h.screen.resize.is_none());
        h.frame();
        assert!(
            h.screen.resize.is_none(),
            "held button restarted the gesture"
        );
        h.input.mouse(MouseButton::Left, false);
        assert!(!h.frame().iter().any(|a| matches!(a, FieldAction::Apply(_))));
        assert!(!h.screen.pointer_cancelled);
    }

    #[test]
    fn clicks_inside_files_do_not_dismiss_the_window() {
        let mut h = Harness::new();
        h.frame();
        h.screen.files_expanded = true;
        h.screen.expansion = None;
        h.click(W / 2, H / 2);
        assert!(h.screen.files_expanded);
        assert!(
            h.screen.expansion.is_none(),
            "an inside click began collapse"
        );
        h.click(h.screen.bounds.x + 1, h.screen.bounds.y + 1);
        assert!(h.screen.expansion.is_some_and(|e| e.collapsing));
    }

    #[test]
    fn tab_visits_every_object_and_shift_tab_reverses_with_selection() {
        let mut h = Harness::new();
        h.frame();
        let mut visited = Vec::new();
        for _ in 0..100 {
            h.key(KeyInput::Tab);
            let id = h.screen.focus.expect("a tile receives focus");
            assert_eq!(h.screen.selection.to_vec(), vec![id]);
            if visited.first() == Some(&id) {
                break;
            }
            assert!(!visited.contains(&id), "Tab got stuck before wrapping");
            visited.push(id);
        }
        assert!(visited.len() > 2);
        h.input.modifiers_changed(Modifiers {
            shift: true,
            ..Modifiers::default()
        });
        h.key(KeyInput::Tab);
        assert_eq!(h.screen.focus, visited.last().copied());
    }

    /// Move and resize are two gestures and they must never share a pixel — at any rung, on any
    /// shape a tile can be given. The move strip is the top edge (a heading where there is one,
    /// a ten-pixel edge where there is not); the grip is the bottom-right corner.
    #[test]
    fn the_grip_never_overlaps_the_move_band_at_any_rung() {
        let mut h = Harness::new();
        h.frame();
        let ui = Ui::new(
            Painter::new(&mut h.buffer),
            &mut h.text,
            &mut h.input,
            design::theme(),
            Scale::ONE,
            &mut h.kernel,
        );
        // Every rung, and every tile shape a cell grid can produce down to one cell.
        for rung in [
            Representation::Micro,
            Representation::Small,
            Representation::Wide,
            Representation::Large,
            Representation::Full,
        ] {
            for width in [40, 98, 210, 640] {
                for height in [30, 98, 210, 640] {
                    let rect = Rect::new(11, 62, width, height);
                    let strip = FieldScreen::grab_strip(rect, rung, &ui);
                    let grip = FieldScreen::grip(rect, &ui);
                    assert!(
                        grip.y >= strip.grab.bottom(),
                        "{rung:?} at {width}x{height}: one gesture would shadow the other — \
                         grab {:?}, grip {grip:?}",
                        strip.grab
                    );
                    // And the body never starts above the strip that was taken from it.
                    assert!(strip.body.y >= rect.y && strip.body.bottom() <= rect.bottom());
                    match strip.heading {
                        Some(heading) => assert_eq!(strip.body.y, heading.bottom()),
                        None => assert_eq!(
                            strip.body, rect,
                            "{rung:?}: the compact edge was taken from the body"
                        ),
                    }
                }
            }
        }
    }

    #[test]
    fn the_keyboard_resizes_a_tile_and_escape_abandons_it() {
        let mut h = Harness::new();
        let downloads = downloads_tile();
        h.frame();
        h.screen.focus = Some(downloads);
        let before = h.doc.location(downloads).unwrap().placement.span;

        h.key(KeyInput::Character('r'));
        assert_eq!(
            h.screen.keyboard_resize,
            Some((downloads, before)),
            "`r` starts from the size the tile already is"
        );
        h.key(KeyInput::Right);
        assert_eq!(
            h.screen.keyboard_resize,
            Some((downloads, Span::new(before.cols + 1, before.rows)))
        );
        h.key(KeyInput::Escape);
        assert!(h.screen.keyboard_resize.is_none());

        // …and Enter commits instead.
        h.key(KeyInput::Character('r'));
        h.key(KeyInput::Down);
        let actions = h.key(KeyInput::Enter);
        let [FieldAction::Apply(FieldOp::Resize { id, span })] = actions.as_slice() else {
            panic!("expected one Resize, got {actions:?}");
        };
        assert_eq!(*id, downloads);
        assert_eq!(*span, Span::new(before.cols, before.rows + 1));
    }

    #[test]
    fn a_tile_that_cannot_change_size_offers_no_grip() {
        let mut h = Harness::new();
        h.frame();
        // App tiles run from 1x1 to 2x2, drawers from 2x2 up; a fixed-span tile has neither.
        let fixed = Tile {
            id: TileId(9_999),
            kind: TileKind::System {
                probe: "fixed".into(),
            },
            title: "fixed".into(),
            spans: SpanRange::fixed(Span::new(2, 2)),
            accent: None,
        };
        assert!(!FieldScreen::resizable(&fixed));
        let growable = Tile {
            spans: SpanRange::new(Span::new(2, 2), Span::new(2, 2), Span::new(4, 4)),
            ..fixed
        };
        assert!(FieldScreen::resizable(&growable));
    }

    // ---- typed drag and drop ------------------------------------------------------------

    impl Harness {
        /// Replace the field with just these tiles, so a drop test is about the drop and not
        /// about what else the default desktop happens to be carrying.
        fn with_only(&mut self, tiles: Vec<(TileId, TileKind, Cell)>) {
            let mut doc = FieldDocument::new("test", self.doc.cols, self.doc.rows);
            for (id, kind, cell) in tiles {
                let spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
                doc = apply(
                    &doc,
                    &FieldOp::Place {
                        tile: Tile {
                            id,
                            title: kind.name().to_owned(),
                            kind,
                            spans,
                            accent: None,
                        },
                        landing: Landing {
                            container: None,
                            cell,
                        },
                    },
                )
                .expect("the test's own placement fits")
                .0;
            }
            self.doc = doc;
            self.screen = FieldScreen::default();
        }

        /// Lift `tile` and carry it to the centre of `onto`, releasing there. `control` is the
        /// copy modifier the grammar consults.
        fn drag_onto(&mut self, tile: TileId, onto: TileId, control: bool) -> Vec<FieldAction> {
            let from = self.rect_of(tile);
            let to = self.rect_of(onto);
            // The band is the drag handle: pressing in the body is a click on the tile's own
            // content, not a lift.
            let (start_x, start_y) = (from.x + 10, from.y + 8);
            let (end_x, end_y) = (to.x + to.w / 2, to.y + to.h / 2);
            self.input.modifiers_changed(Modifiers {
                control,
                ..Modifiers::default()
            });
            self.input.cursor_moved(start_x as f32, start_y as f32);
            self.input.mouse(MouseButton::Left, true);
            self.frame();
            for step in 1..=10 {
                let x = start_x + (end_x - start_x) * step / 10;
                let y = start_y + (end_y - start_y) * step / 10;
                self.input.cursor_moved(x as f32, y as f32);
                self.now += 100;
                self.frame();
            }
            self.input.mouse(MouseButton::Left, false);
            self.frame()
        }
    }

    fn file_tile(id: u64, path: &str) -> (TileId, TileKind, Cell) {
        (
            TileId(id),
            TileKind::File {
                path: PathBuf::from(path),
            },
            Cell::new(0, 0),
        )
    }

    #[test]
    fn drag_drop_a_file_onto_a_folder_is_one_journalled_operation_and_moves_no_tile() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/report.pdf"),
            (
                TileId(2),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        let before = h.doc.clone();
        let actions = h.drag_onto(TileId(1), TileId(2), false);

        let drop = actions
            .iter()
            .find_map(|action| match action {
                FieldAction::DropFiles(drop) => Some(drop.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no file drop in {actions:?}"));
        assert_eq!(drop.sources, vec![PathBuf::from("/home/u/report.pdf")]);
        assert_eq!(drop.destination, PathBuf::from("/home/u/Documents"));
        assert!(!drop.copy, "a plain drag moves; Ctrl copies");
        // The field is untouched: the tile did not move, the file did. Applying any layout op
        // here would leave a tile pointing at a path that is no longer there.
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, FieldAction::Apply(_))),
            "a file drop must not also rearrange the field: {actions:?}"
        );
        assert_eq!(h.doc, before);
    }

    #[test]
    fn drag_drop_with_the_copy_modifier_copies_instead_of_moving() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/report.pdf"),
            (
                TileId(2),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        let actions = h.drag_onto(TileId(1), TileId(2), true);
        let drop = actions
            .iter()
            .find_map(|action| match action {
                FieldAction::DropFiles(drop) => Some(drop.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no file drop in {actions:?}"));
        assert!(drop.copy);
    }

    #[test]
    fn drag_drop_announces_the_exact_operation_before_release() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/report.pdf"),
            (
                TileId(2),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        // Mid-drag, holding over the folder: the ghost carries the label the person reads before
        // deciding to let go, and it names which of the two operations this is.
        let from = h.rect_of(TileId(1));
        let to = h.rect_of(TileId(2));
        for control in [false, true] {
            h.input.modifiers_changed(Modifiers {
                control,
                ..Modifiers::default()
            });
            let (start_x, start_y) = (from.x + 10, from.y + 8);
            h.input.cursor_moved(start_x as f32, start_y as f32);
            h.input.mouse(MouseButton::Left, true);
            h.frame();
            for step in 1..=10 {
                let x = start_x + ((to.x + to.w / 2) - start_x) * step / 10;
                let y = start_y + ((to.y + to.h / 2) - start_y) * step / 10;
                h.input.cursor_moved(x as f32, y as f32);
                h.now += 100;
                h.frame();
            }
            let ui = Ui::new(
                Painter::new(&mut h.buffer),
                &mut h.text,
                &mut h.input,
                design::theme(),
                Scale::ONE,
                &mut h.kernel,
            );
            let ghost = h.screen.ghost(&h.doc, &ui).expect("a ghost while lifted");
            assert_eq!(
                ghost.label,
                Some(if control {
                    "COPY INTO FOLDER"
                } else {
                    "MOVE INTO FOLDER"
                }),
                "control={control}"
            );
            drop(ui);
            h.input.mouse(MouseButton::Left, false);
            h.frame();
        }
    }

    #[test]
    fn drag_drop_carries_a_whole_selection_as_one_operation() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/one.txt"),
            file_tile(2, "/home/u/two.txt"),
            (
                TileId(3),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        h.screen.selection.replace(TileId(1));
        h.screen.selection.toggle(TileId(2));
        let actions = h.drag_onto(TileId(1), TileId(3), false);
        let drop = actions
            .iter()
            .find_map(|action| match action {
                FieldAction::DropFiles(drop) => Some(drop.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no file drop in {actions:?}"));
        // One operation naming both files, never two operations of one file each: the person
        // performed one action and gets one progress report, one error and one undo.
        assert_eq!(drop.sources.len(), 2, "{:?}", drop.sources);
        assert!(drop.sources.contains(&PathBuf::from("/home/u/one.txt")));
        assert!(drop.sources.contains(&PathBuf::from("/home/u/two.txt")));
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, FieldAction::DropFiles(_)))
                .count(),
            1
        );
    }

    #[test]
    fn drag_drop_onto_bare_home_moves_the_tile_and_never_touches_the_file() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/report.pdf"),
            (
                TileId(2),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        // Dropped on empty field rather than on the folder: placement only, which is exactly
        // what an unlabelled drop means. Bare Home never turns a drop into a file operation.
        let from = h.rect_of(TileId(1));
        let (start_x, start_y) = (from.x + 10, from.y + 8);
        h.input.cursor_moved(start_x as f32, start_y as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        for step in 1..=10 {
            h.input
                .cursor_moved(start_x as f32, (start_y + 200 * step / 10) as f32);
            h.now += 100;
            h.frame();
        }
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, FieldAction::DropFiles(_))),
            "empty field is not a file destination: {actions:?}"
        );
    }

    #[test]
    fn drag_drop_distinguishes_a_folder_a_pane_an_application_and_bare_field() {
        // The four typed destinations, told apart by the grammar the screen consults — one
        // source tile, four different things underneath it, four different answers.
        let file = TileKind::File {
            path: PathBuf::from("/home/u/report.pdf"),
        };
        let folder = templar_field::Target::Tile {
            id: TileId(2),
            kind: TileKind::Folder {
                path: PathBuf::from("/home/u/Documents"),
            },
            accepts_open: false,
        };
        let pane = templar_field::Target::Tile {
            id: TileId(3),
            kind: TileKind::Tray {
                tray_id: templar_field::TrayId(1),
            },
            accepts_open: false,
        };
        let application = templar_field::Target::Tile {
            id: TileId(4),
            kind: TileKind::App {
                app_id: "org.templaros.Viewer".parse().unwrap(),
            },
            accepts_open: true,
        };
        let bare = templar_field::Target::Empty {
            container: None,
            cell: Cell::new(6, 3),
        };
        let plain = templar_field::DropModifiers::MOVE;

        // A real folder is a real file operation, and only this one is.
        let onto_folder = templar_field::operation_for(
            TileId(1),
            &file,
            &folder,
            templar_field::DropOrigin::LOOSE,
            plain,
        )
        .unwrap();
        assert!(onto_folder.is_file_operation());
        assert_eq!(onto_folder.label(), Some("MOVE INTO FOLDER"));

        // A shortcut pane takes a reference: the file stays exactly where it is, and the label
        // says so before release.
        let onto_pane = templar_field::operation_for(
            TileId(1),
            &file,
            &pane,
            templar_field::DropOrigin::LOOSE,
            plain,
        )
        .unwrap();
        assert!(!onto_pane.is_file_operation());
        assert_eq!(
            onto_pane.label(),
            Some("ADD REFERENCE · files remain in place")
        );

        // An application that opens this file resolves to Open With.
        assert_eq!(
            templar_field::operation_for(
                TileId(1),
                &file,
                &application,
                templar_field::DropOrigin::LOOSE,
                plain
            ),
            Some(templar_field::Operation::OpenWith { app: TileId(4) })
        );

        // Bare field is placement only — no label, no file operation, and never a loose tile
        // conjured out of a file, which `field_model::route_file_drop` owns.
        let onto_bare = templar_field::operation_for(
            TileId(1),
            &file,
            &bare,
            templar_field::DropOrigin::LOOSE,
            plain,
        )
        .unwrap();
        assert_eq!(onto_bare.label(), None);
        assert!(!onto_bare.is_file_operation());
    }

    #[test]
    fn drag_drop_out_of_home_exports_a_uri_list_for_files_and_nothing_for_anything_else() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/one.txt"),
            file_tile(2, "/home/u/two.txt"),
            (
                TileId(3),
                TileKind::Widget {
                    widget: "battery".into(),
                    scope: None,
                },
                Cell::new(4, 0),
            ),
        ]);
        // Nothing lifted: nothing on offer.
        assert_eq!(h.screen.dragged_uri_list(&h.doc), None);

        // Lift one file and hold it.
        let from = h.rect_of(TileId(1));
        h.input
            .cursor_moved((from.x + 10) as f32, (from.y + 8) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        for step in 1..=6 {
            h.input
                .cursor_moved((from.x + 10 + step * 12) as f32, (from.y + 8) as f32);
            h.now += 60;
            h.frame();
        }
        assert_eq!(
            h.screen.dragged_uri_list(&h.doc).as_deref(),
            Some("file:///home/u/one.txt\r\n")
        );

        // A multi-selection exports every file it carries, in one payload.
        h.screen.selection.replace(TileId(1));
        h.screen.selection.toggle(TileId(2));
        assert_eq!(
            h.screen.dragged_uri_list(&h.doc).as_deref(),
            Some("file:///home/u/one.txt\r\nfile:///home/u/two.txt\r\n")
        );
        h.input.mouse(MouseButton::Left, false);
        h.frame();

        // A widget carries no file, so it offers nothing rather than an empty list — an empty
        // uri-list is a promise of files that turns out to be a lie.
        let widget = h.rect_of(TileId(3));
        h.input
            .cursor_moved((widget.x + 10) as f32, (widget.y + 8) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        for step in 1..=6 {
            h.input
                .cursor_moved((widget.x + 10 + step * 12) as f32, (widget.y + 8) as f32);
            h.now += 60;
            h.frame();
        }
        assert_eq!(h.screen.dragged_uri_list(&h.doc), None);
    }

    #[test]
    fn data_device_a_drag_is_only_exported_once_it_is_actually_lifted() {
        let mut h = Harness::new();
        h.with_only(vec![
            file_tile(1, "/home/u/one.txt"),
            (
                TileId(2),
                TileKind::Folder {
                    path: PathBuf::from("/home/u/Documents"),
                },
                Cell::new(4, 0),
            ),
        ]);
        let from = h.rect_of(TileId(1));

        // Pressed but not moved: this is a click on its way to happening, not a drag. Starting a
        // `wl_data_device` drag here would turn the compositor's grab on and the click would
        // never arrive.
        h.input
            .cursor_moved((from.x + 10) as f32, (from.y + 8) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(
            !h.screen.drag_is_lifted(),
            "a press is not a lift, and must not export a drag"
        );

        // Moved far enough to be a lift: now there is a drag, and it is carrying the file.
        for step in 1..=6 {
            h.input
                .cursor_moved((from.x + 10 + step * 14) as f32, (from.y + 8) as f32);
            h.now += 60;
            h.frame();
        }
        assert!(h.screen.drag_is_lifted());
        assert_eq!(
            h.screen.dragged_file_paths(&h.doc),
            vec![PathBuf::from("/home/u/one.txt")]
        );

        // Released: nothing is lifted, so the next press starts from scratch rather than
        // continuing to look like an export in progress.
        h.input.mouse(MouseButton::Left, false);
        h.frame();
        assert!(!h.screen.drag_is_lifted());
        assert!(h.screen.dragged_file_paths(&h.doc).is_empty());
    }

    #[test]
    fn a_project_zooms_in_and_its_children_can_be_moved_within_and_back_out() {
        let mut h = Harness::new();
        // This is a test about zooming into a project, not about how much of a 11×7 field Home's
        // own furniture claims: the Files object is stashed so the hand-built project below has a
        // quadrant to land in on the small test grid.
        h.doc = apply(
            &h.doc,
            &FieldOp::Stash {
                id: field_model::files_tile_id(),
            },
        )
        .expect("stash applies")
        .0;
        let files = tile_id_for("app:org.templaros.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let op = FieldOp::CreateProject {
            project: Tile {
                id: field_model::project_tile_id("field-1"),
                kind: TileKind::Project {
                    project_id: "field-1".into(),
                },
                title: "Pair".into(),
                spans: field_model::project_spans(h.doc.cols, h.doc.rows),
                accent: None,
            },
            landing: Landing {
                container: None,
                cell: Cell::new(6, 2),
            },
            absorb: vec![files, editor],
        };
        h.apply(&[FieldAction::Apply(op)]);
        let project = field_model::project_tile_id("field-1");

        // Click the project's body: zoomed in, children have rects, the field's tiles do not.
        let rect = h.rect_of(project);
        h.click(rect.x + rect.w / 2, rect.bottom() - 10);
        assert_eq!(h.screen.zoomed, Some(project));
        h.frame();
        assert!(h.rect_of(files).w > 0);
        let inbox = field_model::files_tile_id();
        h.frame();
        {
            let ui = Ui::new(
                Painter::new(&mut h.buffer),
                &mut h.text,
                &mut h.input,
                design::theme(),
                Scale::ONE,
                &mut h.kernel,
            );
            assert_eq!(h.screen.tile_rect(&h.doc, inbox, &ui), None);
        }

        // Keyboard move inside the project stays inside it.
        h.key(KeyInput::Right);
        assert_eq!(h.screen.focus, Some(files).or(Some(editor)));
        let focused = h.screen.focus.unwrap();
        h.key(KeyInput::Character(' '));
        h.key(KeyInput::Down);
        let actions = h.key(KeyInput::Enter);
        let [FieldAction::Apply(FieldOp::Move { id, landing })] = actions.as_slice() else {
            panic!("expected a Move, got {actions:?}");
        };
        assert_eq!(*id, focused);
        assert_eq!(landing.container, Some(project));
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(focused), Some(project));

        // Backspace moves it back out next to the project; Escape leaves the zoom.
        h.frame();
        h.screen.focus = Some(focused);
        let actions = h.key(KeyInput::Backspace);
        let [FieldAction::Apply(FieldOp::Move { landing, .. })] = actions.as_slice() else {
            panic!("expected a Move out, got {actions:?}");
        };
        assert_eq!(landing.container, None);
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(focused), None);
        h.key(KeyInput::Escape);
        assert_eq!(h.screen.zoomed, None);
    }

    /// Nothing of Home is drawn where a window is, so a translucent client composites over flat
    /// ground and the windows behind it — never over tiles, text or a live preview of itself.
    #[test]
    fn a_windows_rectangle_is_flat_ground_so_nothing_of_home_shows_through_it() {
        let mut h = Harness::new();
        h.toplevels[0].shelved = true;
        h.toplevels[1].shelved = true;
        let tile = h.rect_of(field_model::files_tile_id());
        let (x, y) = ((tile.x + tile.w / 2) as u32, (tile.y + tile.h / 2) as u32);
        let at = |h: &Harness, x: u32, y: u32| h.buffer.pixels[(y * h.buffer.width + x) as usize];

        h.frame();
        let bare = at(&h, x, y);
        assert_ne!(
            bare & 0x00ff_ffff,
            design::ground(),
            "with nothing in front of it the Files object is drawn on the field"
        );

        // A window over that tile: what a translucent client composites over is ground alone.
        h.toplevels[0].shelved = false;
        h.toplevels[0].geometry = templar_window::Rect::new(tile.x, tile.y, tile.w, tile.h);
        h.frame();
        assert_eq!(
            at(&h, x, y) & 0x00ff_ffff,
            design::ground(),
            "under a window the field is ground and nothing else"
        );

        // Parking it covers nothing, which is the whole point of parking, and revealing Home puts
        // the field in front of every window, so neither occludes anything.
        h.toplevels[0].shelved = true;
        h.frame();
        assert_eq!(at(&h, x, y), bare, "a parked window covers nothing");
    }

    #[test]
    fn dropping_one_application_on_another_makes_an_app_group_not_a_project() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let from = h.rect_of(files);
        let to = h.rect_of(editor);
        h.input
            .cursor_moved((from.x + 10) as f32, (from.y + 8) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        // To the middle of the target tile, both axes: the two tiles do not have to share a row
        // for a drop onto one to mean anything, and a horizontal-only drag stops landing on it
        // the moment the grid's shape changes.
        let (start_x, start_y) = ((from.x + 10) as f32, (from.y + 8) as f32);
        let (end_x, end_y) = (to.x + to.w / 2, to.y + to.h / 2);
        for step in 1..=10 {
            let t = step as f32 / 10.0;
            h.input.cursor_moved(
                start_x + (end_x as f32 - start_x) * t,
                start_y + (end_y as f32 - start_y) * t,
            );
            h.now += 100;
            h.frame();
        }
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let [
            FieldAction::Apply(FieldOp::CreateProject {
                project, absorb, ..
            }),
        ] = actions.as_slice()
        else {
            panic!("expected CreateProject, got {actions:?}");
        };
        assert_eq!(absorb, &vec![editor, files]);
        // A group of favourites, not a project: nothing on disk is created for it, and the host
        // only asks Projects1 for a `TileKind::Project`.
        assert!(
            matches!(project.kind, TileKind::AppGroup { .. }),
            "{:?}",
            project.kind
        );
        assert_eq!(project.title, "Apps");
        h.apply(&actions);
        assert_eq!(h.doc.parent_of(files), Some(project.id));
        assert_eq!(h.doc.parent_of(editor), Some(project.id));
    }

    /// The group's icons launch; a third application dropped on the group joins it.
    #[test]
    fn an_app_group_launches_from_its_icons_and_takes_another_application() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let actions = h.drag_onto(files, editor, false);
        h.apply(&actions);
        let group = h.doc.parent_of(files).expect("grouped");
        let rect = h.rect_of(group);
        // The first icon slot: inside the body, left of centre.
        let actions = h.click(rect.x + rect.w / 5, rect.y + rect.h * 3 / 5);
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FieldAction::Launch(id) if id.as_str() == "org.example.Editor" || id.as_str() == "org.templaros.Files")),
            "{actions:?}"
        );
        assert!(
            h.screen.zoomed.is_none(),
            "an icon launches; it does not open the group"
        );

        let gimp: AppId = "desktop-import.gimp.desktop".parse().unwrap();
        h.doc = apply(
            &h.doc,
            &FieldOp::Place {
                tile: field_model::app_tile(&gimp),
                landing: Landing {
                    container: None,
                    cell: Cell::new(0, 0),
                },
            },
        )
        .expect("a free cell for gimp")
        .0;
        let actions = h.drag_onto(field_model::app_tile_id(&gimp), group, false);
        let joined = actions.iter().any(|a| {
            matches!(a, FieldAction::Apply(FieldOp::Move { id, landing })
                if *id == field_model::app_tile_id(&gimp) && landing.container == Some(group))
        });
        assert!(joined, "{actions:?}");
    }

    /// Taking the last application out of a group dissolves the group in the same op.
    #[test]
    fn emptying_an_app_group_dissolves_it() {
        let mut h = Harness::new();
        let files = tile_id_for("app:org.templaros.Files");
        let editor = tile_id_for("app:org.example.Editor");
        let actions = h.drag_onto(files, editor, false);
        h.apply(&actions);
        let group = h.doc.parent_of(files).expect("grouped");
        h.screen.zoomed = Some(group);
        h.screen.focus = Some(files);
        let actions = h.key(KeyInput::Backspace);
        assert!(
            matches!(actions.as_slice(), [FieldAction::Apply(FieldOp::Move { id, .. })] if *id == files),
            "one left, so the group stays: {actions:?}"
        );
        h.apply(&actions);
        h.screen.focus = Some(editor);
        let actions = h.key(KeyInput::Backspace);
        let [FieldAction::Apply(FieldOp::Batch { ops })] = actions.as_slice() else {
            panic!("expected a move and a removal, got {actions:?}");
        };
        assert!(
            matches!(ops.as_slice(), [FieldOp::Move { id, .. }, FieldOp::Remove { id: removed }] if *id == editor && *removed == group)
        );
        h.apply(&actions);
        assert!(h.doc.tile(group).is_none());
        assert!(
            h.doc
                .location(editor)
                .is_some_and(|l| l.container.is_none())
        );
    }

    // ---- dragging out of the results --------------------------------------------------------

    impl Harness {
        /// Press on the first result row and carry it to `(x, y)` without releasing.
        fn carry_first_result_to(&mut self, x: i32, y: i32) {
            let (sx, sy) = (W / 2, omnibox_row_y(0));
            self.input.cursor_moved(sx as f32, sy as f32);
            self.input.mouse(MouseButton::Left, true);
            self.frame();
            for step in 1..=10 {
                let t = step as f32 / 10.0;
                self.input.cursor_moved(
                    sx as f32 + (x - sx) as f32 * t,
                    sy as f32 + (y - sy) as f32 * t,
                );
                self.now += 100;
                self.frame();
            }
        }
    }

    #[test]
    fn dragging_an_application_out_of_the_results_places_it_where_it_is_dropped() {
        let mut h = Harness::new();
        h.with_only(Vec::new());
        h.screen.search_focused = true;
        h.screen.search.set_text("image");
        h.frame();
        let gimp: AppId = "desktop-import.gimp.desktop".parse().unwrap();
        let (x, y) = (W - 80, H - 80);
        h.carry_first_result_to(x, y);
        assert!(
            h.screen.pending.is_some(),
            "the row's tile is being carried"
        );
        assert!(!h.screen.search_focused, "the list gave way to the field");
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let placed = actions.iter().find_map(|a| match a {
            FieldAction::Apply(FieldOp::Place { tile, .. }) => Some(tile.clone()),
            _ => None,
        });
        assert_eq!(
            placed.map(|tile| tile.kind),
            Some(TileKind::App {
                app_id: gimp.clone()
            }),
            "{actions:?}"
        );
        assert!(!actions.iter().any(|a| matches!(a, FieldAction::Launch(_))));
        assert!(h.screen.pending.is_none());
        h.apply(&actions);
        let rect = h.rect_of(field_model::app_tile_id(&gimp));
        assert!(
            rect.contains(x, y),
            "{rect:?} is not under the drop at {x},{y}"
        );
    }

    #[test]
    fn a_press_and_release_on_a_result_row_still_launches_it() {
        let mut h = Harness::new();
        h.screen.search_focused = true;
        h.screen.search.set_text("image");
        h.frame();
        h.input
            .cursor_moved((W / 2) as f32, omnibox_row_y(0) as f32);
        h.input.mouse(MouseButton::Left, true);
        h.frame();
        assert!(h.screen.pending.is_some());
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        let gimp: AppId = "desktop-import.gimp.desktop".parse().unwrap();
        assert_eq!(actions, vec![FieldAction::Launch(gimp)]);
        assert!(h.screen.pending.is_none());
        assert_eq!(h.screen.drag, Drag::Idle);
    }

    #[test]
    fn a_cancelled_drag_out_of_the_results_leaves_no_tile() {
        let mut h = Harness::new();
        h.with_only(Vec::new());
        h.screen.search_focused = true;
        h.screen.search.set_text("image");
        h.frame();
        h.carry_first_result_to(W - 80, H - 80);
        let actions = h.key(KeyInput::Escape);
        assert!(actions.is_empty(), "{actions:?}");
        assert!(h.screen.pending.is_none());
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        assert!(actions.is_empty(), "{actions:?}");
        assert!(h.doc.tiles.is_empty());
    }

    #[test]
    fn dragging_an_already_pinned_application_moves_the_tile_that_is_there() {
        let mut h = Harness::new();
        let files_app: AppId = "org.templaros.Files".parse().unwrap();
        h.with_only(vec![(
            tile_id_for("app:org.templaros.Files"),
            TileKind::App { app_id: files_app },
            Cell::new(0, 0),
        )]);
        h.screen.search_focused = true;
        h.screen.search.set_text("Files");
        h.frame();
        let files = tile_id_for("app:org.templaros.Files");
        h.carry_first_result_to(W - 80, H - 80);
        assert!(
            h.screen.pending.is_none(),
            "the pinned tile itself is carried"
        );
        assert_eq!(h.screen.drag.tile(), Some(files));
        h.input.mouse(MouseButton::Left, false);
        let actions = h.frame();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, FieldAction::Apply(FieldOp::Move { id, .. }) if *id == files)),
            "{actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, FieldAction::Apply(FieldOp::Place { .. }))),
            "{actions:?}"
        );
    }

    // ---- the context menu ---------------------------------------------------------------

    impl Harness {
        fn secondary_click(&mut self, x: i32, y: i32) -> Vec<FieldAction> {
            self.input.cursor_moved(x as f32, y as f32);
            self.input.mouse(MouseButton::Right, true);
            self.frame()
        }

        /// The rectangle of the menu's `index`-th row, asked of the menu itself so a test aims
        /// at exactly what will be drawn.
        fn menu_row(&mut self, index: usize) -> Rect {
            self.frame();
            let ui = Ui::new(
                Painter::new(&mut self.buffer),
                &mut self.text,
                &mut self.input,
                design::theme(),
                Scale::ONE,
                &mut self.kernel,
            );
            self.screen
                .menu
                .row(&ui, self.screen.bounds, index)
                .expect("the menu is open and has that row")
        }
    }

    /// Secondary-click was translated into the input layer and consumed by nothing: right-click
    /// did nothing anywhere on the field. Now it opens the menu, and over a live window's tile
    /// that menu carries the window rows.
    #[test]
    fn secondary_click_over_a_session_tile_opens_that_windows_menu() {
        let mut h = Harness::new().with_window_tiles();
        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        h.secondary_click(rect.x + rect.w / 2, rect.bottom() - 8);

        let target = h.screen.menu.target().expect("the menu opened");
        assert_eq!(target.tile, Some(open));
        assert_eq!(target.window, Some(WindowId(1)));
        let ids: Vec<&str> = target.rows().iter().map(|row| row.action).collect();
        assert!(ids.contains(&"window.close"));
        assert!(ids.contains(&"window.cycle-forward"));
        assert!(ids.contains(&"field.undo"));
    }

    #[test]
    fn secondary_click_on_bare_field_offers_the_field_rows_and_nothing_window_shaped() {
        let mut h = Harness::new();
        h.frame();
        let bounds = h.screen.bounds;
        h.secondary_click(bounds.right() - 4, bounds.bottom() - 4);
        let target = h.screen.menu.target().expect("the menu opened");
        assert_eq!(target.window, None);
        let ids: Vec<&str> = target.rows().iter().map(|row| row.action).collect();
        assert!(ids.contains(&"field.undo"));
        assert!(ids.contains(&"window.show-all"));
        assert!(!ids.contains(&"window.close"));
    }

    /// The manifest declares `window.close` with `confirm: true` because a window may hold
    /// unsaved work and the compositor cannot know. One press arms; only the second closes.
    #[test]
    fn closing_a_window_from_the_menu_takes_two_presses() {
        let mut h = Harness::new().with_window_tiles();
        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        h.secondary_click(rect.x + rect.w / 2, rect.bottom() - 8);

        let rows = h.screen.menu.target().unwrap().rows().len();
        let close_at = rows - 1;
        assert_eq!(
            h.screen.menu.target().unwrap().rows()[close_at].action,
            "window.close"
        );
        let row = h.menu_row(close_at);

        let first = h.click(row.x + row.w / 2, row.y + row.h / 2);
        assert!(
            !first
                .iter()
                .any(|action| matches!(action, FieldAction::RunWindowAction { .. })),
            "the first press must only arm the row: {first:?}"
        );
        assert!(
            h.screen.menu.is_open(),
            "and the menu must stay up to say so"
        );

        let second = h.click(row.x + row.w / 2, row.y + row.h / 2);
        assert_eq!(
            second,
            vec![FieldAction::RunWindowAction {
                id: "window.close",
                window: WindowId(1),
            }]
        );
        assert!(!h.screen.menu.is_open(), "and then it closes");
    }

    #[test]
    fn a_menu_row_reaches_the_same_thing_its_chord_does() {
        let mut h = Harness::new();
        h.frame();
        let bounds = h.screen.bounds;
        h.secondary_click(bounds.right() - 4, bounds.bottom() - 4);
        let rows: Vec<&'static str> = h
            .screen
            .menu
            .target()
            .unwrap()
            .rows()
            .iter()
            .map(|row| row.action)
            .collect();
        let undo_at = rows.iter().position(|id| *id == "field.undo").unwrap();
        let row = h.menu_row(undo_at);
        let actions = h.click(row.x + row.w / 2, row.y + row.h / 2);
        // Exactly what Ctrl+Z produces (`field_keyboard_reaches_every_action`).
        assert_eq!(actions, vec![FieldAction::Undo]);
    }

    #[test]
    fn escape_dismisses_the_menu_and_the_field_does_not_also_act_on_that_press() {
        let mut h = Harness::new().with_window_tiles();
        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        h.screen.focus = Some(open);
        h.secondary_click(rect.x + rect.w / 2, rect.bottom() - 8);
        assert!(h.screen.menu.is_open());

        let actions = h.key(KeyInput::Escape);
        assert!(!h.screen.menu.is_open());
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(
            h.screen.focus,
            Some(open),
            "the Escape belonged to the menu, not to the field's own focus"
        );
    }

    /// A click outside the menu dismisses it and is swallowed: dismissing is a gesture of its
    /// own, and must not also open whatever the menu was covering.
    #[test]
    fn clicking_away_dismisses_the_menu_without_activating_anything_underneath() {
        let mut h = Harness::new().with_window_tiles();
        let open = field_model::session_tile_for(&h.doc, &h.toplevels[0]).unwrap();
        let rect = h.rect_of(open);
        h.secondary_click(rect.x + rect.w / 2, rect.bottom() - 8);
        assert!(h.screen.menu.is_open());

        let actions = h.click(rect.x + rect.w / 2, rect.bottom() - 8);
        assert!(!h.screen.menu.is_open());
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn the_menus_navigation_rows_pick_a_real_window() {
        let toplevels = vec![
            toplevel(1, "org.templaros.Files", false),
            toplevel(3, "org.example.Other", false),
        ];
        assert_eq!(
            cycle_from(&toplevels, Some(WindowId(1)), "window.cycle-forward"),
            Some(WindowId(3))
        );
        assert_eq!(
            cycle_from(&toplevels, Some(WindowId(1)), "window.cycle-backward"),
            Some(WindowId(3)),
            "two windows wrap to each other in both directions"
        );
        assert_eq!(
            cycle_from(&toplevels, None, "window.cycle-forward"),
            Some(WindowId(1))
        );
        // A parked window is reached through the shelf, never the switcher.
        let parked = vec![toplevel(1, "org.templaros.Files", true)];
        assert_eq!(cycle_from(&parked, None, "window.cycle-forward"), None);

        let mut side_by_side = toplevels.clone();
        side_by_side[1].geometry = templar_window::Rect::new(700, 0, 640, 480);
        assert_eq!(
            group_step(&side_by_side, WindowId(1), "window.step-right"),
            Some(WindowId(3))
        );
        assert_eq!(
            group_step(&side_by_side, WindowId(3), "window.step-left"),
            Some(WindowId(1))
        );
    }
}
