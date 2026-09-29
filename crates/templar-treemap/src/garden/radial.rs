//! Sunburst: the Place at the centre, one ring per depth, each node spanning its share of its
//! parent's angle in id order, clockwise from the top.

use super::{Ctx, Item, LABEL_LINE, Shape, centred_label};
use std::f64::consts::{FRAC_PI_2, TAU};

/// The empty centre (the Place itself) as a share of the outer radius.
const HOLE: f64 = 0.2;

pub(super) fn sunburst(ctx: &Ctx<'_>) -> Vec<Item> {
    let b = ctx.bounds;
    let ring = Ring {
        cx: f64::from(b.x) + f64::from(b.w) / 2.0,
        cy: f64::from(b.y) + f64::from(b.h) / 2.0,
        hole: f64::from(b.w.min(b.h)) / 2.0 * HOLE,
        thickness: f64::from(b.w.min(b.h)) / 2.0 * (1.0 - HOLE) / ctx.forest.levels() as f64,
    };
    let mut items = Vec::with_capacity(ctx.forest.nodes.len());
    ring.partition(ctx, &ctx.forest.roots, -FRAC_PI_2, TAU, &mut items);
    items
}

struct Ring {
    cx: f64,
    cy: f64,
    hole: f64,
    thickness: f64,
}

impl Ring {
    fn partition(
        &self,
        ctx: &Ctx<'_>,
        siblings: &[usize],
        start: f64,
        span: f64,
        items: &mut Vec<Item>,
    ) {
        let total: f64 = siblings.iter().map(|&i| ctx.forest.weight(i)).sum();
        let end = start + span;
        let mut a0 = start;
        for (k, &index) in siblings.iter().enumerate() {
            let a1 = if k + 1 == siblings.len() {
                end
            } else {
                (a0 + ctx.forest.weight(index) / total * span).min(end)
            };
            let depth = f64::from(ctx.forest.nodes[index].depth);
            let r0 = self.hole + depth * self.thickness;
            let r1 = r0 + self.thickness;
            items.push(ctx.item(
                index,
                Shape::Sector {
                    cx: self.cx as f32,
                    cy: self.cy as f32,
                    r0: r0 as f32,
                    r1: r1 as f32,
                    a0: a0 as f32,
                    a1: a1 as f32,
                },
                self.label(r0, r1, a0, a1),
            ));
            self.partition(ctx, &ctx.forest.nodes[index].children, a0, a1 - a0, items);
            a0 = a1;
        }
    }

    /// Horizontal text on the sector's mid-radius arc, as wide as the contiguous span of the
    /// horizontal line through the anchor that stays inside the sector; the ring must be at
    /// least a line thick. The anchor is the centroid-ish middle (mid-angle) unless another
    /// point along the arc gives a clearly wider span — on the sides of the ring the line
    /// through the middle only crosses the ring's thickness, while near the sector's top or
    /// bottom it runs along the arc.
    fn label(&self, r0: f64, r1: f64, a0: f64, a1: f64) -> Option<(f32, f32, f32)> {
        const ANCHORS: usize = 9;
        if r1 - r0 < f64::from(LABEL_LINE) {
            return None;
        }
        let rm = f64::midpoint(r0, r1);
        let step = (r1 / 400.0).max(0.25);
        let span = |angle: f64| {
            let (px, py) = (self.cx + rm * angle.cos(), self.cy + rm * angle.sin());
            let reach = |dir: f64| {
                let mut s = 0.0;
                while s < 2.0 * r1 && self.contains(px + dir * (s + step), py, r0, r1, a0, a1) {
                    s += step;
                }
                s
            };
            let (left, right) = (reach(-1.0), reach(1.0));
            (px - left, py, left + right)
        };
        // Candidates from the middle outwards, so the middle wins ties.
        let mut offsets: Vec<f64> = (0..ANCHORS)
            .map(|i| (i as f64 + 0.5) / ANCHORS as f64 - 0.5)
            .collect();
        offsets.sort_by(|p, q| p.abs().total_cmp(&q.abs()));
        let (left, y, width) = offsets
            .iter()
            .map(|t| span(f64::midpoint(a0, a1) + t * (a1 - a0)))
            .fold(
                None,
                |best: Option<(f64, f64, f64)>, candidate| match best {
                    Some(best) if best.2 + 1.0 >= candidate.2 => Some(best),
                    _ => Some(candidate),
                },
            )?;
        centred_label((left + width / 2.0) as f32, y as f32, width as f32)
    }

    fn contains(&self, x: f64, y: f64, r0: f64, r1: f64, a0: f64, a1: f64) -> bool {
        let (dx, dy) = (x - self.cx, y - self.cy);
        let r = dx.hypot(dy);
        if r < r0 || r > r1 {
            return false;
        }
        // Bring the angle into [a0, a0 + 2π).
        let a = (dy.atan2(dx) - a0).rem_euclid(TAU) + a0;
        a <= a1
    }
}
