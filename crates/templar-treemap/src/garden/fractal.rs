//! A weighted Pythagoras tree read as a binary L-system: every branch splits into its children,
//! each turned from the parent's heading by its weight's place in the sibling order (heavier
//! children turn less, as the larger square of a Pythagoras tree does) and shortened by the
//! square root of its weight share, so squared lengths add up like the tree's squares. Angles,
//! lengths and a slight bend carry deterministic jitter from `seed ^ node.id`.

use super::{Ctx, Item, LABEL_LINE, Shape, place_tip_labels};

/// Total turn across a sibling fan, radians.
const SPREAD: f64 = 2.2;
/// Root length relative to the trunk; child length relative to its parent, before weighting.
const ROOT_RATIO: f64 = 1.6;
const CHILD_RATIO: f64 = 0.95;
/// Stroke width as a share of branch length, and the taper along a branch.
const WIDTH_RATIO: f64 = 0.12;
const TAPER: f64 = 0.7;
/// Jitter amplitudes: turn (radians), length share and bend share.
const JITTER_TURN: f64 = 0.08;
const JITTER_LENGTH: f64 = 0.08;
const JITTER_BEND: f64 = 0.05;
/// Most the crown may be stretched along one axis to fill a Place of another aspect.
const MAX_STRETCH: f64 = 1.5;

type P = (f64, f64);

struct Branch {
    index: usize,
    points: Vec<P>,
    widths: Vec<f64>,
}

pub(super) fn layout(ctx: &Ctx<'_>) -> Vec<Item> {
    // Grow in unit space: trunk from the origin, straight up, length 1 (the crown's scale).
    let trunk_top = (0.0, -1.0);
    let trunk_width = WIDTH_RATIO * ROOT_RATIO;
    let mut branches = Vec::with_capacity(ctx.forest.nodes.len());
    split(
        ctx,
        &ctx.forest.roots,
        trunk_top,
        -std::f64::consts::FRAC_PI_2,
        ROOT_RATIO,
        Some(trunk_width),
        &mut branches,
    );

    // Fit the stroked extent into the bounds, standing on the bottom edge, stretching one axis
    // a little when the Place's aspect differs from the crown's.
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for branch in &branches {
        for (&(x, y), &w) in branch.points.iter().zip(&branch.widths) {
            x0 = x0.min(x - w / 2.0);
            y0 = y0.min(y - w / 2.0);
            x1 = x1.max(x + w / 2.0);
            y1 = y1.max(y + w / 2.0);
        }
    }
    let b = ctx.bounds;
    let (bw, bh) = (f64::from(b.w), f64::from(b.h));
    // Keep a line free above the crown so the topmost tips have somewhere to put their names.
    let line = f64::from(LABEL_LINE);
    let crown_h = if bh > 4.0 * line { bh - line } else { bh };
    let shrink = 1.0 - 1e-5;
    let (fit_x, fit_y) = (bw / (x1 - x0) * shrink, crown_h / (y1 - y0) * shrink);
    let uniform = fit_x.min(fit_y);
    let (sx, sy) = (
        fit_x.min(uniform * MAX_STRETCH),
        fit_y.min(uniform * MAX_STRETCH),
    );
    let ox = f64::from(b.x) + (bw - (x1 - x0) * sx) / 2.0 - x0 * sx;
    let oy = f64::from(b.bottom()) - y1 * sy;
    let place = |(x, y): P| ((ox + x * sx) as f32, (oy + y * sy) as f32);

    let order: Vec<usize> = branches.iter().map(|branch| branch.index).collect();
    let mut items: Vec<Item> = branches
        .into_iter()
        .map(|branch| {
            ctx.item(
                branch.index,
                Shape::Stroke {
                    points: branch.points.iter().map(|&p| place(p)).collect(),
                    widths: branch
                        .widths
                        .iter()
                        .map(|&w| (w * uniform) as f32)
                        .collect(),
                },
                None,
            )
        })
        .collect();
    place_tip_labels(ctx, &mut items, &order);
    items
}

/// Split the branch ending at `from` (heading `heading`, length `length`) into `siblings`.
/// `trunk` is set for the roots, whose strokes start at the trunk base.
fn split(
    ctx: &Ctx<'_>,
    siblings: &[usize],
    from: P,
    heading: f64,
    length: f64,
    trunk: Option<f64>,
    out: &mut Vec<Branch>,
) {
    let total: f64 = siblings.iter().map(|&i| ctx.forest.weight(i)).sum();
    let mut before = 0.0;
    for &index in siblings {
        let weight = ctx.forest.weight(index);
        let share = weight / total;
        let middle = (before + share / 2.0) - 0.5;
        before += share;
        let mut rng = ctx.rng(ctx.forest.nodes[index].id);
        let turn = if siblings.len() == 1 {
            0.0
        } else {
            middle * SPREAD
        };
        let angle = heading + turn + rng.range(-JITTER_TURN, JITTER_TURN);
        let len = length * share.sqrt() * rng.range(1.0 - JITTER_LENGTH, 1.0 + JITTER_LENGTH);
        let (dx, dy) = (angle.cos(), angle.sin());
        let end = (from.0 + dx * len, from.1 + dy * len);
        let bend = rng.range(-JITTER_BEND, JITTER_BEND) * len;
        let mid = (
            from.0 + dx * len / 2.0 - dy * bend,
            from.1 + dy * len / 2.0 + dx * bend,
        );
        let width = WIDTH_RATIO * len;
        let (mut points, mut widths) = (
            vec![from, mid, end],
            vec![width, width * (1.0 + TAPER) / 2.0, width * TAPER],
        );
        if let Some(trunk_width) = trunk {
            points.insert(0, (0.0, 0.0));
            widths.insert(0, trunk_width);
        }
        out.push(Branch {
            index,
            points,
            widths,
        });
        split(
            ctx,
            &ctx.forest.nodes[index].children,
            end,
            angle,
            len * CHILD_RATIO,
            None,
            out,
        );
    }
}
