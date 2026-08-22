//! The field as text: one line per tile, `kind  col,row  cols×rows  title`, children indented
//! under their project, the shelf last. What `lege-shell --print field` prints, so everything
//! the graphical field shows is available with no display at all.

use crate::field::FieldDocument;
use crate::tile::{Tile, TileId};
use std::fmt::Write as _;

pub fn render(doc: &FieldDocument) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "field {}  {}x{}  revision {}",
        doc.output, doc.cols, doc.rows, doc.revision
    );
    let mut top: Vec<(TileId, crate::field::Placement)> = doc.placements_in(None);
    top.sort_by_key(|(id, placement)| (placement.cell.row, placement.cell.col, *id));
    for (id, placement) in top {
        let Some(tile) = doc.tile(id) else { continue };
        line(&mut out, 0, tile, Some(placement));
        if tile.kind.is_container() {
            let mut children = doc.placements_in(Some(id));
            children.sort_by_key(|(id, placement)| (placement.cell.row, placement.cell.col, *id));
            for (child_id, child_placement) in children {
                if let Some(child) = doc.tile(child_id) {
                    line(&mut out, 1, child, Some(child_placement));
                }
            }
        }
    }
    let mut shelf: Vec<&Tile> = doc.shelved().collect();
    shelf.sort_by_key(|tile| tile.id);
    if !shelf.is_empty() {
        out.push_str("shelf:\n");
        for tile in shelf {
            line(&mut out, 1, tile, None);
        }
    }
    for tray in &doc.trays {
        let _ = writeln!(
            out,
            "tray {}  {}  {} reference(s)",
            tray.id.0,
            tray.name,
            tray.entries.len()
        );
    }
    out
}

fn line(out: &mut String, indent: usize, tile: &Tile, placement: Option<crate::field::Placement>) {
    let pad = "  ".repeat(indent);
    match placement {
        Some(placement) => {
            let _ = writeln!(
                out,
                "{pad}{:<8} {:>2},{:<2}  {}x{}  {}",
                tile.kind.name(),
                placement.cell.col,
                placement.cell.row,
                placement.span.cols,
                placement.span.rows,
                tile.title
            );
        }
        None => {
            let _ = writeln!(
                out,
                "{pad}{:<8} (shelf)       {}",
                tile.kind.name(),
                tile.title
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::{Cell, Landing};
    use crate::ops::{FieldOp, apply};
    use crate::tile::{Span, SpanRange, TileKind};

    #[test]
    fn renders_one_line_per_tile_in_reading_order() {
        let mut doc = FieldDocument::new("eDP-1", 4, 2);
        let tile = |id: u64, title: &str| Tile {
            id: TileId(id),
            kind: TileKind::System { probe: "p".into() },
            title: title.into(),
            spans: SpanRange::fixed(Span::new(1, 1)),
            accent: None,
        };
        for (id, title, col) in [(1, "b", 2), (2, "a", 0)] {
            doc = apply(
                &doc,
                &FieldOp::Place {
                    tile: tile(id, title),
                    landing: Landing {
                        container: None,
                        cell: Cell::new(col, 0),
                    },
                },
            )
            .unwrap()
            .0;
        }
        doc.tiles.push(tile(3, "stashed"));
        let text = render(&doc);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "field eDP-1  4x2  revision 0");
        assert!(
            lines[1].starts_with("system    0,0   1x1  a"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].starts_with("system    2,0   1x1  b"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[3], "shelf:");
        assert!(lines[4].contains("stashed"));
    }
}
