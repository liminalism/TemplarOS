use super::*;
use proptest::prelude::*;
use std::collections::HashMap;

const ALL: [LayoutKind; 9] = [
    LayoutKind::Strip,
    LayoutKind::Squarified,
    LayoutKind::Voronoi,
    LayoutKind::CirclePack,
    LayoutKind::Sunburst,
    LayoutKind::Icicle,
    LayoutKind::Branches,
    LayoutKind::Fractal,
    LayoutKind::Folio,
];

/// A generated tree shape: weight and children.
#[derive(Clone, Debug)]
struct T(f32, Vec<T>);

fn build(shape: &[T], prefix: &str) -> Vec<WeightedNode> {
    shape
        .iter()
        .enumerate()
        .map(|(i, T(weight, kids))| {
            let path = format!("{prefix}/n{i}");
            WeightedNode {
                id: node_id(&path),
                name: format!("n{i}"),
                weight: *weight,
                is_dir: !kids.is_empty(),
                children: build(kids, &path),
            }
        })
        .collect()
}

fn flat(weights: &[f32]) -> Vec<WeightedNode> {
    build(
        &weights
            .iter()
            .map(|&w| T(w, Vec::new()))
            .collect::<Vec<_>>(),
        "",
    )
}

fn arb_forest() -> impl Strategy<Value = Vec<WeightedNode>> {
    let leaf = (0.5_f32..10.0).prop_map(|w| T(w, Vec::new()));
    let node = leaf.prop_recursive(3, 48, 5, |inner| {
        (0.5_f32..10.0, prop::collection::vec(inner, 1..5)).prop_map(|(w, kids)| T(w, kids))
    });
    prop::collection::vec(node, 1..7).prop_map(|shape| build(&shape, ""))
}

fn arb_bounds() -> impl Strategy<Value = Bounds> {
    (
        -50.0_f32..50.0,
        -50.0_f32..50.0,
        40.0_f32..600.0,
        40.0_f32..600.0,
    )
        .prop_map(|(x, y, w, h)| Bounds { x, y, w, h })
}

fn arb_kind() -> impl Strategy<Value = LayoutKind> {
    prop::sample::select(ALL.to_vec())
}

fn request(kind: LayoutKind, roots: &[WeightedNode], bounds: Bounds) -> LayoutRequest<'_> {
    LayoutRequest {
        kind,
        roots,
        bounds,
        seed: 0x5eed,
        max_depth: 8,
        max_nodes: 256,
        previous: None,
    }
}

type Strokes<'a> = HashMap<u64, (&'a Item, &'a Vec<f32>)>;

fn pipe_close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 2e-3 * a.max(b) + 1e-6
}

/// Leonardo's rule below `node`: own start width² = tip width² + Σ children's start width².
fn pipe_rule_holds(node: &WeightedNode, strokes: &Strokes<'_>) -> Result<(), String> {
    let (item, widths) = strokes[&node.id];
    if node.children.is_empty() {
        return Ok(());
    }
    // Root strokes start at the trunk base; their own branch starts at the second vertex.
    let own = widths[usize::from(item.depth == 0)];
    let tip = widths[widths.len() - 1];
    let children: f32 = node
        .children
        .iter()
        .map(|c| strokes[&c.id].1[0].powi(2))
        .sum();
    if !pipe_close(own * own, tip * tip + children) {
        return Err(format!("{} vs {} + {}", own * own, tip * tip, children));
    }
    node.children
        .iter()
        .try_for_each(|child| pipe_rule_holds(child, strokes))
}

fn leaves<'a>(nodes: &'a [WeightedNode], out: &mut Vec<&'a WeightedNode>) {
    for n in nodes {
        if n.children.is_empty() {
            out.push(n);
        } else {
            leaves(&n.children, out);
        }
    }
}

const EPS: f32 = 1e-3;

fn inside(b: Bounds, (x, y): (f32, f32), pad: f32) -> bool {
    let e = EPS * (1.0 + b.w.max(b.h));
    x - pad >= b.x - e
        && x + pad <= b.right() + e
        && y - pad >= b.y - e
        && y + pad <= b.bottom() + e
}

fn item_inside(b: Bounds, item: &Item) -> bool {
    let shape_inside = match &item.shape {
        Shape::Rect { x, y, w, h, .. } => {
            *w >= 0.0 && *h >= 0.0 && inside(b, (*x, *y), 0.0) && inside(b, (x + w, y + h), 0.0)
        }
        Shape::Polygon { points } => points.iter().all(|&p| inside(b, p, 0.0)),
        Shape::Circle { cx, cy, r } => *r >= 0.0 && inside(b, (*cx, *cy), *r),
        Shape::Sector {
            cx,
            cy,
            r0,
            r1,
            a0,
            a1,
        } => {
            a0 < a1
                && r0 <= r1
                && (0..=32).all(|k| {
                    let a = a0 + (a1 - a0) * k as f32 / 32.0;
                    [*r0, *r1]
                        .iter()
                        .all(|&r| inside(b, (cx + r * a.cos(), cy + r * a.sin()), 0.0))
                })
        }
        Shape::Stroke { points, widths } => {
            points.len() == widths.len()
                && points
                    .iter()
                    .zip(widths)
                    .all(|(&p, &w)| w >= 0.0 && inside(b, p, w / 2.0))
        }
    };
    let label_inside = item
        .label
        .is_none_or(|(x, y, w)| w > 0.0 && inside(b, (x, y), 0.0) && inside(b, (x + w, y), 0.0));
    shape_inside && label_inside
}

fn rect_of(item: &Item) -> (f32, f32, f32, f32) {
    match item.shape {
        Shape::Rect { x, y, w, h, .. } => (x, y, w, h),
        _ => panic!("not a rect"),
    }
}

fn polygon_area(item: &Item) -> f64 {
    match &item.shape {
        Shape::Polygon { points } => geometry::area(
            &points
                .iter()
                .map(|&(x, y)| (f64::from(x), f64::from(y)))
                .collect::<Vec<_>>(),
        ),
        Shape::Rect { w, h, .. } => f64::from(*w) * f64::from(*h),
        _ => panic!("no area"),
    }
}

fn polygon_centroid(item: &Item) -> (f64, f64) {
    match &item.shape {
        Shape::Polygon { points } => geometry::centroid(
            &points
                .iter()
                .map(|&(x, y)| (f64::from(x), f64::from(y)))
                .collect::<Vec<_>>(),
        ),
        _ => panic!("not a polygon"),
    }
}

/// Every sibling group of the input (roots included), as `(ids, weights)`.
fn sibling_groups(roots: &[WeightedNode]) -> Vec<Vec<(u64, f32)>> {
    fn walk(nodes: &[WeightedNode], out: &mut Vec<Vec<(u64, f32)>>) {
        out.push(nodes.iter().map(|n| (n.id, n.weight)).collect());
        for n in nodes {
            if !n.children.is_empty() {
                walk(&n.children, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(roots, &mut out);
    out
}

/// Each group's areas are its weight shares within `tolerance` (relative to the share).
fn shares_hold(
    layout: &GardenLayout,
    roots: &[WeightedNode],
    tolerance: f64,
) -> Result<(), String> {
    let by_id: HashMap<u64, &Item> = layout.items.iter().map(|i| (i.node, i)).collect();
    for group in sibling_groups(roots) {
        let Some(items) = group
            .iter()
            .map(|(id, _)| by_id.get(id).copied())
            .collect::<Option<Vec<_>>>()
        else {
            continue; // not laid out (no room in the parent)
        };
        let total_area: f64 = items.iter().map(|i| polygon_area(i)).sum();
        let total_weight: f64 = group.iter().map(|&(_, w)| f64::from(w)).sum();
        if total_area < 1.0 {
            continue;
        }
        for (item, &(_, w)) in items.iter().zip(&group) {
            let share = f64::from(w) / total_weight;
            let got = polygon_area(item) / total_area;
            if (got - share).abs() > tolerance * share + 1e-4 {
                return Err(format!("share {got} vs weight share {share}"));
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn every_item_lies_within_bounds(
        roots in arb_forest(),
        bounds in arb_bounds(),
        kind in arb_kind(),
        max_depth in 0_u8..4,
    ) {
        let req = LayoutRequest { max_depth, ..request(kind, &roots, bounds) };
        let out = layout(&req);
        prop_assert!(!out.items.is_empty());
        for item in &out.items {
            prop_assert!(item_inside(bounds, item), "{kind:?}: {item:?} outside {bounds:?}");
            prop_assert!(item.depth <= max_depth);
        }
    }

    #[test]
    fn same_request_same_layout(
        roots in arb_forest(),
        bounds in arb_bounds(),
        kind in arb_kind(),
        seed in any::<u64>(),
    ) {
        let first = layout(&LayoutRequest { seed, ..request(kind, &roots, bounds) });
        let again = layout(&LayoutRequest { seed, ..request(kind, &roots, bounds) });
        prop_assert_eq!(&first, &again);
        let with_previous = LayoutRequest { seed, previous: Some(&first), ..request(kind, &roots, bounds) };
        prop_assert_eq!(layout(&with_previous), layout(&with_previous));
    }

    /// Siblings keep their reading order whatever the weights: in id order, each later sibling
    /// lies entirely right of or entirely below every earlier one.
    #[test]
    fn rect_treemaps_keep_sibling_order_under_reweighting(
        before in prop::collection::vec(0.1_f32..20.0, 2..14),
        factors in prop::collection::vec(0.2_f32..5.0, 14),
        bounds in arb_bounds(),
    ) {
        let after: Vec<f32> = before.iter().zip(&factors).map(|(w, f)| w * f).collect();
        for kind in [LayoutKind::Strip, LayoutKind::Squarified] {
            for weights in [&before, &after] {
                let roots = flat(weights);
                let mut items = layout(&request(kind, &roots, bounds)).items;
                prop_assert_eq!(items.len(), roots.len());
                items.sort_by_key(|i| i.node);
                for (i, a) in items.iter().enumerate() {
                    let (ax, ay, aw, ah) = rect_of(a);
                    for b in &items[i + 1..] {
                        let (bx, by, _, _) = rect_of(b);
                        prop_assert!(
                            bx >= ax + aw - 1e-2 || by >= ay + ah - 1e-2,
                            "{kind:?}: later sibling left of and above an earlier one"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rect_treemap_areas_follow_weights(roots in arb_forest(), bounds in arb_bounds()) {
        for kind in [LayoutKind::Strip, LayoutKind::Squarified] {
            let out = layout(&request(kind, &roots, bounds));
            prop_assert!(shares_hold(&out, &roots, 1e-3).is_ok(), "{kind:?}: {:?}", shares_hold(&out, &roots, 1e-3));
        }
    }

    #[test]
    fn voronoi_areas_follow_weights(
        weights in prop::collection::vec(1.0_f32..8.0, 2..10),
        bounds in (100.0_f32..600.0, 100.0_f32..600.0).prop_map(|(w, h)| Bounds { x: 0.0, y: 0.0, w, h }),
        seed in any::<u64>(),
    ) {
        let roots = flat(&weights);
        let out = layout(&LayoutRequest { seed, ..request(LayoutKind::Voronoi, &roots, bounds) });
        prop_assert_eq!(out.items.len(), roots.len());
        let checked = shares_hold(&out, &roots, 0.15);
        prop_assert!(checked.is_ok(), "{:?}", checked);
    }

    /// Leonardo's rule at every fork: a branch's own start width squared is its tip's plus its
    /// children's start widths squared; the shared trunk carries every root.
    #[test]
    fn branch_widths_follow_the_pipe_model(roots in arb_forest(), bounds in arb_bounds(), seed in any::<u64>()) {
        let out = layout(&LayoutRequest { seed, ..request(LayoutKind::Branches, &roots, bounds) });
        let strokes: Strokes<'_> = out
            .items
            .iter()
            .map(|item| match &item.shape {
                Shape::Stroke { widths, .. } => (item.node, (item, widths)),
                other => panic!("not a stroke: {other:?}"),
            })
            .collect();

        let trunk = strokes[&roots[0].id].1[0];
        let roots_sq: f32 = roots.iter().map(|r| strokes[&r.id].1[1].powi(2)).sum();
        prop_assert!(pipe_close(trunk * trunk, roots_sq), "trunk {} vs roots {}", trunk * trunk, roots_sq);
        for root in &roots {
            let checked = pipe_rule_holds(root, &strokes);
            prop_assert!(checked.is_ok(), "{:?}", checked);
        }
    }

    /// The budget keeps at most `max_nodes` nodes and drops leaves weakest first: no dropped
    /// input leaf outweighs a kept one.
    #[test]
    fn budget_drops_weakest_leaves_first(roots in arb_forest(), max_nodes in 0_usize..24, kind in arb_kind()) {
        let forest = Forest::prepare(&roots, 8, max_nodes);
        prop_assert!(forest.nodes.len() <= max_nodes);
        let kept: std::collections::HashSet<u64> = forest.nodes.iter().map(|n| n.id).collect();
        let mut all = Vec::new();
        leaves(&roots, &mut all);
        let strongest_dropped = all.iter().filter(|n| !kept.contains(&n.id)).map(|n| n.weight).fold(f32::NEG_INFINITY, f32::max);
        let weakest_kept = all.iter().filter(|n| kept.contains(&n.id)).map(|n| n.weight).fold(f32::INFINITY, f32::min);
        prop_assert!(strongest_dropped <= weakest_kept);

        let bounds = Bounds { x: 0.0, y: 0.0, w: 320.0, h: 240.0 };
        let out = layout(&LayoutRequest { max_nodes, ..request(kind, &roots, bounds) });
        prop_assert!(out.items.len() <= max_nodes);
    }
}

#[test]
fn empty_and_weightless_inputs_lay_out_nothing() {
    let bounds = Bounds {
        x: 0.0,
        y: 0.0,
        w: 300.0,
        h: 200.0,
    };
    let weightless = build(
        &[
            T(0.0, vec![T(4.0, Vec::new())]),
            T(-1.0, Vec::new()),
            T(f32::NAN, Vec::new()),
        ],
        "",
    );
    let some = flat(&[1.0, 2.0]);
    for kind in ALL {
        assert!(layout(&request(kind, &[], bounds)).items.is_empty());
        assert!(layout(&request(kind, &weightless, bounds)).items.is_empty());
        let no_room = Bounds { w: 0.0, ..bounds };
        assert!(layout(&request(kind, &some, no_room)).items.is_empty());
    }
}

#[test]
fn zero_weight_nodes_are_skipped_with_their_subtree() {
    let roots = build(
        &[
            T(
                3.0,
                vec![T(0.0, vec![T(2.0, Vec::new())]), T(1.0, Vec::new())],
            ),
            T(0.0, Vec::new()),
        ],
        "",
    );
    let kept = [node_id("/n0"), node_id("/n0/n1")];
    let bounds = Bounds {
        x: 0.0,
        y: 0.0,
        w: 300.0,
        h: 200.0,
    };
    for kind in ALL {
        let mut ids: Vec<u64> = layout(&request(kind, &roots, bounds))
            .items
            .iter()
            .map(|i| i.node)
            .collect();
        ids.sort_unstable();
        let mut expected = kept.to_vec();
        expected.sort_unstable();
        assert_eq!(ids, expected, "{kind:?}");
    }
}

/// Re-laying out from the previous layout keeps Voronoi cells nearer their old places than a
/// fresh layout of the same reweighted tree does: never notably worse on any tree, and less
/// movement overall. (A fresh layout re-seeds from the same ids, so on some trees the two tie.)
#[test]
fn voronoi_previous_layout_damps_movement() {
    let bounds = Bounds {
        x: 0.0,
        y: 0.0,
        w: 480.0,
        h: 320.0,
    };
    let (mut total_with, mut total_without) = (0.0, 0.0);
    for case in 0..40_u64 {
        let mut rng = super::Rng::new(case);
        let n = 5 + (case % 5) as usize;
        let before: Vec<f32> = (0..n).map(|_| rng.range(1.0, 8.0) as f32).collect();
        let after: Vec<f32> = before
            .iter()
            .map(|w| w * rng.range(0.7, 1.3) as f32)
            .collect();
        let (old_roots, new_roots) = (flat(&before), flat(&after));
        let req = |roots| LayoutRequest {
            seed: case,
            ..request(LayoutKind::Voronoi, roots, bounds)
        };
        let old = layout(&req(&old_roots));
        let fresh = layout(&req(&new_roots));
        let kept = layout(&LayoutRequest {
            previous: Some(&old),
            ..req(&new_roots)
        });
        let movement = |next: &GardenLayout| -> f64 {
            old.items
                .iter()
                .zip(&next.items)
                .map(|(a, b)| {
                    assert_eq!(a.node, b.node);
                    let (p, q) = (polygon_centroid(a), polygon_centroid(b));
                    (p.0 - q.0).hypot(p.1 - q.1)
                })
                .sum()
        };
        let (with, without) = (movement(&kept), movement(&fresh));
        assert!(
            with <= without * 1.1,
            "case {case}: moved {with} with previous, {without} without"
        );
        total_with += with;
        total_without += without;
    }
    assert!(
        total_with < total_without * 0.8,
        "moved {total_with} with previous, {total_without} without"
    );
}

#[test]
#[ignore = "timing report, run with --ignored --nocapture"]
fn layout_time_for_96_nodes() {
    let shape: Vec<T> = (0..8)
        .map(|i| {
            T(
                1.0 + i as f32,
                (0..11)
                    .map(|j| T(0.5 + ((i * 7 + j * 3) % 10) as f32, Vec::new()))
                    .collect(),
            )
        })
        .collect();
    let roots = build(&shape, "");
    let bounds = Bounds {
        x: 0.0,
        y: 0.0,
        w: 640.0,
        h: 420.0,
    };
    for kind in ALL {
        let start = std::time::Instant::now();
        let rounds = 20;
        let mut items = 0;
        for _ in 0..rounds {
            items = layout(&request(kind, &roots, bounds)).items.len();
        }
        println!(
            "{kind:?}: {items} items, {:.3} ms/layout",
            start.elapsed().as_secs_f64() * 1e3 / f64::from(rounds)
        );
    }
}
