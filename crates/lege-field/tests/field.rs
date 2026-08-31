//! The invariants `docs/homefield.md` asks for, as properties: no overlap, locality, exact
//! inverses, boundedness, ghost fidelity, and stability under everything that is not a layout
//! command.

use lege_field::field::{Cell, FieldDocument, Landing, Located, PixelRect, Placement};
use lege_field::ops::{FieldError, FieldOp, apply};
use lege_field::tile::{Span, SpanRange, Tile, TileId, TileKind, TrayId};
use lege_field::{cell_at, cell_rect, tile_rect};
use proptest::prelude::*;

const COLS: u16 = 8;
const ROWS: u16 = 6;

fn kind_strategy() -> impl Strategy<Value = TileKind> {
    prop_oneof![
        Just(TileKind::App {
            app_id: "org.legeos.Files".parse().unwrap()
        }),
        Just(TileKind::ParkedSession {
            app_id: None,
            identifier: "w".into()
        }),
        Just(TileKind::Tray { tray_id: TrayId(1) }),
        Just(TileKind::Widget {
            widget: "calendar".into(),
            scope: None
        }),
        Just(TileKind::File {
            path: "/tmp/x".into()
        }),
        Just(TileKind::System {
            probe: "drives".into()
        }),
    ]
}

fn tile(id: u64, kind: TileKind) -> Tile {
    let spans = SpanRange::for_kind(&kind, COLS, ROWS);
    Tile {
        id: TileId(id),
        title: format!("tile {id}"),
        spans,
        accent: None,
        kind,
    }
}

fn project(id: u64) -> Tile {
    tile(
        id,
        TileKind::Project {
            project_id: format!("p{id}"),
        },
    )
}

fn landing() -> impl Strategy<Value = Landing> {
    (0..COLS, 0..ROWS).prop_map(|(col, row)| Landing {
        container: None,
        cell: Cell::new(col, row),
    })
}

/// A random op against a document whose tile ids are `1..=n`; `next` is the next fresh id.
#[derive(Clone, Debug)]
enum Step {
    Place(TileKind, Landing),
    Move(u64, Landing),
    Resize(u64, Span),
    Stash(u64),
    Remove(u64),
    Tidy,
    Group(u64, u64, Landing),
    MoveInto(u64, u64, Cell),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (kind_strategy(), landing()).prop_map(|(k, l)| Step::Place(k, l)),
        3 => (1u64..40, landing()).prop_map(|(id, l)| Step::Move(id, l)),
        2 => (1u64..40, 1u16..5, 1u16..4).prop_map(|(id, c, r)| Step::Resize(id, Span::new(c, r))),
        1 => (1u64..40).prop_map(Step::Stash),
        1 => (1u64..40).prop_map(Step::Remove),
        1 => Just(Step::Tidy),
        1 => (1u64..40, 1u64..40, landing()).prop_map(|(a, b, l)| Step::Group(a, b, l)),
        1 => (1u64..40, 1u64..40, (0..4u16, 0..4u16)).prop_map(|(a, p, (c, r))| Step::MoveInto(a, p, Cell::new(c, r))),
    ]
}

fn to_op(doc: &FieldDocument, step: &Step) -> FieldOp {
    let fresh = doc.next_tile_id().0;
    match step {
        Step::Place(kind, landing) => FieldOp::Place {
            tile: tile(fresh, kind.clone()),
            landing: *landing,
        },
        Step::Move(id, landing) => FieldOp::Move {
            id: TileId(*id),
            landing: *landing,
        },
        Step::Resize(id, span) => FieldOp::Resize {
            id: TileId(*id),
            span: *span,
        },
        Step::Stash(id) => FieldOp::Stash { id: TileId(*id) },
        Step::Remove(id) => FieldOp::Remove { id: TileId(*id) },
        Step::Tidy => FieldOp::Tidy { container: None },
        Step::Group(a, b, landing) => FieldOp::CreateProject {
            project: project(fresh),
            landing: *landing,
            absorb: vec![TileId(*a), TileId(*b)],
        },
        Step::MoveInto(id, p, cell) => FieldOp::Move {
            id: TileId(*id),
            landing: Landing {
                container: Some(TileId(*p)),
                cell: *cell,
            },
        },
    }
}

/// Every invariant a document must satisfy after any sequence of ops.
fn check_invariants(doc: &FieldDocument) -> Result<(), TestCaseError> {
    // Bounded and non-overlapping on every grid.
    let mut grids: Vec<Option<TileId>> = vec![None];
    grids.extend(doc.containers.iter().map(|c| Some(c.id)));
    for grid in grids {
        let (cols, rows) = doc.grid_size(grid).expect("grid exists");
        let places = doc.placements_in(grid);
        for (i, (id, a)) in places.iter().enumerate() {
            prop_assert!(
                a.fits(cols, rows),
                "{id:?} {a:?} outside {cols}x{rows} on {grid:?}"
            );
            for (other, b) in places.iter().skip(i + 1) {
                prop_assert!(
                    !a.overlaps(*b),
                    "{id:?} {a:?} overlaps {other:?} {b:?} on {grid:?}"
                );
            }
        }
    }
    // Every located tile exists, every container names an existing project, one level deep.
    for (id, located) in &doc.located {
        let tile = doc.tile(*id);
        prop_assert!(tile.is_some(), "{id:?} located but missing");
        if let Some(container) = located.container {
            prop_assert!(doc.container(container).is_some());
            prop_assert!(!tile.unwrap().kind.is_container(), "project inside project");
        }
    }
    // `located` stays sorted, which is what makes document equality meaningful.
    prop_assert!(doc.located.windows(2).all(|w| w[0].0 < w[1].0));
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// After any sequence of up to 20 ops, nothing overlaps, nothing is outside its grid, and
    /// every op's inverse restores the document it was applied to, exactly.
    #[test]
    fn ops_keep_invariants_and_invert_exactly(steps in proptest::collection::vec(step(), 1..20)) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            let op = to_op(&doc, step);
            match apply(&doc, &op) {
                Ok((next, inverse)) => {
                    check_invariants(&next)?;
                    let (back, _) = apply(&next, &inverse).expect("an inverse always applies");
                    prop_assert_eq!(&back, &doc, "inverse of {:?} did not restore", op);
                    doc = next;
                }
                Err(FieldError::UnknownTile | FieldError::NoRoom | FieldError::BadSpan
                    | FieldError::NotFree | FieldError::NestingTooDeep | FieldError::UnknownContainer
                    | FieldError::NotAContainer) => {}
                Err(other) => prop_assert!(false, "unexpected error {other:?} for {op:?}"),
            }
        }
    }

    /// Locality: placing a tile moves only tiles that share a row or column with where it landed
    /// (top level), and never anything inside a container.
    #[test]
    fn placing_moves_only_band_mates(
        steps in proptest::collection::vec(step(), 1..15),
        kind in kind_strategy(),
        landing in landing(),
    ) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        let op = FieldOp::Place { tile: tile(doc.next_tile_id().0, kind), landing };
        let Ok((next, _)) = apply(&doc, &op) else { return Ok(()) };
        let placed = next.location(doc.next_tile_id()).expect("placed").placement;
        for (id, before) in &doc.located {
            let after = next.location(*id).expect("existing tiles are never removed by a place");
            if before.container.is_some() {
                prop_assert_eq!(after, *before, "a tile inside a container moved");
            } else if !before.placement.shares_band(placed) {
                prop_assert_eq!(after, *before, "{:?} moved without sharing a band with {:?}", id, placed);
            }
        }
    }

    /// Ghost fidelity: applying the same op twice to the same document gives the same placement
    /// — the dry run the ghost is drawn from is exactly where release lands.
    #[test]
    fn placement_is_deterministic(steps in proptest::collection::vec(step(), 1..15), kind in kind_strategy(), landing in landing()) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        let op = FieldOp::Place { tile: tile(doc.next_tile_id().0, kind), landing };
        let first = apply(&doc, &op).map(|(d, _)| d);
        let second = apply(&doc, &op).map(|(d, _)| d);
        prop_assert_eq!(first, second);
    }

    /// Stability: content changes (titles, accents, tray entries) move zero tiles.
    #[test]
    fn content_changes_move_nothing(steps in proptest::collection::vec(step(), 1..15), title in "[a-z ]{0,20}") {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        let before = doc.located.clone();
        for tile in &mut doc.tiles {
            tile.title = title.clone();
            tile.accent = Some(0x0033_66cc);
        }
        prop_assert_eq!(doc.located, before);
    }

    /// `cell_at` is the exact inverse of `cell_rect` on every cell's centre, for any grid and
    /// any bounds large enough to draw it.
    #[test]
    fn cell_at_inverts_cell_rect(
        cols in 1u16..24, rows in 1u16..16, x in -500i32..500, y in -500i32..500,
        w in 200i32..4000, h in 200i32..3000, gap in 0i32..12,
    ) {
        let bounds = PixelRect::new(x, y, w, h);
        for col in 0..cols {
            for row in 0..rows {
                let cell = Cell::new(col, row);
                let rect = cell_rect(cols, rows, bounds, cell, Span::new(1, 1), gap);
                let (cx, cy) = rect.centre();
                prop_assert_eq!(cell_at(cols, rows, bounds, cx, cy, gap), Some(cell));
            }
        }
    }
}

#[test]
fn moving_a_selection_preserves_its_shape_and_inverts_as_one_operation() {
    let mut doc = FieldDocument::new("eDP-1", 6, 4);
    for (id, col, row) in [(1, 0, 0), (2, 1, 0), (3, 5, 3)] {
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: Tile {
                    id: TileId(id),
                    title: format!("tile {id}"),
                    spans: SpanRange::fixed(Span::new(1, 1)),
                    accent: None,
                    kind: TileKind::System { probe: "x".into() },
                },
                landing: Landing {
                    container: None,
                    cell: Cell::new(col, row),
                },
            },
        )
        .expect("place")
        .0;
    }
    let op = FieldOp::MoveMany {
        ids: vec![TileId(2), TileId(1), TileId(2)],
        anchor: TileId(1),
        landing: Landing {
            container: None,
            cell: Cell::new(2, 2),
        },
    };
    let (moved, inverse) = apply(&doc, &op).expect("group fits");
    assert_eq!(
        moved.location(TileId(1)).unwrap().placement.cell,
        Cell::new(2, 2)
    );
    assert_eq!(
        moved.location(TileId(2)).unwrap().placement.cell,
        Cell::new(3, 2)
    );
    assert_eq!(moved.location(TileId(3)), doc.location(TileId(3)));
    assert_eq!(apply(&moved, &inverse).expect("inverse").0, doc);
}

#[test]
fn a_group_move_is_refused_atomically_when_any_member_cannot_land() {
    let mut doc = FieldDocument::new("eDP-1", 4, 2);
    for (id, col) in [(1, 0), (2, 1), (3, 3)] {
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: Tile {
                    id: TileId(id),
                    title: format!("tile {id}"),
                    spans: SpanRange::fixed(Span::new(1, 1)),
                    accent: None,
                    kind: TileKind::System { probe: "x".into() },
                },
                landing: Landing {
                    container: None,
                    cell: Cell::new(col, 0),
                },
            },
        )
        .expect("place")
        .0;
    }
    let before = doc.clone();
    assert_eq!(
        apply(
            &doc,
            &FieldOp::MoveMany {
                ids: vec![TileId(1), TileId(2)],
                anchor: TileId(1),
                landing: Landing {
                    container: None,
                    cell: Cell::new(2, 0),
                },
            },
        ),
        Err(FieldError::NotFree)
    );
    assert_eq!(doc, before, "apply-on-a-copy leaves the source untouched");
}

#[test]
fn stashing_a_selection_has_one_inverse_for_the_whole_group() {
    let mut doc = FieldDocument::new("eDP-1", 4, 2);
    for id in 1..=2 {
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: tile(
                    id,
                    TileKind::File {
                        path: format!("/tmp/{id}").into(),
                    },
                ),
                landing: Landing {
                    container: None,
                    cell: Cell::new((id - 1) as u16, 0),
                },
            },
        )
        .expect("place")
        .0;
    }
    let (stashed, inverse) = apply(
        &doc,
        &FieldOp::StashMany {
            ids: vec![TileId(1), TileId(2)],
        },
    )
    .expect("stash group");
    assert!(stashed.location(TileId(1)).is_none());
    assert!(stashed.location(TileId(2)).is_none());
    assert_eq!(apply(&stashed, &inverse).expect("inverse").0, doc);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A resize relocates nothing.
    ///
    /// This is the resize counterpart of `placing_moves_only_band_mates`, and it is deliberately
    /// *stronger* than that rule rather than a relaxation of it. A drop may evict a band-mate to
    /// any free cell on the grid; a resize may not move anything at all. What a neighbour may do
    /// is give up the cells the growth needs — and even then only from the edge facing the
    /// growth, so its far edge stays exactly where the user last put it and its new rectangle is
    /// a subset of its old one. Everything not overlapping the growth rectangle is untouched,
    /// full stop.
    #[test]
    fn resizing_yields_span_and_never_relocates(
        steps in proptest::collection::vec(step(), 1..12),
        target in 1u64..40,
        span in (1u16..7, 1u16..5),
    ) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        let id = TileId(target);
        let Some(before) = doc.location(id) else { return Ok(()); };
        let op = FieldOp::Resize { id, span: Span::new(span.0, span.1) };
        let Ok((after, inverse)) = apply(&doc, &op) else { return Ok(()); };
        check_invariants(&after)?;

        let placed = after.location(id).expect("a resized tile keeps its place");
        prop_assert_eq!(placed.placement.cell, before.placement.cell, "the resized tile moved");
        prop_assert_eq!(placed.container, before.container);
        let growth = Placement::new(before.placement.cell, placed.placement.span);

        for (other, was) in &doc.located {
            if *other == id {
                continue;
            }
            let now = after.location(*other);
            if now == Some(*was) {
                continue;
            }
            // The only tiles allowed to differ are the ones the growth actually reached, on the
            // same grid, and only by yielding.
            prop_assert_eq!(was.container, before.container, "{:?} changed on another grid", other);
            prop_assert!(
                was.placement.overlaps(growth),
                "{:?} changed without being overlapped by the growth {:?}", other, growth
            );
            let now = now.expect("a yielding tile is never unplaced").placement;
            prop_assert_eq!(
                (now.right(), now.bottom()),
                (was.placement.right(), was.placement.bottom()),
                "{:?} did not keep its far edge", other
            );
            prop_assert!(
                now.cell.col >= was.placement.cell.col && now.cell.row >= was.placement.cell.row,
                "{:?} grew instead of yielding", other
            );
            let floor = doc.tile(*other).expect("a located tile exists").spans.min;
            prop_assert!(
                now.span.cols >= floor.cols && now.span.rows >= floor.rows,
                "{:?} was pushed below its minimum {:?}", other, floor
            );
        }

        // And the whole thing still undoes exactly — neighbours' spans included, which is the
        // property that makes `Layout` sufficient as the inverse.
        let (back, _) = apply(&after, &inverse).expect("an inverse always applies");
        prop_assert_eq!(back, doc);
    }

    /// The clamp never overshoots the request and never turns a growth into a shrink: whatever
    /// span comes out lies between where the tile was and where the user asked it to go.
    #[test]
    fn resizing_clamps_between_the_current_span_and_the_request(
        steps in proptest::collection::vec(step(), 1..12),
        target in 1u64..40,
        span in (1u16..7, 1u16..5),
    ) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        let id = TileId(target);
        let Some(before) = doc.location(id) else { return Ok(()); };
        let requested = Span::new(span.0, span.1);
        let Ok((after, _)) = apply(&doc, &FieldOp::Resize { id, span: requested }) else {
            return Ok(());
        };
        let was = before.placement.span;
        let now = after.location(id).expect("still placed").placement.span;
        for (now, was, requested, axis) in [
            (now.cols, was.cols, requested.cols, "cols"),
            (now.rows, was.rows, requested.rows, "rows"),
        ] {
            prop_assert!(
                now >= was.min(requested) && now <= was.max(requested),
                "{axis}: {now} is not between the current {was} and the requested {requested}"
            );
        }
    }
}

/// A tile with spans stated outright, so a resize test says what it is testing instead of
/// depending on one kind's class floors staying where they are today.
fn sized(id: u64, min: Span, preferred: Span, max: Span) -> Tile {
    Tile {
        id: TileId(id),
        title: format!("tile {id}"),
        spans: SpanRange::new(min, preferred, max),
        accent: None,
        kind: TileKind::Tray { tray_id: TrayId(1) },
    }
}

/// Two tiles of `preferred` size placed at columns 0 and `preferred.cols`, row 0.
fn side_by_side(min: Span, preferred: Span, max: Span) -> FieldDocument {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    for (id, col) in [(1u64, 0u16), (2, preferred.cols)] {
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: sized(id, min, preferred, max),
                landing: Landing {
                    container: None,
                    cell: Cell::new(col, 0),
                },
            },
        )
        .unwrap()
        .0;
    }
    doc
}

#[test]
fn growing_into_free_space_disturbs_nothing() {
    // The user's rule: while the field has room, a resize is nobody else's business.
    let one = Span::new(1, 1);
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    for (id, col) in [(1u64, 0u16), (2, 5)] {
        doc = apply(
            &doc,
            &FieldOp::Place {
                tile: sized(id, one, one, Span::new(4, 4)),
                landing: Landing {
                    container: None,
                    cell: Cell::new(col, 0),
                },
            },
        )
        .unwrap()
        .0;
    }
    let far = doc.location(TileId(2)).unwrap();
    let (grown, _) = apply(
        &doc,
        &FieldOp::Resize {
            id: TileId(1),
            span: Span::new(3, 3),
        },
    )
    .unwrap();
    assert_eq!(
        grown.location(TileId(1)).unwrap().placement.span,
        Span::new(3, 3),
        "there was room, so the request is granted whole"
    );
    assert_eq!(
        grown.location(TileId(2)),
        Some(far),
        "the far tile never moved"
    );
}

#[test]
fn a_neighbour_yields_from_the_edge_facing_the_growth_and_undoes() {
    // Two 2x2 tiles side by side, each able to shrink to 1 wide. Growing the left one rightwards
    // must narrow the right one against its own right edge — not shove it somewhere else.
    let doc = side_by_side(Span::new(1, 1), Span::new(2, 2), Span::new(4, 4));
    assert_eq!(
        doc.location(TileId(2)).unwrap().placement,
        Placement::new(Cell::new(2, 0), Span::new(2, 2))
    );

    let (after, undo) = apply(
        &doc,
        &FieldOp::Resize {
            id: TileId(1),
            span: Span::new(3, 2),
        },
    )
    .unwrap();
    assert_eq!(
        after.location(TileId(1)).unwrap().placement,
        Placement::new(Cell::new(0, 0), Span::new(3, 2))
    );
    let yielded = after.location(TileId(2)).unwrap().placement;
    assert_eq!(
        yielded,
        Placement::new(Cell::new(3, 0), Span::new(1, 2)),
        "the neighbour keeps its right edge at column 4 and gives up the column it lost"
    );
    assert_eq!(yielded.right(), 4, "the far edge is exactly where it was");

    // Ctrl+Z restores both tiles' sizes, not just their positions.
    assert_eq!(apply(&after, &undo).unwrap().0, doc);
}

#[test]
fn a_neighbour_at_its_floor_clamps_the_drag_instead_of_being_evicted() {
    // Both tiles are already at their 2x2 minimum. The right-hand one can give up nothing, so
    // the growth stops rather than squashing it further or pushing it away.
    let two = Span::new(2, 2);
    let doc = side_by_side(two, two, Span::new(4, 4));
    let before_neighbour = doc.location(TileId(2)).unwrap();
    let (after, _) = apply(
        &doc,
        &FieldOp::Resize {
            id: TileId(1),
            span: Span::new(4, 2),
        },
    )
    .unwrap();
    assert_eq!(
        after.location(TileId(1)).unwrap().placement.span,
        Span::new(2, 2),
        "growing to 4 would take the 2x2 neighbour under its floor, so the drag clamps"
    );
    assert_eq!(
        after.location(TileId(2)),
        Some(before_neighbour),
        "a clamped resize leaves the neighbour completely alone"
    );
}

#[test]
fn no_tile_but_a_project_may_cover_the_whole_field() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    doc = apply(
        &doc,
        &FieldOp::Place {
            tile: tile(
                1,
                TileKind::Widget {
                    widget: "launcher".into(),
                    scope: None,
                },
            ),
            landing: Landing {
                container: None,
                cell: Cell::new(0, 0),
            },
        },
    )
    .unwrap()
    .0;
    assert_eq!(
        apply(
            &doc,
            &FieldOp::Resize {
                id: TileId(1),
                span: Span::new(COLS, ROWS),
            },
        ),
        Err(FieldError::BadSpan),
        "a widget may not become the whole screen"
    );
    // …but it may grow, which it could not before: its ceiling used to equal its preferred size.
    assert!(
        apply(
            &doc,
            &FieldOp::Resize {
                id: TileId(1),
                span: Span::new(6, 5),
            },
        )
        .is_ok()
    );
}

#[test]
fn a_removed_projects_children_go_to_the_shelf_and_come_back_on_undo() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    for id in 1..=2u64 {
        let op = FieldOp::Place {
            tile: tile(id, TileKind::System { probe: "p".into() }),
            landing: Landing {
                container: None,
                cell: Cell::new(id as u16, 0),
            },
        };
        doc = apply(&doc, &op).unwrap().0;
    }
    let group = FieldOp::CreateProject {
        project: project(3),
        landing: Landing {
            container: None,
            cell: Cell::new(1, 0),
        },
        absorb: vec![TileId(1), TileId(2)],
    };
    let (grouped, ungroup) = apply(&doc, &group).unwrap();
    assert_eq!(grouped.parent_of(TileId(1)), Some(TileId(3)));
    assert_eq!(grouped.parent_of(TileId(2)), Some(TileId(3)));
    assert_eq!(
        grouped.location(TileId(3)).unwrap().placement.span,
        Span::new((COLS / 2).max(4), (ROWS / 2).max(4)),
        "a new project is one quadrant"
    );

    let (removed, restore) = apply(&grouped, &FieldOp::Remove { id: TileId(3) }).unwrap();
    assert_eq!(
        removed.location(TileId(1)),
        None,
        "children are on the shelf, not lost"
    );
    assert!(removed.tile(TileId(1)).is_some());
    assert_eq!(apply(&removed, &restore).unwrap().0, grouped);
    assert_eq!(apply(&grouped, &ungroup).unwrap().0, doc);
}

#[test]
fn shrinking_the_grid_keeps_every_tile_reachable_and_undoes() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    for (id, col, row) in [(1, 7, 5), (2, 0, 0), (3, 6, 1)] {
        let op = FieldOp::Place {
            tile: tile(id, TileKind::System { probe: "p".into() }),
            landing: Landing {
                container: None,
                cell: Cell::new(col, row),
            },
        };
        doc = apply(&doc, &op).unwrap().0;
    }
    let (small, undo) = apply(&doc, &FieldOp::SetGrid { cols: 4, rows: 3 }).unwrap();
    for (id, located) in &small.located {
        assert!(located.placement.fits(4, 3), "{id:?} at {located:?}");
    }
    assert_eq!(small.located.len(), 1, "one 3×2 system instrument fits");
    assert_eq!(small.shelved().count(), 2, "the others remain on the shelf");
    assert_eq!(small.tiles.len(), 3, "shrinking never loses an instrument");
    assert_eq!(apply(&small, &undo).unwrap().0, doc);
}

#[test]
fn tile_rect_nests_children_inside_their_project() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    let op = FieldOp::Place {
        tile: project(1),
        landing: Landing {
            container: None,
            cell: Cell::new(0, 0),
        },
    };
    doc = apply(&doc, &op).unwrap().0;
    let child = FieldOp::Place {
        tile: tile(
            2,
            TileKind::File {
                path: "/tmp/a".into(),
            },
        ),
        landing: Landing {
            container: Some(TileId(1)),
            cell: Cell::new(0, 0),
        },
    };
    doc = apply(&doc, &child).unwrap().0;
    let bounds = PixelRect::new(0, 0, 1600, 900);
    let outer = tile_rect(&doc, bounds, TileId(1), 8).unwrap();
    let inner = tile_rect(&doc, bounds, TileId(2), 8).unwrap();
    assert!(inner.x >= outer.x && inner.y > outer.y);
    assert!(inner.x + inner.w <= outer.x + outer.w);
    assert!(inner.y + inner.h <= outer.y + outer.h);
    assert_eq!(
        doc.tile_at(Some(TileId(1)), Cell::new(0, 0)),
        Some(TileId(2))
    );
    let located = doc.location(TileId(2)).unwrap();
    assert_eq!(
        located,
        Located {
            container: Some(TileId(1)),
            placement: Placement::new(Cell::new(0, 0), located.placement.span)
        }
    );
}

#[test]
fn a_document_round_trips_through_json() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    let op = FieldOp::Place {
        tile: tile(
            1,
            TileKind::ParkedSession {
                app_id: Some("org.legeos.Files".parse().unwrap()),
                identifier: "abc".into(),
            },
        ),
        landing: Landing {
            container: None,
            cell: Cell::new(3, 3),
        },
    };
    doc = apply(&doc, &op).unwrap().0;
    let json = serde_json::to_string(&doc).unwrap();
    let back: FieldDocument = serde_json::from_str(&json).unwrap();
    assert_eq!(back, doc);
    let op_json = serde_json::to_string(&op).unwrap();
    let op_back: FieldOp = serde_json::from_str(&op_json).unwrap();
    assert_eq!(op_back, op);

    // Inverses are batches, and batches must survive the wire too — sessiond stores them.
    let (_, inverse) = apply(&FieldDocument::new("eDP-1", COLS, ROWS), &op).unwrap();
    assert!(matches!(inverse, FieldOp::Batch { .. }));
    let inverse_json = serde_json::to_string(&inverse).unwrap();
    let inverse_back: FieldOp = serde_json::from_str(&inverse_json).unwrap();
    assert_eq!(inverse_back, inverse);
}

#[test]
fn a_document_from_another_version_is_refused() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    doc.version = 99;
    assert_eq!(
        apply(&doc, &FieldOp::Tidy { container: None }).unwrap_err(),
        FieldError::VersionMismatch
    );
}

proptest! {
    /// Reshaping onto another grid is still a legal field: nothing overlaps, nothing is out of
    /// bounds, and no tile is silently dropped — a tile with nowhere to go is shelved, which
    /// leaves it listed and recoverable rather than gone.
    ///
    /// This is the invariant behind reloading a persisted layout on a display that is not the one
    /// it was saved on.
    #[test]
    fn reshaping_onto_another_grid_keeps_the_field_legal(
        steps in proptest::collection::vec(step(), 1..15),
        cols in 2..14u16,
        rows in 2..12u16,
    ) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }

        let next = lege_field::reshape(&doc, cols, rows);
        prop_assert_eq!(next.cols, cols);
        prop_assert_eq!(next.rows, rows);
        prop_assert_eq!(
            next.tiles.len(),
            doc.tiles.len(),
            "reshape must not drop a tile from the document"
        );

        let top = next.placements_in(None);
        for (index, (id, placement)) in top.iter().enumerate() {
            prop_assert!(
                placement.fits(cols, rows),
                "{:?} at {:?} is outside the {}x{} grid",
                id, placement, cols, rows
            );
            for (other_id, other) in top.iter().skip(index + 1) {
                prop_assert!(
                    !placement.overlaps(*other),
                    "{:?} at {:?} overlaps {:?} at {:?}",
                    id, placement, other_id, other
                );
            }
        }

        // A tile inside a project keeps its interior placement: the project's grid is its own and
        // does not change because the screen did.
        for (id, before) in &doc.located {
            if before.container.is_some() {
                prop_assert_eq!(
                    next.location(*id),
                    Some(*before),
                    "{:?} moved inside its container",
                    id
                );
            }
        }
    }

    /// Reshaping to the size the document already has changes nothing at all.
    #[test]
    fn reshaping_to_the_same_grid_is_the_identity(
        steps in proptest::collection::vec(step(), 1..15),
    ) {
        let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
        for step in &steps {
            if let Ok((next, _)) = apply(&doc, &to_op(&doc, step)) {
                doc = next;
            }
        }
        prop_assert_eq!(lege_field::reshape(&doc, COLS, ROWS), doc);
    }
}

/// A field that fits comfortably on the new grid keeps every tile placed. Shelving during a
/// reshape is a last resort, not a shortcut: nothing offers a route back off the shelf, so a tile
/// put there by a change of resolution would be gone until the user rebuilt it by hand.
#[test]
fn reshaping_onto_a_roomier_grid_shelves_nothing() {
    let mut doc = FieldDocument::new("eDP-1", COLS, ROWS);
    for index in 0..6u64 {
        let op = FieldOp::Place {
            tile: tile(
                index + 1,
                TileKind::App {
                    app_id: "org.legeos.Files".parse().unwrap(),
                },
            ),
            landing: Landing {
                container: None,
                cell: Cell::new((index as u16) % COLS, (index as u16) / COLS),
            },
        };
        doc = apply(&doc, &op).expect("placed").0;
    }
    let placed = doc.placements_in(None).len();
    assert_eq!(placed, 6, "the fixture itself must start fully placed");

    for (cols, rows) in [(16, 12), (4, 4), (COLS, ROWS), (32, 3)] {
        let next = lege_field::reshape(&doc, cols, rows);
        assert_eq!(
            next.placements_in(None).len(),
            placed,
            "{cols}x{rows} has room for all six, so none may be shelved"
        );
        assert_eq!(next.shelved().count(), 0, "{cols}x{rows} shelved a tile");
    }
}
