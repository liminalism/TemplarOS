//! Operations on the document, each applied by [`apply`] and answered with its exact inverse.
//! Undo is `apply(inverse)`: there is no second undo model, in this crate or anywhere above it.
//!
//! Two kinds of op live here. The ones a user performs (`Place`, `Move`, `Resize`, `Stash`,
//! `CreateProject`, trays…) decide *where* things go through the reflow; and two exact ones —
//! [`FieldOp::Layout`] and [`FieldOp::AddTile`] — that set locations precisely, which is what an
//! inverse is made of. [`FieldOp::Batch`] composes, and inverts to the reversed inverses.

use crate::field::{Cell, Container, FieldDocument, Landing, Located, Placement, Tray};
use crate::reflow::{Grid, apply_reflow, find_placement, find_resize, tidy};
use crate::tile::{Span, SpanRange, Tile, TileId, TrayId};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Why an op was refused. Nothing is applied on error: [`apply`] works on a copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FieldError {
    UnknownTile,
    UnknownContainer,
    UnknownTray,
    DuplicateTile,
    DuplicateTray,
    DuplicateReference,
    UnknownReference,
    /// The exact placement asked for is occupied or outside the grid.
    NotFree,
    /// Not even the tile's minimum span can be made to fit.
    NoRoom,
    /// A project cannot go inside a project (containers are one level deep).
    NestingTooDeep,
    /// Only a project tile has a container.
    NotAContainer,
    /// The span is outside the tile's accepted range.
    BadSpan,
    /// The document's `version` is not [`crate::DOCUMENT_VERSION`].
    VersionMismatch,
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnknownTile => "no such tile",
            Self::UnknownContainer => "no such project container",
            Self::UnknownTray => "no such tray",
            Self::DuplicateTile => "a tile with that id already exists",
            Self::DuplicateTray => "a tray with that id already exists",
            Self::DuplicateReference => "the tray already holds that reference",
            Self::UnknownReference => "the tray does not hold that reference",
            Self::NotFree => "that placement is occupied or outside the grid",
            Self::NoRoom => "no room for the tile, even at its minimum size",
            Self::NestingTooDeep => "a project cannot be placed inside a project",
            Self::NotAContainer => "only a project tile can hold other tiles",
            Self::BadSpan => "the span is outside the tile's accepted range",
            Self::VersionMismatch => "the document is from another version of the field",
        };
        f.write_str(message)
    }
}

impl std::error::Error for FieldError {}

/// One change to a [`FieldDocument`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum FieldOp {
    /// A new tile, dropped at `landing`; the reflow decides the exact placement.
    Place {
        tile: Tile,
        landing: Landing,
    },
    /// A new tile at an exact location (or on the shelf), no reflow — the inverse of `Remove`.
    /// `container` recreates a project's interior grid.
    AddTile {
        tile: Tile,
        at: Option<Located>,
        container: Option<Container>,
    },
    /// The tile is gone from the document. A removed project's children go to the shelf.
    Remove {
        id: TileId,
    },
    /// An existing tile dropped at `landing` (possibly on another grid, or out of a project).
    Move {
        id: TileId,
        landing: Landing,
    },
    /// A tile's span changes in place; band-mates may be pushed.
    Resize {
        id: TileId,
        span: Span,
    },
    /// To the shelf: the tile keeps existing but has no place on the field.
    Stash {
        id: TileId,
    },
    /// Exact locations, no reflow: `None` means the shelf. Every entry must end up free.
    Layout {
        set: Vec<(TileId, Option<Located>)>,
    },
    /// Explicit global packing of one grid (`None` = the top level).
    Tidy {
        container: Option<TileId>,
    },
    /// The field is now `cols`×`rows`; tiles that no longer fit are re-placed or shelved.
    SetGrid {
        cols: u16,
        rows: u16,
    },
    /// A new project whose container absorbs `absorb` (tiles currently on the top level),
    /// placed at `landing` with its preferred span — "throw things into a quadrant".
    CreateProject {
        project: Tile,
        landing: Landing,
        absorb: Vec<TileId>,
    },
    CreateTray {
        tray: Tray,
    },
    RemoveTray {
        tray: TrayId,
    },
    AddReference {
        tray: TrayId,
        path: PathBuf,
    },
    RemoveReference {
        tray: TrayId,
        path: PathBuf,
    },
    /// Several ops as one; inverts to the reversed inverses.
    Batch {
        ops: Vec<FieldOp>,
    },
}

impl FieldOp {
    /// The text a pending operation is announced with before release — and what `--print`
    /// shows — so the two can never disagree.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Place { .. } => "PLACE",
            Self::AddTile { .. } => "ADD",
            Self::Remove { .. } => "REMOVE FROM FIELD",
            Self::Move { .. } => "MOVE",
            Self::Resize { .. } => "RESIZE",
            Self::Stash { .. } => "STASH ON SHELF",
            Self::Layout { .. } => "ARRANGE",
            Self::Tidy { .. } => "TIDY",
            Self::SetGrid { .. } => "RESIZE FIELD",
            Self::CreateProject { .. } => "NEW PROJECT FROM THESE",
            Self::CreateTray { .. } => "NEW TRAY",
            Self::RemoveTray { .. } => "REMOVE TRAY",
            Self::AddReference { .. } => "ADD REFERENCE · files remain in place",
            Self::RemoveReference { .. } => "REMOVE REFERENCE",
            Self::Batch { .. } => "SEVERAL CHANGES",
        }
    }
}

/// Apply `op` to a copy of `doc`. On success, the new document and the op that exactly undoes
/// it; on failure, nothing changed.
pub fn apply(doc: &FieldDocument, op: &FieldOp) -> Result<(FieldDocument, FieldOp), FieldError> {
    if doc.version != crate::DOCUMENT_VERSION {
        return Err(FieldError::VersionMismatch);
    }
    let mut next = doc.clone();
    let inverse = apply_in_place(&mut next, op)?;
    Ok((next, inverse))
}

fn apply_in_place(doc: &mut FieldDocument, op: &FieldOp) -> Result<FieldOp, FieldError> {
    match op {
        FieldOp::Place { tile, landing } => place(doc, tile, *landing),
        FieldOp::AddTile {
            tile,
            at,
            container,
        } => add_tile(doc, tile, *at, container.as_ref()),
        FieldOp::Remove { id } => remove(doc, *id),
        FieldOp::Move { id, landing } => move_tile(doc, *id, *landing),
        FieldOp::Resize { id, span } => resize(doc, *id, *span),
        FieldOp::Stash { id } => {
            doc.tile(*id).ok_or(FieldError::UnknownTile)?;
            let before = doc.location(*id);
            doc.set_location(*id, None);
            Ok(FieldOp::Layout {
                set: vec![(*id, before)],
            })
        }
        FieldOp::Layout { set } => layout(doc, set),
        FieldOp::Tidy { container } => {
            let mut grid = grid_of(doc, *container)?;
            let moved = tidy(&grid);
            let set = moved_back(*container, &moved);
            for (id, _, to) in &moved {
                if let Some(entry) = grid.places.iter_mut().find(|(tile, _)| tile == id) {
                    entry.1 = *to;
                }
            }
            write_grid(doc, *container, &grid);
            Ok(FieldOp::Layout { set })
        }
        FieldOp::SetGrid { cols, rows } => set_grid(doc, *cols, *rows),
        FieldOp::CreateProject {
            project,
            landing,
            absorb,
        } => create_project(doc, project, *landing, absorb),
        FieldOp::CreateTray { tray } => {
            if doc.tray(tray.id).is_some() {
                return Err(FieldError::DuplicateTray);
            }
            doc.insert_tray(tray.clone());
            Ok(FieldOp::RemoveTray { tray: tray.id })
        }
        FieldOp::RemoveTray { tray } => {
            let index = doc
                .trays
                .iter()
                .position(|t| t.id == *tray)
                .ok_or(FieldError::UnknownTray)?;
            let tray = doc.trays.remove(index);
            Ok(FieldOp::CreateTray { tray })
        }
        FieldOp::AddReference { tray, path } => {
            let entry = doc.tray_mut(*tray).ok_or(FieldError::UnknownTray)?;
            if entry.entries.contains(path) {
                return Err(FieldError::DuplicateReference);
            }
            entry.entries.push(path.clone());
            Ok(FieldOp::RemoveReference {
                tray: *tray,
                path: path.clone(),
            })
        }
        FieldOp::RemoveReference { tray, path } => {
            let entry = doc.tray_mut(*tray).ok_or(FieldError::UnknownTray)?;
            let index = entry
                .entries
                .iter()
                .position(|p| p == path)
                .ok_or(FieldError::UnknownReference)?;
            entry.entries.remove(index);
            Ok(FieldOp::AddReference {
                tray: *tray,
                path: path.clone(),
            })
        }
        FieldOp::Batch { ops } => {
            let mut inverses = Vec::with_capacity(ops.len());
            for op in ops {
                inverses.push(apply_in_place(doc, op)?);
            }
            inverses.reverse();
            Ok(FieldOp::Batch { ops: inverses })
        }
    }
}

fn place(doc: &mut FieldDocument, tile: &Tile, landing: Landing) -> Result<FieldOp, FieldError> {
    if doc.tile(tile.id).is_some() {
        return Err(FieldError::DuplicateTile);
    }
    check_grid_accepts(doc, landing.container, tile)?;
    let mut grid = grid_of(doc, landing.container)?;
    let reflow = find_placement(&grid, &tile.spans, landing.cell, None)?;
    let moved_back = moved_back(landing.container, &reflow.moved);
    apply_reflow(&mut grid, tile.id, &reflow);
    doc.insert_tile(tile.clone());
    if tile.kind.is_container() {
        doc.insert_container(container_for(tile));
    }
    write_grid(doc, landing.container, &grid);
    Ok(FieldOp::Batch {
        ops: vec![
            FieldOp::Remove { id: tile.id },
            FieldOp::Layout { set: moved_back },
        ],
    })
}

fn add_tile(
    doc: &mut FieldDocument,
    tile: &Tile,
    at: Option<Located>,
    container: Option<&Container>,
) -> Result<FieldOp, FieldError> {
    if doc.tile(tile.id).is_some() {
        return Err(FieldError::DuplicateTile);
    }
    if let Some(at) = at {
        check_grid_accepts(doc, at.container, tile)?;
        if !doc.is_free(at.container, at.placement, None) {
            return Err(FieldError::NotFree);
        }
    }
    doc.insert_tile(tile.clone());
    if tile.kind.is_container() {
        doc.insert_container(container.cloned().unwrap_or_else(|| container_for(tile)));
    }
    doc.set_location(tile.id, at);
    Ok(FieldOp::Remove { id: tile.id })
}

fn remove(doc: &mut FieldDocument, id: TileId) -> Result<FieldOp, FieldError> {
    let index = doc
        .tiles
        .iter()
        .position(|tile| tile.id == id)
        .ok_or(FieldError::UnknownTile)?;
    let tile = doc.tiles.remove(index);
    let at = doc.location(id);
    doc.set_location(id, None);
    let container = doc.container(id).cloned();
    let mut restore_children = Vec::new();
    if container.is_some() {
        for (child, placement) in doc.placements_in(Some(id)) {
            restore_children.push((
                child,
                Some(Located {
                    container: Some(id),
                    placement,
                }),
            ));
            doc.set_location(child, None);
        }
        doc.containers.retain(|c| c.id != id);
    }
    let mut inverse = vec![FieldOp::AddTile {
        tile,
        at,
        container,
    }];
    if !restore_children.is_empty() {
        inverse.push(FieldOp::Layout {
            set: restore_children,
        });
    }
    Ok(FieldOp::Batch { ops: inverse })
}

fn move_tile(doc: &mut FieldDocument, id: TileId, landing: Landing) -> Result<FieldOp, FieldError> {
    let tile = doc.tile(id).cloned().ok_or(FieldError::UnknownTile)?;
    if landing.container == Some(id) {
        return Err(FieldError::NestingTooDeep);
    }
    check_grid_accepts(doc, landing.container, &tile)?;
    let before = doc.location(id);
    doc.set_location(id, None);
    let mut grid = grid_of(doc, landing.container)?;
    let reflow = match find_placement(&grid, &tile.spans, landing.cell, None) {
        Ok(reflow) => reflow,
        Err(error) => {
            doc.set_location(id, before);
            return Err(error);
        }
    };
    let mut set = moved_back(landing.container, &reflow.moved);
    set.push((id, before));
    apply_reflow(&mut grid, id, &reflow);
    write_grid(doc, landing.container, &grid);
    Ok(FieldOp::Layout { set })
}

fn resize(doc: &mut FieldDocument, id: TileId, span: Span) -> Result<FieldOp, FieldError> {
    let tile = doc.tile(id).cloned().ok_or(FieldError::UnknownTile)?;
    if !tile.spans.accepts(span) {
        return Err(FieldError::BadSpan);
    }
    let before = doc.location(id).ok_or(FieldError::NotFree)?;
    let (grid_cols, grid_rows) = doc
        .grid_size(before.container)
        .ok_or(FieldError::UnknownTile)?;
    // Nothing in the widget system may fill its whole grid. A tile that covers everything is a
    // maximised window wearing a tile's clothes, and the field stops being a field. Only a
    // container may, because a project's full-field state is the zoom *view* — a different
    // mechanism that leaves the document alone (`docs/homefield.md`, "Do not recreate virtual
    // desktops under another name").
    if !tile.kind.is_container() && span.cols >= grid_cols && span.rows >= grid_rows {
        return Err(FieldError::BadSpan);
    }
    let mut grid = grid_of(doc, before.container)?;
    let ranges: Vec<(TileId, SpanRange)> = grid
        .places
        .iter()
        .filter_map(|(placed, _)| doc.tile(*placed).map(|tile| (*placed, tile.spans)))
        .collect();
    let reflow = find_resize(&grid, &ranges, id, span)?;
    // `reflow.moved` records each yielding neighbour's *whole prior placement*, span included,
    // and `moved_back` turns those into `Layout` entries — so the inverse restores neighbours'
    // sizes as exactly as it restores their positions, with no new op and no format change.
    let mut set = moved_back(before.container, &reflow.moved);
    set.push((id, Some(before)));
    apply_reflow(&mut grid, id, &reflow);
    write_grid(doc, before.container, &grid);
    Ok(FieldOp::Layout { set })
}

fn layout(
    doc: &mut FieldDocument,
    set: &[(TileId, Option<Located>)],
) -> Result<FieldOp, FieldError> {
    let mut before = Vec::with_capacity(set.len());
    for (id, _) in set {
        doc.tile(*id).ok_or(FieldError::UnknownTile)?;
        before.push((*id, doc.location(*id)));
    }
    for (id, _) in set {
        doc.set_location(*id, None);
    }
    for (id, location) in set {
        if let Some(located) = location {
            let tile = doc.tile(*id).cloned().ok_or(FieldError::UnknownTile)?;
            check_grid_accepts(doc, located.container, &tile)?;
            if !doc.is_free(located.container, located.placement, None) {
                return Err(FieldError::NotFree);
            }
        }
        doc.set_location(*id, *location);
    }
    Ok(FieldOp::Layout { set: before })
}

fn set_grid(doc: &mut FieldDocument, cols: u16, rows: u16) -> Result<FieldOp, FieldError> {
    let (old_cols, old_rows) = (doc.cols, doc.rows);
    doc.cols = cols.max(1);
    doc.rows = rows.max(1);
    let mut set = Vec::new();
    let misfits: Vec<(TileId, Placement)> = doc
        .placements_in(None)
        .into_iter()
        .filter(|(_, placement)| !placement.fits(doc.cols, doc.rows))
        .collect();
    for (id, from) in misfits {
        let tile = doc.tile(id).cloned().ok_or(FieldError::UnknownTile)?;
        let before = doc.location(id);
        doc.set_location(id, None);
        let mut grid = grid_of(doc, None)?;
        match find_placement(&grid, &tile.spans, from.cell, None) {
            Ok(reflow) => {
                set.extend(moved_back(None, &reflow.moved));
                apply_reflow(&mut grid, id, &reflow);
                write_grid(doc, None, &grid);
            }
            Err(FieldError::NoRoom) => {}
            Err(error) => return Err(error),
        }
        set.push((id, before));
    }
    Ok(FieldOp::Batch {
        ops: vec![
            FieldOp::SetGrid {
                cols: old_cols,
                rows: old_rows,
            },
            FieldOp::Layout { set },
        ],
    })
}

fn create_project(
    doc: &mut FieldDocument,
    project: &Tile,
    landing: Landing,
    absorb: &[TileId],
) -> Result<FieldOp, FieldError> {
    if !project.kind.is_container() {
        return Err(FieldError::NotAContainer);
    }
    if doc.tile(project.id).is_some() {
        return Err(FieldError::DuplicateTile);
    }
    if landing.container.is_some() {
        return Err(FieldError::NestingTooDeep);
    }
    // The same tile named twice is absorbed once — recording it twice would make the inverse
    // put it back and then take it away again.
    let mut absorb: Vec<TileId> = absorb.to_vec();
    let mut seen = std::collections::HashSet::new();
    absorb.retain(|id| seen.insert(*id));
    let absorb = &absorb;
    for id in absorb {
        let tile = doc.tile(*id).ok_or(FieldError::UnknownTile)?;
        if tile.kind.is_container() {
            return Err(FieldError::NestingTooDeep);
        }
    }
    let mut absorbed_back = Vec::with_capacity(absorb.len());
    for id in absorb {
        absorbed_back.push((*id, doc.location(*id)));
        doc.set_location(*id, None);
    }
    let mut grid = grid_of(doc, None)?;
    let reflow = match find_placement(&grid, &project.spans, landing.cell, None) {
        Ok(reflow) => reflow,
        Err(error) => {
            for (id, location) in absorbed_back {
                doc.set_location(id, location);
            }
            return Err(error);
        }
    };
    let moved = moved_back(None, &reflow.moved);
    apply_reflow(&mut grid, project.id, &reflow);
    doc.insert_tile(project.clone());
    doc.insert_container(container_for(project));
    write_grid(doc, None, &grid);
    // Children: first-fit in the order given, at their preferred span, shrinking if needed. A
    // child that cannot fit even at its minimum stays on the shelf, visibly.
    let mut inner = grid_of(doc, Some(project.id))?;
    for id in absorb {
        let tile = doc.tile(*id).cloned().ok_or(FieldError::UnknownTile)?;
        let spans = SpanRange::new(
            tile.spans.min,
            tile.spans.preferred,
            Span::new(
                tile.spans.max.cols.min(inner.cols),
                tile.spans.max.rows.min(inner.rows),
            ),
        );
        if let Ok(child) = find_placement(&inner, &spans, Cell::new(0, 0), None) {
            apply_reflow(&mut inner, *id, &child);
        }
    }
    write_grid(doc, Some(project.id), &inner);
    // Restore absorbed and displaced tiles atomically. Their original and post-reflow
    // rectangles can cross: two separate Layout ops could try to restore an absorbed tile into
    // a rectangle still occupied by a displaced tile and reject a valid inverse as NotFree.
    absorbed_back.extend(moved);
    Ok(FieldOp::Batch {
        ops: vec![
            FieldOp::Remove { id: project.id },
            FieldOp::Layout { set: absorbed_back },
        ],
    })
}

/// A project's interior grid when it is created: twice its preferred span in each direction, so
/// a quadrant-sized project holds a comfortable handful of tiles.
fn container_for(project: &Tile) -> Container {
    Container {
        id: project.id,
        cols: project.spans.preferred.cols.saturating_mul(2).max(2),
        rows: project.spans.preferred.rows.saturating_mul(2).max(2),
    }
}

/// Whether `tile` may go on `container`'s grid: the grid exists, and containers stay one deep.
fn check_grid_accepts(
    doc: &FieldDocument,
    container: Option<TileId>,
    tile: &Tile,
) -> Result<(), FieldError> {
    if let Some(project) = container {
        if tile.kind.is_container() {
            return Err(FieldError::NestingTooDeep);
        }
        let parent = doc.tile(project).ok_or(FieldError::UnknownContainer)?;
        if !parent.kind.is_container() || doc.container(project).is_none() {
            return Err(FieldError::NotAContainer);
        }
    }
    Ok(())
}

fn grid_of(doc: &FieldDocument, container: Option<TileId>) -> Result<Grid, FieldError> {
    let (cols, rows) = doc
        .grid_size(container)
        .ok_or(FieldError::UnknownContainer)?;
    Ok(Grid {
        cols,
        rows,
        places: doc.placements_in(container),
    })
}

fn write_grid(doc: &mut FieldDocument, container: Option<TileId>, grid: &Grid) {
    for (id, placement) in &grid.places {
        doc.set_location(
            *id,
            Some(Located {
                container,
                placement: *placement,
            }),
        );
    }
}

/// The `Layout` entries that put every displaced tile back where it was.
fn moved_back(
    container: Option<TileId>,
    moved: &[(TileId, Placement, Placement)],
) -> Vec<(TileId, Option<Located>)> {
    moved
        .iter()
        .map(|(id, from, _)| {
            (
                *id,
                Some(Located {
                    container,
                    placement: *from,
                }),
            )
        })
        .collect()
}
