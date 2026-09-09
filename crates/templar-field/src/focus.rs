//! Keyboard navigation between tiles: the nearest tile in a direction, by pixel geometry, so the
//! arrow keys reach every tile a pointer can (the accessibility parity the shell promises).

use crate::field::{FieldDocument, PixelRect, tile_rect};
use crate::tile::TileId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Every placed tile's rect, top level and inside containers, with the field filling `bounds`.
pub fn visible_rects(doc: &FieldDocument, bounds: PixelRect, gap: i32) -> Vec<(TileId, PixelRect)> {
    doc.located
        .iter()
        .filter_map(|(id, _)| tile_rect(doc, bounds, *id, gap).map(|rect| (*id, rect)))
        .collect()
}

/// The tile to focus when moving `direction` from `from`: the one whose centre is furthest
/// along that axis in the right direction with the least sideways offset, weighting the primary
/// axis so a tile straight ahead beats a nearer one off to the side. `None` at the edge.
pub fn neighbour(
    doc: &FieldDocument,
    bounds: PixelRect,
    from: TileId,
    direction: Direction,
    gap: i32,
) -> Option<TileId> {
    let rects = visible_rects(doc, bounds, gap);
    nearest_in(&rects, from, direction)
}

/// The nearest tile to `from` in `direction`, among exactly the rects given.
///
/// Split out of [`neighbour`] unchanged so a caller can supply a *filtered* list — cycling
/// between live windows looks only at window-bearing tiles, and must not stop on a drawer that
/// happens to sit between two of them.
pub fn nearest_in(
    rects: &[(TileId, PixelRect)],
    from: TileId,
    direction: Direction,
) -> Option<TileId> {
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = origin.centre();
    let mut best: Option<(i64, TileId)> = None;
    for (id, rect) in rects {
        if *id == from {
            continue;
        }
        let (cx, cy) = rect.centre();
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

/// The tile `hops` contiguous steps from `from` in `direction`, over `rects` alone.
///
/// This is how `Super+Left`/`Super+Right` move between the windows of one contiguous group. Two
/// things about it are deliberate:
///
/// * **It is geometry, computed on demand, and never stored.**
///   `@templaros.decision.home-field-stable-bounded-semantic-grammar` is explicit
///   that adjacency must not imply a relationship — "two adjacent tiles should not automatically
///   become related" — because local reflow could then silently change what things mean. Nothing
///   here is written back to the document, nothing changes what a drop does, and nothing about a
///   tile changes by being someone's neighbour. It only decides which window a keypress moves to.
/// * **`hops` is a parameter, not a constant.** The rule ("two hops, then the next group starts")
///   is a first guess, and the call site owns it so it can change without touching this crate.
///
/// Stops early at the edge of the field, returning the furthest tile actually reached, so the
/// chord does nothing at a boundary rather than wrapping around to the far side.
pub fn contiguous_neighbour(
    rects: &[(TileId, PixelRect)],
    from: TileId,
    direction: Direction,
    hops: u8,
) -> Option<TileId> {
    let mut current = from;
    let mut reached = None;
    for _ in 0..hops {
        match nearest_in(rects, current, direction) {
            Some(next) => {
                current = next;
                reached = Some(next);
            }
            None => break,
        }
    }
    reached
}

/// The first tile in reading order, for when nothing is focused yet.
pub fn first(doc: &FieldDocument, bounds: PixelRect, gap: i32) -> Option<TileId> {
    visible_rects(doc, bounds, gap)
        .into_iter()
        .min_by_key(|(id, rect)| (rect.y, rect.x, *id))
        .map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::{Cell, Landing};
    use crate::ops::{FieldOp, apply};
    use crate::tile::{Span, SpanRange, Tile, TileKind};

    fn system(id: u64) -> Tile {
        Tile {
            id: TileId(id),
            kind: TileKind::System { probe: "x".into() },
            title: format!("t{id}"),
            spans: SpanRange::fixed(Span::new(1, 1)),
            accent: None,
        }
    }

    #[test]
    fn arrows_reach_the_nearest_tile_in_each_direction() {
        let mut doc = FieldDocument::new("eDP-1", 4, 3);
        for (id, col, row) in [(1, 0, 0), (2, 2, 0), (3, 0, 2), (4, 3, 2)] {
            let op = FieldOp::Place {
                tile: system(id),
                landing: Landing {
                    container: None,
                    cell: Cell::new(col, row),
                },
            };
            doc = apply(&doc, &op).unwrap().0;
        }
        let bounds = PixelRect::new(0, 0, 800, 600);
        assert_eq!(first(&doc, bounds, 8), Some(TileId(1)));
        assert_eq!(
            neighbour(&doc, bounds, TileId(1), Direction::Right, 8),
            Some(TileId(2))
        );
        assert_eq!(
            neighbour(&doc, bounds, TileId(1), Direction::Down, 8),
            Some(TileId(3))
        );
        assert_eq!(neighbour(&doc, bounds, TileId(1), Direction::Left, 8), None);
        assert_eq!(
            neighbour(&doc, bounds, TileId(3), Direction::Right, 8),
            Some(TileId(4))
        );
        assert_eq!(
            neighbour(&doc, bounds, TileId(4), Direction::Up, 8),
            Some(TileId(2))
        );
    }

    #[test]
    fn contiguous_steps_skip_what_is_not_in_the_list_and_stop_at_the_edge() {
        // Four tiles in a row. Cycling sees only the ones the caller passed in, so a tile that
        // carries no window is stepped over rather than counted as a hop.
        let mut doc = FieldDocument::new("eDP-1", 4, 1);
        for (id, col) in [(1u64, 0u16), (2, 1), (3, 2), (4, 3)] {
            doc = apply(
                &doc,
                &FieldOp::Place {
                    tile: system(id),
                    landing: Landing {
                        container: None,
                        cell: Cell::new(col, 0),
                    },
                },
            )
            .unwrap()
            .0;
        }
        let bounds = PixelRect::new(0, 0, 800, 200);
        let all = visible_rects(&doc, bounds, 8);

        assert_eq!(
            contiguous_neighbour(&all, TileId(1), Direction::Right, 1),
            Some(TileId(2))
        );
        assert_eq!(
            contiguous_neighbour(&all, TileId(1), Direction::Right, 2),
            Some(TileId(3)),
            "two hops is two tiles along"
        );

        // With tile 2 filtered out — it holds no window — two hops from 1 reaches 4.
        let windows: Vec<_> = all
            .iter()
            .filter(|(id, _)| *id != TileId(2))
            .copied()
            .collect();
        assert_eq!(
            contiguous_neighbour(&windows, TileId(1), Direction::Right, 2),
            Some(TileId(4))
        );

        // At the edge it stops rather than wrapping, and reports the furthest it got.
        assert_eq!(
            contiguous_neighbour(&all, TileId(3), Direction::Right, 5),
            Some(TileId(4))
        );
        assert_eq!(
            contiguous_neighbour(&all, TileId(4), Direction::Right, 2),
            None,
            "nothing to the right of the last tile"
        );
        // Zero hops is a no-op, not "the tile you are on".
        assert_eq!(
            contiguous_neighbour(&all, TileId(1), Direction::Right, 0),
            None
        );
    }

    #[test]
    fn nearest_in_is_exactly_what_neighbour_already_did() {
        let mut doc = FieldDocument::new("eDP-1", 4, 3);
        for (id, col, row) in [(1u64, 0u16, 0u16), (2, 2, 0), (3, 0, 2)] {
            doc = apply(
                &doc,
                &FieldOp::Place {
                    tile: system(id),
                    landing: Landing {
                        container: None,
                        cell: Cell::new(col, row),
                    },
                },
            )
            .unwrap()
            .0;
        }
        let bounds = PixelRect::new(0, 0, 800, 600);
        let rects = visible_rects(&doc, bounds, 8);
        for direction in [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ] {
            for id in [TileId(1), TileId(2), TileId(3)] {
                assert_eq!(
                    neighbour(&doc, bounds, id, direction, 8),
                    nearest_in(&rects, id, direction),
                    "extracting nearest_in changed {id:?} {direction:?}"
                );
            }
        }
    }
}
