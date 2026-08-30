//! The document: a bounded grid, the tiles on it, the project containers one level down, the
//! shelf (tiles with no location), and trays of references. Plus the one place grid cells meet
//! pixels (`cell_rect` / `cell_at` / `tile_rect`).

use crate::tile::{Span, Tile, TileId, TileKind, TrayId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A grid coordinate, column then row, zero-based from the top left.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Cell {
    pub col: u16,
    pub row: u16,
}

impl Cell {
    pub const fn new(col: u16, row: u16) -> Self {
        Self { col, row }
    }

    /// Manhattan distance, the field's notion of "near".
    pub fn distance(self, other: Cell) -> u32 {
        u32::from(self.col.abs_diff(other.col)) + u32::from(self.row.abs_diff(other.row))
    }
}

/// A tile's rectangle on a grid: its top-left cell and its span.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub cell: Cell,
    pub span: Span,
}

impl Placement {
    pub const fn new(cell: Cell, span: Span) -> Self {
        Self { cell, span }
    }

    pub fn right(self) -> u16 {
        self.cell.col.saturating_add(self.span.cols)
    }

    pub fn bottom(self) -> u16 {
        self.cell.row.saturating_add(self.span.rows)
    }

    pub fn overlaps(self, other: Placement) -> bool {
        self.cell.col < other.right()
            && other.cell.col < self.right()
            && self.cell.row < other.bottom()
            && other.cell.row < self.bottom()
    }

    pub fn contains(self, cell: Cell) -> bool {
        cell.col >= self.cell.col
            && cell.col < self.right()
            && cell.row >= self.cell.row
            && cell.row < self.bottom()
    }

    /// Whether the whole placement lies inside a `cols`×`rows` grid.
    pub fn fits(self, cols: u16, rows: u16) -> bool {
        self.right() <= cols && self.bottom() <= rows
    }

    /// Whether this placement shares a row or a column with `other` — the "affected band" a
    /// local reflow is allowed to touch.
    pub fn shares_band(self, other: Placement) -> bool {
        let rows_meet = self.cell.row < other.bottom() && other.cell.row < self.bottom();
        let cols_meet = self.cell.col < other.right() && other.cell.col < self.right();
        rows_meet || cols_meet
    }
}

/// Where a tile is: on the top-level grid, or inside one project container. A tile that has no
/// `Located` at all is on the shelf.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Located {
    /// The project tile whose container holds this one; `None` for the top level.
    pub container: Option<TileId>,
    pub placement: Placement,
}

/// Where the user asked a tile to go: which grid, and the cell under the pointer. The exact
/// placement is decided by the reflow, and shown as a ghost before release.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct Landing {
    pub container: Option<TileId>,
    pub cell: Cell,
}

/// A project's interior grid. Children are the tiles whose `Located::container` names this
/// project; their cells are relative to this grid, which is `cols`×`rows` regardless of the
/// project's own span on the field (its contents zoom with it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Container {
    pub id: TileId,
    pub cols: u16,
    pub rows: u16,
}

/// A non-destructive collection of references. Adding a file to a tray adds a path here; the
/// file itself never moves (`docs/homefield.md` "Keep file operations non-destructive").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tray {
    pub id: TrayId,
    pub name: String,
    pub entries: Vec<PathBuf>,
}

/// One output's Home Field. `version` is checked against [`crate::DOCUMENT_VERSION`] on load;
/// `revision` is bumped by whoever persists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDocument {
    pub version: u32,
    pub revision: u64,
    /// The output this field belongs to (`lege_window::Output::name`), so spatial memory
    /// survives a monitor being unplugged and plugged back in.
    pub output: String,
    pub cols: u16,
    pub rows: u16,
    pub tiles: Vec<Tile>,
    /// Every placed tile's location, kept sorted by `TileId` so two documents with the same
    /// content compare equal whatever order edits happened in.
    pub located: Vec<(TileId, Located)>,
    pub containers: Vec<Container>,
    pub trays: Vec<Tray>,
}

impl FieldDocument {
    /// An empty field for `output`, `cols`×`rows`.
    pub fn new(output: impl Into<String>, cols: u16, rows: u16) -> Self {
        Self {
            version: crate::DOCUMENT_VERSION,
            revision: 0,
            output: output.into(),
            cols: cols.max(1),
            rows: rows.max(1),
            tiles: Vec::new(),
            located: Vec::new(),
            containers: Vec::new(),
            trays: Vec::new(),
        }
    }

    pub fn tile(&self, id: TileId) -> Option<&Tile> {
        self.tiles.iter().find(|tile| tile.id == id)
    }

    pub fn tile_mut(&mut self, id: TileId) -> Option<&mut Tile> {
        self.tiles.iter_mut().find(|tile| tile.id == id)
    }

    pub fn container(&self, id: TileId) -> Option<&Container> {
        self.containers.iter().find(|container| container.id == id)
    }

    pub fn tray(&self, id: TrayId) -> Option<&Tray> {
        self.trays.iter().find(|tray| tray.id == id)
    }

    pub fn tray_mut(&mut self, id: TrayId) -> Option<&mut Tray> {
        self.trays.iter_mut().find(|tray| tray.id == id)
    }

    /// Where `id` is, or `None` for a tile on the shelf (or an unknown one).
    pub fn location(&self, id: TileId) -> Option<Located> {
        self.located
            .binary_search_by_key(&id, |(tile, _)| *tile)
            .ok()
            .map(|index| self.located[index].1)
    }

    /// The project holding `id`, if it is inside one.
    pub fn parent_of(&self, id: TileId) -> Option<TileId> {
        self.location(id).and_then(|located| located.container)
    }

    /// Tiles on the shelf: known, but placed nowhere.
    pub fn shelved(&self) -> impl Iterator<Item = &Tile> {
        self.tiles
            .iter()
            .filter(|tile| self.location(tile.id).is_none())
    }

    /// Every placement on one grid (`None` = the top level), as `(id, placement)`.
    pub fn placements_in(&self, container: Option<TileId>) -> Vec<(TileId, Placement)> {
        self.located
            .iter()
            .filter(|(_, located)| located.container == container)
            .map(|(id, located)| (*id, located.placement))
            .collect()
    }

    /// How many tiles sit on one grid, without building the list. The field's draw asks this of
    /// every tile on every frame, purely to decide whether a project shows a child count; going
    /// through [`Self::placements_in`] allocated a vector per tile per frame to call `.len()` on.
    pub fn count_in(&self, container: Option<TileId>) -> usize {
        self.located
            .iter()
            .filter(|(_, located)| located.container == container)
            .count()
    }

    /// The size of one grid: the field's, or a container's.
    pub fn grid_size(&self, container: Option<TileId>) -> Option<(u16, u16)> {
        match container {
            None => Some((self.cols, self.rows)),
            Some(id) => self.container(id).map(|c| (c.cols, c.rows)),
        }
    }

    /// The tile whose placement on `container`'s grid covers `cell`.
    pub fn tile_at(&self, container: Option<TileId>, cell: Cell) -> Option<TileId> {
        self.located
            .iter()
            .find(|(_, located)| located.container == container && located.placement.contains(cell))
            .map(|(id, _)| *id)
    }

    /// Whether `placement` on `container`'s grid is inside the grid and overlaps no tile other
    /// than `ignoring`.
    pub fn is_free(
        &self,
        container: Option<TileId>,
        placement: Placement,
        ignoring: Option<TileId>,
    ) -> bool {
        let Some((cols, rows)) = self.grid_size(container) else {
            return false;
        };
        placement.fits(cols, rows)
            && !self.located.iter().any(|(id, located)| {
                Some(*id) != ignoring
                    && located.container == container
                    && located.placement.overlaps(placement)
            })
    }

    /// Set (or clear, with `None`) where `id` is, keeping `located` sorted. Does not check for
    /// overlap — `crate::ops` does, before calling this.
    pub(crate) fn set_location(&mut self, id: TileId, location: Option<Located>) {
        match (
            self.located.binary_search_by_key(&id, |(tile, _)| *tile),
            location,
        ) {
            (Ok(index), Some(located)) => self.located[index].1 = located,
            (Ok(index), None) => {
                self.located.remove(index);
            }
            (Err(index), Some(located)) => self.located.insert(index, (id, located)),
            (Err(_), None) => {}
        }
    }

    /// Add a tile, keeping `tiles` sorted by id — so a document rebuilt by an inverse compares
    /// equal to the original whatever order the edits happened in. Same for containers and trays.
    pub(crate) fn insert_tile(&mut self, tile: Tile) {
        let index = self.tiles.partition_point(|t| t.id < tile.id);
        self.tiles.insert(index, tile);
    }

    pub(crate) fn insert_container(&mut self, container: Container) {
        let index = self.containers.partition_point(|c| c.id < container.id);
        self.containers.insert(index, container);
    }

    pub(crate) fn insert_tray(&mut self, tray: Tray) {
        let index = self.trays.partition_point(|t| t.id < tray.id);
        self.trays.insert(index, tray);
    }

    /// A fresh, never-used tile id.
    pub fn next_tile_id(&self) -> TileId {
        TileId(
            self.tiles
                .iter()
                .map(|tile| tile.id.0)
                .max()
                .map_or(1, |max| max + 1),
        )
    }

    /// A fresh, never-used tray id.
    pub fn next_tray_id(&self) -> TrayId {
        TrayId(
            self.trays
                .iter()
                .map(|tray| tray.id.0)
                .max()
                .map_or(1, |max| max + 1),
        )
    }

    /// The kind of `id`, for the grammar and the ladder.
    pub fn kind_of(&self, id: TileId) -> Option<&TileKind> {
        self.tile(id).map(|tile| &tile.kind)
    }
}

/// A rectangle in pixels, toolkit-free so this crate needs no toolkit. The shell converts it to
/// its own `Rect` at the edge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PixelRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl PixelRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn centre(self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// The left edge of column `col` (or top edge of row `row`) on a grid of `count` cells laid
/// along `extent` pixels from `origin`, with `gap` pixels between cells and around the edge.
/// Fixed-point pitch — `(extent - gap) / count` in 1/1024ths — so rounding is consistent at
/// every cell, and [`cell_at`] can invert [`cell_rect`] exactly by asking the same function.
fn edge(origin: i32, extent: i32, count: u16, gap: i32, index: i32) -> i32 {
    let count = i64::from(count.max(1));
    let pitch = (i64::from(extent - gap) * 1024) / count;
    origin + gap + ((i64::from(index) * pitch) / 1024) as i32
}

/// The pixel rectangle of a `span`-sized tile at `cell` on a `cols`×`rows` grid filling
/// `bounds` with `gap` pixels between cells (and around the edge). Cells are equal-sized: the
/// grid is the field's geometry, and the field is the whole output, so integer rounding only
/// ever shifts an edge by a pixel, never a tile by a cell.
pub fn cell_rect(
    cols: u16,
    rows: u16,
    bounds: PixelRect,
    cell: Cell,
    span: Span,
    gap: i32,
) -> PixelRect {
    let gap = gap.max(0);
    let col = i32::from(cell.col);
    let row = i32::from(cell.row);
    let x = edge(bounds.x, bounds.w, cols, gap, col);
    let y = edge(bounds.y, bounds.h, rows, gap, row);
    let right = edge(bounds.x, bounds.w, cols, gap, col + i32::from(span.cols)) - gap;
    let bottom = edge(bounds.y, bounds.h, rows, gap, row + i32::from(span.rows)) - gap;
    PixelRect::new(x, y, (right - x).max(1), (bottom - y).max(1))
}

/// The index of the cell whose left/top edge is the last one at or before `position`.
fn index_at(origin: i32, extent: i32, count: u16, gap: i32, position: i32) -> u16 {
    let last = i32::from(count.max(1)) - 1;
    let mut index = 0;
    // Cells are few (≤ 64 an axis); a scan over the same edge function is exact where a
    // division would round against it in the degenerate case of cells narrower than the gap.
    while index < last && edge(origin, extent, count, gap, index + 1) <= position {
        index += 1;
    }
    index as u16
}

/// The cell under pixel `(x, y)` on the same grid, or `None` outside `bounds`. The exact inverse
/// of [`cell_rect`] on every cell's centre (property-tested); a point in a gap belongs to the
/// cell whose rectangle it precedes, so dropping onto a seam still lands somewhere sensible.
pub fn cell_at(cols: u16, rows: u16, bounds: PixelRect, x: i32, y: i32, gap: i32) -> Option<Cell> {
    if !bounds.contains(x, y) {
        return None;
    }
    let gap = gap.max(0);
    Some(Cell::new(
        index_at(bounds.x, bounds.w, cols, gap, x),
        index_at(bounds.y, bounds.h, rows, gap, y),
    ))
}

/// The pixel rectangle of tile `id` with the field filling `bounds`: its own cell rect at the
/// top level, or — inside a project — a rect nested within the project's, laid out on the
/// container's grid with a smaller gap. `None` for a shelved or unknown tile.
pub fn tile_rect(
    doc: &FieldDocument,
    bounds: PixelRect,
    id: TileId,
    gap: i32,
) -> Option<PixelRect> {
    let located = doc.location(id)?;
    match located.container {
        None => Some(cell_rect(
            doc.cols,
            doc.rows,
            bounds,
            located.placement.cell,
            located.placement.span,
            gap,
        )),
        Some(project) => {
            let outer = tile_rect(doc, bounds, project, gap)?;
            let container = doc.container(project)?;
            // Leave the project's title band, then lay children out in what remains.
            let band = (outer.h / 8).clamp(16, 40);
            let inner = PixelRect::new(outer.x, outer.y + band, outer.w, (outer.h - band).max(1));
            Some(cell_rect(
                container.cols,
                container.rows,
                inner,
                located.placement.cell,
                located.placement.span,
                (gap / 2).max(2),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_and_containment_follow_half_open_ranges() {
        let a = Placement::new(Cell::new(0, 0), Span::new(2, 2));
        let b = Placement::new(Cell::new(2, 0), Span::new(1, 1));
        let c = Placement::new(Cell::new(1, 1), Span::new(1, 1));
        assert!(!a.overlaps(b), "touching edges do not overlap");
        assert!(a.overlaps(c));
        assert!(a.contains(Cell::new(1, 1)));
        assert!(!a.contains(Cell::new(2, 0)));
        assert!(a.fits(2, 2));
        assert!(!a.fits(1, 2));
        assert!(a.shares_band(b), "same rows");
        assert!(!Placement::new(Cell::new(3, 3), Span::new(1, 1)).shares_band(a));
    }

    #[test]
    fn cell_rects_tile_the_bounds_without_overlapping() {
        let bounds = PixelRect::new(0, 0, 1920, 1080);
        let (cols, rows, gap) = (12u16, 8u16, 8);
        let a = cell_rect(cols, rows, bounds, Cell::new(0, 0), Span::new(1, 1), gap);
        let b = cell_rect(cols, rows, bounds, Cell::new(1, 0), Span::new(1, 1), gap);
        assert_eq!(a.x, gap);
        assert!(a.x + a.w + gap <= b.x + 1, "{a:?} then {b:?}");
        let last = cell_rect(cols, rows, bounds, Cell::new(11, 7), Span::new(1, 1), gap);
        assert!(last.x + last.w <= bounds.w - gap + 1);
        assert!(last.y + last.h <= bounds.h - gap + 1);
        let wide = cell_rect(cols, rows, bounds, Cell::new(0, 0), Span::new(2, 1), gap);
        assert_eq!(
            wide.x + wide.w,
            b.x + b.w,
            "a 2-wide tile ends where the second cell ends"
        );
    }

    #[test]
    fn cell_at_inverts_cell_rect_on_centres() {
        let bounds = PixelRect::new(100, 50, 1000, 700);
        for cols in [1u16, 5, 12] {
            for rows in [1u16, 4, 8] {
                for col in 0..cols {
                    for row in 0..rows {
                        let cell = Cell::new(col, row);
                        let rect = cell_rect(cols, rows, bounds, cell, Span::new(1, 1), 6);
                        let (x, y) = rect.centre();
                        assert_eq!(cell_at(cols, rows, bounds, x, y, 6), Some(cell));
                    }
                }
            }
        }
        assert_eq!(cell_at(12, 8, bounds, 5, 5, 6), None, "outside the bounds");
    }
}
