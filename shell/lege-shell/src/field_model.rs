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
    Arrangement, Cell, FieldDocument, FieldError, FieldOp, Landing, Panel, PanelId, Span,
    SpanRange, Tile, TileId, TileKind, apply, panel_id_for,
};
use lege_types::AppId;
use lege_window::{ShellToplevel, WindowId};
use std::path::Path;

use crate::home_surface::Drawer;

/// Logical pixels per grid cell. `shell.home-field.tile-size` once the field is persisted.
pub const TILE_SIZE: i32 = 104;
/// Logical pixels between tiles.
pub const GAP: i32 = 6;

/// The widget name the one Files object carries (`TileKind::Widget::widget`).
///
/// A widget rather than a new `TileKind`: it keeps the widget class's 8×6 ceiling, so the object
/// that is the way into every file on the machine still cannot cover the field and become a
/// maximised window wearing a tile's clothes.
pub const FILES_WIDGET: &str = "files";

/// The `TileKind::System` probes Home places for itself. Each must be a widget the shell knows
/// (`crate::surface::widgets::WIDGETS`), which a test asserts.
///
/// Settings is the third and last: the battery, the volume and the display brightness on one
/// tile, with the way into the window that changes the rest of them. LegeOS has no tray and no
/// menu bar by design, so a widget is the only place those readings can be — but one tile each was
/// three headings for three numbers, which is a tray by another name.
///
/// There is no launcher among them any more. The search control at the top of Home is the
/// launcher, and a tile that listed what it matched was the same list twice.
pub const SYSTEM_WIDGETS: [&str; 3] = ["session", "notifications", "settings"];

/// The system widget a saved field may still be carrying and this build no longer draws.
///
/// The launcher tile was a list of applications filtered by the search bar's query — which is
/// what the omnibox under that same bar now is, at every size, for every kind of thing. There is
/// no successor tile: the capability moved into the control that was already there.
const LEGACY_LAUNCHER_WIDGET: &str = "launcher";

/// The `TileKind::System` probes a saved field may still be carrying: one tile per hardware
/// reading, as Home placed them before the Settings widget replaced all three.
///
/// A saved document outlives the roster that made it, and these three are not merely unknown —
/// they are *superseded*. Left alone they would each render "no such widget" for as long as the
/// layout survives, which on this desktop is forever. [`migrate_hardware_tiles`] is what turns
/// them into the one tile they became.
const LEGACY_HARDWARE_WIDGETS: [&str; 3] = ["battery", "volume", "brightness"];

/// The share of the field the system shelf may claim, as a divisor: at most one cell in this many.
///
/// Home's furniture is there to be reached, not to *be* the desktop: the middle of
/// the field belongs to windows and applications. Four widgets at three columns each reach it on
/// a small field, which is exactly when it matters — and reaching it is how the shelf sheds a
/// widget rather than the field losing a row of working space.
const SYSTEM_SHELF_SHARE: u16 = 4;

/// The system widgets that fit on a `cols`x`rows` field, in declaration order.
///
/// Declaration order is the priority order, and [`SYSTEM_WIDGETS`] is written accordingly: session
/// is what a desktop is unusable without, so on a field with room for only some of them it is the
/// one that stays. A readout that does not fit is not placed rather than squeezed in on top of
/// the space windows need — a laptop screen that arrives with no room to work is a worse outcome
/// than one that shows two instruments instead of three.
fn system_widgets_that_fit(cols: u16, rows: u16) -> Vec<&'static str> {
    let budget = u32::from(cols) * u32::from(rows) / u32::from(SYSTEM_SHELF_SHARE.max(1));
    let mut spent = 0u32;
    let mut fitted = Vec::new();
    for widget in SYSTEM_WIDGETS {
        let (span_cols, span_rows) = crate::surface::widgets::descriptor(widget)
            .map_or((3, 2), |descriptor| descriptor.preferred_span);
        let cost = u32::from(span_cols.max(1)) * u32::from(span_rows.max(1));
        // Always place the first: a field too small for even one widget still needs the session
        // controls, and an empty shelf has no route to logging out at all.
        if fitted.is_empty() || spent + cost <= budget {
            spent += cost;
            fitted.push(widget);
        }
    }
    fitted
}

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
    // FNV-1a, 64-bit. Collisions are astronomically unlikely for a few hundred keys; `merge`
    // still checks for one and skips the second tile rather than corrupting the first.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    TileId(hash.max(1))
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
    // First, before anything places a tile: a superseded widget has to become its replacement
    // rather than sit beside one. Running this after `merge_system` would mean the Settings tile
    // already existed by the time the old three were found, and the migration would have nowhere
    // to put it but the shelf's default corner — losing the place a person had chosen.
    migrate_hardware_tiles(doc, &mut applied);
    // Same rule, one object further: the eight drawer tiles are superseded by the single Files
    // object, and the migration has to run before `merge_files` so the replacement can inherit
    // the place the block held instead of landing in the default corner.
    migrate_drawer_tiles(doc, &mut applied);
    migrate_launcher_tile(doc, &mut applied);
    merge_files(doc, &mut wanted, &mut applied);
    // A Home overview must never sacrifice a real open window to a project shortcut or redundant
    // application badge when space is tight, so sessions get first claim after the utility block.
    // The system shelf still always lands, because its tiles may compact to 2x1 — see
    // `merge_system`; a field can be too full for a large launcher but never too full for one.
    merge_sessions(doc, live, &mut wanted, &mut applied);
    merge_projects(doc, live, &mut applied);
    merge_system(doc, &mut applied);
    merge_panels(doc, &mut applied);
    adopt_loose_files(doc, &mut applied);

    // Windows that closed take their tiles with them; everything else the user placed stays.
    //
    // `ParkedSession` and nothing else, and this is load-bearing — please do not "fix" it by
    // widening the match back out. A session tile is the only kind whose subject can cease to
    // exist without anyone asking: the window closed. An application tile is on Home because
    // someone pinned it (`pin_app`), and a widget is on Home because Home put it there; sweeping
    // either because this refresh did not happen to ask for it would delete a person's own
    // arrangement every time a service failed to answer.
    //
    // The cost is that a tile naming a widget kind a future build no longer draws is stranded
    // rather than swept. That is deliberate: a migration is how a retired widget leaves a saved
    // field (`migrate_hardware_tiles`, `migrate_drawer_tiles`), because only a migration knows
    // what should take its place.
    //
    // A window group's tile is swept by exactly the same rule and for exactly the same reason:
    // its subject is the compositor's group, the compositor dissolves a group the moment it drops
    // below two members, and a tile for a group that no longer exists is a container nobody can
    // put anything in.
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
    let spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
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

/// The panel the system shelf belongs to: [`merge_system`]'s hard-coded landings made
/// adjustable. Created add-only (a user's move, resize or re-orientation survives every merge),
/// starting as `Arrangement::Custom` over the block the landing heuristics build, and picking up
/// newly added system tiles as members without moving anything.
pub fn system_panel_id() -> PanelId {
    panel_id_for("panel:system")
}

fn merge_panels(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let systems: Vec<TileId> = system_widgets_that_fit(doc.cols, doc.rows)
        .iter()
        .map(|widget| tile_id_for(&format!("system:{widget}")))
        .collect();
    let shelf_rows = 2u16.min(doc.rows);
    let groups = [(
        system_panel_id(),
        "System shelf",
        systems,
        Placement::new(
            Cell::new(0, doc.rows - shelf_rows),
            Span::new(doc.cols.max(1), shelf_rows),
        ),
    )];
    for (id, title, members, region) in groups {
        match doc.panel(id).cloned() {
            None => {
                let panel = Panel {
                    id,
                    title: title.to_owned(),
                    region,
                    arrangement: Arrangement::Custom,
                    members,
                    baseline: None,
                };
                try_apply(doc, FieldOp::CreatePanel { panel }, applied);
            }
            Some(existing) => {
                // Membership follows the tile set; layout, region and arrangement stay the
                // user's. ConfigurePanel is metadata-only, so nothing moves here.
                let missing: Vec<TileId> = members
                    .iter()
                    .copied()
                    .filter(|member| !existing.members.contains(member))
                    .collect();
                if !missing.is_empty() {
                    let mut panel = existing;
                    panel.members.extend(missing);
                    try_apply(doc, FieldOp::ConfigurePanel { panel }, applied);
                }
            }
        }
    }
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

/// The system widgets Home always offers: Session, which is the only route out of a running
/// session that does not involve killing the compositor from a virtual terminal; notifications;
/// and Settings, which is the hardware readout and the way into the rest.
///
/// Add-only, like projects: a system tile is never removed by a merge, so a user who has moved
/// or resized one keeps their arrangement. The landing is a shelf along the bottom, to the right
/// of the drawer block; if that is taken the reflow finds the nearest place rather than
/// displacing a drawer.
fn merge_system(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let fitting = system_widgets_that_fit(doc.cols, doc.rows);
    for (index, widget) in fitting.iter().copied().enumerate() {
        let tile = system_tile(widget, doc);
        let cell = system_landing(index, doc, tile.spans, fitting.len());
        if doc.tile(tile.id).is_some() {
            continue;
        }
        let landing = Landing {
            container: None,
            cell,
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// The tile Home places for one system widget — id, heading and span range, all derived from the
/// widget's own declaration.
///
/// Shared by [`merge_system`] and [`migrate_hardware_tiles`] so a field that grew its Settings
/// tile out of three older ones gets exactly the tile a fresh field would have been given.
///
/// Compact by default, with the class ceiling intact — the same bargain the drawer block strikes
/// above. Four system widgets at the class's *preferred* 4x2 claim thirty-two cells of a laptop's
/// seventy-seven before a single window exists, which is a field that arrives full; at the widgets'
/// own declared spans they are a strip along the bottom that a person grows if they want the
/// launcher to list more than a handful of rows at a time.
///
/// The tile's own floor is lower still, at 2x1. These are the desktop's furniture and are placed
/// last, after every open window has its overview space: without room to compact, a busy field
/// simply failed to place the launcher, which is precisely the field on which someone reaches for
/// it. A 2x1 launcher is a small tile that expands, not a missing one.
fn system_tile(widget: &str, doc: &FieldDocument) -> Tile {
    let kind = TileKind::System {
        probe: widget.to_owned(),
    };
    let class = SpanRange::for_kind(&kind, doc.cols, doc.rows);
    // The arrival span comes from the widget, not from the tile class: the class figure is the
    // launcher's — a list needs rows to be a list — and applying it to everything is what turns
    // the system shelf into four rows of a seven-row field. The 2x1 floor below is every system
    // tile's and is unchanged, so this decides what a widget arrives at, never what a person may
    // shrink it to.
    let descriptor = crate::surface::widgets::descriptor(widget);
    let wanted = descriptor.map_or((class.preferred.cols, class.preferred.rows), |declared| {
        declared.preferred_span
    });
    Tile {
        id: tile_id_for(&format!("system:{widget}")),
        kind,
        title: descriptor.map_or_else(
            || widget.to_owned(),
            |descriptor| descriptor.title.to_owned(),
        ),
        spans: SpanRange::new(
            Span::new(2, 1),
            Span::new(wanted.0.max(1), wanted.1.max(1)),
            class.max,
        ),
        accent: None,
    }
}

/// Turn a saved field's Battery, Volume and Brightness tiles into the one Settings tile that
/// replaced them, in the first place any of them held.
///
/// The only step of the merge that removes a system tile, and it is not an exception to the
/// add-only rule so much as the reason the rule can stay: these three tiles no longer name a
/// widget the shell can draw, so leaving them alone means three tiles reading "no such widget"
/// for the life of the layout. Keeping the *place* is the part that matters — a person who moved
/// their readouts to a corner finds the tile that replaced them in that corner.
///
/// Expressed as ordinary ops through [`try_apply`] like every other merge step, so the removals
/// and the placement have the same exact inverses a drag does and a caller can forward them to
/// `HomeField1.Apply` unchanged.
fn migrate_hardware_tiles(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let legacy: Vec<TileId> = LEGACY_HARDWARE_WIDGETS
        .iter()
        .map(|widget| (widget, tile_id_for(&format!("system:{widget}"))))
        // The kind is checked, not just the id: a tile id is a hash, and removing something else
        // that happened to collide with one would be the worst possible outcome here.
        .filter(|(widget, id)| {
            doc.tile(*id).is_some_and(|tile| {
                matches!(&tile.kind, TileKind::System { probe } if probe == *widget)
            })
        })
        .map(|(_, id)| id)
        .collect();
    if legacy.is_empty() {
        return;
    }
    // The first of their positions, in the order Home originally placed them. A field where all
    // three had been stashed has no position to inherit, and `merge_system` then lands the
    // replacement at the shelf's own corner.
    let inherited = legacy
        .iter()
        .find_map(|id| doc.location(*id).map(|located| located.placement.cell));
    for id in legacy {
        try_apply(doc, FieldOp::Remove { id }, applied);
    }
    let tile = system_tile("settings", doc);
    // A field that already carries the replacement wanted only the leftovers gone. Placing a
    // second one is impossible anyway — ids are derived from the probe — and asking would move
    // the tile the person is already using.
    if doc.tile(tile.id).is_some() {
        return;
    }
    let Some(cell) = inherited else {
        return;
    };
    let landing = Landing {
        container: None,
        cell,
    };
    try_apply(doc, FieldOp::Place { tile, landing }, applied);
}

/// Take the launcher tile off a saved field.
///
/// The one migration with no successor, and that is the point rather than an omission: the
/// launcher's whole content was "the applications matching what is typed in the search bar", and
/// that is now the omnibox under the search bar itself — at every size, for files and projects
/// and actions as well as applications. Putting a replacement tile in its place would be putting
/// the same list on the field twice.
///
/// Kind-checked and expressed as an ordinary op, like every other migration here, so it inverts
/// exactly and a person who wants the space back does not have to hunt for it.
fn migrate_launcher_tile(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    let id = tile_id_for(&format!("system:{LEGACY_LAUNCHER_WIDGET}"));
    let is_launcher = doc.tile(id).is_some_and(
        |tile| matches!(&tile.kind, TileKind::System { probe } if probe == LEGACY_LAUNCHER_WIDGET),
    );
    if is_launcher {
        try_apply(doc, FieldOp::Remove { id }, applied);
    }
}

/// System widgets form a shelf in the bottom-right corner, packed right to left in declaration
/// order and stacking upward once the row is full.
///
/// The rule used to alternate bottom-left and bottom-right, which has room for exactly two: a
/// third widget landed on the first one's cell and the reflow scattered it, moving every tile
/// placed after it. Packing is stable as widgets are added.
///
/// The corner rather than the drawer block's right edge: anchored to the drawers the shelf sat
/// wherever the block happened to end and trailed off into empty field, which reads as an
/// accident on a wide screen. In the corner it balances the status cluster diagonally opposite
/// it, and the middle of the field — where windows and applications live — stays clear.
fn system_landing(index: usize, doc: &FieldDocument, spans: SpanRange, placed: usize) -> Cell {
    let width = spans.preferred.cols.max(1);
    let height = spans.preferred.rows.max(1);
    let per_row = (doc.cols / width).max(1);
    // How many are actually being placed, not how many exist: a field too small for every widget
    // packs the ones it has, and counting the absent ones would leave a gap where they would be.
    let len = u16::try_from(placed).unwrap_or(u16::MAX);
    let index = u16::try_from(index).unwrap_or(u16::MAX);
    // Right to left from the corner, so the group's right edge is the field's — however many
    // widgets share the row, and whatever width they take.
    let in_row = per_row
        .min(len.saturating_sub((index / per_row) * per_row))
        .max(1);
    let from_right = in_row.saturating_sub(index % per_row).saturating_mul(width);
    let col = doc.cols.saturating_sub(from_right);
    let row = doc
        .rows
        .saturating_sub(height)
        .saturating_sub((index / per_row).saturating_mul(height));
    Cell::new(col, row)
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
pub fn pin_app(doc: &mut FieldDocument, app_id: &AppId) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let id = app_tile_id(app_id);
    if doc.tile(id).is_some() {
        return applied;
    }
    let tile = Tile {
        id,
        kind: TileKind::App {
            app_id: app_id.clone(),
        },
        title: short_name(app_id),
        spans: SpanRange::new(Span::new(1, 1), Span::new(1, 1), Span::new(2, 2)),
        accent: None,
    };
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

/// How many rows the system shelf occupies across `available_cols`, at the class floor its tiles
/// prefer. Sessions size themselves around this rather than over it: with the shelf unaccounted
/// for, session previews claimed the bottom rows and the shelf was squeezed into whatever was
/// left, so the launcher arrived as a sliver on a field with plenty of room for it.
fn system_shelf_rows(available_cols: u16, cols: u16, rows: u16) -> u16 {
    // From the widgets that are actually placed and at the sizes they are actually placed at.
    // This used to assume every system tile was 3x2, which was true while all three were the
    // launcher's shape and stopped being true the moment a 2x1 battery joined them: the estimate
    // then reserved four rows of an eight-row field for a shelf occupying two, and took the
    // difference out of the session previews for nothing.
    let per_row_budget = available_cols.max(1);
    let mut used_in_row = 0u16;
    let mut row_height = 0u16;
    let mut total = 0u16;
    for widget in system_widgets_that_fit(cols, rows) {
        let (span_cols, span_rows) = crate::surface::widgets::descriptor(widget)
            .map_or((3, 2), |descriptor| descriptor.preferred_span);
        let (span_cols, span_rows) = (span_cols.max(1), span_rows.max(1));
        if used_in_row > 0 && used_in_row + span_cols > per_row_budget {
            total = total.saturating_add(row_height);
            used_in_row = 0;
            row_height = 0;
        }
        used_in_row = used_in_row.saturating_add(span_cols);
        row_height = row_height.max(span_rows);
    }
    total.saturating_add(row_height)
}

/// The largest common overview span that lets every live session fit in the field area above the
/// system shelf. Large previews remain the normal case;
/// busy workspaces step down through still-legible shapes before reaching the 2×1 session floor.
fn session_overview_span(cols: u16, rows: u16, session_count: usize) -> Span {
    let available_cols = cols;
    let rows = rows
        .saturating_sub(system_shelf_rows(available_cols, cols, rows))
        .max(1);
    let count = u32::try_from(session_count).unwrap_or(u32::MAX);
    [
        Span::new(4, 2),
        Span::new(3, 2),
        Span::new(2, 2),
        Span::new(4, 1),
        Span::new(3, 1),
        Span::new(2, 1),
    ]
    .into_iter()
    .find(|span| {
        let capacity = u32::from(available_cols / span.cols) * u32::from(rows / span.rows);
        capacity >= count
    })
    .unwrap_or(Span::new(2, 1))
}

/// Sessions fill from the bottom in reading order, most recent window first. Computing explicit
/// rows matters: repeated off-edge landings would only search one local reflow band and could
/// leave windows off Home even when another row had room.
fn merge_sessions(
    doc: &mut FieldDocument,
    live: &LiveState<'_>,
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    let mut toplevels: Vec<&ShellToplevel> = live
        .toplevels
        .iter()
        .filter(|toplevel| {
            matches!(
                toplevel.layer,
                lege_window::Layer::Normal | lege_window::Layer::AppOnTop
            )
        })
        .collect();
    toplevels.sort_by_key(|toplevel| std::cmp::Reverse(toplevel.window_id));
    let overview_span = session_overview_span(doc.cols, doc.rows, toplevels.len());
    let available_cols = doc.cols;
    let per_row = available_cols
        .checked_div(overview_span.cols)
        .unwrap_or(0)
        .max(1);
    // Sessions stack up from the row above the shelf, not from the bottom edge: the shelf is
    // fixed furniture placed later, and previews that started at the edge sat on top of where it
    // goes, leaving it to squeeze into the gap above them.
    let session_floor = doc
        .rows
        .saturating_sub(system_shelf_rows(available_cols, doc.cols, doc.rows))
        .max(overview_span.rows);
    for (index, toplevel) in toplevels.iter().enumerate() {
        let identifier = session_identifier(toplevel, None);
        let id = tile_id_for(&format!("session:{identifier}"));
        wanted.insert(id);
        if doc.tile(id).is_some() {
            continue;
        }
        let kind = TileKind::ParkedSession {
            app_id: toplevel.canonical_app_id.clone(),
            identifier,
        };
        let mut spans = SpanRange::for_kind(&kind, doc.cols, doc.rows);
        spans.preferred = overview_span;
        let tile = Tile {
            id,
            kind,
            title: if toplevel.title.is_empty() {
                toplevel
                    .canonical_app_id
                    .as_ref()
                    .map_or_else(|| "(untitled)".to_owned(), short_name)
            } else {
                toplevel.title.clone()
            },
            spans,
            accent: None,
        };
        let slot = u16::try_from(index).unwrap_or(u16::MAX);
        let row = session_floor.saturating_sub(
            slot.checked_div(per_row)
                .unwrap_or(0)
                .saturating_add(1)
                .saturating_mul(overview_span.rows),
        );
        let landing = Landing {
            container: None,
            cell: Cell::new((slot % per_row) * overview_span.cols, row),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
    merge_window_groups(doc, &toplevels, wanted, applied);
}

/// The tile id a window group's tile always has. The group id is the compositor's
/// (`lege_window::GroupInfo::group_id`), so the same group is one tile across refreshes.
pub fn group_tile_id(group_id: u64) -> TileId {
    tile_id_for(&format!("group:{group_id}"))
}

/// The title a group's tile carries: the name the group was given, or how many windows it holds
/// until somebody names it. Never empty — a tile with no title is a tile that says nothing.
fn group_title(name: &str, members: usize) -> String {
    if name.trim().is_empty() {
        match members {
            1 => "1 window".to_owned(),
            other => format!("{other} windows"),
        }
    } else {
        name.to_owned()
    }
}

/// Window groups as tiles: one container per group the compositor reports, holding the session
/// tiles of its members.
///
/// This never *makes* a group. `lege_window` owns membership and the compositor is the source of
/// truth (`docs/planning/window-groups-and-application-integration.md` §2 rule 2); a group tile is
/// a representation that follows the `group` event, which is why every decision here reads
/// `ShellToplevel::group` and nothing else. A drop on Home asks the compositor to group two
/// windows and the tile appears on the *next* merge, so the field and the workspace can never
/// disagree about what is grouped even for a frame.
///
/// Runs after the session tiles are placed, so a member's tile always exists to be moved in.
fn merge_window_groups(
    doc: &mut FieldDocument,
    toplevels: &[&ShellToplevel],
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    // Grouped by the compositor's group id, in id order so the result never depends on the order
    // windows happen to arrive in.
    let mut groups: std::collections::BTreeMap<u64, Vec<&ShellToplevel>> =
        std::collections::BTreeMap::new();
    for toplevel in toplevels {
        if let Some(group) = &toplevel.group {
            groups.entry(group.group_id).or_default().push(toplevel);
        }
    }

    for (group_id, members) in &groups {
        let tile_id = group_tile_id(*group_id);
        // Every member reports the same name and accent (the compositor sends the group's, not
        // the window's), so the first is the group's.
        let info = members[0]
            .group
            .as_ref()
            .expect("grouped by the presence of `group`");
        let title = group_title(&info.name, members.len());
        wanted.insert(tile_id);

        if doc.tile(tile_id).is_none() {
            // Created at the first member's place: the group appears where the work already was,
            // not in whatever corner a fresh landing would find.
            let first = session_tile_id(members[0]);
            let landing = Landing {
                container: None,
                cell: doc
                    .location(first)
                    .filter(|located| located.container.is_none())
                    .map_or_else(|| Cell::new(0, 0), |located| located.placement.cell),
            };
            let kind = TileKind::WindowGroup {
                identifier: group_id.to_string(),
            };
            let container_tile = Tile {
                id: tile_id,
                spans: SpanRange::for_kind(&kind, doc.cols, doc.rows),
                kind,
                title: title.clone(),
                // The group's own colour, so the tile's edge, the band stripe and the outline on
                // the workspace are one colour (§2 rule 7).
                accent: Some(info.accent),
            };
            // `CreateProject` is the one op that makes a container; it is named for the first
            // container there was, not for projects alone, and it absorbs the tiles it is given.
            // The members whose tiles already sit on the field are absorbed here so the group
            // arrives holding them rather than empty and then filling.
            let absorb: Vec<TileId> = members
                .iter()
                .map(|member| session_tile_id(member))
                .filter(|id| doc.location(*id).is_some_and(|at| at.container.is_none()))
                .collect();
            try_apply(
                doc,
                FieldOp::CreateProject {
                    project: container_tile,
                    landing,
                    absorb,
                },
                applied,
            );
        } else if let Some(existing) = doc.tile_mut(tile_id) {
            // Content only, which by the field's own rule moves nothing.
            if existing.title != title {
                existing.title = title;
            }
            if existing.accent != Some(info.accent) {
                existing.accent = Some(info.accent);
            }
        }

        // Any member whose tile is not inside the group yet — a window that joined an existing
        // group, or one whose tile could not be absorbed above.
        if doc.container(tile_id).is_some() {
            for member in members {
                let session = session_tile_id(member);
                let inside = doc
                    .location(session)
                    .is_some_and(|at| at.container == Some(tile_id));
                if doc.tile(session).is_some() && !inside {
                    try_apply(
                        doc,
                        FieldOp::Move {
                            id: session,
                            landing: Landing {
                                container: Some(tile_id),
                                cell: Cell::new(0, 0),
                            },
                        },
                        applied,
                    );
                }
            }
        }
    }

    move_departed_members_out(doc, toplevels, applied);
}

/// A window that left its group: its tile is still inside a group tile the compositor no longer
/// puts it in.
///
/// Moved out to the nearest free cell — the landing named is the group's own place and the reflow
/// finds the first free rectangle from there, so the tile lands beside the work it came from
/// rather than in a far corner. This is also what empties a dissolving group's tile before the
/// merge's sweep removes it, which is why a dissolved group never takes its members' tiles to the
/// shelf with it.
fn move_departed_members_out(
    doc: &mut FieldDocument,
    toplevels: &[&ShellToplevel],
    applied: &mut Vec<FieldOp>,
) {
    let mut departures: Vec<(TileId, TileId)> = Vec::new();
    for toplevel in toplevels {
        let session = session_tile_id(toplevel);
        let Some(container) = doc.location(session).and_then(|at| at.container) else {
            continue;
        };
        if !matches!(doc.kind_of(container), Some(TileKind::WindowGroup { .. })) {
            continue;
        }
        let belongs = toplevel
            .group
            .as_ref()
            .is_some_and(|group| group_tile_id(group.group_id) == container);
        if !belongs {
            departures.push((session, container));
        }
    }
    for (session, container) in departures {
        let cell = doc
            .location(container)
            .map_or_else(|| Cell::new(0, 0), |located| located.placement.cell);
        try_apply(
            doc,
            FieldOp::Move {
                id: session,
                landing: Landing {
                    container: None,
                    cell,
                },
            },
            applied,
        );
    }
}

/// The tile id one window's session tile has, wherever the merge needs to name it.
fn session_tile_id(toplevel: &ShellToplevel) -> TileId {
    tile_id_for(&format!("session:{}", session_identifier(toplevel, None)))
}

/// Refresh the titles live state carries (a window's title changes; its tile's does too) —
/// content only, which by the field's own rule moves nothing.
pub fn refresh_titles(doc: &mut FieldDocument, live: &LiveState<'_>) {
    for toplevel in live.toplevels {
        let identifier = session_identifier(toplevel, None);
        let id = tile_id_for(&format!("session:{identifier}"));
        if let Some(tile) = doc.tile_mut(id)
            && !toplevel.title.is_empty()
        {
            tile.title.clone_from(&toplevel.title);
        }
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

    #[test]
    fn a_group_becomes_one_tile_holding_its_members() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let toplevels = [
            grouped(toplevel(1, Some("org.legeos.Files"), "one", false), 7, ""),
            grouped(toplevel(2, Some("org.example.Editor"), "two", false), 7, ""),
            toplevel(3, Some("org.example.Player"), "three", false),
        ];
        let live = LiveState {
            toplevels: &toplevels,
            projects: &[],
        };
        merge(&mut doc, &live);

        let group = group_tile_id(7);
        let tile = doc.tile(group).expect("the group has a tile");
        assert!(matches!(tile.kind, TileKind::WindowGroup { .. }));
        // Unnamed, so the tile says how many windows it holds; the accent is the group's.
        assert_eq!(tile.title, "2 windows");
        assert_eq!(tile.accent, Some(0x0033_88ff));
        assert!(doc.container(group).is_some(), "a group is a container");

        let members = group_members(&doc, group, &toplevels);
        let ids: BTreeSet<u64> = members
            .iter()
            .map(|member| member.window_id.0)
            .collect::<BTreeSet<_>>();
        assert_eq!(ids, BTreeSet::from([1, 2]));

        // The ungrouped window's tile stays on the bare field.
        let loose = session_tile_for(&doc, &toplevels[2]).expect("the loose window has a tile");
        assert_eq!(
            doc.location(loose).and_then(|located| located.container),
            None
        );
    }

    #[test]
    fn a_named_group_takes_its_name_and_a_window_that_joins_later_moves_in() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let before = [
            grouped(toplevel(1, Some("org.legeos.Files"), "one", false), 7, ""),
            grouped(toplevel(2, Some("org.example.Editor"), "two", false), 7, ""),
            toplevel(3, Some("org.example.Player"), "three", false),
        ];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &before,
                projects: &[],
            },
        );

        // The third window joins, and the group is named.
        let after = [
            grouped(
                toplevel(1, Some("org.legeos.Files"), "one", false),
                7,
                "Writing",
            ),
            grouped(
                toplevel(2, Some("org.example.Editor"), "two", false),
                7,
                "Writing",
            ),
            grouped(
                toplevel(3, Some("org.example.Player"), "three", false),
                7,
                "Writing",
            ),
        ];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &after,
                projects: &[],
            },
        );

        let group = group_tile_id(7);
        assert_eq!(doc.tile(group).expect("still a tile").title, "Writing");
        assert_eq!(group_members(&doc, group, &after).len(), 3);
    }

    #[test]
    fn a_window_that_leaves_its_group_moves_out_and_an_empty_group_tile_is_swept() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let together = [
            grouped(toplevel(1, Some("org.legeos.Files"), "one", false), 7, ""),
            grouped(toplevel(2, Some("org.example.Editor"), "two", false), 7, ""),
        ];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &together,
                projects: &[],
            },
        );
        let group = group_tile_id(7);
        assert_eq!(group_members(&doc, group, &together).len(), 2);

        // The compositor dissolves a group the moment it drops below two members, so both windows
        // come back with no group at all.
        let apart = [
            toplevel(1, Some("org.legeos.Files"), "one", false),
            toplevel(2, Some("org.example.Editor"), "two", false),
        ];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &apart,
                projects: &[],
            },
        );

        assert!(doc.tile(group).is_none(), "the group's tile is swept");
        for toplevel in &apart {
            let tile = session_tile_for(&doc, toplevel).expect("the window keeps its tile");
            let located = doc.location(tile).expect("and keeps a place on the field");
            assert_eq!(
                located.container, None,
                "a window out of its group is back on the bare field, not on the shelf"
            );
        }
    }

    #[test]
    fn a_group_whose_windows_all_close_takes_every_tile_with_it() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let together = [
            grouped(toplevel(1, Some("org.legeos.Files"), "one", false), 7, ""),
            grouped(toplevel(2, Some("org.example.Editor"), "two", false), 7, ""),
        ];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &together,
                projects: &[],
            },
        );
        let sessions: Vec<TileId> = together
            .iter()
            .map(|toplevel| session_tile_for(&doc, toplevel).expect("a tile"))
            .collect();

        merge(
            &mut doc,
            &LiveState {
                toplevels: &[],
                projects: &[],
            },
        );
        assert!(doc.tile(group_tile_id(7)).is_none());
        for session in sessions {
            assert!(doc.tile(session).is_none(), "a closed window's tile goes");
        }
    }

    /// A resize the user performed must survive the background refresh that runs every three
    /// seconds. `merge_sessions` rewrites `spans.preferred` for the session overview, and a
    /// resize lives in the *placement's* span — two different things that would be easy to
    /// conflate later, at which point every resize would silently snap back within one tick.
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

    /// Home may not place a tile the painter has no widget for: it would draw as "no such
    /// widget" and be a dead rectangle on the default field.
    #[test]
    fn every_system_tile_home_places_is_a_widget_the_shell_can_draw() {
        for probe in SYSTEM_WIDGETS {
            assert!(
                crate::surface::widgets::descriptor(probe).is_some(),
                "Home places a {probe:?} tile that `surface::widgets::WIDGETS` does not define"
            );
        }
    }

    /// A field saved when Home had a tile per hardware reading, with the three of them wherever
    /// the person left them.
    fn field_with_the_old_hardware_tiles(cells: [Cell; 3]) -> FieldDocument {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        for (widget, cell) in LEGACY_HARDWARE_WIDGETS.into_iter().zip(cells) {
            let kind = TileKind::System {
                probe: widget.to_owned(),
            };
            let spans = SpanRange::new(
                Span::new(2, 1),
                Span::new(3, 1),
                SpanRange::for_kind(&kind, doc.cols, doc.rows).max,
            );
            let tile = Tile {
                id: tile_id_for(&format!("system:{widget}")),
                kind,
                title: widget.to_owned(),
                spans,
                accent: None,
            };
            doc = apply(
                &doc,
                &FieldOp::Place {
                    tile,
                    landing: Landing {
                        container: None,
                        cell,
                    },
                },
            )
            .expect("the old tile places")
            .0;
        }
        doc
    }

    /// The three superseded tiles become the one that replaced them, in the first place any of
    /// them held — not back in the shelf's default corner, which would throw away the arrangement
    /// the upgrade is supposed to preserve.
    #[test]
    fn a_saved_field_of_hardware_tiles_becomes_one_settings_tile_in_their_place() {
        let corner = [Cell::new(9, 2), Cell::new(12, 2), Cell::new(9, 3)];
        let mut doc = field_with_the_old_hardware_tiles(corner);
        merge(&mut doc, &LiveState::default());

        for widget in LEGACY_HARDWARE_WIDGETS {
            assert!(
                doc.tile(tile_id_for(&format!("system:{widget}"))).is_none(),
                "{widget} is a reading on the Settings tile now, not a tile"
            );
        }
        let settings = tile_id_for("system:settings");
        let located = doc.tile(settings).and(doc.location(settings));
        assert_eq!(
            located.map(|located| located.placement.cell),
            Some(corner[0]),
            "the replacement did not inherit the first of their places"
        );
    }

    /// A field that already has the Settings tile — one saved after the upgrade but before a
    /// stale copy of the old three was written back — loses the leftovers and keeps its own tile
    /// exactly where it is.
    #[test]
    fn a_field_that_already_has_settings_only_loses_the_leftovers() {
        let mut doc =
            field_with_the_old_hardware_tiles([Cell::new(9, 2), Cell::new(12, 2), Cell::new(9, 3)]);
        let chosen = Cell::new(4, 6);
        let tile = system_tile("settings", &doc);
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile,
                landing: Landing {
                    container: None,
                    cell: chosen,
                },
            },
        )
        .expect("the settings tile places")
        .0;

        merge(&mut doc, &LiveState::default());
        for widget in LEGACY_HARDWARE_WIDGETS {
            assert!(doc.tile(tile_id_for(&format!("system:{widget}"))).is_none());
        }
        let settings = tile_id_for("system:settings");
        assert_eq!(
            doc.location(settings).map(|located| located.placement.cell),
            Some(chosen),
            "the tile a person is already using was moved"
        );
    }

    /// The migration touches nothing on a field that never had the old tiles, and never runs
    /// twice: a second merge of the same state is still empty.
    #[test]
    fn the_hardware_migration_is_silent_on_a_field_that_never_had_them() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        merge(&mut doc, &LiveState::default());
        let mut ops = Vec::new();
        migrate_hardware_tiles(&mut doc, &mut ops);
        assert!(ops.is_empty(), "{ops:?}");
        assert!(merge(&mut doc, &LiveState::default()).is_empty());
    }

    #[test]
    fn the_session_widget_is_placed_and_is_never_taken_away_again() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let live = LiveState::default();
        merge(&mut doc, &live);
        let session = tile_id_for("system:session");
        let placed = doc
            .location(session)
            .expect("Home offers a way out of itself");

        // Move it somewhere the user chose, then refresh: add-only means it stays put.
        doc = lege_field::apply(
            &doc,
            &FieldOp::Move {
                id: session,
                landing: Landing {
                    container: None,
                    cell: Cell::new(8, 0),
                },
            },
        )
        .expect("a move on an empty-ish field")
        .0;
        let moved = doc.location(session).unwrap();
        assert_ne!(moved, placed);
        for _ in 0..3 {
            merge(&mut doc, &live);
        }
        assert_eq!(
            doc.location(session),
            Some(moved),
            "a refresh put the session widget back where Home wanted it"
        );
    }

    /// The shelf sits in the bottom-right corner, in bounds, and with one tile per cell — the
    /// failure the alternating-corners rule had when a third widget arrived was two tiles
    /// landing on the same cell.
    #[test]
    fn the_system_shelf_lands_in_the_corner() {
        for cols in 4..=64u16 {
            for rows in 4..=64u16 {
                let doc = FieldDocument::new("eDP-1", cols, rows);
                let kind = TileKind::System {
                    probe: "session".to_owned(),
                };
                let class = SpanRange::for_kind(&kind, cols, rows);
                let spans = SpanRange::new(Span::new(2, 1), class.min, class.max);
                let per_row = (cols / spans.preferred.cols.max(1)).max(1);
                let cells: Vec<Cell> = (0..SYSTEM_WIDGETS.len())
                    .map(|index| system_landing(index, &doc, spans, SYSTEM_WIDGETS.len()))
                    .collect();
                for cell in &cells {
                    assert!(
                        cell.col + spans.preferred.cols <= cols || cell.col == 0,
                        "{cols}x{rows}: {cell:?} runs off the right edge"
                    );
                    assert!(
                        cell.row + spans.preferred.rows <= rows,
                        "{cols}x{rows}: {cell:?} runs off the bottom"
                    );
                }
                // Distinct landings wherever the shelf can actually be seated. On a field too
                // small to hold it — three 3x2 tiles want more than a 4x4 grid has — the landing
                // is only a preference and the reflow settles the rest.
                let seats = (rows / spans.preferred.rows) * per_row;
                if usize::from(seats) >= SYSTEM_WIDGETS.len() {
                    let mut distinct = cells.clone();
                    distinct.sort_by_key(|cell| (cell.row, cell.col));
                    distinct.dedup();
                    assert_eq!(
                        distinct.len(),
                        cells.len(),
                        "{cols}x{rows}: two shelf tiles share a landing — {cells:?}"
                    );
                }
                // The rightmost tile of the bottom row reaches the field's right edge, so the
                // shelf reads as anchored to the corner rather than trailing off mid-field.
                if cols >= spans.preferred.cols {
                    let right = cells.iter().map(|cell| cell.col).max().unwrap();
                    assert_eq!(
                        right + spans.preferred.cols,
                        cols,
                        "{cols}x{rows}: the shelf stops short of the right edge"
                    );
                }
            }
        }
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
        let shelved: Vec<TileId> = [
            files_tile_id(),
            tile_id_for("system:notifications"),
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
        assert_eq!(grid_for(1920, 1016, TILE_SIZE), (18, 9));
        assert_eq!(grid_for(100, 100, TILE_SIZE), (4, 4));
        assert_eq!(grid_for(1920, 1016, 240), (8, 4));
    }

    #[test]
    fn the_system_shelf_never_claims_more_of_a_field_than_its_share() {
        for (cols, rows) in [(4u16, 4u16), (11, 7), (16, 8), (18, 9), (32, 18)] {
            let fitted = system_widgets_that_fit(cols, rows);
            let spent: u32 = fitted
                .iter()
                .map(|widget| {
                    let (c, r) = crate::surface::widgets::descriptor(widget)
                        .map_or((3, 2), |d| d.preferred_span);
                    u32::from(c) * u32::from(r)
                })
                .sum();
            let field = u32::from(cols) * u32::from(rows);
            assert!(
                !fitted.is_empty(),
                "{cols}x{rows}: a field with no widgets at all has no way to log out"
            );
            // The first is always placed even on a field too small for the share, so the ceiling
            // is only binding once there is more than one.
            if fitted.len() > 1 {
                assert!(
                    spent <= field / u32::from(SYSTEM_SHELF_SHARE),
                    "{cols}x{rows}: the shelf claims {spent} of {field} cells"
                );
            }
            // Declaration order is priority order: whatever fits, session comes first.
            assert_eq!(fitted[0], "session");
            assert!(
                fitted.iter().all(|widget| SYSTEM_WIDGETS.contains(widget)),
                "{cols}x{rows}: placed a widget that is not declared"
            );
        }
    }

    #[test]
    fn a_roomy_field_gets_every_widget_and_a_cramped_one_keeps_the_essential_ones() {
        // A laptop at 1920x1080 has room for all six.
        let (cols, rows) = grid_for(1920, 1080, TILE_SIZE);
        assert_eq!(
            system_widgets_that_fit(cols, rows).len(),
            SYSTEM_WIDGETS.len(),
            "{cols}x{rows} should carry the whole shelf"
        );
        // A small field keeps the three a desktop is unusable without and drops the readouts,
        // rather than filling the space windows need with instruments.
        let fitted = system_widgets_that_fit(11, 7);
        assert_eq!(fitted, vec!["session", "notifications", "settings"]);
    }

    #[test]
    fn merge_adds_the_files_object_a_tile_per_app_and_window_and_keeps_places_on_refresh() {
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
        // One Files object, two sessions, and the system widgets Home always offers — plus one
        // op for the system shelf panel. No application tiles: a running window is a session,
        // and an application is on Home only because someone pinned it.
        let expected = 1 + 2 + SYSTEM_WIDGETS.len();
        assert_eq!(ops.len(), expected + 1);
        assert_eq!(doc.tiles.len(), expected);
        assert!(
            !doc.tiles
                .iter()
                .any(|tile| matches!(tile.kind, TileKind::App { .. })),
            "two open windows grew an application badge apiece"
        );
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

    /// The launcher tile leaves a saved field and nothing takes its place: its whole content was
    /// "the applications matching what is typed in the search bar", and that is the omnibox under
    /// the search bar now. Nothing else on the field moves.
    #[test]
    fn a_saved_field_loses_its_launcher_tile_and_gains_no_replacement() {
        let mut doc = FieldDocument::new("eDP-1", 16, 10);
        let kind = TileKind::System {
            probe: LEGACY_LAUNCHER_WIDGET.to_owned(),
        };
        let chosen = Cell::new(9, 6);
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: Tile {
                    id: tile_id_for("system:launcher"),
                    kind,
                    title: "Applications".to_owned(),
                    spans: SpanRange::new(Span::new(2, 1), Span::new(3, 2), Span::new(7, 4)),
                    accent: None,
                },
                landing: Landing {
                    container: None,
                    cell: chosen,
                },
            },
        )
        .expect("the old launcher tile places")
        .0;

        merge(&mut doc, &LiveState::default());
        assert!(
            doc.tile(tile_id_for("system:launcher")).is_none(),
            "the launcher tile is still there"
        );
        assert!(
            !doc.tiles.iter().any(|tile| doc
                .location(tile.id)
                .is_some_and(|located| located.placement.cell == chosen)),
            "something was put in its place; there is no successor tile"
        );
        // Idempotent, and silent on a field that never had one.
        assert!(merge(&mut doc, &LiveState::default()).is_empty());
        let mut fresh = FieldDocument::new("eDP-1", 16, 10);
        merge(&mut fresh, &LiveState::default());
        let mut ops = Vec::new();
        migrate_launcher_tile(&mut fresh, &mut ops);
        assert!(ops.is_empty(), "{ops:?}");
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

    #[test]
    fn every_open_window_gets_overview_space_before_projects_or_app_badges() {
        // A deliberately tight field, saturated by live windows: on eight columns the drawers
        // compact to a single-column rail and the previews step down to their 2×1 floor, and at
        // twenty windows the last free cells are gone. Merging projects or app badges before
        // sessions would hide a window behind a shortcut.
        let mut doc = FieldDocument::new("eDP-1", 8, 8);
        let windows: Vec<_> = (1..=20)
            .map(|id| {
                toplevel(
                    id,
                    Some("org.example.Editor"),
                    &format!("document-{id}"),
                    false,
                )
            })
            .collect();
        let projects = vec![ProjectInfo {
            id: "competing-project".into(),
            name: "Competing project".into(),
            is_open: true,
            ..ProjectInfo::default()
        }];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &windows,
                projects: &projects,
            },
        );

        for window in &windows {
            assert!(
                session_tile_for(&doc, window).is_some(),
                "open window {} must remain visible on Home",
                window.window_id.0
            );
        }
        assert!(
            doc.tile(project_tile_id("competing-project")).is_none(),
            "a generated project shortcut yields when only live-window space remains"
        );
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

    #[test]
    fn a_closed_window_takes_its_tile_with_it() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![toplevel(7, None, "x", false)];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &windows,
                projects: &[],
            },
        );
        let session = session_tile_for(&doc, &windows[0]).unwrap();
        assert!(window_for_tile(&doc, session, &windows).is_some());
        let ops = merge(
            &mut doc,
            &LiveState {
                toplevels: &[],
                projects: &[],
            },
        );
        assert_eq!(ops, vec![FieldOp::Remove { id: session }]);
        assert!(doc.tile(session).is_none());
    }

    #[test]
    fn titles_refresh_without_moving_anything() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let mut windows = vec![toplevel(7, None, "before", false)];
        merge(
            &mut doc,
            &LiveState {
                toplevels: &windows,
                projects: &[],
            },
        );
        let located = doc.located.clone();
        windows[0].title = "after".into();
        refresh_titles(
            &mut doc,
            &LiveState {
                toplevels: &windows,
                projects: &[],
            },
        );
        let session = session_tile_for(&doc, &windows[0]).unwrap();
        assert_eq!(doc.tile(session).unwrap().title, "after");
        assert_eq!(doc.located, located);
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

    /// The system shelf arrives as an adjustable panel: created by the merge, grouping exactly
    /// the system tiles, and add-only — a second merge changes nothing, and a user's
    /// configuration survives. Home creates no other panel; the drawer block that had one is
    /// gone, and the Files object is a single tile with nothing to group.
    #[test]
    fn panel_layout_groups_the_system_shelf_and_nothing_else() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let live = LiveState::default();
        merge(&mut doc, &live);

        let system = doc.panel(system_panel_id()).expect("system panel");
        for widget in SYSTEM_WIDGETS {
            let id = tile_id_for(&format!("system:{widget}"));
            assert!(system.members.contains(&id), "{widget} not grouped");
        }
        assert_eq!(system.arrangement, Arrangement::Custom);
        assert!(doc.panel(drawer_panel_id()).is_none());
        assert_eq!(doc.panels.len(), 1);

        let before = doc.clone();
        let applied = merge(&mut doc, &live);
        assert_eq!(doc, before, "the second merge is a no-op");
        assert!(applied.is_empty());
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
