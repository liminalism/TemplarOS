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
        // A tile that cannot be packed anywhere keeps its place: tidy never loses a tile.
        let to = destination.unwrap_or(from);
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
