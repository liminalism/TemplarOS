//! The stable local layout. Inserting, moving or resizing a tile touches only the rows, columns
//! or container it lands in; nothing outside that band is ever a candidate for displacement —
//! the "unaffected tiles never move" rule is structural here, not a tuning outcome. Among the
//! candidates, [`LayoutCost`]'s *lexicographic* order decides: fewest displaced tiles first,
//! then nearest the requested landing, then least aspect distortion, then least total
//! relocation. Lexicographic by `Ord` on the field order, so there are no weights to get wrong.
//!
//! Removing a tile leaves a hole on purpose. Only [`tidy`] — an explicit command — packs.

use crate::field::{Cell, Placement};
use crate::ops::FieldError;
use crate::tile::{Span, SpanRange, TileId};

/// The working set one reflow operates on: one grid and what is on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    pub places: Vec<(TileId, Placement)>,
}

impl Grid {
    pub fn is_free(&self, placement: Placement, ignoring: Option<TileId>) -> bool {
        placement.fits(self.cols, self.rows)
            && !self
                .places
                .iter()
                .any(|(id, other)| Some(*id) != ignoring && other.overlaps(placement))
    }

    #[cfg(test)]
    fn placement_of(&self, id: TileId) -> Option<Placement> {
        self.places
            .iter()
            .find(|(tile, _)| *tile == id)
            .map(|(_, placement)| *placement)
    }

    fn set(&mut self, id: TileId, placement: Placement) {
        match self.places.iter_mut().find(|(tile, _)| *tile == id) {
            Some(entry) => entry.1 = placement,
            None => self.places.push((id, placement)),
        }
    }
}

/// What a placement costs. **Field order is the priority order**: `Ord` is derived, so the
/// comparison is lexicographic — a candidate that displaces one tile fewer wins regardless of
/// every other term, and so on down. `docs/homefield.md` lists the same terms in this order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct LayoutCost {
    /// Tiles that had to move to make room.
    pub displaced: u32,
    /// Manhattan cells between where the user pointed and where the tile landed.
    pub landing_distance: u32,
    /// How far the placed span is from the tile's preferred span, in cells of difference.
    pub aspect_distortion: u32,
    /// Total cells the displaced tiles moved.
    pub relocation_distance: u32,
}

/// The result of one insert/move/resize: where the tile went, who had to move (and from/to
/// where), and what it cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reflow {
    pub placed: Placement,
    /// `(tile, from, to)` for every displaced tile, in the order they were moved.
    pub moved: Vec<(TileId, Placement, Placement)>,
    pub cost: LayoutCost,
}

/// Every span from `preferred` down to `min`, largest first, preferring to give up rows before
/// columns (a wide tile reads better than a tall one on a landscape field).
fn shrink_steps(spans: &SpanRange) -> Vec<Span> {
    let preferred = spans.preferred.clamp(spans.min, spans.max);
    let mut steps = Vec::new();
    let mut rows = preferred.rows;
    while rows >= spans.min.rows.max(1) {
        let mut cols = preferred.cols;
        while cols >= spans.min.cols.max(1) {
            steps.push(Span::new(cols, rows));
            if cols == 1 {
                break;
            }
            cols -= 1;
        }
        if rows == 1 {
            break;
        }
        rows -= 1;
    }
    steps.sort_by_key(|span| std::cmp::Reverse(span.cells()));
    steps.dedup();
    steps
}

/// Clamp `landing` so a `span`-sized tile starting there stays inside the grid.
fn clamp_landing(grid: &Grid, landing: Cell, span: Span) -> Cell {
    Cell::new(
        landing.col.min(grid.cols.saturating_sub(span.cols)),
        landing.row.min(grid.rows.saturating_sub(span.rows)),
    )
}

/// Where a tile with `spans` lands when dropped at `landing` on `grid`, moving as little as
/// possible. `ignoring` is the tile being moved/resized (its old placement does not block).
/// Fails with [`FieldError::NoRoom`] only when even the tile's minimum span cannot be made to
/// fit by moving band-mates.
pub fn find_placement(
    grid: &Grid,
    spans: &SpanRange,
    landing: Cell,
    ignoring: Option<TileId>,
) -> Result<Reflow, FieldError> {
    let preferred = spans.preferred.clamp(spans.min, spans.max);
    let aspect = |span: Span| {
        u32::from(span.cols.abs_diff(preferred.cols))
            + u32::from(span.rows.abs_diff(preferred.rows))
    };

    // Pass 1: somewhere free in the band, at the largest span that has room.
    let mut best: Option<(LayoutCost, Placement)> = None;
    for span in shrink_steps(spans) {
        let anchor = Placement::new(clamp_landing(grid, landing, span), span);
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let candidate = Placement::new(Cell::new(col, row), span);
                if !candidate.shares_band(anchor) || !grid.is_free(candidate, ignoring) {
                    continue;
                }
                let cost = LayoutCost {
                    displaced: 0,
                    landing_distance: candidate.cell.distance(anchor.cell),
                    aspect_distortion: aspect(span),
                    relocation_distance: 0,
                };
                if best.is_none_or(|(best_cost, _)| cost < best_cost) {
                    best = Some((cost, candidate));
                }
            }
        }
        if let Some((cost, placed)) = best
            && cost.landing_distance == 0
        {
            return Ok(Reflow {
                placed,
                moved: Vec::new(),
                cost,
            });
        }
    }
    if let Some((cost, placed)) = best {
        return Ok(Reflow {
            placed,
            moved: Vec::new(),
            cost,
        });
    }

    // Pass 2: take the landing spot and push whatever is there to the nearest free cell in the
    // band, largest span first. Deterministic: overlapping tiles are handled top-left first.
    for span in shrink_steps(spans) {
        let target = Placement::new(clamp_landing(grid, landing, span), span);
        if !target.fits(grid.cols, grid.rows) {
            continue;
        }
        let mut overlapping: Vec<(TileId, Placement)> = grid
            .places
            .iter()
            .filter(|(id, placement)| Some(*id) != ignoring && placement.overlaps(target))
            .copied()
            .collect();
        overlapping.sort_by_key(|(_, placement)| (placement.cell.row, placement.cell.col));

        let mut scratch = Grid {
            cols: grid.cols,
            rows: grid.rows,
            places: grid
                .places
                .iter()
                .filter(|(id, _)| {
                    Some(*id) != ignoring && !overlapping.iter().any(|(moved, _)| moved == id)
                })
                .copied()
                .collect(),
        };
        // The landing tile's own rectangle blocks relocation targets.
        let reserved = TileId(u64::MAX);
        scratch.set(reserved, target);

        let mut moved = Vec::with_capacity(overlapping.len());
        let mut relocation_distance = 0u32;
        let mut stuck = false;
        for (id, from) in &overlapping {
            let Some(to) = nearest_free(&scratch, *from, Some(target)) else {
                stuck = true;
                break;
            };
            relocation_distance += from.cell.distance(to.cell);
            scratch.set(*id, to);
            moved.push((*id, *from, to));
        }
        if stuck {
            continue;
        }
        return Ok(Reflow {
            placed: target,
            moved,
            cost: LayoutCost {
                displaced: overlapping.len() as u32,
                landing_distance: 0,
                aspect_distortion: aspect(span),
                relocation_distance,
            },
        });
    }
    Err(FieldError::NoRoom)
}

/// Every span between `current` and `requested` inclusive, nearest the request first. `current`
/// is always a member, which is what makes the clamp search below total: the status quo always
/// works, so a resize degrades to "nothing happened" rather than failing.
///
/// Ordering is total and deterministic — nearest the request, then larger, then by column and
/// row — because the drag ghost runs this same search a frame before the real drop and the two
/// must agree (the `placement_is_deterministic` property).
fn resize_steps(current: Span, requested: Span) -> Vec<Span> {
    let (low_cols, high_cols) = (
        current.cols.min(requested.cols),
        current.cols.max(requested.cols),
    );
    let (low_rows, high_rows) = (
        current.rows.min(requested.rows),
        current.rows.max(requested.rows),
    );
    let mut steps = Vec::new();
    for cols in low_cols..=high_cols {
        for rows in low_rows..=high_rows {
            steps.push(Span::new(cols, rows));
        }
    }
    steps.sort_by_key(|span| {
        (
            u32::from(span.cols.abs_diff(requested.cols))
                + u32::from(span.rows.abs_diff(requested.rows)),
            std::cmp::Reverse(span.cells()),
            span.cols,
            span.rows,
        )
    });
    steps
}

/// How `from` gives up the cells `target` now needs, or `None` if it cannot.
///
/// A yielding tile is pushed on the edge facing the growth and **keeps its opposite edge
/// exactly where it was** — grow a tile rightwards and its right-hand neighbour becomes
/// narrower against its own right edge; it does not slide away across the field. The new
/// rectangle is therefore always a subset of the old one, which is what guarantees a yield can
/// never create a fresh overlap somewhere else.
///
/// `prefer_horizontal` is the axis the resize actually grew along, so a downward drag narrows
/// nothing sideways when it could just as well shorten the tile below.
fn yielded_placement(
    from: Placement,
    target: Placement,
    min: Span,
    prefer_horizontal: bool,
) -> Option<Placement> {
    // The near edge moves to the target's far edge; `from.right() > target.right()` is what
    // guarantees at least one column survives, so `Span::new`'s zero-clamp is never load-bearing.
    let horizontal = (from.right() > target.right() && from.cell.col < target.right())
        .then(|| {
            Placement::new(
                Cell::new(target.right(), from.cell.row),
                Span::new(from.right() - target.right(), from.span.rows),
            )
        })
        .filter(|placement| placement.span.cols >= min.cols);
    let vertical = (from.bottom() > target.bottom() && from.cell.row < target.bottom())
        .then(|| {
            Placement::new(
                Cell::new(from.cell.col, target.bottom()),
                Span::new(from.span.cols, from.bottom() - target.bottom()),
            )
        })
        .filter(|placement| placement.span.rows >= min.rows);

    if prefer_horizontal {
        horizontal.or(vertical)
    } else {
        vertical.or(horizontal)
    }
}

/// Every tile overlapping `target` yielding to it, or `None` when even one of them cannot.
fn yields_for(
    grid: &Grid,
    ranges: &[(TileId, SpanRange)],
    id: TileId,
    current: Placement,
    target: Placement,
) -> Option<Vec<(TileId, Placement, Placement)>> {
    let mut overlapping: Vec<(TileId, Placement)> = grid
        .places
        .iter()
        .filter(|(tile, placement)| *tile != id && placement.overlaps(target))
        .copied()
        .collect();
    // Top-left first, so the result does not depend on `places`' insertion order.
    overlapping.sort_by_key(|(_, placement)| (placement.cell.row, placement.cell.col));

    let prefer_horizontal = target.span.cols > current.span.cols;
    let mut yielded = Vec::with_capacity(overlapping.len());
    for (neighbour, from) in overlapping {
        let min = ranges
            .iter()
            .find(|(tile, _)| *tile == neighbour)
            // Fail closed. A neighbour with no declared range is treated as already at its
            // floor, so a caller that supplied an incomplete `ranges` can only ever refuse a
            // resize — never invent room that the document does not have.
            .map_or(from.span, |(_, range)| range.min);
        yielded.push((
            neighbour,
            from,
            yielded_placement(from, target, min, prefer_horizontal)?,
        ));
    }
    Some(yielded)
}

/// Resize `id` in place, letting the tiles it grows into **give up span rather than move**.
///
/// This is the one reflow that never relocates anything. [`find_placement`] answers "where does
/// this tile go", and its second pass evicts whoever is in the way to the nearest free cell;
/// that is right for a drop, and wrong for a resize, where the neighbour jumping to the far side
/// of the field is exactly the loss of spatial memory `docs/homefield.md` warns about. Here the
/// resized tile keeps its cell, every neighbour keeps its far edge, and a request that would
/// take some neighbour below its `min` is **clamped to the largest span that fits** instead of
/// being refused or paid for by an eviction.
///
/// `ranges` supplies each tile's [`SpanRange`] because a [`Grid`] carries placements only; it
/// should cover every tile in `grid.places` (see [`yields_for`] for what happens when it does
/// not). Growth is anchored at the tile's top-left, so a span that would leave the grid is
/// simply not achievable and the clamp finds a smaller one.
pub fn find_resize(
    grid: &Grid,
    ranges: &[(TileId, SpanRange)],
    id: TileId,
    requested: Span,
) -> Result<Reflow, FieldError> {
    let current = grid
        .places
        .iter()
        .find(|(tile, _)| *tile == id)
        .map(|(_, placement)| *placement)
        .ok_or(FieldError::UnknownTile)?;

    for span in resize_steps(current.span, requested) {
        let target = Placement::new(current.cell, span);
        if !target.fits(grid.cols, grid.rows) {
            continue;
        }
        let Some(moved) = yields_for(grid, ranges, id, current, target) else {
            continue;
        };
        let relocation_distance = moved
            .iter()
            .map(|(_, from, to)| from.cell.distance(to.cell))
            .sum();
        return Ok(Reflow {
            placed: target,
            cost: LayoutCost {
                displaced: moved.len() as u32,
                landing_distance: 0,
                aspect_distortion: u32::from(span.cols.abs_diff(requested.cols))
                    + u32::from(span.rows.abs_diff(requested.rows)),
                relocation_distance,
            },
            moved,
        });
    }
    // Unreachable in practice: `resize_steps` always contains the current span, which fits and
    // overlaps nobody. Kept as an error rather than an `unwrap` so a future caller that passes a
    // placement inconsistent with the grid gets a refusal, not a panic.
    Err(FieldError::NoRoom)
}

/// The free placement of `from`'s span nearest `from`, preferring cells in the band of `band`
/// (the landing rectangle) so displaced tiles stay near where they were; anywhere on the grid
/// failing that. Ties go to the top-left.
fn nearest_free(grid: &Grid, from: Placement, band: Option<Placement>) -> Option<Placement> {
    let mut best: Option<(u32, bool, Placement)> = None;
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let candidate = Placement::new(Cell::new(col, row), from.span);
            if !grid.is_free(candidate, None) {
                continue;
            }
            let in_band = band.is_some_and(|band| candidate.shares_band(band));
            let distance = candidate.cell.distance(from.cell);
            // Prefer in-band, then nearest; `!in_band` sorts false (in band) first.
            let key = (distance, !in_band, candidate);
            if best.is_none_or(|(d, out, p)| (key.0, key.1, key.2.cell) < (d, out, p.cell)) {
                best = Some(key);
            }
        }
    }
    best.map(|(_, _, placement)| placement)
}

/// Explicit global packing: every tile re-placed first-fit in reading order of its current
/// position, keeping its span. Returns what moved. The one operation allowed to rearrange the
/// whole grid, and only because the user asked for it by name.
pub fn tidy(grid: &Grid) -> Vec<(TileId, Placement, Placement)> {
    let mut order: Vec<(TileId, Placement)> = grid.places.clone();
    order.sort_by_key(|(id, placement)| (placement.cell.row, placement.cell.col, *id));
    let mut packed = Grid {
        cols: grid.cols,
        rows: grid.rows,
        places: Vec::new(),
    };
    let mut moved = Vec::new();
    for (id, from) in order {
        let mut destination = None;
        'scan: for row in 0..grid.rows {
            for col in 0..grid.cols {
                let candidate = Placement::new(Cell::new(col, row), from.span);
                if packed.is_free(candidate, None) {
                    destination = Some(candidate);
                    break 'scan;
                }
            }
        }
        // First-fit can fragment a grid even though the original arrangement fit. A partial
        // tidy must not put a later tile back on top of an earlier packed one, so the entire
        // command becomes a no-op if any tile cannot be packed. Tidy never loses or overlaps a
        // tile; the user's stable layout is the safe fallback.
        let Some(to) = destination else {
            return Vec::new();
        };
        packed.set(id, to);
        if to != from {
            moved.push((id, from, to));
        }
    }
    moved
}

/// `grid` with `reflow` applied (the placed tile is `id`).
pub fn apply_reflow(grid: &mut Grid, id: TileId, reflow: &Reflow) {
    for (moved, _, to) in &reflow.moved {
        grid.set(*moved, *to);
    }
    grid.set(id, reflow.placed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(cols: u16, rows: u16, places: &[(u64, u16, u16, u16, u16)]) -> Grid {
        Grid {
            cols,
            rows,
            places: places
                .iter()
                .map(|&(id, col, row, w, h)| {
                    (
                        TileId(id),
                        Placement::new(Cell::new(col, row), Span::new(w, h)),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn an_empty_spot_at_the_landing_costs_nothing() {
        let grid = grid(6, 4, &[(1, 0, 0, 1, 1)]);
        let reflow = find_placement(
            &grid,
            &SpanRange::fixed(Span::new(1, 1)),
            Cell::new(3, 2),
            None,
        )
        .unwrap();
        assert_eq!(
            reflow.placed,
            Placement::new(Cell::new(3, 2), Span::new(1, 1))
        );
        assert_eq!(reflow.cost, LayoutCost::default());
        assert!(reflow.moved.is_empty());
    }

    #[test]
    fn a_landing_off_the_edge_is_clamped_inside() {
        let grid = grid(6, 4, &[]);
        let reflow = find_placement(
            &grid,
            &SpanRange::fixed(Span::new(2, 2)),
            Cell::new(9, 9),
            None,
        )
        .unwrap();
        assert_eq!(reflow.placed.cell, Cell::new(4, 2));
        assert!(reflow.placed.fits(6, 4));
    }

    #[test]
    fn an_occupied_landing_prefers_a_free_neighbour_over_displacing() {
        let grid = grid(6, 4, &[(1, 2, 1, 1, 1)]);
        let reflow = find_placement(
            &grid,
            &SpanRange::fixed(Span::new(1, 1)),
            Cell::new(2, 1),
            None,
        )
        .unwrap();
        assert_eq!(reflow.cost.displaced, 0);
        assert_eq!(reflow.cost.landing_distance, 1);
        assert!(
            reflow
                .placed
                .shares_band(Placement::new(Cell::new(2, 1), Span::new(1, 1)))
        );
    }

    #[test]
    fn a_full_band_displaces_the_fewest_tiles_and_only_band_mates() {
        // Row 1 and column 2 fully occupied except nothing free in band: 1-wide grid column.
        let grid = grid(1, 3, &[(1, 0, 0, 1, 1), (2, 0, 1, 1, 1)]);
        let reflow = find_placement(
            &grid,
            &SpanRange::fixed(Span::new(1, 1)),
            Cell::new(0, 0),
            None,
        )
        .unwrap();
        // The free cell (0,2) shares the column, so pass 1 finds it: distance 2, nobody moves.
        assert_eq!(reflow.cost.displaced, 0);
        assert_eq!(reflow.placed.cell, Cell::new(0, 2));

        let full = grid_full();
        let reflow = find_placement(
            &full,
            &SpanRange::fixed(Span::new(1, 1)),
            Cell::new(0, 0),
            None,
        );
        assert_eq!(reflow, Err(FieldError::NoRoom));
    }

    fn grid_full() -> Grid {
        grid(
            2,
            2,
            &[
                (1, 0, 0, 1, 1),
                (2, 1, 0, 1, 1),
                (3, 0, 1, 1, 1),
                (4, 1, 1, 1, 1),
            ],
        )
    }

    #[test]
    fn displacing_pushes_the_occupant_to_the_nearest_free_cell() {
        // 3 wide, 1 tall; (0,0) taken, (1,0) taken, (2,0) free. Land a 2-wide at (0,0): no free
        // 2-wide exists, so shrink to 1 wide finds (2,0) at distance 2 — still no displacement.
        let three = grid(3, 1, &[(1, 0, 0, 1, 1), (2, 1, 0, 1, 1)]);
        let spans = SpanRange::new(Span::new(1, 1), Span::new(2, 1), Span::new(2, 1));
        let reflow = find_placement(&three, &spans, Cell::new(0, 0), None).unwrap();
        assert_eq!(reflow.cost.displaced, 0);
        assert_eq!(
            reflow.placed,
            Placement::new(Cell::new(2, 0), Span::new(1, 1))
        );

        // With a fixed 2-wide span there is no free spot: tile 1 and 2 are pushed.
        let fixed = SpanRange::fixed(Span::new(2, 1));
        let reflow = find_placement(&three, &fixed, Cell::new(0, 0), None);
        assert_eq!(
            reflow,
            Err(FieldError::NoRoom),
            "three cells cannot hold 2+1+1"
        );

        // Four cells with two free: the free 2-wide at (2,0) wins over displacing anyone.
        let four = grid(4, 1, &[(1, 0, 0, 1, 1), (2, 1, 0, 1, 1)]);
        let reflow = find_placement(&four, &fixed, Cell::new(0, 0), None).unwrap();
        assert_eq!(
            reflow.placed,
            Placement::new(Cell::new(2, 0), Span::new(2, 1))
        );
        assert_eq!(reflow.cost.displaced, 0);

        // Occupied at 0 and 2: no free 2-wide anywhere, so the landing is taken and its one
        // occupant goes to the nearest free cell, (3,0).
        let four = grid(4, 1, &[(1, 0, 0, 1, 1), (2, 2, 0, 1, 1)]);
        let reflow = find_placement(&four, &fixed, Cell::new(0, 0), None).unwrap();
        assert_eq!(
            reflow.placed,
            Placement::new(Cell::new(0, 0), Span::new(2, 1))
        );
        assert_eq!(reflow.cost.displaced, 1);
        assert_eq!(
            reflow.moved,
            vec![(
                TileId(1),
                Placement::new(Cell::new(0, 0), Span::new(1, 1)),
                Placement::new(Cell::new(3, 0), Span::new(1, 1))
            )]
        );
        let mut after = four.clone();
        apply_reflow(&mut after, TileId(9), &reflow);
        for (id, placement) in &after.places {
            for (other, other_placement) in &after.places {
                assert!(id == other || !placement.overlaps(*other_placement));
            }
        }
    }

    #[test]
    fn tidy_packs_in_reading_order_and_never_loses_a_tile() {
        let grid = grid(3, 2, &[(1, 2, 1, 1, 1), (2, 0, 1, 1, 1), (3, 1, 0, 2, 1)]);
        let moved = tidy(&grid);
        let mut packed = grid.clone();
        for (id, _, to) in &moved {
            packed.set(*id, *to);
        }
        assert_eq!(
            packed.placement_of(TileId(3)).unwrap().cell,
            Cell::new(0, 0)
        );
        assert_eq!(
            packed.placement_of(TileId(2)).unwrap().cell,
            Cell::new(2, 0)
        );
        assert_eq!(
            packed.placement_of(TileId(1)).unwrap().cell,
            Cell::new(0, 1)
        );
        assert_eq!(packed.places.len(), 3);
    }

    #[test]
    fn cost_order_is_lexicographic_in_field_order() {
        let a = LayoutCost {
            displaced: 0,
            landing_distance: 100,
            aspect_distortion: 100,
            relocation_distance: 100,
        };
        let b = LayoutCost {
            displaced: 1,
            landing_distance: 0,
            aspect_distortion: 0,
            relocation_distance: 0,
        };
        assert!(a < b, "one displaced tile outweighs any distance");
    }
}
