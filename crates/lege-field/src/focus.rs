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
    let (_, origin) = rects.iter().find(|(id, _)| *id == from)?;
    let (ox, oy) = origin.centre();
    let mut best: Option<(i64, TileId)> = None;
    for (id, rect) in &rects {
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
}
