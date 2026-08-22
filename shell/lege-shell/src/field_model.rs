//! The Home Field's document as the shell keeps it: the persisted layout (one day; for now an
//! in-session one) *merged* with what is live — every application the session knows, every
//! window the compositor reports — so a tile exists for each without the user placing it, and
//! keeps its place once they have.
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
pub const TILE_SIZE: i32 = 120;
/// Logical pixels between tiles.
pub const GAP: i32 = 10;

/// The drawers that appear as tiles on the field, in order: the inbox first (what is waiting
/// for attention), then recent work, then the system's own measurements.
pub const DRAWER_TILES: [Drawer; 6] = [
    Drawer::Downloads,
    Drawer::Recent,
    Drawer::Projects,
    Drawer::Drives,
    Drawer::Operations,
    Drawer::Trash,
];

/// How many cells a field of `width`×`height` logical pixels has at `tile_size` pixels a cell,
/// never fewer than 4×3.
pub fn grid_for(width: i32, height: i32, tile_size: i32) -> (u16, u16) {
    let tile = tile_size.max(1);
    let cols = (width / tile).clamp(4, 64) as u16;
    let rows = (height / tile).clamp(3, 64) as u16;
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
    /// Installed applications (`AppIdentity1.list_apps`).
    pub apps: &'a [AppId],
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

/// The size range a project tile read from `Projects1` gets: small enough to sit beside the
/// applications, never smaller than the 2×2 a container needs; one the user grouped on the
/// field takes a quadrant instead (`SpanRange::for_kind`).
pub fn project_spans(cols: u16, rows: u16) -> SpanRange {
    SpanRange::new(Span::new(2, 2), Span::new(3, 2), Span::new(cols, rows))
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

/// Bring `doc` up to date with `live`: a tile for every drawer, application and window that
/// lacks one, placed by the reflow near a sensible landing; the tile of every window that is
/// gone removed. Existing tiles keep their places. Returns the ops that were applied (already
/// applied to `doc`), so a caller persisting the document can forward them.
pub fn merge(doc: &mut FieldDocument, live: &LiveState<'_>) -> Vec<FieldOp> {
    let mut applied = Vec::new();
    let mut wanted: BTreeSet<TileId> = BTreeSet::new();
    merge_drawers(doc, &mut wanted, &mut applied);
    merge_projects(doc, live, &mut applied);
    merge_apps(doc, live, &mut wanted, &mut applied);
    merge_sessions(doc, live, &mut wanted, &mut applied);

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

/// Drawers down the left edge, two cells wide.
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
        let tile = Tile {
            id,
            kind: TileKind::Widget {
                widget: drawer_widget(*drawer),
                scope: None,
            },
            title: drawer.name().to_owned(),
            spans: SpanRange::new(Span::new(1, 1), Span::new(2, 1), Span::new(4, 3)),
            accent: None,
        };
        let landing = Landing {
            container: None,
            cell: Cell::new(0, index as u16),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
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

/// Applications fill from the third column, in reading order, sorted by id.
fn merge_apps(
    doc: &mut FieldDocument,
    live: &LiveState<'_>,
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    let mut app_ids: Vec<AppId> = live.apps.to_vec();
    for toplevel in live.toplevels {
        if let Some(app_id) = &toplevel.canonical_app_id
            && !app_ids.contains(app_id)
        {
            app_ids.push(app_id.clone());
        }
    }
    app_ids.sort();
    app_ids.dedup();
    let app_cols = doc.cols.saturating_sub(2).max(1);
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
            cell: Cell::new(2 + index % app_cols, index / app_cols),
        };
        try_apply(doc, FieldOp::Place { tile, landing }, applied);
    }
}

/// Sessions along the bottom, most recent window first from the left.
fn merge_sessions(
    doc: &mut FieldDocument,
    live: &LiveState<'_>,
    wanted: &mut BTreeSet<TileId>,
    applied: &mut Vec<FieldOp>,
) {
    let bottom = doc.rows.saturating_sub(1);
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
    for (index, toplevel) in toplevels.iter().enumerate() {
        let identifier = session_identifier(toplevel, None);
        let id = tile_id_for(&format!("session:{identifier}"));
        wanted.insert(id);
        if doc.tile(id).is_some() {
            continue;
        }
        let tile = Tile {
            id,
            kind: TileKind::ParkedSession {
                app_id: toplevel.canonical_app_id.clone(),
                identifier,
            },
            title: if toplevel.title.is_empty() {
                toplevel
                    .canonical_app_id
                    .as_ref()
                    .map_or_else(|| "(untitled)".to_owned(), short_name)
            } else {
                toplevel.title.clone()
            },
            spans: SpanRange::new(Span::new(1, 1), Span::new(2, 1), Span::new(4, 3)),
            accent: None,
        };
        let landing = Landing {
            container: None,
            cell: Cell::new((index as u16) * 2, bottom),
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

/// `org.legeos.Files` → `Files`: the part after the last dot, which is what fits on a tile.
pub fn short_name(app_id: &AppId) -> String {
    let text = app_id.to_string();
    text.rsplit('.').next().unwrap_or(&text).to_owned()
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

    #[test]
    fn a_grid_is_never_smaller_than_four_by_three() {
        assert_eq!(grid_for(1920, 1016, TILE_SIZE), (16, 8));
        assert_eq!(grid_for(100, 100, TILE_SIZE), (4, 3));
        assert_eq!(grid_for(1920, 1016, 240), (8, 4));
    }

    #[test]
    fn merge_adds_a_tile_per_drawer_app_and_window_and_keeps_places_on_refresh() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let apps: Vec<AppId> = vec!["org.legeos.Files".parse().unwrap()];
        let windows = vec![
            toplevel(1, Some("org.legeos.Files"), "Downloads", false),
            toplevel(2, Some("org.example.Editor"), "notes.md", true),
        ];
        let live = LiveState {
            apps: &apps,
            toplevels: &windows,
            projects: &[],
        };
        let ops = merge(&mut doc, &live);
        assert_eq!(ops.len(), DRAWER_TILES.len() + 2 + 2);
        assert_eq!(doc.tiles.len(), DRAWER_TILES.len() + 2 + 2);
        assert_eq!(doc.located.len(), doc.tiles.len(), "everything was placed");

        // Drawers down the left, apps from column 2, sessions along the bottom.
        let inbox = tile_id_for("drawer:Downloads");
        assert_eq!(doc.location(inbox).unwrap().placement.cell, Cell::new(0, 0));
        // Apps are sorted by id: the Editor (known only from its window) precedes Files.
        let files = tile_id_for("app:org.legeos.Files");
        assert_eq!(doc.location(files).unwrap().placement.cell, Cell::new(3, 0));
        let editor = tile_id_for("app:org.example.Editor");
        assert_eq!(
            doc.location(editor).unwrap().placement.cell,
            Cell::new(2, 0)
        );
        let session = session_tile_for(&doc, &windows[1]).unwrap();
        assert_eq!(doc.location(session).unwrap().placement.cell.row, 7);

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
                apps: &[],
                toplevels: &[],
                projects: &projects,
            },
        );
        let thesis = project_tile_id("thesis");
        let located = doc.location(thesis).unwrap();
        assert_eq!(located.placement.cell, Cell::new(13, 0));
        assert_eq!(located.placement.span, Span::new(3, 2));
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
    fn a_closed_window_takes_its_tile_with_it() {
        let mut doc = FieldDocument::new("eDP-1", 16, 8);
        let windows = vec![toplevel(7, None, "x", false)];
        merge(
            &mut doc,
            &LiveState {
                apps: &[],
                toplevels: &windows,
                projects: &[],
            },
        );
        let session = session_tile_for(&doc, &windows[0]).unwrap();
        assert!(window_for_tile(&doc, session, &windows).is_some());
        let ops = merge(
            &mut doc,
            &LiveState {
                apps: &[],
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
                apps: &[],
                toplevels: &windows,
                projects: &[],
            },
        );
        let located = doc.located.clone();
        windows[0].title = "after".into();
        refresh_titles(
            &mut doc,
            &LiveState {
                apps: &[],
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
}
