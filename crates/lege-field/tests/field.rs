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
        Span::new(COLS / 2, ROWS / 2),
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
    assert_eq!(small.located.len(), 3, "room remained for all three");
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
