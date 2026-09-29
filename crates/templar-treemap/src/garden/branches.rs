//! Space-colonization branches (Runions, Lane & Prusinkiewicz, *Modeling Trees with a Space
//! Colonization Algorithm*, 2007), with the Place's tree as the topology: every node owns one
//! branch whose tip is steered, step by step, towards the attractors of its own cloud. Clouds
//! sit in angular sectors ∝ weight (in id order) and in one radial band per depth; leaves reach
//! the rim. Widths follow the pipe model (Leonardo's rule, `w_parent² = Σ w_child²`): a branch
//! carries the squared widths of every child still to leave it plus its own tip.
//!
//! All root branches start at the trunk base and run up the shared trunk at the Place's full
//! pipe width, splitting at the trunk top.

use super::{Bounds, Ctx, Item, LABEL_LINE, Rng, Shape, TIP_LABEL_MAX, place_tip_labels};

/// Half-opening of the fan, radians from the growth axis.
const FAN: f64 = 1.1;
/// Trunk length as a share of the growth axis.
const TRUNK: f64 = 0.2;
/// Growth step as a share of the fan's reach.
const STEP: f64 = 0.03;
/// Attractors within this many steps steer the tip; within `KILL` steps they are consumed.
const INFLUENCE: f64 = 4.0;
const KILL: f64 = 1.6;
/// Weight of the previous direction in each growth step.
const INERTIA: f64 = 1.0;
const MAX_STEPS: usize = 64;
/// Every branch grows at least this many steps, so children never leave from its tip.
const MIN_STEPS: usize = 2;
/// Leaves taper to this share of their start width.
const LEAF_TAPER: f64 = 0.4;
/// Smallest leaf pipe as a share of the heaviest leaf's, so faint leaves stay visible.
const LEAF_FLOOR: f64 = 0.02;

type P = (f64, f64);

pub(super) fn layout(ctx: &Ctx<'_>) -> Vec<Item> {
    let forest = ctx.forest;
    let frame = Frame::new(ctx.bounds);

    // Pipe cross-sections (squared widths) in weight units, bottom-up.
    let max_weight = forest
        .nodes
        .iter()
        .map(|n| f64::from(n.weight))
        .fold(0.0, f64::max);
    let mut pipe = vec![0.0; forest.nodes.len()];
    let mut tip = vec![0.0; forest.nodes.len()];
    for index in (0..forest.nodes.len()).rev() {
        let node = &forest.nodes[index];
        let weight = f64::from(node.weight);
        let children: f64 = node.children.iter().map(|&c| pipe[c]).sum();
        let child_weight: f64 = node.children.iter().map(|&c| forest.weight(c)).sum();
        tip[index] = if node.children.is_empty() {
            weight.max(max_weight * LEAF_FLOOR)
        } else {
            (weight - child_weight).max(weight * LEAF_FLOOR)
        };
        pipe[index] = tip[index] + children;
    }
    let total: f64 = forest.roots.iter().map(|&r| pipe[r]).sum();
    let k = frame.trunk_width / total.sqrt();

    let mut grower = Grower {
        ctx,
        frame: &frame,
        levels: forest.levels() as f64,
        chains: vec![Vec::new(); forest.nodes.len()],
        attach: vec![0; forest.nodes.len()],
    };
    let trunk_top = frame.at(0.0, TRUNK + ctx.rng(0).range(-0.03, 0.03));
    grower.grow_siblings(&forest.roots, None, -FAN, FAN, trunk_top);

    let mut items = Vec::with_capacity(forest.nodes.len());
    for (index, node) in forest.nodes.iter().enumerate() {
        let chain = &grower.chains[index];
        let root = node.depth == 0;
        let own = usize::from(root);
        let mut widths = vec![0.0_f64; chain.len()];
        if root {
            widths[0] = frame.trunk_width;
        }
        if node.children.is_empty() {
            let start = k * pipe[index].sqrt();
            let span = (chain.len() - 1 - own).max(1) as f64;
            for (v, width) in widths.iter_mut().enumerate().skip(own) {
                *width = start * (1.0 - (1.0 - LEAF_TAPER) * (v - own) as f64 / span);
            }
        } else {
            for (v, width) in widths.iter_mut().enumerate().skip(own) {
                let carried: f64 = node
                    .children
                    .iter()
                    .filter(|&&c| grower.attach[c] >= v)
                    .map(|&c| pipe[c])
                    .sum();
                *width = k * (tip[index] + carried).sqrt();
            }
        }
        items.push(ctx.item(
            index,
            Shape::Stroke {
                points: chain.iter().map(|&(x, y)| (x as f32, y as f32)).collect(),
                widths: widths.iter().map(|&w| w as f32).collect(),
            },
            None,
        ));
    }
    let order: Vec<usize> = (0..items.len()).collect();
    place_tip_labels(ctx, &mut items, &order);
    items
}

/// The fan's polar frame: angle from the growth axis, radius as a share of the reach.
struct Frame {
    base: P,
    /// Growth axis and the side axis (unit vectors).
    up: P,
    side: P,
    reach_up: f64,
    reach_side: f64,
    /// Points stay within `[x0, x1] × [y0, y1]`, a trunk half-width inside the bounds.
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    trunk_width: f64,
    /// Radius share left free beyond the leaves for their labels.
    label_room: f64,
}

impl Frame {
    fn new(bounds: Bounds) -> Self {
        let (bx, by, bw, bh) = (
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.w),
            f64::from(bounds.h),
        );
        let short = bw.min(bh);
        let trunk_width = (short * 0.05).min(14.0);
        let margin = trunk_width / 2.0 + (short * 0.05).min(1.0);
        let (x0, y0, x1, y1) = (bx + margin, by + margin, bx + bw - margin, by + bh - margin);
        let (iw, ih) = (x1 - x0, y1 - y0);
        let sideways = bw > 1.6 * bh;
        let (base, up, side, reach_up, reach_side) = if sideways {
            ((x0, y0 + ih / 2.0), (1.0, 0.0), (0.0, 1.0), iw, ih / 2.0)
        } else {
            ((x0 + iw / 2.0, y1), (0.0, -1.0), (1.0, 0.0), ih, iw / 2.0)
        };
        let label_room = if sideways {
            (f64::from(TIP_LABEL_MAX) / reach_up).min(0.3)
        } else {
            (1.2 * f64::from(LABEL_LINE) / reach_up).min(0.2)
        };
        Self {
            base,
            up,
            side,
            reach_up,
            reach_side,
            x0,
            y0,
            x1,
            y1,
            trunk_width,
            label_room,
        }
    }

    fn at(&self, angle: f64, radius: f64) -> P {
        let u = angle.cos() * radius * self.reach_up;
        let s = angle.sin() * radius * self.reach_side;
        self.clamp((
            self.base.0 + u * self.up.0 + s * self.side.0,
            self.base.1 + u * self.up.1 + s * self.side.1,
        ))
    }

    fn angle_of(&self, p: P) -> f64 {
        let (dx, dy) = (p.0 - self.base.0, p.1 - self.base.1);
        let u = (dx * self.up.0 + dy * self.up.1) / self.reach_up;
        let s = (dx * self.side.0 + dy * self.side.1) / self.reach_side;
        s.atan2(u)
    }

    fn clamp(&self, (x, y): P) -> P {
        (x.clamp(self.x0, self.x1), y.clamp(self.y0, self.y1))
    }

    fn step(&self) -> f64 {
        STEP * self.reach_up.max(self.reach_side)
    }
}

struct Grower<'a> {
    ctx: &'a Ctx<'a>,
    frame: &'a Frame,
    levels: f64,
    chains: Vec<Vec<P>>,
    /// Vertex of the parent's chain each branch leaves from.
    attach: Vec<usize>,
}

impl Grower<'_> {
    /// Grow `siblings` in the sector `[a0, a1]`, leaving from `parent`'s chain (or, for the
    /// roots, from the trunk top).
    fn grow_siblings(
        &mut self,
        siblings: &[usize],
        parent: Option<usize>,
        a0: f64,
        a1: f64,
        trunk_top: P,
    ) {
        let forest = self.ctx.forest;
        let total: f64 = siblings.iter().map(|&i| forest.weight(i)).sum();
        let mut start = a0;
        for (k, &index) in siblings.iter().enumerate() {
            let end = if k + 1 == siblings.len() {
                a1
            } else {
                start + forest.weight(index) / total * (a1 - a0)
            };
            let cloud = self.cloud(index, start, end);
            let (chain, attach) = match parent {
                None => {
                    let mut chain = vec![self.frame.base, trunk_top];
                    self.grow(&mut chain, cloud);
                    (chain, 0)
                }
                Some(parent) => {
                    let (x, y) = centre(&cloud);
                    let parent_chain = &self.chains[parent];
                    let first = if forest.nodes[parent].depth == 0 {
                        2
                    } else {
                        1
                    };
                    let last = parent_chain.len().saturating_sub(2).max(first);
                    let attach = (first..=last)
                        .min_by(|&p, &q| {
                            dist2(parent_chain[p], (x, y))
                                .total_cmp(&dist2(parent_chain[q], (x, y)))
                        })
                        .unwrap_or(first)
                        .min(parent_chain.len() - 1);
                    let mut chain = vec![parent_chain[attach]];
                    self.grow(&mut chain, cloud);
                    (chain, attach)
                }
            };
            self.chains[index] = chain;
            self.attach[index] = attach;
            let children = forest.nodes[index].children.clone();
            self.grow_siblings(&children, Some(index), start, end, trunk_top);
            start = end;
        }
    }

    /// The attractors of `index`: a patch across the middle of its sector, centred on its
    /// previous tip's angle when that still falls in the sector. A directory's patch fills its
    /// depth's band; a leaf's reaches from its band to a seeded length short of the rim (kept
    /// for labels), so neighbouring tips stagger.
    fn cloud(&self, index: usize, a0: f64, a1: f64) -> Vec<P> {
        let node = &self.ctx.forest.nodes[index];
        let depth = f64::from(node.depth);
        let band = (1.0 - TRUNK) / self.levels;
        let r0 = TRUNK + band * depth + band * 0.2;
        let mut rng: Rng = self.ctx.rng(node.id);
        let r1 = if node.children.is_empty() {
            let rim = (1.0 - self.frame.label_room).max(r0 + band * 0.3);
            r0 + (rim - r0) * rng.range(0.45, 1.0)
        } else {
            TRUNK + band * (depth + 1.0)
        };
        let span = a1 - a0;
        let (lo, hi) = (a0 + span * 0.15, a1 - span * 0.15);
        let previous = self
            .ctx
            .previous_shape(node.id)
            .and_then(|shape| match shape {
                Shape::Stroke { points, .. } => points.last().copied(),
                _ => None,
            })
            .map(|(x, y)| self.frame.angle_of((f64::from(x), f64::from(y))));
        let centre = previous.map_or(f64::midpoint(lo, hi), |a| a.clamp(lo, hi));
        let count = 6 + (18.0 * (f64::from(node.weight) / self.max_weight()).sqrt()) as usize;
        (0..count)
            .map(|_| {
                let angle = (centre + rng.range(-0.35, 0.35) * (hi - lo)).clamp(lo, hi);
                let radius = rng.range(r0, r1);
                self.frame.at(angle, radius)
            })
            .collect()
    }

    fn max_weight(&self) -> f64 {
        self.ctx
            .forest
            .nodes
            .iter()
            .map(|n| f64::from(n.weight))
            .fold(f64::MIN_POSITIVE, f64::max)
    }

    /// Extend `chain` from its last point towards its attractors (Runions et al.'s growth rule
    /// for a single tip): each step heads along the mean direction to the attractors within the
    /// influence radius (all of them when none is), consumes those within the kill radius, and
    /// forgets those the tip has passed, so the branch never doubles back.
    fn grow(&self, chain: &mut Vec<P>, mut attractors: Vec<P>) {
        let step = self.frame.step();
        let influence2 = (INFLUENCE * step).powi(2);
        let kill2 = (KILL * step).powi(2);
        let start = chain[chain.len() - 1];
        let aim = centre(&attractors);
        let mut heading = unit((aim.0 - start.0, aim.1 - start.1)).unwrap_or(self.frame.up);
        for steps in 0..MAX_STEPS {
            if attractors.is_empty() && steps >= MIN_STEPS {
                break;
            }
            let tip = chain[chain.len() - 1];
            let near: Vec<P> = attractors
                .iter()
                .copied()
                .filter(|&a| dist2(a, tip) <= influence2)
                .collect();
            let pull = if near.is_empty() { &attractors } else { &near };
            let sum = pull.iter().fold((0.0, 0.0), |acc, &a| {
                let d = unit((a.0 - tip.0, a.1 - tip.1)).unwrap_or((0.0, 0.0));
                (acc.0 + d.0, acc.1 + d.1)
            });
            if let Some(d) = unit(sum) {
                heading =
                    unit((heading.0 * INERTIA + d.0, heading.1 * INERTIA + d.1)).unwrap_or(heading);
            }
            let next = self
                .frame
                .clamp((tip.0 + heading.0 * step, tip.1 + heading.1 * step));
            chain.push(next);
            attractors.retain(|&a| {
                dist2(a, next) > kill2
                    && (a.0 - next.0) * heading.0 + (a.1 - next.1) * heading.1 > 0.0
            });
        }
    }
}

fn centre(points: &[P]) -> P {
    let n = points.len().max(1) as f64;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    (sx / n, sy / n)
}

fn dist2(a: P, b: P) -> f64 {
    (a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)
}

fn unit((x, y): P) -> Option<P> {
    let len = x.hypot(y);
    (len > 1e-12).then(|| (x / len, y / len))
}
