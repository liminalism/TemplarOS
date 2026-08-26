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

use lege_field::{
    Cell, FieldDocument, FieldError, FieldOp, Landing, Span, SpanRange, Tile, TileId, TileKind,
    apply,
};
use lege_types::AppId;
use lege_window::{ShellToplevel, WindowId};

use crate::home_surface::Drawer;

/// Logical pixels per grid cell. `shell.home-field.tile-size` once the field is persisted.
pub const TILE_SIZE: i32 = 104;
/// Logical pixels between tiles.
pub const GAP: i32 = 6;

/// The drawers that appear as tiles on the field: real file locations first, then recent work
/// and the system's own measurements. Documents and Media used to exist only inside the expanded
/// rail, leaving Home sparse and hiding useful file surfaces from pointer users.
pub const DRAWER_TILES: [Drawer; 8] = [
    Drawer::Downloads,
    Drawer::Recent,
    Drawer::Projects,
    Drawer::Drives,
    Drawer::Operations,
    Drawer::Trash,
    Drawer::Documents,
    Drawer::Media,
];

/// The `TileKind::System` probes Home places for itself. Each must be a widget the shell knows
/// (`crate::surface::widgets::WIDGETS`), which a test asserts.
///
/// The launcher is here for the same reason Session is: without it a freshly booted session has
/// nothing on the field to open anything with, because application tiles are derived from windows
/// that already exist (`merge_apps`) and go away with the last one.
pub const SYSTEM_WIDGETS: [&str; 2] = ["session", "launcher"];

const DRAWER_GRID_COLUMNS: u16 = 2;
const DRAWER_SPAN: u16 = 2;

fn drawer_reserved_cols(cols: u16) -> u16 {
    (DRAWER_GRID_COLUMNS * DRAWER_SPAN).min(cols.saturating_sub(1))
}

/// How many cells a field of `width`×`height` logical pixels has at `tile_size` pixels a cell,
/// never fewer than 4×4 (a project tile's design floor).
pub fn grid_for(width: i32, height: i32, tile_size: i32) -> (u16, u16) {
    let tile = tile_size.max(1);
    let cols = (width / tile).clamp(4, 64) as u16;
    let rows = (height / tile).clamp(4, 64) as u16;
    (cols, rows)
}

/// One project as `Projects1.List` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
    pub is_open: bool,
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

/// The widget name a drawer tile carries (`TileKind::Widget::widget`).
pub fn drawer_widget(drawer: Drawer) -> String {
    format!("drawer:{}", drawer.name().to_ascii_lowercase())
}

/// The drawer a widget tile stands for, if it is one.
pub fn widget_drawer(widget: &str) -> Option<Drawer> {
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
    merge_drawers(doc, &mut wanted, &mut applied);
    // A Home overview must never sacrifice a real open window to a project shortcut or redundant
    // application badge when space is tight, so sessions get first claim after the utility block.
    merge_sessions(doc, live, &mut wanted, &mut applied);
    merge_projects(doc, live, &mut applied);
    merge_system(doc, &mut applied);
    merge_apps(doc, live, &mut wanted, &mut applied);

    // Windows that closed take their tiles with them; everything else the user placed stays.
    let gone: Vec<TileId> = doc
        .tiles
        .iter()
        .filter(|tile| {
            matches!(
                tile.kind,
                TileKind::ParkedSession { .. } | TileKind::App { .. } | TileKind::Widget { .. }
            ) && !wanted.contains(&tile.id)
        })
        .map(|tile| tile.id)
        .collect();
    for id in gone {
        try_apply(doc, FieldOp::Remove { id }, &mut applied);
    }
    applied
}

/// Compact drawers form a 2×4 utility block. They obey the widget's 2×2 floor but do not all
/// demand its 4×4 instrument view on first sight; the user can grow the ones that matter.
fn merge_drawers(
    doc: &mut FieldDocument,
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    for (index, drawer) in DRAWER_TILES.iter().enumerate() {
        let id = tile_id_for(&format!("drawer:{}", drawer.name()));
        wanted.insert(id);
        if doc.tile(id).is_some() {
            continue;
        }
        let kind = TileKind::Widget {
            widget: drawer_widget(*drawer),
            scope: None,
        };
        let tile = Tile {
            id,
            kind,
            title: drawer.name().to_owned(),
            // Compact by default, but the ceiling is the widget class's, not the default size:
            // a drawer the user grows is how Downloads becomes a real treemap instead of a
            // colour band, and it was previously capped one step above its own preferred size.
            spans: SpanRange::new(
                Span::new(DRAWER_SPAN, DRAWER_SPAN),
                Span::new(DRAWER_SPAN, DRAWER_SPAN),
                Span::new(8, 6),
            ),
            accent: None,
        };
        let landing = Landing {
            container: None,
            cell: Cell::new(
                (index as u16 % DRAWER_GRID_COLUMNS) * DRAWER_SPAN,
                (index as u16 / DRAWER_GRID_COLUMNS) * DRAWER_SPAN,
            ),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// The system widgets Home always offers. One today: Session, which is the only route out of a
/// running session that does not involve killing the compositor from a virtual terminal.
///
/// Add-only, like projects: a system tile is never removed by a merge, so a user who has moved
/// or resized one keeps their arrangement. The landing is the bottom-left, under the drawer
/// block; if that is taken the reflow finds the nearest place rather than displacing a drawer.
fn merge_system(doc: &mut FieldDocument, applied: &mut Vec<FieldOp>) {
    for (index, widget) in SYSTEM_WIDGETS.iter().enumerate() {
        let id = tile_id_for(&format!("system:{widget}"));
        let kind = TileKind::System {
            probe: (*widget).to_owned(),
        };
        // Compact by default, with the class ceiling intact — the same bargain the drawer block
        // strikes above. Two system widgets at their *preferred* 4x2 claim sixteen cells of a
        // laptop's seventy-seven before a single window exists, which is a field that arrives
        // full; at the 3x2 floor they are a strip along the bottom that a person grows if they
        // want the launcher to list more than a handful of rows at a time.
        let class = SpanRange::for_kind(&kind, doc.cols, doc.rows);
        let spans = SpanRange::new(class.min, class.min, class.max);
        let cell = system_landing(index, doc, spans);
        if doc.tile(id).is_some() {
            continue;
        }
        let heading = crate::surface::widgets::descriptor(widget).map_or_else(
            || (*widget).to_owned(),
            |descriptor| descriptor.title.to_owned(),
        );
        let tile = Tile {
            id,
            kind,
            title: heading,
            spans,
            accent: None,
        };
        let landing = Landing {
            container: None,
            cell,
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// Where Home first puts each system tile: alternating bottom corners.
///
/// Both used to land on the bottom left, which meant the second one arrived on top of the first
/// and the reflow shoved it — and then a drawer — somewhere neither belonged. Opposite corners
/// keep the two apart on any grid wide enough for them, and leave the middle of the field, where
/// windows and applications live, alone.
fn system_landing(index: usize, doc: &FieldDocument, spans: SpanRange) -> Cell {
    let row = doc.rows.saturating_sub(spans.preferred.rows);
    if index.is_multiple_of(2) {
        Cell::new(0, row)
    } else {
        Cell::new(doc.cols.saturating_sub(spans.preferred.cols), row)
    }
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

/// Active applications fill after the compact drawer block, in reading order, sorted by id.
/// Installed-but-idle applications belong in search/launcher results, not as hundreds of
/// automatic 1×1 Home tiles.
fn merge_apps(
    doc: &mut FieldDocument,
    live: &LiveState<'_>,
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    let mut app_ids: Vec<AppId> = Vec::new();
    for toplevel in live.toplevels {
        if let Some(app_id) = &toplevel.canonical_app_id
            && !app_ids.contains(app_id)
        {
            app_ids.push(app_id.clone());
        }
    }
    app_ids.sort();
    app_ids.dedup();
    let start_col = drawer_reserved_cols(doc.cols);
    let app_cols = doc.cols.saturating_sub(start_col).max(1);
    for (index, app_id) in app_ids.iter().enumerate() {
        let id = tile_id_for(&format!("app:{app_id}"));
        wanted.insert(id);
        if doc.tile(id).is_some() {
            continue;
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
        let index = index as u16;
        let landing = Landing {
            container: None,
            cell: Cell::new(start_col + index % app_cols, index / app_cols),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// The largest common overview span that lets every live session fit in the field area to the
/// right of the drawer block. Large previews remain the normal case; busy workspaces step down
/// through still-legible shapes before reaching the 2×1 session floor.
fn session_overview_span(cols: u16, rows: u16, session_count: usize) -> Span {
    let available_cols = cols.saturating_sub(drawer_reserved_cols(cols));
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
    let start_col = drawer_reserved_cols(doc.cols);
    let per_row = doc
        .cols
        .saturating_sub(start_col)
        .checked_div(overview_span.cols)
        .unwrap_or(0)
        .max(1);
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
        let row = doc.rows.saturating_sub(
            slot.checked_div(per_row)
                .unwrap_or(0)
                .saturating_add(1)
                .saturating_mul(overview_span.rows),
        );
        let landing = Landing {
            container: None,
            cell: Cell::new(start_col + (slot % per_row) * overview_span.cols, row),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
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

        // An application tile, which sits in the open part of the field rather than inside the
        // packed drawer block, so the resize has somewhere to go.
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

    #[test]
    fn a_grid_is_never_smaller_than_four_by_four() {
        assert_eq!(grid_for(1920, 1016, TILE_SIZE), (18, 9));
        assert_eq!(grid_for(100, 100, TILE_SIZE), (4, 4));
        assert_eq!(grid_for(1920, 1016, 240), (8, 4));
    }

    #[test]
    fn merge_adds_a_tile_per_drawer_app_and_window_and_keeps_places_on_refresh() {
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
        // Eight drawers, two apps, two sessions, and the system widgets Home always offers.
        let expected = DRAWER_TILES.len() + 2 + 2 + SYSTEM_WIDGETS.len();
        assert_eq!(ops.len(), expected);
        assert_eq!(doc.tiles.len(), expected);
        assert_eq!(doc.located.len(), doc.tiles.len(), "everything was placed");

        // Drawers occupy a compact 4×8 block, apps follow it, sessions sit at the bottom.
        let inbox = tile_id_for("drawer:Downloads");
        assert_eq!(doc.location(inbox).unwrap().placement.cell, Cell::new(0, 0));
        // Apps are sorted by id: the Editor (known only from its window) precedes Files.
        let files = tile_id_for("app:org.legeos.Files");
        assert_eq!(doc.location(files).unwrap().placement.cell, Cell::new(5, 0));
        let editor = tile_id_for("app:org.example.Editor");
        assert_eq!(
            doc.location(editor).unwrap().placement.cell,
            Cell::new(4, 0)
        );
        let session = session_tile_for(&doc, &windows[1]).unwrap();
        assert_eq!(doc.location(session).unwrap().placement.cell.row, 6);

        // A second merge with the same state changes nothing.
        let before = doc.clone();
        assert!(merge(&mut doc, &live).is_empty());
        assert_eq!(doc, before);

        // The user moves a tile; the next refresh keeps it there.
        let moved = apply(
            &doc,
            &FieldOp::Move {
                id: files,
                landing: Landing {
                    container: None,
                    cell: Cell::new(10, 4),
                },
            },
        )
        .unwrap()
        .0;
        doc = moved;
        merge(&mut doc, &live);
        assert_eq!(
            doc.location(files).unwrap().placement.cell,
            Cell::new(10, 4)
        );
    }

    #[test]
    fn projects_get_tiles_at_the_right_edge_and_are_never_auto_removed() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let projects = vec![ProjectInfo {
            id: "thesis".into(),
            name: "Thesis".into(),
            is_open: false,
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
        // Eight drawers fill the left half of this deliberately tight field. Eight 2×2 session
        // previews exactly fill the right half, so merging projects or app badges first would
        // hide windows.
        let mut doc = FieldDocument::new("eDP-1", 8, 8);
        let windows: Vec<_> = (1..=8)
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

    #[test]
    fn drawer_widgets_round_trip() {
        for drawer in DRAWER_TILES {
            assert_eq!(widget_drawer(&drawer_widget(drawer)), Some(drawer));
        }
        assert_eq!(widget_drawer("clock"), None);
    }

    #[test]
    fn imported_desktop_ids_keep_the_application_name_not_the_desktop_suffix() {
        let imported: AppId = "desktop-import.org.example.Editor.desktop".parse().unwrap();
        assert_eq!(short_name(&imported), "Editor");
        let simple: AppId = "desktop-import.nm-tray.desktop".parse().unwrap();
        assert_eq!(short_name(&simple), "nm-tray");
    }
}
