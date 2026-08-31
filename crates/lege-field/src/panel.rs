//! Home-only layout groups — panels, rails and docks (`docs/homefield.md`). A panel is a named
//! set of member tiles and a rectangle of the field they are arranged within. It owns *layout*,
//! never identity: arranging a panel moves and sizes its members, and nothing else about them.
//!
//! Panels never map as compositor chrome and never reserve an exclusive zone: they exist only in
//! the Home Field document, so behind a fullscreen or maximized application there is no panel at
//! all — the whole surface is the application's.
//!
//! The arrangement functions here are pure and deterministic: the same document, panel and
//! arrangement always produce the same placements, they never overlap one another, and every
//! placement lies inside the grid. A member whose minimum span cannot fit its slot goes to the
//! shelf rather than being forced somewhere it does not fit. Mutation happens only through
//! `FieldOp::{CreatePanel, RemovePanel, ConfigurePanel, ArrangePanel}` in [`crate::ops`], so
//! every panel change has an exact inverse like every other field edit.

use crate::field::{Cell, FieldDocument, Located, Placement};
use crate::tile::{Span, TileId};
use serde::{Deserialize, Serialize};

/// A panel's stable identity, hashed from what it stands for exactly like [`TileId`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct PanelId(pub u64);

/// Which way a balanced arrangement runs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Orientation {
    /// Members flow left-to-right, wrapping into further rows.
    Rows,
    /// Members flow top-to-bottom, wrapping into further columns.
    Columns,
}

/// The field edge a rail runs along.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// How a panel lays its members out.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Arrangement {
    /// Wherever the user put them. The layout a preset retains as its restore target.
    Custom,
    /// Near-equal slots in rows or columns filling the panel's region.
    Balanced(Orientation),
    /// A single file along one field edge; the panel's region moves to that edge.
    Rail(Edge),
    /// A cushion-map: each member's area follows the measure it declares (its preferred span's
    /// area), packed by balanced binary splits of the region.
    Cushion,
}

/// One Home-only layout group.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Panel {
    pub id: PanelId,
    pub title: String,
    /// The field rectangle arrangements fill. A [`Arrangement::Rail`] ignores it in favour of
    /// its edge, but keeps it as the region a non-rail arrangement returns to.
    pub region: Placement,
    pub arrangement: Arrangement,
    /// Arrangement order. Members reference tiles; a member whose tile is gone is simply
    /// skipped, so removing a tile never has to know about panels.
    pub members: Vec<TileId>,
    /// The member placements retained when the first preset replaced a custom layout —
    /// [`Arrangement::Custom`] restores exactly these and clears them.
    #[serde(default)]
    pub baseline: Option<Vec<(TileId, Option<Located>)>>,
}

/// A panel's stable id from what it stands for, mirroring the shell's tile id scheme.
pub fn panel_id_for(key: &str) -> PanelId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    PanelId(hash.max(1))
}

/// The region a rail along `edge` occupies on a `cols`×`rows` field, `thickness` cells deep.
fn rail_region(cols: u16, rows: u16, edge: Edge, thickness: u16) -> Placement {
    let thickness = thickness.max(1);
    match edge {
        Edge::Left => Placement::new(Cell::new(0, 0), Span::new(thickness.min(cols), rows)),
        Edge::Right => Placement::new(
            Cell::new(cols.saturating_sub(thickness), 0),
            Span::new(thickness.min(cols), rows),
        ),
        Edge::Top => Placement::new(Cell::new(0, 0), Span::new(cols, thickness.min(rows))),
        Edge::Bottom => Placement::new(
            Cell::new(0, rows.saturating_sub(thickness)),
            Span::new(cols, thickness.min(rows)),
        ),
    }
}

/// The members of `panel` that exist in `doc`, in arrangement order.
fn present_members(doc: &FieldDocument, panel: &Panel) -> Vec<TileId> {
    panel
        .members
        .iter()
        .copied()
        .filter(|id| doc.tile(*id).is_some())
        .collect()
}

/// The placement a member gets inside its slot: its preferred span clamped to its own range and
/// to the slot, at the slot's origin — or the shelf when not even its minimum fits.
fn seat(doc: &FieldDocument, id: TileId, slot: Placement) -> Option<Located> {
    let spans = doc.tile(id)?.spans;
    if spans.min.cols > slot.span.cols || spans.min.rows > slot.span.rows {
        return None;
    }
    let span = Span::new(
        spans.preferred.cols.clamp(spans.min.cols, slot.span.cols),
        spans.preferred.rows.clamp(spans.min.rows, slot.span.rows),
    );
    Some(Located {
        container: None,
        placement: Placement::new(slot.cell, span),
    })
}

/// Split `members` into near-equal runs of `count` and hand each a slot of `region` — rows or
/// columns per `orientation`. The split is square-ish: four members become 2×2, eight become
/// three runs of three, not a strip of slivers. A slot that falls outside the region (a region
/// too small for the member count) is `None`, which shelves its member — `Span::new` clamps to
/// one cell, so a zero-span sentinel cannot express "no slot".
fn balanced_slots(
    region: Placement,
    count: u16,
    orientation: Orientation,
) -> Vec<Option<Placement>> {
    let count = count.max(1);
    // per_run members flow along a run (a row for Rows, a column for Columns); runs wrap.
    let per_run = (1..=count)
        .find(|candidate| candidate * candidate >= count)
        .unwrap_or(count);
    let runs = count.div_ceil(per_run);
    let (main, cross) = match orientation {
        Orientation::Rows => (region.span.rows, region.span.cols),
        Orientation::Columns => (region.span.cols, region.span.rows),
    };
    let run_depth = (main / runs).max(1);
    let slot_width = (cross / per_run).max(1);
    (0..count)
        .map(|index| {
            let run = index / per_run;
            let in_run = index % per_run;
            let (col, row, span) = match orientation {
                Orientation::Rows => (
                    region.cell.col + in_run * slot_width,
                    region.cell.row + run * run_depth,
                    Span::new(slot_width, run_depth),
                ),
                Orientation::Columns => (
                    region.cell.col + run * run_depth,
                    region.cell.row + in_run * slot_width,
                    Span::new(run_depth, slot_width),
                ),
            };
            let slot = Placement::new(Cell::new(col, row), span);
            (slot.right() <= region.right() && slot.bottom() <= region.bottom()).then_some(slot)
        })
        .collect()
}

/// Recursively split `region` at the members' cumulative weight midpoint, along the longer
/// axis — a deterministic cushion pack where area follows the declared measure. A region too
/// small to cut goes whole to the heaviest member and shelves the rest (`None`): `Span::new`
/// clamps zero extents to one cell, so a degenerate cut would silently overlap instead.
fn cushion_slots(
    region: Placement,
    weighted: &[(TileId, u32)],
    out: &mut Vec<(TileId, Option<Placement>)>,
) {
    match weighted {
        [] => {}
        [(id, _)] => out.push((*id, Some(region))),
        _ => {
            let extent = region.span.cols.max(region.span.rows);
            if extent < 2 {
                let mut members = weighted.iter();
                if let Some((first, _)) = members.next() {
                    out.push((*first, Some(region)));
                }
                out.extend(members.map(|(id, _)| (*id, None)));
                return;
            }
            let total: u64 = weighted.iter().map(|(_, weight)| u64::from(*weight)).sum();
            let mut cumulative = 0u64;
            let mut split = 1;
            for (index, (_, weight)) in weighted.iter().enumerate() {
                if cumulative * 2 >= total && index > 0 {
                    break;
                }
                cumulative += u64::from(*weight);
                split = index + 1;
            }
            let split = split.clamp(1, weighted.len() - 1);
            let first_weight: u64 = weighted[..split]
                .iter()
                .map(|(_, weight)| u64::from(*weight))
                .sum();
            let share = |extent: u16| -> u16 {
                let cut = u16::try_from(u64::from(extent) * first_weight / total.max(1))
                    .unwrap_or(extent);
                cut.clamp(1, extent - 1)
            };
            let (first, second) = if region.span.cols >= region.span.rows {
                let cut = share(region.span.cols);
                (
                    Placement::new(region.cell, Span::new(cut, region.span.rows)),
                    Placement::new(
                        Cell::new(region.cell.col + cut, region.cell.row),
                        Span::new(region.span.cols - cut, region.span.rows),
                    ),
                )
            } else {
                let cut = share(region.span.rows);
                (
                    Placement::new(region.cell, Span::new(region.span.cols, cut)),
                    Placement::new(
                        Cell::new(region.cell.col, region.cell.row + cut),
                        Span::new(region.span.cols, region.span.rows - cut),
                    ),
                )
            };
            cushion_slots(first, &weighted[..split], out);
            cushion_slots(second, &weighted[split..], out);
        }
    }
}

/// The placements `arrangement` gives `panel`'s members on `doc` — pure, deterministic, and
/// non-overlapping, every placement inside the grid. Members whose minimum span cannot fit
/// their slot are sent to the shelf (`None`) rather than forced. [`Arrangement::Custom`] is not
/// computed here: it is the retained baseline, restored by the op.
pub fn arrangement_layout(
    doc: &FieldDocument,
    panel: &Panel,
    arrangement: &Arrangement,
) -> Vec<(TileId, Option<Located>)> {
    let members = present_members(doc, panel);
    let count = u16::try_from(members.len()).unwrap_or(u16::MAX);
    if members.is_empty() {
        return Vec::new();
    }
    // Clip the region to the grid so every slot is inside it whatever the stored region says
    // (an output change may have shrunk the field under a persisted panel).
    let clip = |region: Placement| -> Placement {
        let col = region.cell.col.min(doc.cols.saturating_sub(1));
        let row = region.cell.row.min(doc.rows.saturating_sub(1));
        Placement::new(
            Cell::new(col, row),
            Span::new(
                region.span.cols.min(doc.cols - col).max(1),
                region.span.rows.min(doc.rows - row).max(1),
            ),
        )
    };
    match arrangement {
        Arrangement::Custom => panel.baseline.clone().unwrap_or_default(),
        Arrangement::Balanced(orientation) => {
            let region = clip(panel.region);
            let slots = balanced_slots(region, count, *orientation);
            members
                .iter()
                .zip(slots)
                .map(|(id, slot)| (*id, slot.and_then(|slot| seat(doc, *id, slot))))
                .collect()
        }
        Arrangement::Rail(edge) => {
            let thickness = members
                .iter()
                .filter_map(|id| doc.tile(*id))
                .map(|tile| match edge {
                    Edge::Left | Edge::Right => tile.spans.min.cols,
                    Edge::Top | Edge::Bottom => tile.spans.min.rows,
                })
                .max()
                .unwrap_or(1);
            let region = clip(rail_region(doc.cols, doc.rows, *edge, thickness));
            let orientation = match edge {
                Edge::Left | Edge::Right => Orientation::Columns,
                Edge::Top | Edge::Bottom => Orientation::Rows,
            };
            // A rail is a single file: one run, members split along the edge.
            let main = match orientation {
                Orientation::Rows => region.span.cols,
                Orientation::Columns => region.span.rows,
            };
            let slot_len = (main / count).max(1);
            members
                .iter()
                .enumerate()
                .map(|(index, id)| {
                    let index = u16::try_from(index).unwrap_or(u16::MAX);
                    let slot = match orientation {
                        Orientation::Rows => Placement::new(
                            Cell::new(region.cell.col + index * slot_len, region.cell.row),
                            Span::new(slot_len, region.span.rows),
                        ),
                        Orientation::Columns => Placement::new(
                            Cell::new(region.cell.col, region.cell.row + index * slot_len),
                            Span::new(region.span.cols, slot_len),
                        ),
                    };
                    // Past the end of the rail: no slot at all, the member shelves.
                    let slot = slot.fits(doc.cols, doc.rows).then_some(slot);
                    (*id, slot.and_then(|slot| seat(doc, *id, slot)))
                })
                .collect()
        }
        Arrangement::Cushion => {
            let region = clip(panel.region);
            let weighted: Vec<(TileId, u32)> = members
                .iter()
                .filter_map(|id| doc.tile(*id).map(|tile| (*id, tile.spans)))
                .map(|(id, spans)| {
                    let area = u32::from(spans.preferred.cols) * u32::from(spans.preferred.rows);
                    (id, area.max(1))
                })
                .collect();
            let mut slots = Vec::with_capacity(weighted.len());
            cushion_slots(region, &weighted, &mut slots);
            slots
                .into_iter()
                .map(|(id, slot)| (id, slot.and_then(|slot| seat(doc, id, slot))))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{FieldOp, apply};
    use crate::tile::{SpanRange, Tile, TileKind};

    fn tile(id: u64, min: (u16, u16), preferred: (u16, u16)) -> Tile {
        Tile {
            id: TileId(id),
            kind: TileKind::Widget {
                widget: format!("test:{id}"),
                scope: None,
            },
            title: format!("tile {id}"),
            spans: SpanRange::new(
                Span::new(min.0, min.1),
                Span::new(preferred.0, preferred.1),
                Span::new(8, 8),
            ),
            accent: None,
        }
    }

    /// A `cols`×`rows` document holding `n` 1×1-floor tiles at custom spots along the top row.
    fn doc_with_members(cols: u16, rows: u16, n: u64) -> (FieldDocument, Vec<TileId>) {
        let mut doc = FieldDocument::new("test-output", cols, rows);
        let mut members = Vec::new();
        for index in 0..n {
            let tile = tile(index + 1, (1, 1), (2, 2));
            members.push(tile.id);
            let index = u16::try_from(index).unwrap_or(u16::MAX);
            let op = FieldOp::AddTile {
                tile,
                at: Some(Located {
                    container: None,
                    placement: Placement::new(
                        Cell::new(index % cols, index / cols),
                        Span::new(1, 1),
                    ),
                }),
                container: None,
            };
            doc = apply(&doc, &op).expect("seed tile").0;
        }
        (doc, members)
    }

    fn panel_over(members: &[TileId], region: Placement) -> Panel {
        Panel {
            id: panel_id_for("panel:test"),
            title: "Test panel".to_owned(),
            region,
            arrangement: Arrangement::Custom,
            members: members.to_vec(),
            baseline: None,
        }
    }

    fn assert_bounded_and_disjoint(doc: &FieldDocument, set: &[(TileId, Option<Located>)]) {
        let placed: Vec<(TileId, Placement)> = set
            .iter()
            .filter_map(|(id, located)| located.map(|located| (*id, located.placement)))
            .collect();
        for (id, placement) in &placed {
            assert!(
                placement.fits(doc.cols, doc.rows),
                "{id:?} out of bounds: {placement:?} on {}x{}",
                doc.cols,
                doc.rows
            );
            assert!(placement.span.cols >= 1 && placement.span.rows >= 1);
        }
        for (index, (id, placement)) in placed.iter().enumerate() {
            for (other_id, other) in &placed[index + 1..] {
                assert!(
                    !placement.overlaps(*other),
                    "{id:?} overlaps {other_id:?}: {placement:?} vs {other:?}"
                );
            }
        }
    }

    const ARRANGEMENTS: [Arrangement; 8] = [
        Arrangement::Balanced(Orientation::Rows),
        Arrangement::Balanced(Orientation::Columns),
        Arrangement::Rail(Edge::Left),
        Arrangement::Rail(Edge::Right),
        Arrangement::Rail(Edge::Top),
        Arrangement::Rail(Edge::Bottom),
        Arrangement::Cushion,
        Arrangement::Custom,
    ];

    #[test]
    fn panel_layout_balanced_is_deterministic_nonoverlapping_and_bounded() {
        let (doc, members) = doc_with_members(12, 8, 5);
        let region = Placement::new(Cell::new(2, 2), Span::new(8, 6));
        let panel = panel_over(&members, region);
        for orientation in [Orientation::Rows, Orientation::Columns] {
            let arrangement = Arrangement::Balanced(orientation);
            let first = arrangement_layout(&doc, &panel, &arrangement);
            let second = arrangement_layout(&doc, &panel, &arrangement);
            assert_eq!(first, second, "same inputs, same layout");
            assert_eq!(first.len(), members.len());
            assert_bounded_and_disjoint(&doc, &first);
            for (_, located) in &first {
                let placement = located.expect("room for every member here").placement;
                assert!(placement.cell.col >= region.cell.col);
                assert!(placement.cell.row >= region.cell.row);
                assert!(placement.right() <= region.right());
                assert!(placement.bottom() <= region.bottom());
            }
        }
    }

    #[test]
    fn panel_layout_rail_hugs_its_edge() {
        let (doc, members) = doc_with_members(12, 8, 4);
        let panel = panel_over(&members, Placement::new(Cell::new(0, 0), Span::new(4, 4)));
        for (edge, on_edge) in [
            (
                Edge::Left,
                Box::new(|p: Placement| p.cell.col == 0) as Box<dyn Fn(Placement) -> bool>,
            ),
            (
                Edge::Right,
                Box::new(move |p: Placement| p.right() <= 12 && p.cell.col >= 11),
            ),
            (Edge::Top, Box::new(|p: Placement| p.cell.row == 0)),
            (
                Edge::Bottom,
                Box::new(move |p: Placement| p.bottom() <= 8 && p.cell.row >= 7),
            ),
        ] {
            let set = arrangement_layout(&doc, &panel, &Arrangement::Rail(edge));
            assert_bounded_and_disjoint(&doc, &set);
            for (id, located) in &set {
                let placement = located
                    .expect("a 1x1-floor member always fits a rail")
                    .placement;
                assert!(
                    on_edge(placement),
                    "{id:?} off the {edge:?} rail: {placement:?}"
                );
            }
        }
    }

    #[test]
    fn panel_layout_cushion_area_follows_the_declared_measure() {
        let mut doc = FieldDocument::new("test-output", 12, 8);
        for (id, preferred) in [(1u64, (6, 6)), (2, (2, 2))] {
            let op = FieldOp::AddTile {
                tile: tile(id, (1, 1), preferred),
                at: None,
                container: None,
            };
            doc = apply(&doc, &op).expect("seed tile").0;
        }
        let members = [TileId(1), TileId(2)];
        let panel = panel_over(&members, Placement::new(Cell::new(0, 0), Span::new(12, 8)));
        let set = arrangement_layout(&doc, &panel, &Arrangement::Cushion);
        assert_bounded_and_disjoint(&doc, &set);
        let area = |id: TileId| -> u32 {
            let placement = set
                .iter()
                .find(|(member, _)| *member == id)
                .and_then(|(_, located)| *located)
                .expect("placed")
                .placement;
            u32::from(placement.span.cols) * u32::from(placement.span.rows)
        };
        assert!(
            area(TileId(1)) > area(TileId(2)),
            "the heavier measure gets the larger cushion"
        );
    }

    #[test]
    fn panel_layout_ops_change_grouping_and_layout_but_never_member_identity() {
        let (doc, members) = doc_with_members(12, 8, 4);
        let panel = panel_over(&members, Placement::new(Cell::new(0, 0), Span::new(6, 6)));
        let (doc, _) = apply(
            &doc,
            &FieldOp::CreatePanel {
                panel: panel.clone(),
            },
        )
        .expect("create panel");
        let tiles_before = doc.tiles.clone();

        let mut moved = panel.clone();
        moved.region = Placement::new(Cell::new(4, 2), Span::new(8, 6));
        moved.title = "Moved".to_owned();
        let (doc, _) =
            apply(&doc, &FieldOp::ConfigurePanel { panel: moved }).expect("configure panel");
        let (doc, _) = apply(
            &doc,
            &FieldOp::ArrangePanel {
                id: panel.id,
                arrangement: Arrangement::Balanced(Orientation::Rows),
            },
        )
        .expect("arrange panel");
        assert_eq!(
            doc.tiles, tiles_before,
            "panel ops arrange members, never change them"
        );
        for member in &members {
            let placement = doc.location(*member).expect("still placed").placement;
            assert!(placement.cell.col >= 4 && placement.cell.row >= 2);
        }

        let (doc, _) = apply(&doc, &FieldOp::RemovePanel { id: panel.id }).expect("remove panel");
        assert_eq!(
            doc.tiles, tiles_before,
            "removing the group keeps the tiles"
        );
        assert!(doc.panel(panel.id).is_none());
    }

    #[test]
    #[ignore = "exhaustive sweep across output shapes; run with --include-ignored"]
    fn panel_layout_sweep_is_deterministic_bounded_and_disjoint_across_shapes() {
        for cols in [4u16, 7, 12, 21, 32, 64] {
            for rows in [4u16, 6, 9, 16, 40] {
                for n in 1..=9u64 {
                    let (doc, members) = doc_with_members(cols, rows, n);
                    let region = Placement::new(
                        Cell::new(cols / 4, rows / 4),
                        Span::new((cols - cols / 4).max(1), (rows - rows / 4).max(1)),
                    );
                    let panel = panel_over(&members, region);
                    for arrangement in ARRANGEMENTS {
                        let first = arrangement_layout(&doc, &panel, &arrangement);
                        let second = arrangement_layout(&doc, &panel, &arrangement);
                        assert_eq!(first, second);
                        assert_bounded_and_disjoint(&doc, &first);
                    }
                }
            }
        }
    }

    #[test]
    fn panel_presets_switch_from_the_baseline_and_restore_custom_exactly() {
        let (doc, members) = doc_with_members(12, 8, 4);
        let panel = panel_over(&members, Placement::new(Cell::new(0, 0), Span::new(12, 8)));
        let (doc, _) = apply(
            &doc,
            &FieldOp::CreatePanel {
                panel: panel.clone(),
            },
        )
        .expect("create panel");
        let custom: Vec<(TileId, Option<Located>)> = members
            .iter()
            .map(|member| (*member, doc.location(*member)))
            .collect();

        // First preset: one operation, and the custom layout becomes the retained baseline.
        let (doc, _) = apply(
            &doc,
            &FieldOp::ArrangePanel {
                id: panel.id,
                arrangement: Arrangement::Balanced(Orientation::Rows),
            },
        )
        .expect("first preset");
        assert_eq!(
            doc.panel(panel.id).expect("panel").baseline,
            Some(custom.clone()),
            "the first preset retains the custom layout"
        );

        // Second preset: still one operation, baseline untouched.
        let (doc, _) = apply(
            &doc,
            &FieldOp::ArrangePanel {
                id: panel.id,
                arrangement: Arrangement::Cushion,
            },
        )
        .expect("second preset");
        assert_eq!(
            doc.panel(panel.id).expect("panel").baseline,
            Some(custom.clone()),
            "switching presets preserves the saved custom arrangement"
        );

        // Restore Custom: every member back exactly, and the baseline is spent.
        let (doc, _) = apply(
            &doc,
            &FieldOp::ArrangePanel {
                id: panel.id,
                arrangement: Arrangement::Custom,
            },
        )
        .expect("restore custom");
        for (member, location) in &custom {
            assert_eq!(&doc.location(*member), location, "exact prior placement");
        }
        let restored = doc.panel(panel.id).expect("panel");
        assert_eq!(restored.arrangement, Arrangement::Custom);
        assert_eq!(restored.baseline, None);
    }

    #[test]
    fn panel_presets_are_exactly_invertible() {
        let (doc, members) = doc_with_members(12, 8, 5);
        let panel = panel_over(&members, Placement::new(Cell::new(1, 1), Span::new(10, 6)));
        let (doc, _) = apply(
            &doc,
            &FieldOp::CreatePanel {
                panel: panel.clone(),
            },
        )
        .expect("create panel");
        for arrangement in ARRANGEMENTS {
            let op = FieldOp::ArrangePanel {
                id: panel.id,
                arrangement,
            };
            let (next, inverse) = apply(&doc, &op).expect("arrange");
            let (back, _) = apply(&next, &inverse).expect("undo");
            assert_eq!(back, doc, "apply(inverse) restores the document exactly");
        }
    }
}
