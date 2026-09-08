//! The Home Field's document as the shell keeps it: the persisted layout (one day; for now an
//! in-session one) *merged* with what is live — active applications, every window the compositor
//! reports, and the standard Home drawers — so useful tiles appear without covering the field in
//! every installed desktop entry, and keep their places once they have.
//!
//! Pure: takes snapshots, returns ops. The background refresh hands the result to
//! [`merge`], and everything it decides is expressed as `lege_field::FieldOp`s applied through
//! `lege_field::apply`, so the same invariants hold here as for a drop (no overlap, bounded,
//! exact inverses) and a test needs neither a bus nor a compositor.

use std::collections::BTreeSet;

use lege_field::field::Placement;
use lege_field::field::Tray;
use lege_field::{
    Cell, FieldDocument, FieldError, FieldOp, Landing, PanelId, Span, SpanRange, Tile, TileId,
    TileKind, apply, panel_id_for,
};
use lege_types::AppId;
use lege_window::{ShellToplevel, WindowId};
use std::path::Path;

use crate::home_surface::Drawer;

/// Logical pixels per grid cell. `shell.home-field.tile-size` once the field is persisted.
///
/// Finer than a tile is tall on purpose: the cell is the step a dragged tile snaps to, so it is
/// the whole of how precisely anything can be placed. At 104 a 1080p field was 18×9 and a tile
/// could only ever land on one of nine rows, which reads as a coarse magnet rather than as
/// placement (the fifth hands-on run). 88 gives 21×11 there — a step small enough to aim with,
/// still large enough that a one-cell tile is a legible icon.
pub const TILE_SIZE: i32 = 88;
/// Logical pixels between tiles.
pub const GAP: i32 = 6;

/// The widget name the one Files object carries (`TileKind::Widget::widget`).
///
/// A widget rather than a new `TileKind`: it keeps the widget class's 8×6 ceiling, so the object
/// that is the way into every file on the machine still cannot cover the field and become a
/// maximised window wearing a tile's clothes.
pub const FILES_WIDGET: &str = "files";

/// Every `TileKind::System` probe a saved field may still be carrying, and none Home places any
/// more.
///
/// Home deals with your stuff; the bar deals with the machine (`docs/garden/garden.md`). Session,
/// notifications and the hardware readouts are the machine's business, so they belong in the
/// bar's cluster at one end of the screen rather than as furniture standing in the middle of the
/// field where the windows go. Battery, Volume and Brightness were retired earlier still (one
/// Settings tile replaced all three), and the launcher tile before them (the omnibox under the
/// search bar is the launcher).
///
/// A saved document outlives the roster that made it, so [`migrate_system_tiles`] takes them off
/// — otherwise each renders "no such widget" for as long as the layout survives, which on this
/// desktop is forever.
const RETIRED_SYSTEM_WIDGETS: [&str; 7] = [
    "battery",
    "volume",
    "brightness",
    "launcher",
    "session",
    "notifications",
    "settings",
];

/// How many cells a field of `width`×`height` logical pixels has at `tile_size` pixels a cell,
/// never fewer than 4×4 (a project tile's design floor).
pub fn grid_for(width: i32, height: i32, tile_size: i32) -> (u16, u16) {
    let tile = tile_size.max(1);
    let cols = (width / tile).clamp(4, 64) as u16;
    let rows = (height / tile).clamp(4, 64) as u16;
    (cols, rows)
}

/// One project as `Projects1.ListDetailed` reports it.
///
/// Read permissively: every field of the project object is optional on the wire, so a shell
/// talking to an older or newer daemon degrades to a name and an id rather than to nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    /// The directory the project is rooted at, empty when it declares none.
    pub root: String,
    /// The project the session is currently working in (`Projects1.GetCurrent`).
    pub is_open: bool,
    /// Archived projects stay searchable and openable — the state is a badge, never a lock.
    pub archived: bool,
}

/// What the field is merged against.
#[derive(Debug, Default)]
pub struct LiveState<'a> {
    /// Every window the compositor reports.
    pub toplevels: &'a [ShellToplevel],
    /// Every project sessiond knows (`Projects1.List`).
    pub projects: &'a [ProjectInfo],
}

/// The tile id a project's tile always has, so a project the field created by grouping and the
/// same project read back from `Projects1` are one tile, not two.
pub fn project_tile_id(project_id: &str) -> TileId {
    tile_id_for(&format!("project:{project_id}"))
}

/// The size range a project tile read from `Projects1` gets. Projects are working sets rather
/// than launcher shortcuts, so the design floor is 4×4 and the preferred size is one quadrant.
pub fn project_spans(cols: u16, rows: u16) -> SpanRange {
    SpanRange::for_kind(
        &TileKind::Project {
            project_id: String::new(),
        },
        cols,
        rows,
    )
}

/// The tile id the one Files object always has.
pub fn files_tile_id() -> TileId {
    tile_id_for("widget:files")
}

/// The drawer a widget tile stood for, back when each drawer was its own tile.
///
/// Nothing places one of these any more — [`merge_files`] places the single Files object instead —
/// but a saved document written before that is still out there, and this is how
/// [`migrate_drawer_tiles`] recognises one. It stays for as long as such a document might exist,
/// which is to say for good.
pub fn legacy_drawer_widget(widget: &str) -> Option<Drawer> {
    let name = widget.strip_prefix("drawer:")?;
    Drawer::DEFAULT
        .into_iter()
        .find(|drawer| drawer.name().eq_ignore_ascii_case(name))
}

/// The window a session tile stands for: its identifier is `window:<id>` until the compositor
/// relays the foreign-toplevel identifier, which takes over once known.
pub fn session_identifier(toplevel: &ShellToplevel, foreign: Option<&str>) -> String {
    foreign.map_or_else(|| format!("window:{}", toplevel.window_id.0), str::to_owned)
}

/// Stable tile ids: a hash of what the tile stands for, so the same app or window gets the same
/// tile across refreshes and restarts, and focus or a drag never lands on a renumbered tile.
pub fn tile_id_for(key: &str) -> TileId {
    TileId::for_key(key)
}

/// One window as the field sees it: which tile is its, and whether that window is parked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTile {
    pub tile: TileId,
    pub window: WindowId,
    pub identifier: String,
}

/// Bring `doc` up to date with `live`: a tile for every drawer, active application and window that
/// lacks one, placed by the reflow near a sensible landing; the tile of every window that is
/// gone removed. Existing tiles keep their places. Returns the ops that were applied (already
/// applied to `doc`), so a caller persisting the document can forward them.
pub fn merge(doc: &mut FieldDocument, live: &LiveState<'_>) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let mut wanted: BTreeSet<TileId> = BTreeSet::new();
    migrate_system_tiles(doc, &mut applied);
    // The eight drawer tiles are superseded by the single Files
    // object, and the migration has to run before `merge_files` so the replacement can inherit
    // the place the block held instead of landing in the default corner.
    migrate_drawer_tiles(doc, &mut applied);
    merge_files(doc, &mut wanted, &mut applied);
    merge_projects(doc, live, &mut applied);
    adopt_loose_files(doc, &mut applied);

    // Running windows leave Home. Home is where a person's *things* are — their folders, their
    // applications, their places — and a window is not a thing you keep, it is work in progress;
    // it belongs on the bar, which is where the machine's own business is reported
    // (`docs/garden/garden.md`). So no refresh places a `ParkedSession` or a `WindowGroup` tile
    // any more, and this sweep — which used to remove only the tiles whose windows had closed —
    // is what takes the ones a saved field is still carrying. It needs no migration of its own:
    // `wanted` never names them, so the first refresh after this build removes every one.
    //
    // Ordinary `Remove` ops through `try_apply`, so the removals invert exactly like any other
    // and a group tile hands its members back on undo (`ops::remove` stashes a container's
    // children into its inverse).
    //
    // Only these two kinds, and that is load-bearing — please do not widen the match. An
    // application tile is on Home because someone pinned it (`pin_app`) and a widget is on Home
    // because Home put it there; sweeping either because this refresh did not happen to ask for
    // it would delete a person's own arrangement every time a service failed to answer.
    let gone: Vec<TileId> = doc
        .tiles
        .iter()
        .filter(|tile| {
            matches!(
                tile.kind,
                TileKind::ParkedSession { .. } | TileKind::WindowGroup { .. }
            ) && !wanted.contains(&tile.id)
        })
        .map(|tile| tile.id)
        .collect();
    for id in gone {
        try_apply(doc, FieldOp::Remove { id }, &mut applied);
    }
    applied
}

/// The one default shortcut pane loose files route to. A pane is a `Tray` — references, never
/// the files themselves — and this is the reason no path through Home ever creates a loose
/// `TileKind::File` tile again: a file arriving on bare Home lands here instead
/// (`route_file_drop`), and legacy File tiles are adopted here by the merge.
pub const DEFAULT_PANE_TITLE: &str = "Shortcuts";

pub fn default_pane_tile_id() -> TileId {
    tile_id_for("pane:shortcuts")
}

pub fn desktop_folder_tile_id() -> TileId {
    tile_id_for("folder:desktop")
}

/// The tile id a folder window for `path` always has, so opening the same path twice raises the
/// window that is already there instead of stacking a second one on top of it.
pub fn folder_tile_id(path: &Path) -> TileId {
    if crate::drawers::standard_paths::desktop_dir().as_deref() == Some(path) {
        return desktop_folder_tile_id();
    }
    tile_id_for(&format!("folder:{}", path.to_string_lossy()))
}

/// Open `path` as a folder window on the field: the ops that place one, or nothing at all if a
/// window for it is already open (the caller then raises what is there).
///
/// A folder window is a tile and nothing more exotic than that, which is the whole point — it can
/// be dragged, resized, dropped into a project or a group, and it survives a restart, because
/// every one of those is something the field already does for tiles. There is no second windowing
/// system inside the shell.
pub fn open_folder_ops(doc: &FieldDocument, path: &Path) -> Vec<FieldOp> {
    lege_field::place_folder_op(doc, folder_tile_id(path), path)
        .into_iter()
        .collect()
}

/// The XDG Desktop directory as one placeable Home folder object — the visible destination for
/// software that still writes desktop files, without recreating scattered desktop icons: the
/// directory's children stay behind the folder's door, never exploded into Home tiles.
/// Add-only like every merge step; `None` (no such directory) adds nothing, which is the
/// readable degradation. The host passes `crate::drawers::standard_paths::desktop_dir()`.
pub fn merge_desktop_folder(doc: &mut FieldDocument, desktop: Option<&Path>) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let Some(path) = desktop else {
        return applied;
    };
    let id = desktop_folder_tile_id();
    if doc.tile(id).is_some() {
        return applied;
    }
    let kind = TileKind::Folder {
        path: path.to_path_buf(),
    };
    // A tray, not a pane: Desktop opens at the folder kind's floor and grows when it is pressed
    // (`FieldScreen::primary_action`). The desktop folder is somewhere things arrive, so it has
    // to be *present* on Home and it must not be the biggest thing there.
    let kind_spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
    let spans = SpanRange::new(kind_spans.min, kind_spans.min, kind_spans.max);
    let tile = Tile {
        id,
        kind,
        title: "Desktop".to_owned(),
        spans,
        accent: None,
    };
    // Beside the projects column, under the information row: a large, findable landing that
    // the reflow adjusts if the corner is taken.
    let landing = Landing {
        container: None,
        cell: Cell::new(doc.cols.saturating_sub(spans_preferred_cols(&tile)), 0),
    };
    try_apply(doc, FieldOp::Place { tile, landing }, &mut applied);
    applied
}

fn spans_preferred_cols(tile: &Tile) -> u16 {
    tile.spans.preferred.cols
}

/// The default pane's tray, created on first need: one op batch that makes the tray and places
/// its tile. Returns the tray id to reference into.
fn ensure_default_pane(
    doc: &mut FieldDocument,
    applied: &mut Vec<FieldOp>,
) -> Option<lege_field::TrayId> {
    let tile_id = default_pane_tile_id();
    if let Some(tile) = doc.tile(tile_id)
        && let TileKind::Tray { tray_id } = tile.kind
    {
        return Some(tray_id);
    }
    let tray_id = doc.next_tray_id();
    try_apply(
        doc,
        FieldOp::CreateTray {
            tray: Tray {
                id: tray_id,
                name: DEFAULT_PANE_TITLE.to_owned(),
                entries: Vec::new(),
            },
        },
        applied,
    );
    let kind = TileKind::Tray { tray_id };
    let spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
    let tile = Tile {
        id: tile_id,
        kind,
        title: DEFAULT_PANE_TITLE.to_owned(),
        spans,
        accent: None,
    };
    let landing = Landing {
        container: None,
        cell: Cell::new(doc.cols / 2, doc.rows / 2),
    };
    try_apply(doc, FieldOp::Place { tile, landing }, applied);
    match doc.tile(tile_id).map(|tile| &tile.kind) {
        Some(TileKind::Tray { tray_id }) => Some(*tray_id),
        _ => None,
    }
}

/// A new, empty shortcut pane with `title`, placed near the field's centre. The pointer route
/// behind the declared `pane.new` action.
pub fn create_pane(doc: &mut FieldDocument, title: &str) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let tray_id = doc.next_tray_id();
    try_apply(
        doc,
        FieldOp::CreateTray {
            tray: Tray {
                id: tray_id,
                name: title.to_owned(),
                entries: Vec::new(),
            },
        },
        &mut applied,
    );
    let kind = TileKind::Tray { tray_id };
    let spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
    let pane = Tile {
        id: tile_id_for(&format!("pane:{}", tray_id.0)),
        kind,
        title: title.to_owned(),
        spans,
        accent: None,
    };
    let landing = Landing {
        container: None,
        cell: Cell::new(doc.cols / 2, doc.rows / 2),
    };
    try_apply(
        doc,
        FieldOp::Place {
            tile: pane,
            landing,
        },
        &mut applied,
    );
    applied
}

/// One real file arriving on bare Home. Never a loose tile: it becomes a reference in the
/// default shortcut pane (created on first use), and the file itself does not move.
pub fn route_file_drop(doc: &mut FieldDocument, path: &Path) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let Some(tray) = ensure_default_pane(doc, &mut applied) else {
        return applied;
    };
    let already = doc
        .tray(tray)
        .is_some_and(|tray| tray.entries.iter().any(|entry| entry == path));
    if !already {
        try_apply(
            doc,
            FieldOp::AddReference {
                tray,
                path: path.to_path_buf(),
                at: None,
            },
            &mut applied,
        );
    }
    applied
}

/// Legacy loose `TileKind::File` tiles — from documents serialized before the object model
/// ruled them out — migrate into the default pane as references, losing nothing: the path is
/// preserved, the file never moves, and the tile goes away.
fn adopt_loose_files(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let loose: Vec<(TileId, std::path::PathBuf)> = doc
        .tiles
        .iter()
        .filter_map(|tile| match &tile.kind {
            TileKind::File { path } => Some((tile.id, path.clone())),
            _ => None,
        })
        .collect();
    if loose.is_empty() {
        return;
    }
    let Some(tray) = ensure_default_pane(doc, applied) else {
        return;
    };
    for (id, path) in loose {
        let already = doc
            .tray(tray)
            .is_some_and(|tray| tray.entries.contains(&path));
        if !already {
            try_apply(
                doc,
                FieldOp::AddReference {
                    tray,
                    path,
                    at: None,
                },
                applied,
            );
        }
        try_apply(doc, FieldOp::Remove { id }, applied);
    }
}

/// The panel the eight drawer tiles used to belong to. Nothing creates it any more; the id
/// survives so [`migrate_drawer_tiles`] can take it off a saved field along with its members.
pub fn drawer_panel_id() -> PanelId {
    panel_id_for("panel:drawers")
}

/// The span range the Files object accepts: a 2×2 floor so a busy field can compact it, a 4×3
/// preferred size — wide enough for the pinned locations to sit beside the recent list — and the
/// widget class's own 8×6 ceiling.
fn files_spans(doc: &FieldDocument) -> SpanRange {
    let class = SpanRange::for_kind(&files_kind(), doc.cols, doc.rows);
    SpanRange::new(Span::new(2, 2), Span::new(4, 3), class.max)
}

fn files_kind() -> TileKind {
    TileKind::Widget {
        widget: FILES_WIDGET.to_owned(),
        scope: None,
    }
}

/// The one Files object, in the corner the eight drawers used to fill between them.
///
/// One object with many sizes of representation, rather than eight shortcuts to the same file
/// service: Downloads, Recent, Documents, Media, Drives, Operations, Trash and Projects were
/// eight headings for one thing, and the block of them was a third of the field's width before a
/// single window existed.
fn merge_files(doc: &mut FieldDocument, wanted: &mut BTreeSet<TileId>, applied: &mut Vec<FieldOp>) {
    let id = files_tile_id();
    wanted.insert(id);
    if doc.tile(id).is_some() {
        return;
    }
    let tile = Tile {
        id,
        kind: files_kind(),
        title: "Files".to_owned(),
        spans: files_spans(doc),
        accent: None,
    };
    let landing = Landing {
        container: None,
        cell: Cell::new(0, 0),
    };
    try_apply(doc, FieldOp::Place { tile, landing }, applied);
}

/// Take every retired system widget off a saved field.
///
/// No successor tile, and that is the point rather than an omission: what each of these said
/// belongs to the bar now, or to the omnibox, and putting a replacement on Home would be saying
/// it twice. Kind-checked rather than trusting the id — a tile id is a hash, and removing
/// something that merely collided with one would be the worst outcome here — and expressed as
/// ordinary ops through [`try_apply`], so the removals invert exactly and a caller can forward
/// them to `HomeField1.Apply` unchanged.
fn migrate_system_tiles(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let retired: Vec<TileId> = RETIRED_SYSTEM_WIDGETS
        .iter()
        .map(|widget| (widget, tile_id_for(&format!("system:{widget}"))))
        .filter(|(widget, id)| {
            doc.tile(*id).is_some_and(
                |tile| matches!(&tile.kind, TileKind::System { probe } if probe == *widget),
            )
        })
        .map(|(_, id)| id)
        .collect();
    for id in retired {
        try_apply(doc, FieldOp::Remove { id }, applied);
    }
}

/// Turn a saved field's drawer tiles into the one Files object that replaced them, in the first
/// place any of them held, and take the drawer panel with them.
///
/// The same shape as [`migrate_hardware_tiles`] and for the same reason: these tiles name a
/// widget the shell no longer draws, so leaving them alone means up to ten rectangles reading
/// "no such widget" for the life of the layout. All ten `Drawer` variants are collected, not
/// just the eight that were ever placed by default — Network and Windows could reach the field
/// by hand, and a migration that left two of them behind would be worse than none.
///
/// Kind-checked like its predecessor: a tile id is a hash, and removing something that merely
/// collided with one would be the worst possible outcome. Expressed as ordinary ops through
/// [`try_apply`], so every removal and the placement invert exactly.
fn migrate_drawer_tiles(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let legacy: Vec<TileId> = Drawer::DEFAULT
        .into_iter()
        .map(|drawer| tile_id_for(&format!("drawer:{}", drawer.name())))
        .filter(|id| {
            doc.tile(*id).is_some_and(|tile| {
                matches!(&tile.kind, TileKind::Widget { widget, .. }
                    if legacy_drawer_widget(widget).is_some())
            })
        })
        .collect();
    let had_panel = doc.panel(drawer_panel_id()).is_some();
    if legacy.is_empty() {
        // The panel outlives its members only on a field where every drawer was stashed and then
        // removed by hand; sweeping it here keeps that field from carrying an empty group for good.
        if had_panel {
            try_apply(
                doc,
                FieldOp::RemovePanel {
                    id: drawer_panel_id(),
                },
                applied,
            );
        }
        return;
    }
    // The first of their positions in the order Home originally placed them, and the largest
    // span any of them had grown to: a person who made Downloads a full-width treemap gets a
    // Files object that size rather than the default corner tile.
    let placements: Vec<Placement> = legacy
        .iter()
        .filter_map(|id| doc.location(*id))
        .map(|located| located.placement)
        .collect();
    let inherited = placements.first().map(|first| {
        let largest = placements
            .iter()
            .map(|placement| placement.span)
            .max_by_key(|span| span.cells())
            .unwrap_or(first.span);
        (first.cell, largest)
    });
    for id in legacy {
        try_apply(doc, FieldOp::Remove { id }, applied);
    }
    if had_panel {
        try_apply(
            doc,
            FieldOp::RemovePanel {
                id: drawer_panel_id(),
            },
            applied,
        );
    }
    if doc.tile(files_tile_id()).is_some() {
        // A field that already carries the replacement wanted only the leftovers gone; placing a
        // second one is impossible anyway, and asking would move the tile in use.
        return;
    }
    let Some((cell, span)) = inherited else {
        // Every drawer was on the shelf: nothing to inherit, and `merge_files` lands the
        // replacement in its own corner.
        return;
    };
    let spans = files_spans(doc);
    let tile = Tile {
        id: files_tile_id(),
        kind: files_kind(),
        title: "Files".to_owned(),
        spans: SpanRange::new(spans.min, span.clamp(spans.min, spans.max), spans.max),
        accent: None,
    };
    let landing = Landing {
        container: None,
        cell,
    };
    try_apply(doc, FieldOp::Place { tile, landing }, applied);
}

/// Projects from the right edge, stacked down. Add-only: a project tile is never removed by a
/// merge, because a project the field just created may not have reached `Projects1` yet (or
/// sessiond may be absent), and a tile that vanished and came back would lose its place.
fn merge_projects(doc: &mut FieldDocument, live: &LiveState<'_>, applied: &mut Vec<FieldOp>) {
    let spans = project_spans(doc.cols, doc.rows);
    let col = doc.cols.saturating_sub(spans.preferred.cols);
    for (index, project) in live.projects.iter().enumerate() {
        let id = project_tile_id(&project.id);
        if let Some(tile) = doc.tile_mut(id) {
            if tile.title != project.name && !project.name.is_empty() {
                tile.title.clone_from(&project.name);
            }
            continue;
        }
        let tile = Tile {
            id,
            kind: TileKind::Project {
                project_id: project.id.clone(),
            },
            title: if project.name.is_empty() {
                project.id.clone()
            } else {
                project.name.clone()
            },
            spans,
            accent: None,
        };
        let landing = Landing {
            container: None,
            cell: Cell::new(col, (index as u16) * spans.preferred.rows),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// The tile id an application's Home tile always has, pinned or inherited from an older layout.
pub fn app_tile_id(app_id: &AppId) -> TileId {
    tile_id_for(&format!("app:{app_id}"))
}

/// Keep this application on Home: one tile, placed where it will fit, that outlives the window
/// it was pinned from.
///
/// Home used to grow an application tile for every window that opened and drop it again with the
/// last one, which meant a badge that duplicated the session tile beside it and vanished exactly
/// when a person might have wanted to relaunch. An application belongs on Home because someone
/// put it there, and for no other reason — everything else is what search is for.
///
/// Add-only and idempotent: pinning something already pinned asks for nothing.
pub fn app_tile(app_id: &AppId) -> Tile {
    Tile {
        id: app_tile_id(app_id),
        kind: TileKind::App {
            app_id: app_id.clone(),
        },
        title: short_name(app_id),
        spans: SpanRange::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 2)),
        accent: None,
    }
}

pub fn pin_app(doc: &mut FieldDocument, app_id: &AppId) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let tile = app_tile(app_id);
    if doc.tile(tile.id).is_some() {
        return applied;
    }
    // The top-left corner of the working area, where the reflow will find it the nearest free
    // cell: a pinned tile is small, and hunting for one that landed in a far corner is worse
    // than having it appear near where the eye already is.
    let landing = Landing {
        container: None,
        cell: Cell::new(0, 0),
    };
    try_apply(doc, FieldOp::Place { tile, landing }, &mut applied);
    applied
}

/// The tile id a window group's tile always has. The group id is the compositor's
/// (`lege_window::GroupInfo::group_id`), so the same group is one tile across refreshes.
pub fn group_tile_id(group_id: u64) -> TileId {
    tile_id_for(&format!("group:{group_id}"))
}

/// The title a group's tile carries: the name the group was given, or how many windows it holds
/// until somebody names it. Never empty — a tile with no title is a tile that says nothing.
pub(crate) fn group_title(name: &str, members: usize) -> String {
    if name.trim().is_empty() {
        match members {
            1 => "1 window".to_owned(),
            other => format!("{other} windows"),
        }
    } else {
        name.to_owned()
    }
}

fn try_apply(doc: &mut FieldDocument, op: FieldOp, applied: &mut Vec<FieldOp>) {
    match apply(doc, &op) {
        Ok((next, _)) => {
            *doc = next;
            applied.push(op);
        }
        // A full field, or a hash collision, leaves the tile off rather than breaking the
        // document; the text form and the logs both show what is missing.
        Err(FieldError::NoRoom | FieldError::DuplicateTile) => {}
        Err(error) => tracing::warn!(%error, ?op, "field merge op refused"),
    }
}

/// One op that brings every shelved tile back onto the field, or `None` when the shelf is empty.
///
/// Stashing had no inverse. `FieldOp::Stash` keeps the tile in the document with no location, and
/// only an immediate `field.undo` could reverse it — after a save, a tile on the shelf was
/// unreachable, and `merge` cannot help because it skips any drawer whose tile already exists and
/// must not second-guess a deliberate stash. A drawer shelved by accident and one shelved on
/// purpose look identical once written down, so the way back has to be something the user asks
/// for.
///
/// Each landing is chosen by dry-running the move here rather than guessing: `apply` is
/// deterministic (the field's own `placement_is_deterministic` property), so the batch replays on
/// the service exactly as it did locally. A tile with nowhere to go stays on the shelf instead of
/// failing the whole batch.
pub fn restore_shelved(doc: &FieldDocument) -> Option<FieldOp> {
    let shelved: Vec<TileId> = doc.shelved().map(|tile| tile.id).collect();
    if shelved.is_empty() {
        return None;
    }
    let mut probe = doc.clone();
    let mut ops = Vec::new();
    for id in shelved {
        let landings = (0..probe.rows)
            .flat_map(|row| (0..probe.cols).map(move |col| Cell::new(col, row)))
            .map(|cell| Landing {
                container: None,
                cell,
            });
        for landing in landings {
            let op = FieldOp::Move { id, landing };
            if let Ok((next, _)) = apply(&probe, &op) {
                probe = next;
                ops.push(op);
                break;
            }
        }
    }
    match ops.len() {
        0 => None,
        1 => ops.pop(),
        _ => Some(FieldOp::Batch { ops }),
    }
}

/// The tile standing for `window`, if the field has one.
pub fn session_tile_for(doc: &FieldDocument, toplevel: &ShellToplevel) -> Option<TileId> {
    let identifier = session_identifier(toplevel, None);
    let id = tile_id_for(&format!("session:{identifier}"));
    doc.tile(id).map(|tile| tile.id)
}

/// The window a session tile stands for, if it is still open.
pub fn window_for_tile<'a>(
    doc: &FieldDocument,
    tile: TileId,
    toplevels: &'a [ShellToplevel],
) -> Option<&'a ShellToplevel> {
    let TileKind::ParkedSession { identifier, .. } = &doc.tile(tile)?.kind else {
        return None;
    };
    toplevels
        .iter()
        .find(|toplevel| session_identifier(toplevel, None) == *identifier)
}

/// Place a `ParkedSession` tile per window, and a `WindowGroup` container per group holding its
/// members — what the merge used to do on every refresh, kept for the tests of the code that
/// draws and acts on those tiles.
///
/// Nothing in a running session places these any more: running windows are the bar's business
/// (`merge`'s sweep comment). The *field* still understands them, and `FieldScreen` still draws
/// and acts on them, because the bar's taskbar and its hover row are the successor consumer of
/// exactly that code.
///
// ponytail: a test-only placer over production code no production path reaches. It goes when
// Lane 2's taskbar owns the window verbs and the preview row outright — at which point the
// `ParkedSession`/`WindowGroup` arms of `FieldScreen` go with it.
#[cfg(test)]
pub(crate) fn place_window_tiles(doc: &mut FieldDocument, toplevels: &[ShellToplevel]) {
    let mut applied = Vec::new();
    // From the bottom row upward, the way the merge used to fill them, so seeding them disturbs
    // nothing a test already placed at the top of the field.
    let mut row = doc.rows.saturating_sub(1);
    for toplevel in toplevels {
        let identifier = session_identifier(toplevel, None);
        let kind = TileKind::ParkedSession {
            app_id: toplevel.canonical_app_id.clone(),
            identifier: identifier.clone(),
        };
        let class = SpanRange::for_kind(&kind, doc.cols, doc.rows);
        let spans = SpanRange::new(class.min, class.min, class.max);
        let tile = Tile {
            id: tile_id_for(&format!("session:{identifier}")),
            kind,
            title: toplevel.title.clone(),
            spans,
            accent: None,
        };
        try_apply(
            doc,
            FieldOp::Place {
                tile,
                landing: Landing {
                    container: None,
                    cell: Cell::new(0, row),
                },
            },
            &mut applied,
        );
        row = row.saturating_sub(spans.min.rows.max(1));
    }
    let mut groups: std::collections::BTreeMap<u64, Vec<TileId>> =
        std::collections::BTreeMap::new();
    for toplevel in toplevels {
        if let Some(group) = &toplevel.group {
            groups
                .entry(group.group_id)
                .or_default()
                .push(tile_id_for(&format!(
                    "session:{}",
                    session_identifier(toplevel, None)
                )));
        }
    }
    for (group_id, members) in groups {
        let info = toplevels
            .iter()
            .find_map(|t| t.group.as_ref().filter(|g| g.group_id == group_id))
            .expect("grouped by the presence of `group`");
        let kind = TileKind::WindowGroup {
            identifier: group_id.to_string(),
        };
        let project = Tile {
            id: group_tile_id(group_id),
            spans: SpanRange::for_kind(&kind, doc.cols, doc.rows),
            kind,
            title: group_title(&info.name, members.len()),
            accent: Some(info.accent),
        };
        try_apply(
            doc,
            FieldOp::CreateProject {
                project,
                landing: Landing {
                    container: None,
                    cell: Cell::new(0, row),
                },
                absorb: members,
            },
            &mut applied,
        );
    }
}

/// The windows a group tile stands for, in the order its children are placed.
///
/// A group tile's children are session tiles, so this is [`window_for_tile`] over each of them.
/// A child whose window has closed is simply absent — the tile is swept on the next merge.
pub fn group_members<'a>(
    doc: &FieldDocument,
    tile: TileId,
    toplevels: &'a [ShellToplevel],
) -> Vec<&'a ShellToplevel> {
    if !matches!(doc.kind_of(tile), Some(TileKind::WindowGroup { .. })) {
        return Vec::new();
    }
    doc.placements_in(Some(tile))
        .into_iter()
        .filter_map(|(child, _)| window_for_tile(doc, child, toplevels))
        .collect()
}

/// One member of `tile`'s group to address the compositor through.
///
/// Every group request on the wire is sent *to a window* and applies to its whole group, so any
/// member will do. The activated one is preferred where there is one, because that is the member
/// the compositor will treat as the group's own last-active on the way back.
pub fn group_window<'a>(
    doc: &FieldDocument,
    tile: TileId,
    toplevels: &'a [ShellToplevel],
) -> Option<&'a ShellToplevel> {
    let members = group_members(doc, tile, toplevels);
    members
        .iter()
        .find(|toplevel| toplevel.activated)
        .or_else(|| members.first())
        .copied()
}

/// `org.legeos.Files` → `Files` and `desktop-import.org.foo.Editor.desktop` → `Editor`.
/// Imported desktop IDs used to take the last component literally, making every tile read
/// "desktop" regardless of the application.
pub fn short_name(app_id: &AppId) -> String {
    let text = app_id.to_string();
    let meaningful = text
        .strip_prefix("desktop-import.")
        .unwrap_or(&text)
        .strip_suffix(".desktop")
        .unwrap_or_else(|| text.strip_prefix("desktop-import.").unwrap_or(&text));
    meaningful
        .rsplit('.')
        .next()
        .unwrap_or(meaningful)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    // The drawer fixture rebuilds a saved field that still had a panel; nothing on Home creates
    // one any more, so these live here rather than at the top of the module.
    use lege_field::{Arrangement, Panel};
    use lege_window::{Layer, Rect};

    fn toplevel(id: u64, app: Option<&str>, title: &str, shelved: bool) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: app.map(|a| a.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: app.is_some(),
            activated: false,
            fullscreen: false,
            shelved,
            maximized: false,
            geometry: Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
            group: None,
        }
    }

    /// The same window, in a group. The compositor sends the group's name and accent on every
    /// member, so the fixture does too.
    fn grouped(mut toplevel: ShellToplevel, group_id: u64, name: &str) -> ShellToplevel {
        toplevel.group = Some(lege_window::GroupInfo {
            group_id,
            name: name.to_owned(),
            accent: 0x0033_88ff,
        });
        toplevel
    }

    /// A resize the user performed must survive the background refresh that runs every three
    /// seconds: a resize lives in the *placement's* span, not in the tile's declared preference,
    /// and conflating the two would make every resize snap back within one tick.
    #[test]
    fn a_resized_tile_keeps_its_size_across_a_refresh() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let toplevels = [
            toplevel(1, Some("org.legeos.Files"), "one", false),
            toplevel(2, Some("org.example.Editor"), "two", false),
        ];
        let live = LiveState {
            toplevels: &toplevels,
            projects: &[],
        };
        merge(&mut doc, &live);
        pin_app(&mut doc, &"org.legeos.Files".parse().unwrap());

        // A pinned application tile, which sits in the open part of the field, so the resize has
        // somewhere to go.
        let subject = tile_id_for("app:org.legeos.Files");
        let before = doc.location(subject).unwrap().placement.span;
        doc = lege_field::apply(
            &doc,
            &FieldOp::Resize {
                id: subject,
                span: Span::new(2, 2),
            },
        )
        .expect("resizing a placed tile is always answerable")
        .0;
        let grown = doc.location(subject).unwrap().placement.span;
        assert_ne!(
            grown, before,
            "the resize has to have done something to test"
        );

        for _ in 0..3 {
            merge(&mut doc, &live);
        }

        assert_eq!(
            doc.location(subject).unwrap().placement.span,
            grown,
            "the refresh took the user's resize away"
        );
    }

    /// Stash is reversible again: what `field.restore` emits puts every shelved tile back, and
    /// an empty shelf asks for nothing.
    #[test]
    fn restoring_brings_every_shelved_tile_back() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![
            toplevel(1, Some("org.legeos.Files"), "Downloads", false),
            toplevel(2, Some("org.example.Editor"), "notes.md", true),
        ];
        let live = LiveState {
            toplevels: &windows,
            projects: &[],
        };
        merge(&mut doc, &live);
        assert!(restore_shelved(&doc).is_none(), "nothing is shelved yet");

        pin_app(&mut doc, &"org.legeos.Files".parse().unwrap());
        merge_desktop_folder(&mut doc, Some(Path::new("/home/someone/Desktop")));
        let shelved: Vec<TileId> = [
            files_tile_id(),
            desktop_folder_tile_id(),
            tile_id_for("app:org.legeos.Files"),
        ]
        .into_iter()
        .collect();
        for id in &shelved {
            doc = apply(&doc, &FieldOp::Stash { id: *id })
                .expect("the tile was on the field")
                .0;
            assert!(doc.location(*id).is_none());
        }
        assert_eq!(doc.shelved().count(), shelved.len());

        let op = restore_shelved(&doc).expect("three tiles are on the shelf");
        let restored = apply(&doc, &op).expect("the restore applies").0;
        assert_eq!(restored.shelved().count(), 0, "the shelf is empty again");
        for id in &shelved {
            assert!(
                restored.location(*id).is_some(),
                "{id:?} is back on the field"
            );
        }
        assert!(restore_shelved(&restored).is_none());
    }

    #[test]
    fn a_grid_is_never_smaller_than_four_by_four() {
        assert_eq!(grid_for(1920, 1016, TILE_SIZE), (21, 11));
        assert_eq!(grid_for(100, 100, TILE_SIZE), (4, 4));
        assert_eq!(grid_for(1920, 1016, 240), (8, 4));
    }

    #[test]
    fn merge_adds_the_files_object_and_leaves_running_windows_to_the_bar() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![
            toplevel(1, Some("org.legeos.Files"), "Downloads", false),
            toplevel(2, Some("org.example.Editor"), "notes.md", true),
        ];
        let live = LiveState {
            toplevels: &windows,
            projects: &[],
        };
        let ops = merge(&mut doc, &live);
        // The Files object, and nothing else. Two open windows — one of them shelved — grow no
        // tile of their own: a running window is the bar's business, not Home's. Nor does either
        // grow an application badge; an application is on Home only because someone pinned it.
        assert_eq!(ops.len(), 1);
        assert_eq!(doc.tiles.len(), 1);
        assert!(
            !doc.tiles.iter().any(|tile| matches!(
                tile.kind,
                TileKind::App { .. }
                    | TileKind::ParkedSession { .. }
                    | TileKind::WindowGroup { .. }
            )),
            "a running window put something on Home"
        );
        assert!(doc.panels.is_empty(), "Home grew a panel with no members");
        assert_eq!(doc.located.len(), doc.tiles.len(), "everything was placed");

        let files = files_tile_id();
        let located = doc.location(files).expect("the Files object is placed");
        assert_eq!(located.placement.cell, Cell::new(0, 0));
        assert_eq!(located.placement.span, Span::new(4, 3));

        // A second merge with the same state changes nothing.
        let before = doc.clone();
        assert!(merge(&mut doc, &live).is_empty());
        assert_eq!(doc, before);

        // The user pins an application and moves its tile; the next refresh keeps it there.
        let app = tile_id_for("app:org.legeos.Files");
        pin_app(&mut doc, &"org.legeos.Files".parse().unwrap());
        doc = apply(
            &doc,
            &FieldOp::Move {
                id: app,
                landing: Landing {
                    container: None,
                    cell: Cell::new(10, 1),
                },
            },
        )
        .unwrap()
        .0;
        merge(&mut doc, &live);
        assert_eq!(doc.location(app).unwrap().placement.cell, Cell::new(10, 1));
    }

    /// A field saved when Home had a tile per drawer, wherever the person left them, with the
    /// panel that grouped them.
    fn field_with_the_old_drawer_tiles(cells: &[(Drawer, Cell, Span)]) -> FieldDocument {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let mut members = Vec::new();
        for (drawer, cell, span) in cells {
            let id = tile_id_for(&format!("drawer:{}", drawer.name()));
            members.push(id);
            let tile = Tile {
                id,
                kind: TileKind::Widget {
                    widget: format!("drawer:{}", drawer.name().to_ascii_lowercase()),
                    scope: None,
                },
                title: drawer.name().to_owned(),
                spans: SpanRange::new(Span::new(1, 1), *span, Span::new(8, 6)),
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
            .expect("the old drawer tile places")
            .0;
        }
        doc = apply(
            &doc,
            &FieldOp::CreatePanel {
                panel: Panel {
                    id: drawer_panel_id(),
                    title: "Drawers".to_owned(),
                    region: Placement::new(Cell::new(0, 0), Span::new(4, 10)),
                    arrangement: Arrangement::Custom,
                    members,
                    baseline: None,
                },
            },
        )
        .expect("the old drawer panel")
        .0;
        doc
    }

    /// Every drawer tile becomes the one Files object, in the first place any of them held and
    /// at the largest size any of them had grown to; the panel that grouped them goes too.
    #[test]
    fn a_saved_field_of_drawer_tiles_becomes_one_files_object_in_their_place() {
        // Recent is first in `Drawer::DEFAULT`, so its cell is the one the replacement takes;
        // the largest span among them all is the one it grows to.
        let seeded = [
            (Drawer::Downloads, Cell::new(6, 2), Span::new(2, 2)),
            (Drawer::Recent, Cell::new(6, 4), Span::new(4, 3)),
            (Drawer::Trash, Cell::new(10, 2), Span::new(2, 2)),
            (Drawer::Network, Cell::new(10, 4), Span::new(2, 2)),
            (Drawer::WindowShelf, Cell::new(12, 2), Span::new(2, 2)),
        ];
        let mut doc = field_with_the_old_drawer_tiles(&seeded);
        merge(&mut doc, &LiveState::default());

        for (drawer, _, _) in seeded {
            assert!(
                doc.tile(tile_id_for(&format!("drawer:{}", drawer.name())))
                    .is_none(),
                "{drawer:?} is a location inside Files now, not a tile"
            );
        }
        assert!(doc.panel(drawer_panel_id()).is_none(), "the panel went too");
        let located = doc.location(files_tile_id()).expect("the Files object");
        assert_eq!(
            located.placement.cell,
            Cell::new(6, 4),
            "the replacement did not inherit the first of their places"
        );
        assert_eq!(
            located.placement.span,
            Span::new(4, 3),
            "the replacement did not inherit the largest of their sizes"
        );
        // Idempotent: a second merge of the same state asks for nothing.
        assert!(merge(&mut doc, &LiveState::default()).is_empty());
    }

    /// A field that already carries the Files object keeps it exactly where it is and only loses
    /// the leftovers.
    #[test]
    fn a_field_that_already_has_files_only_loses_the_leftover_drawers() {
        let mut doc =
            field_with_the_old_drawer_tiles(&[(Drawer::Recent, Cell::new(6, 2), Span::new(2, 2))]);
        let chosen = Cell::new(10, 6);
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: Tile {
                    id: files_tile_id(),
                    kind: files_kind(),
                    title: "Files".to_owned(),
                    spans: files_spans(&doc),
                    accent: None,
                },
                landing: Landing {
                    container: None,
                    cell: chosen,
                },
            },
        )
        .expect("the Files object places")
        .0;

        merge(&mut doc, &LiveState::default());
        assert!(
            doc.tile(tile_id_for("drawer:Recent")).is_none(),
            "the leftover drawer is gone"
        );
        assert_eq!(
            doc.location(files_tile_id())
                .map(|located| located.placement.cell),
            Some(chosen),
            "the tile a person is already using was moved"
        );
    }

    /// The migration touches nothing on a field that never had drawer tiles, and never runs
    /// twice.
    #[test]
    fn the_drawer_migration_is_silent_on_a_field_that_never_had_them() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        merge(&mut doc, &LiveState::default());
        let mut ops = Vec::new();
        migrate_drawer_tiles(&mut doc, &mut ops);
        assert!(ops.is_empty(), "{ops:?}");
        assert!(merge(&mut doc, &LiveState::default()).is_empty());
    }

    /// Every op the migration applies inverts exactly, like any other field edit: the removals
    /// and the placement replay backwards onto the document they started from.
    #[test]
    fn the_drawer_migration_inverts_exactly() {
        let doc = field_with_the_old_drawer_tiles(&[
            (Drawer::Downloads, Cell::new(6, 2), Span::new(2, 2)),
            (Drawer::Trash, Cell::new(10, 2), Span::new(2, 2)),
        ]);
        let before = doc.clone();
        let mut after = doc;
        let mut ops = Vec::new();
        migrate_drawer_tiles(&mut after, &mut ops);
        assert!(!ops.is_empty());

        let mut inverses = Vec::new();
        let mut replay = before.clone();
        for op in &ops {
            let (next, inverse) = apply(&replay, op).expect("the migration op replays");
            replay = next;
            inverses.push(inverse);
        }
        assert_eq!(replay, after, "the ops describe exactly what happened");
        for inverse in inverses.into_iter().rev() {
            replay = apply(&replay, &inverse).expect("the inverse applies").0;
        }
        assert_eq!(replay, before, "the migration did not invert exactly");
    }

    #[test]
    fn projects_get_tiles_at_the_right_edge_and_are_never_auto_removed() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let projects = vec![ProjectInfo {
            id: "thesis".into(),
            name: "Thesis".into(),
            is_open: false,
            ..ProjectInfo::default()
        }];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &[],
                projects: &projects,
            },
        );
        let thesis = project_tile_id("thesis");
        let located = doc.location(thesis).unwrap();
        assert_eq!(located.placement.cell, Cell::new(8, 0));
        assert_eq!(located.placement.span, Span::new(8, 4));
        assert!(
            doc.container(thesis).is_some(),
            "a project tile has an interior grid"
        );
        assert_eq!(doc.tile(thesis).unwrap().title, "Thesis");

        // Gone from Projects1 (or sessiond absent): the tile stays.
        merge(&mut doc, &LiveState::default());
        assert!(doc.tile(thesis).is_some());
    }

    /// The one route to an application tile, and the only reason one exists: someone pinned it.
    /// It outlives every window of that application, survives the refresh, and pinning twice
    /// asks for nothing.
    #[test]
    fn a_pinned_application_outlives_its_windows_and_pins_only_once() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![toplevel(1, Some("org.legeos.Files"), "Downloads", false)];
        let live = LiveState {
            toplevels: &windows,
            projects: &[],
        };
        merge(&mut doc, &live);
        let app: AppId = "org.legeos.Files".parse().unwrap();
        let id = app_tile_id(&app);
        assert!(doc.tile(id).is_none(), "a running window is not a pin");

        assert_eq!(pin_app(&mut doc, &app).len(), 1);
        let placed = doc.location(id).expect("the pinned tile is placed");
        assert!(
            pin_app(&mut doc, &app).is_empty(),
            "pinning twice is a no-op"
        );

        // Every window of that application closes: the session tiles go, the pin stays.
        merge(&mut doc, &LiveState::default());
        assert_eq!(
            doc.location(id),
            Some(placed),
            "the pin went away with the last window"
        );
        assert_eq!(doc.tile(id).map(|tile| tile.title.as_str()), Some("Files"));
    }

    /// The refresh removes exactly one thing: the tile of a window that closed. Everything else
    /// on the field is there because a person or Home put it there, and a merge that could not
    /// reach a service must not take it away.
    #[test]
    fn a_refresh_never_removes_a_placed_tile_that_is_not_a_closed_window() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        merge(&mut doc, &LiveState::default());
        pin_app(&mut doc, &"org.example.Editor".parse().unwrap());
        // A widget kind no build knows: stranded on purpose, because only a migration knows what
        // should replace one, and sweeping it would delete a person's arrangement instead.
        let orphan = tile_id_for("widget:from-the-future");
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: Tile {
                    id: orphan,
                    kind: TileKind::Widget {
                        widget: "from-the-future".to_owned(),
                        scope: None,
                    },
                    title: "Future".to_owned(),
                    spans: SpanRange::new(Span::new(2, 2), Span::new(2, 2), Span::new(4, 4)),
                    accent: None,
                },
                landing: Landing {
                    container: None,
                    cell: Cell::new(8, 6),
                },
            },
        )
        .expect("the unknown widget places")
        .0;

        let before = doc.clone();
        for _ in 0..3 {
            merge(&mut doc, &LiveState::default());
        }
        assert_eq!(doc, before, "a refresh took something away");
    }

    /// Place a tile of `kind` at `cell`, the way a saved document from an older build carries one.
    fn seed(doc: &FieldDocument, id: TileId, kind: TileKind, cell: Cell) -> FieldDocument {
        let spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
        apply(
            doc,
            &FieldOp::Place {
                tile: Tile {
                    id,
                    kind,
                    title: "seeded".to_owned(),
                    spans,
                    accent: None,
                },
                landing: Landing {
                    container: None,
                    cell,
                },
            },
        )
        .expect("the seeded tile places")
        .0
    }

    /// Running windows and their groups leave Home: the merge places none, and the sweep takes
    /// the ones a saved field is still carrying. They are the bar's business now
    /// (`docs/garden/garden.md` — Home deals with your stuff, the bar deals with the machine).
    ///
    /// No migration of its own is needed for this and that is worth stating: `wanted` never names
    /// a window tile, so the same sweep that used to remove a closed window's tile removes every
    /// one of them on the first refresh after this build.
    #[test]
    fn window_tiles_are_never_placed_and_a_saved_field_loses_the_ones_it_has() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![
            grouped(toplevel(1, Some("org.legeos.Files"), "one", false), 7, ""),
            grouped(toplevel(2, Some("org.example.Editor"), "two", false), 7, ""),
            toplevel(3, None, "three", true),
        ];
        let live = LiveState {
            toplevels: &windows,
            projects: &[],
        };
        merge(&mut doc, &live);
        assert!(
            !doc.tiles.iter().any(|tile| matches!(
                tile.kind,
                TileKind::ParkedSession { .. } | TileKind::WindowGroup { .. }
            )),
            "three running windows, one of them shelved and two of them grouped, put a tile on \
             Home"
        );

        // A field saved by an older build: two session tiles inside a group tile.
        let group = group_tile_id(7);
        let one = tile_id_for("session:legeos:1");
        doc = seed(
            &doc,
            group,
            TileKind::WindowGroup {
                identifier: "7".to_owned(),
            },
            Cell::new(8, 0),
        );
        doc = seed(
            &doc,
            one,
            TileKind::ParkedSession {
                app_id: None,
                identifier: "legeos:1".to_owned(),
            },
            Cell::new(12, 0),
        );
        let ops = merge(&mut doc, &live);
        assert!(doc.tile(group).is_none(), "the group tile is still there");
        assert!(doc.tile(one).is_none(), "the session tile is still there");
        assert!(
            ops.iter().all(|op| matches!(op, FieldOp::Remove { .. })),
            "the sweep did something other than remove: {ops:?}"
        );
        // Idempotent, and silent on a field that never had one.
        assert!(merge(&mut doc, &live).is_empty());
    }

    /// Every retired system widget leaves a saved field and nothing takes its place. Session,
    /// notifications and the Settings readouts are the bar's cluster now; battery, volume and
    /// brightness were folded into Settings before that; the launcher tile became the omnibox.
    #[test]
    fn a_saved_field_loses_every_retired_system_tile_and_gains_no_replacement() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let mut cells = Vec::new();
        for (index, widget) in RETIRED_SYSTEM_WIDGETS.into_iter().enumerate() {
            let cell = Cell::new(u16::try_from(index).unwrap() * 2, 8);
            cells.push(cell);
            doc = seed(
                &doc,
                tile_id_for(&format!("system:{widget}")),
                TileKind::System {
                    probe: widget.to_owned(),
                },
                cell,
            );
        }
        merge(&mut doc, &LiveState::default());

        for widget in RETIRED_SYSTEM_WIDGETS {
            assert!(
                doc.tile(tile_id_for(&format!("system:{widget}"))).is_none(),
                "{widget} is the bar's business now, not a tile"
            );
        }
        assert!(
            !doc.tiles.iter().any(|tile| doc
                .location(tile.id)
                .is_some_and(|located| cells.contains(&located.placement.cell))),
            "something was put in their place; there is no successor tile"
        );
        // Idempotent, and silent on a field that never had one.
        assert!(merge(&mut doc, &LiveState::default()).is_empty());
        let mut fresh = FieldDocument::new("eDP-1", 16, 10);
        merge(&mut fresh, &LiveState::default());
        let mut ops = Vec::new();
        migrate_system_tiles(&mut fresh, &mut ops);
        assert!(ops.is_empty(), "{ops:?}");
    }

    /// The legacy parser still recognises every drawer a saved field could be carrying — all ten
    /// variants, not just the eight that were ever placed by default — and nothing else.
    #[test]
    fn the_legacy_drawer_parser_still_knows_every_drawer() {
        for drawer in Drawer::DEFAULT {
            let widget = format!("drawer:{}", drawer.name().to_ascii_lowercase());
            assert_eq!(legacy_drawer_widget(&widget), Some(drawer));
        }
        assert_eq!(legacy_drawer_widget("clock"), None);
        assert_eq!(legacy_drawer_widget(FILES_WIDGET), None);
    }

    #[test]
    fn imported_desktop_ids_keep_the_application_name_not_the_desktop_suffix() {
        let imported: AppId = "desktop-import.org.example.Editor.desktop".parse().unwrap();
        assert_eq!(short_name(&imported), "Editor");
        let simple: AppId = "desktop-import.nm-tray.desktop".parse().unwrap();
        assert_eq!(short_name(&simple), "nm-tray");
    }

    /// Typing a path opens a window for it, and typing the same path again does not open a
    /// second one: the tile id is the path, so "already open" is exact.
    #[test]
    fn a_folder_window_opens_once_per_path_and_is_an_ordinary_tile() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        merge(&mut doc, &LiveState::default());
        let path = Path::new("/home/dk/src/legeos");

        let ops = open_folder_ops(&doc, path);
        assert_eq!(ops.len(), 1, "one place op, nothing else");
        doc = apply(&doc, &ops[0]).expect("the place applies").0;

        let id = folder_tile_id(path);
        let tile = doc.tile(id).expect("the window is a tile on the field");
        assert_eq!(
            tile.kind,
            TileKind::Folder {
                path: path.to_path_buf()
            }
        );
        assert_eq!(tile.title, "legeos", "the window is named for its folder");
        assert!(
            doc.location(id).is_some(),
            "a folder window is placed, so it can be moved, resized and grouped like any tile"
        );
        assert!(
            open_folder_ops(&doc, path).is_empty(),
            "the same path a second time raises what is there rather than stacking a copy"
        );
    }

    /// Desktop is a tray: it opens at the folder floor, not at the folder kind's preferred size,
    /// so the folder things arrive in is present on Home without being the largest thing on it.
    #[test]
    fn the_desktop_folder_opens_as_a_small_tray() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        merge(&mut doc, &LiveState::default());
        merge_desktop_folder(&mut doc, Some(Path::new("/home/dk/Desktop")));
        let id = desktop_folder_tile_id();
        let tile = doc.tile(id).expect("desktop folder");
        let floor = SpanRange::for_kind(&tile.kind, doc.cols, doc.rows).min;
        assert_eq!(tile.spans.preferred, floor);
        assert_eq!(
            doc.location(id).expect("placed").placement.span,
            floor,
            "it starts collapsed; pressing it is what grows it"
        );
    }

    /// The XDG Desktop directory appears as one placeable Home folder object when it exists,
    /// simply does not appear when it is absent, and never explodes its children into loose
    /// tiles — however many files the directory holds, the field gains exactly one tile.
    #[test]
    fn desktop_folder_appears_when_present_and_degrades_when_absent() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        merge(&mut doc, &LiveState::default());
        let tiles_before = doc.tiles.len();

        let absent = merge_desktop_folder(&mut doc, None);
        assert!(absent.is_empty(), "no directory, no tile, no error");
        assert_eq!(doc.tiles.len(), tiles_before);

        let ops = merge_desktop_folder(&mut doc, Some(Path::new("/home/someone/Desktop")));
        assert_eq!(ops.len(), 1, "one folder object, never one tile per child");
        let tile = doc.tile(desktop_folder_tile_id()).expect("desktop folder");
        assert_eq!(
            tile.kind,
            TileKind::Folder {
                path: "/home/someone/Desktop".into()
            }
        );
        assert_eq!(tile.title, "Desktop");
        assert!(
            !doc.tiles
                .iter()
                .any(|tile| matches!(tile.kind, TileKind::File { .. })),
            "no loose file tiles came with it"
        );

        // Add-only: the next merge changes nothing, so a moved or resized folder stays put.
        let again = merge_desktop_folder(&mut doc, Some(Path::new("/home/someone/Desktop")));
        assert!(again.is_empty());
    }

    /// No Home path creates a loose file tile: a file arriving on bare Home becomes a
    /// reference in the default shortcut pane, and legacy File tiles from older documents are
    /// adopted into it by the merge without losing their paths.
    #[test]
    fn no_loose_file_tiles_drops_route_to_the_pane_and_legacy_tiles_migrate() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        merge(&mut doc, &LiveState::default());

        let ops = route_file_drop(&mut doc, Path::new("/home/a/notes.md"));
        assert!(!ops.is_empty());
        assert!(
            !doc.tiles
                .iter()
                .any(|tile| matches!(tile.kind, TileKind::File { .. })),
            "a dropped file never becomes a loose tile"
        );
        let pane = doc.tile(default_pane_tile_id()).expect("default pane");
        let TileKind::Tray { tray_id } = pane.kind else {
            panic!("the pane is a tray");
        };
        assert_eq!(
            doc.tray(tray_id).unwrap().entries,
            vec![std::path::PathBuf::from("/home/a/notes.md")]
        );
        // The same file again adds nothing twice.
        let again = route_file_drop(&mut doc, Path::new("/home/a/notes.md"));
        assert!(again.is_empty());

        // A legacy document with a serialized loose File tile: the merge adopts it into the
        // pane and removes the tile — the path survives, the file never moves.
        let legacy = Tile {
            id: tile_id_for("file:legacy"),
            kind: TileKind::File {
                path: "/home/a/old-report.pdf".into(),
            },
            title: "old-report.pdf".to_owned(),
            spans: SpanRange::for_kind(
                &TileKind::File {
                    path: std::path::PathBuf::new(),
                },
                16,
                8,
            ),
            accent: None,
        };
        let (next, _) = apply(
            &doc,
            &FieldOp::AddTile {
                tile: legacy,
                at: None,
                container: None,
            },
        )
        .expect("seed legacy tile");
        doc = next;
        merge(&mut doc, &LiveState::default());
        assert!(
            !doc.tiles
                .iter()
                .any(|tile| matches!(tile.kind, TileKind::File { .. })),
            "the legacy tile is gone"
        );
        assert!(
            doc.tray(tray_id)
                .unwrap()
                .entries
                .contains(&std::path::PathBuf::from("/home/a/old-report.pdf")),
            "its path became a reference"
        );
    }

    /// Removing a reference from a pane is a field edit that touches only the reference — the
    /// document's op has an exact inverse and no file-service call — while trashing the target
    /// is a separate, labelled service operation (`lege-filed`'s own test covers its undo).
    #[test]
    fn shortcut_pane_remove_deletes_only_the_reference() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        merge(&mut doc, &LiveState::default());
        route_file_drop(&mut doc, Path::new("/home/a/notes.md"));
        route_file_drop(&mut doc, Path::new("/home/a/song.flac"));
        let TileKind::Tray { tray_id } = doc.tile(default_pane_tile_id()).unwrap().kind else {
            panic!("the pane is a tray");
        };

        let before = doc.clone();
        let (next, inverse) = apply(
            &doc,
            &FieldOp::RemoveReference {
                tray: tray_id,
                path: "/home/a/notes.md".into(),
            },
        )
        .expect("remove reference");
        assert_eq!(
            next.tray(tray_id).unwrap().entries,
            vec![std::path::PathBuf::from("/home/a/song.flac")],
            "exactly one reference gone; every tile untouched"
        );
        assert_eq!(next.tiles, before.tiles);
        let (restored, _) = apply(&next, &inverse).expect("undo");
        assert_eq!(restored, before, "reference removal inverts exactly");
    }
}

/// The ops that bring `local` in line with what another client did to `remote`: a tile placed
/// elsewhere is adopted where it was put; a Place the garden resized is resized here; a folder
/// tile absent from the daemon's copy — a Place the garden removed, or the user hid or forgot —
/// goes. Nothing is moved, and only folder tiles are ever removed: anything else absent from
/// the daemon's copy is a local change still on its way there, not a deletion.
pub fn reconcile_remote(local: &FieldDocument, remote: &FieldDocument) -> Vec<FieldOp> {
    let mut ops = Vec::new();
    for (id, located) in &remote.located {
        match local.location(*id) {
            None => {
                if let Some(tile) = remote.tile(*id) {
                    ops.push(FieldOp::AddTile {
                        tile: tile.clone(),
                        at: Some(*located),
                        container: None,
                    });
                }
            }
            Some(here)
                if here.placement.cell == located.placement.cell
                    && here.placement.span != located.placement.span =>
            {
                ops.push(FieldOp::Resize {
                    id: *id,
                    span: located.placement.span,
                });
            }
            Some(_) => {}
        }
    }
    ops.extend(
        local
            .located
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| remote.location(*id).is_none())
            .filter(|id| matches!(local.kind_of(*id), Some(TileKind::Folder { .. })))
            .map(|id| FieldOp::Remove { id }),
    );
    ops
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use lege_field::{Cell, Landing, Placement, Span, apply};
    use std::path::Path;

    fn with_folder(doc: &FieldDocument, id: u64, path: &str, cell: Cell) -> FieldDocument {
        let tile = lege_field::folder_tile(TileId(id), Path::new(path), doc.cols, doc.rows);
        apply(
            doc,
            &FieldOp::Place {
                tile,
                landing: Landing {
                    container: None,
                    cell,
                },
            },
        )
        .unwrap()
        .0
    }

    #[test]
    fn a_remote_resize_and_removal_of_a_place_reach_the_local_field_and_nothing_else_moves() {
        let base = FieldDocument::new("DP-1", 12, 8);
        let local = with_folder(&base, 1, "/home/dk/a", Cell::new(0, 5));
        let local = with_folder(&local, 2, "/home/dk/b", Cell::new(6, 0));
        // Remotely: `a` grew to 4x3 in place, `b` was removed, and `c` was placed elsewhere.
        let remote = apply(
            &local,
            &FieldOp::Resize {
                id: TileId(1),
                span: Span::new(4, 3),
            },
        )
        .unwrap()
        .0;
        let remote = apply(&remote, &FieldOp::Remove { id: TileId(2) })
            .unwrap()
            .0;
        let remote = with_folder(&remote, 3, "/home/dk/c", Cell::new(8, 4));
        let ops = reconcile_remote(&local, &remote);
        let mut doc = local.clone();
        for op in &ops {
            doc = apply(&doc, op).unwrap().0;
        }
        assert_eq!(
            doc.location(TileId(1)).unwrap().placement,
            Placement::new(Cell::new(0, 5), Span::new(4, 3))
        );
        assert!(doc.location(TileId(2)).is_none());
        assert_eq!(
            doc.location(TileId(3)).unwrap().placement.cell,
            Cell::new(8, 4)
        );
        assert!(ops.iter().all(|op| !matches!(op, FieldOp::Move { .. })));
        assert!(reconcile_remote(&doc, &remote).is_empty(), "idempotent");
    }

    #[test]
    fn a_non_folder_tile_missing_remotely_is_a_local_change_in_flight_not_a_deletion() {
        let base = FieldDocument::new("DP-1", 12, 8);
        let app = lege_field::Tile {
            id: TileId(9),
            kind: TileKind::App {
                app_id: "org.example.A".parse().unwrap(),
            },
            title: "A".into(),
            spans: lege_field::SpanRange::new(Span::new(2, 2), Span::new(2, 2), Span::new(2, 2)),
            accent: None,
        };
        let local = apply(
            &base,
            &FieldOp::Place {
                tile: app,
                landing: Landing {
                    container: None,
                    cell: Cell::new(0, 0),
                },
            },
        )
        .unwrap()
        .0;
        assert!(reconcile_remote(&local, &base).is_empty());
    }
}
