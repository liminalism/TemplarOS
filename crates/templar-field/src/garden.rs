//! The tender: Homegarden's planner (`@templaros.decision.homegarden-activity-grown-places`).
//!
//! A pure function from the field and the ledger's view of its contexts to the ops that bring
//! the field in line with them. It emits **only** `Place`, `Resize` and `Remove`, never `Move`:
//! position is spatial memory, and the only hand that moves a tile is the user's. A `Resize` is
//! accepted only when `reflow::find_resize` satisfies it without any tile the tender does not own
//! giving up span **and without any neighbour's cell changing** — a neighbour that keeps its far
//! edge and loses its near one has moved, whatever the op is called — and a `Place` lands only on
//! free cells. The tender never displaces anything.
//!
//! The ledger (`templar-session-core::homegarden`) decides *stages*; this module decides *ops*. It
//! sees contexts through [`ContextView`], the small projection it needs, so this crate stays
//! free of session-core and of any I/O.

use crate::{
    Cell, FieldDocument, FieldOp, Landing, Placement, Span, SpanRange, Tile, TileId, TileKind,
    reflow::{Grid, find_resize},
};
use std::path::{Path, PathBuf};

/// What the planner needs to know about one context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextView {
    pub root: PathBuf,
    /// The span the context's stage asks for; `None` for a Dormant or hidden context.
    pub wanted: Option<Span>,
    /// Where it was last placed, so it regrows there.
    pub landing: Option<Cell>,
    pub pinned: bool,
    /// The user resized the tile: the tender never resizes it again.
    pub user_span: Option<Span>,
    /// The ledger's account of why: `14 opens, 3 terminals, 4 sessions`.
    pub summary: String,
}

/// The largest span a place can grow to (the ledger's top stage). Growth is anchored at a
/// tile's top-left, so a first landing is chosen as if the tile were already this big: a Trace
/// flush against the bottom edge could never become Expanded, and the representation ladder the
/// stages promise would stop one rung short with empty rows above it.
pub const GROWTH_ROOM: Span = Span::new(6, 4);

/// The corner first growths cluster at, so the user learns where to look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HomeCorner {
    #[default]
    BottomLeft,
    BottomRight,
    TopLeft,
    TopRight,
}

impl HomeCorner {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bottom-left" => Some(Self::BottomLeft),
            "bottom-right" => Some(Self::BottomRight),
            "top-left" => Some(Self::TopLeft),
            "top-right" => Some(Self::TopRight),
            _ => None,
        }
    }

    fn cell(self, cols: u16, rows: u16, span: Span) -> Cell {
        let right = cols.saturating_sub(span.cols);
        let bottom = rows.saturating_sub(span.rows);
        match self {
            Self::BottomLeft => Cell::new(0, bottom),
            Self::BottomRight => Cell::new(right, bottom),
            Self::TopLeft => Cell::new(0, 0),
            Self::TopRight => Cell::new(right, 0),
        }
    }
}

/// One op the tender wants applied, with the sentence that explains it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tended {
    pub op: FieldOp,
    pub reason: String,
}

/// The tile id a garden Place has: the same key `HomeField1.PlaceFolder` and the shell derive,
/// so a folder placed by hand and one grown here are one tile.
#[must_use]
pub fn garden_tile_id(root: &Path) -> TileId {
    TileId::for_key(&format!("folder:{}", root.to_string_lossy()))
}

/// Bring `doc` in line with `contexts`. Deterministic: contexts are handled in root order, and
/// each op is planned against the document as the earlier ops leave it.
#[must_use]
pub fn tend(doc: &FieldDocument, contexts: &[ContextView], corner: HomeCorner) -> Vec<Tended> {
    let mut working = doc.clone();
    let mut out = Vec::new();
    let mut ordered: Vec<&ContextView> = contexts.iter().collect();
    ordered.sort_by(|a, b| a.root.cmp(&b.root));
    for view in ordered {
        let id = garden_tile_id(&view.root);
        let present = working.tile(id).is_some();
        let planned = match (view.wanted, present) {
            (None, true) => (!view.pinned).then(|| Tended {
                op: FieldOp::Remove { id },
                reason: format!("removed {}: {}", name_of(&view.root), view.summary),
            }),
            (Some(span), false) => plan_place(&working, view, id, span, corner),
            (Some(span), true) => plan_resize(&working, contexts, view, id, span),
            (None, false) => None,
        };
        let Some(tended) = planned else {
            continue;
        };
        if let Ok((next, _)) = crate::apply(&working, &tended.op) {
            working = next;
            out.push(tended);
        }
    }
    out
}

fn plan_place(
    doc: &FieldDocument,
    view: &ContextView,
    id: TileId,
    span: Span,
    corner: HomeCorner,
) -> Option<Tended> {
    let cell = free_landing(doc, span, view.landing, corner)?;
    let tile = garden_tile(id, &view.root, span, doc.cols, doc.rows);
    Some(Tended {
        op: FieldOp::Place {
            tile,
            landing: Landing {
                container: None,
                cell,
            },
        },
        reason: format!("placed {}: {}", name_of(&view.root), view.summary),
    })
}

/// A resize is accepted only when nothing but an unpinned, un-user-sized garden tile yields, and
/// every yielding neighbour keeps its cell. `find_resize` clamps toward the request one cell at a
/// time and answers with the first span that fits; when that answer would shift a neighbour, the
/// next-smaller spans are tried (the request, then each axis alone) before giving up, so growth
/// that can only be had by moving something is not had at all.
fn plan_resize(
    doc: &FieldDocument,
    contexts: &[ContextView],
    view: &ContextView,
    id: TileId,
    span: Span,
) -> Option<Tended> {
    let current = doc.location(id)?.placement;
    if view.user_span.is_some() || current.span == span {
        return None;
    }
    let places = doc.placements_in(None);
    let ranges: Vec<(TileId, SpanRange)> = places
        .iter()
        .filter_map(|(tile_id, _)| doc.tile(*tile_id).map(|tile| (*tile_id, tile.spans)))
        .collect();
    let grid = Grid {
        cols: doc.cols,
        rows: doc.rows,
        places,
    };
    let candidates = [
        span,
        Span::new(span.cols, current.span.rows),
        Span::new(current.span.cols, span.rows),
    ];
    let target = candidates.iter().find_map(|candidate| {
        let reflow = find_resize(&grid, &ranges, id, *candidate).ok()?;
        let target = reflow.placed.span;
        if target == current.span {
            return None;
        }
        let yields_stay_put = reflow.moved.iter().all(|(other, before, after)| {
            before.cell == after.cell
                && contexts
                    .iter()
                    .find(|c| garden_tile_id(&c.root) == *other)
                    .is_some_and(|c| !c.pinned && c.user_span.is_none())
        });
        yields_stay_put.then_some(target)
    })?;
    let verb = if u32::from(target.cols) * u32::from(target.rows)
        > u32::from(current.span.cols) * u32::from(current.span.rows)
    {
        "grew"
    } else {
        "shrank"
    };
    Some(Tended {
        op: FieldOp::Resize { id, span: target },
        reason: format!(
            "{verb} {} to {}x{}: {}",
            name_of(&view.root),
            target.cols,
            target.rows,
            view.summary
        ),
    })
}

fn garden_tile(id: TileId, root: &Path, span: Span, cols: u16, rows: u16) -> Tile {
    let mut tile = crate::folder_tile(id, root, cols, rows);
    tile.spans.preferred = span;
    tile
}

fn name_of(root: &Path) -> String {
    root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The remembered landing if free, else the nearest free cell to it, else the nearest free
/// cell to the home corner — the corner as a [`GROWTH_ROOM`] tile would sit in it, so what lands
/// small has somewhere to grow. `None` when nothing fits: the tender never displaces.
fn free_landing(
    doc: &FieldDocument,
    span: Span,
    remembered: Option<Cell>,
    corner: HomeCorner,
) -> Option<Cell> {
    let room = Span::new(
        span.cols.max(GROWTH_ROOM.cols),
        span.rows.max(GROWTH_ROOM.rows),
    );
    let anchor = remembered.unwrap_or_else(|| corner.cell(doc.cols, doc.rows, room));
    let mut best: Option<(u32, Cell)> = None;
    for row in 0..doc.rows {
        for col in 0..doc.cols {
            let cell = Cell::new(col, row);
            let placement = Placement::new(cell, span);
            if !doc.is_free(None, placement, None) {
                continue;
            }
            let distance = cell.distance(anchor);
            if best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, cell));
            }
        }
    }
    best.map(|(_, cell)| cell)
}

/// Whether `tile` is a garden Place for one of `roots`.
#[must_use]
pub fn is_garden_tile(tile: &Tile, roots: &[PathBuf]) -> bool {
    matches!(&tile.kind, TileKind::Folder { path } if roots.iter().any(|r| r == path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(root: &str, wanted: Option<Span>) -> ContextView {
        ContextView {
            root: PathBuf::from(root),
            wanted,
            landing: None,
            pinned: false,
            user_span: None,
            summary: "2 opens, 1 session".into(),
        }
    }

    #[test]
    fn a_new_context_lands_at_the_home_corner_and_never_moves_anything() {
        let doc = FieldDocument::new("DP-1", 12, 8);
        let ops = tend(
            &doc,
            &[view("/home/dk/src/templaros", Some(Span::new(2, 2)))],
            HomeCorner::BottomLeft,
        );
        assert_eq!(ops.len(), 1);
        assert!(ops[0].reason.starts_with("placed templaros: "));
        let FieldOp::Place { landing, .. } = &ops[0].op else {
            panic!("{:?}", ops[0].op)
        };
        // Row 4, not 6: the corner is taken as a `GROWTH_ROOM` tile would sit in it, so the 2x2
        // can become 6x4 in place, growing down and right into free cells.
        assert_eq!(landing.cell, Cell::new(0, 4));
    }

    #[test]
    fn a_full_field_gets_nothing_placed_rather_than_something_displaced() {
        let mut doc = FieldDocument::new("DP-1", 2, 2);
        let user = crate::folder_tile(TileId(9), Path::new("/home/dk/mine"), 2, 2);
        let (next, _) = crate::apply(
            &doc,
            &FieldOp::Place {
                tile: user,
                landing: Landing {
                    container: None,
                    cell: Cell::new(0, 0),
                },
            },
        )
        .unwrap();
        doc = next;
        let before = doc.clone();
        let ops = tend(
            &doc,
            &[view("/home/dk/src/x", Some(Span::new(2, 2)))],
            HomeCorner::default(),
        );
        assert!(ops.is_empty());
        assert_eq!(doc, before);
    }

    #[test]
    fn a_dormant_context_is_removed_unless_pinned_and_a_user_sized_tile_is_never_resized() {
        let doc = FieldDocument::new("DP-1", 12, 8);
        let placed = tend(
            &doc,
            &[view("/home/dk/a", Some(Span::new(2, 2)))],
            HomeCorner::default(),
        );
        let (doc, _) = crate::apply(&doc, &placed[0].op).unwrap();
        let mut gone = view("/home/dk/a", None);
        let ops = tend(&doc, &[gone.clone()], HomeCorner::default());
        assert!(matches!(ops[0].op, FieldOp::Remove { .. }));
        gone.pinned = true;
        assert!(tend(&doc, &[gone], HomeCorner::default()).is_empty());
        let mut sized = view("/home/dk/a", Some(Span::new(4, 3)));
        sized.user_span = Some(Span::new(2, 2));
        assert!(tend(&doc, &[sized], HomeCorner::default()).is_empty());
    }

    /// A garden neighbour to the *right* of a growing place yields from its near edge and keeps
    /// its far one — its cell shifts, which is a move by another name. The tender must decline.
    #[test]
    fn growth_never_shifts_a_neighbours_cell() {
        let doc = FieldDocument::new("DP-1", 8, 4);
        let a = view("/home/dk/a", Some(Span::new(2, 2)));
        let mut b = view("/home/dk/b", Some(Span::new(2, 2)));
        b.landing = Some(Cell::new(2, 0));
        let mut a_first = a.clone();
        a_first.landing = Some(Cell::new(0, 0));
        let doc = {
            let ops = tend(&doc, &[a_first.clone(), b.clone()], HomeCorner::TopLeft);
            let mut doc = doc;
            for op in &ops {
                doc = crate::apply(&doc, &op.op).unwrap().0;
            }
            doc
        };
        assert_eq!(
            doc.location(garden_tile_id(&b.root))
                .unwrap()
                .placement
                .cell,
            Cell::new(2, 0)
        );
        // `a` wants 4x2: the only way is for `b` to give up its left column, moving to (3,0).
        let mut a_wants = a_first;
        a_wants.wanted = Some(Span::new(4, 2));
        let ops = tend(&doc, &[a_wants, b.clone()], HomeCorner::TopLeft);
        for op in &ops {
            let next = crate::apply(&doc, &op.op).unwrap().0;
            assert_eq!(
                next.location(garden_tile_id(&b.root))
                    .unwrap()
                    .placement
                    .cell,
                Cell::new(2, 0),
                "{:?} shifted b",
                op.op
            );
        }
        // Growing *down* instead is free and is taken: the axis ladder finds 2x4 is out of the
        // grid, so nothing there either — the field is 4 rows and `a` would need row 4.
        assert!(
            ops.iter()
                .all(|t| !matches!(t.op, FieldOp::Resize { span, .. } if span.cols == 4))
        );
    }

    /// A Trace is a *line*, and the corner it lands in leaves room for it to become the whole
    /// top stage in place: the ladder is 2x1 to 6x4, twelve times the area, and none of it is
    /// had by moving a neighbour.
    #[test]
    fn a_corner_landing_leaves_room_to_reach_the_top_span() {
        assert!(
            SpanRange::for_kind(
                &TileKind::Folder {
                    path: PathBuf::from("/home/dk/a")
                },
                12,
                8
            )
            .accepts(Span::new(2, 1)),
            "a folder must be able to be a line"
        );
        let doc = FieldDocument::new("DP-1", 12, 8);
        let ops = tend(
            &doc,
            &[view("/home/dk/a", Some(Span::new(2, 1)))],
            HomeCorner::BottomRight,
        );
        let (doc, _) = crate::apply(&doc, &ops[0].op).unwrap();
        let ops = tend(
            &doc,
            &[view("/home/dk/a", Some(GROWTH_ROOM))],
            HomeCorner::BottomRight,
        );
        assert_eq!(ops.len(), 1, "{ops:?}");
        let FieldOp::Resize { span, .. } = ops[0].op else {
            panic!("{:?}", ops[0].op)
        };
        assert_eq!(span, GROWTH_ROOM);
    }

    #[test]
    fn growth_yields_only_garden_tiles_and_is_idempotent() {
        let doc = FieldDocument::new("DP-1", 6, 4);
        // A user tile sits at (2,2) 2x2; the garden place at (0,2) 2x2 wants 4x2 and cannot.
        let user = crate::folder_tile(TileId(9), Path::new("/home/dk/mine"), 6, 4);
        let (doc, _) = crate::apply(
            &doc,
            &FieldOp::Place {
                tile: user,
                landing: Landing {
                    container: None,
                    cell: Cell::new(2, 2),
                },
            },
        )
        .unwrap();
        let grown = view("/home/dk/a", Some(Span::new(2, 2)));
        let ops = tend(&doc, &[grown], HomeCorner::BottomLeft);
        let (doc, _) = crate::apply(&doc, &ops[0].op).unwrap();
        let wants_more = view("/home/dk/a", Some(Span::new(4, 2)));
        let ops = tend(
            &doc,
            std::slice::from_ref(&wants_more),
            HomeCorner::BottomLeft,
        );
        // find_resize clamps to what fits without touching the user tile: 2 wide stays 2 wide,
        // so nothing is emitted rather than a resize that would yield the user's tile.
        assert!(ops.is_empty(), "{ops:?}");
        // Idempotence at the same wants.
        assert!(
            tend(
                &doc,
                &[view("/home/dk/a", Some(Span::new(2, 2)))],
                HomeCorner::BottomLeft
            )
            .is_empty()
        );
    }
}
