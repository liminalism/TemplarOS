//! Hierarchical circle packing. Siblings get radii ∝ √weight and are packed in id order with the
//! front-chain algorithm (Wang, Wang, Dai & Wang, *Visualization of Large Hierarchical Data by
//! Circle Packing*, CHI 2006, as refined in d3-hierarchy); each directory's packing is then
//! scaled into the directory's own circle. The top level grows the chain towards an ellipse of
//! the Place's aspect, so wide Places are filled rather than holding one round pack.

use super::{Ctx, Item, LABEL_LINE, Shape, centred_label};

/// Share of a directory's radius its children may fill.
const FILL: f64 = 0.9;
/// Most elongated top-level packing, as width over height (or its inverse).
const MAX_ASPECT: f64 = 2.5;
/// Turns of the top-level packing tried (evenly over half a turn).
const ROTATIONS: usize = 12;

#[derive(Clone, Copy, Debug)]
struct Circle {
    x: f64,
    y: f64,
    r: f64,
}

pub(super) fn layout(ctx: &Ctx<'_>) -> Vec<Item> {
    let b = ctx.bounds;
    let aspect = f64::from(b.w / b.h).clamp(1.0 / MAX_ASPECT, MAX_ASPECT);
    let (bw, bh) = (f64::from(b.w), f64::from(b.h));
    // Turn the packing to whichever of a few fixed angles fills the bounds best, then fit its
    // bounding box (not its enclosing circle) so wide Places use their width.
    let upright = pack(&radii(ctx, &ctx.forest.roots), aspect);
    let packed = (0..ROTATIONS)
        .map(|k| {
            let (s, c) = (std::f64::consts::PI * k as f64 / ROTATIONS as f64).sin_cos();
            let turned: Vec<Circle> = upright
                .iter()
                .map(|p| Circle {
                    x: p.x * c - p.y * s,
                    y: p.x * s + p.y * c,
                    r: p.r,
                })
                .collect();
            let (x0, y0, x1, y1) = extent(&turned);
            let fit = (bw / (x1 - x0)).min(bh / (y1 - y0));
            (fit, turned)
        })
        .fold(
            None,
            |best: Option<(f64, Vec<Circle>)>, candidate| match best {
                Some(best) if best.0 >= candidate.0 * (1.0 - 1e-9) => Some(best),
                _ => Some(candidate),
            },
        )
        .map(|(_, packed)| packed)
        .unwrap_or_default();
    let (x0, y0, x1, y1) = extent(&packed);
    let scale = (bw / (x1 - x0)).min(bh / (y1 - y0));
    let ox = f64::from(b.x) + (bw - (x1 - x0) * scale) / 2.0 - x0 * scale;
    let oy = f64::from(b.y) + (bh - (y1 - y0) * scale) / 2.0 - y0 * scale;

    let mut items = Vec::with_capacity(ctx.forest.nodes.len());
    for (&index, c) in ctx.forest.roots.iter().zip(&packed) {
        let placed = Circle {
            x: ox + c.x * scale,
            y: oy + c.y * scale,
            r: c.r * scale,
        };
        emit(ctx, index, placed, &mut items);
    }
    items
}

/// Bounding box `(x0, y0, x1, y1)` of a set of circles.
fn extent(circles: &[Circle]) -> (f64, f64, f64, f64) {
    circles.iter().fold(
        (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |(x0, y0, x1, y1), c| {
            (
                x0.min(c.x - c.r),
                y0.min(c.y - c.r),
                x1.max(c.x + c.r),
                y1.max(c.y + c.r),
            )
        },
    )
}

fn radii(ctx: &Ctx<'_>, siblings: &[usize]) -> Vec<Circle> {
    siblings
        .iter()
        .map(|&i| Circle {
            x: 0.0,
            y: 0.0,
            r: ctx.forest.weight(i).sqrt(),
        })
        .collect()
}

fn emit(ctx: &Ctx<'_>, index: usize, c: Circle, items: &mut Vec<Item>) {
    let children = &ctx.forest.nodes[index].children;
    let label = if children.is_empty() {
        centred_label(c.x as f32, c.y as f32, (2.0 * c.r * 0.9) as f32)
    } else {
        // First line under the top of the rim, where the children rarely reach.
        let dy = c.r - f64::from(LABEL_LINE);
        let half = (c.r * c.r - dy * dy).max(0.0).sqrt();
        (dy > 0.0)
            .then(|| centred_label(c.x as f32, (c.y - dy) as f32, (2.0 * half) as f32))
            .flatten()
    };
    items.push(ctx.item(
        index,
        Shape::Circle {
            cx: c.x as f32,
            cy: c.y as f32,
            r: c.r as f32,
        },
        label,
    ));
    if children.is_empty() {
        return;
    }
    let packed = pack(&radii(ctx, children), 1.0);
    let enclosing = enclose(&packed);
    if enclosing.r <= 0.0 {
        return;
    }
    let scale = c.r * FILL / enclosing.r;
    for (&child, p) in children.iter().zip(&packed) {
        let placed = Circle {
            x: c.x + (p.x - enclosing.x) * scale,
            y: c.y + (p.y - enclosing.y) * scale,
            r: p.r * scale,
        };
        emit(ctx, child, placed, items);
    }
}

/// Front-chain sibling packing (d3-hierarchy `packSiblings`), in input order. The chain is
/// re-anchored at the pair nearest the origin in a metric stretched by `aspect` (width over
/// height), so the packing grows into that ellipse.
fn pack(input: &[Circle], aspect: f64) -> Vec<Circle> {
    let mut c = input.to_vec();
    let n = c.len();
    if n == 0 {
        return c;
    }
    c[0].x = 0.0;
    c[0].y = 0.0;
    if n == 1 {
        return c;
    }
    c[0].x = -c[1].r;
    c[1].x = c[0].r;
    c[1].y = 0.0;
    if n == 2 {
        return c;
    }
    c[2] = place(c[1], c[0], c[2]);

    // Circular doubly-linked front chain over circle indices.
    let mut next = vec![usize::MAX; n];
    let mut prev = vec![usize::MAX; n];
    let (mut a, mut b) = (0, 1);
    next[0] = 1;
    prev[1] = 0;
    next[1] = 2;
    prev[2] = 1;
    next[2] = 0;
    prev[0] = 2;

    let mut i = 3;
    'pack: while i < n {
        c[i] = place(c[a], c[b], c[i]);
        let mut j = next[b];
        let mut k = prev[a];
        let mut sj = c[b].r;
        let mut sk = c[a].r;
        loop {
            if sj <= sk {
                if intersects(c[j], c[i]) {
                    b = j;
                    next[a] = b;
                    prev[b] = a;
                    continue 'pack;
                }
                sj += c[j].r;
                j = next[j];
            } else {
                if intersects(c[k], c[i]) {
                    a = k;
                    next[a] = b;
                    prev[b] = a;
                    continue 'pack;
                }
                sk += c[k].r;
                k = prev[k];
            }
            if j == next[k] {
                break;
            }
        }

        // Insert i between a and b.
        prev[i] = a;
        next[i] = b;
        next[a] = i;
        prev[b] = i;
        b = i;

        // Re-anchor the chain at the pair closest to the origin.
        let mut best = score(&c, a, next[a], aspect);
        let mut cursor = next[i];
        while cursor != b {
            let s = score(&c, cursor, next[cursor], aspect);
            if s < best {
                a = cursor;
                best = s;
            }
            cursor = next[cursor];
        }
        b = next[a];
        i += 1;
    }
    c
}

/// Place `c` tangent to both `a` and `b` (d3's `place(b, a, c)` with the arguments named for the
/// chain order).
fn place(b: Circle, a: Circle, mut c: Circle) -> Circle {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let d2 = dx * dx + dy * dy;
    if d2 > 0.0 {
        let a2 = (a.r + c.r).powi(2);
        let b2 = (b.r + c.r).powi(2);
        if a2 > b2 {
            let x = (d2 + b2 - a2) / (2.0 * d2);
            let y = (b2 / d2 - x * x).max(0.0).sqrt();
            c.x = b.x - x * dx - y * dy;
            c.y = b.y - x * dy + y * dx;
        } else {
            let x = (d2 + a2 - b2) / (2.0 * d2);
            let y = (a2 / d2 - x * x).max(0.0).sqrt();
            c.x = a.x + x * dx - y * dy;
            c.y = a.y + x * dy + y * dx;
        }
    } else {
        c.x = a.x + c.r;
        c.y = a.y;
    }
    c
}

fn intersects(a: Circle, b: Circle) -> bool {
    let dr = a.r + b.r - 1e-6;
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    dr > 0.0 && dr * dr > dx * dx + dy * dy
}

fn score(c: &[Circle], a: usize, b: usize, aspect: f64) -> f64 {
    let (a, b) = (c[a], c[b]);
    let ab = a.r + b.r;
    let x = (a.x * b.r + b.x * a.r) / ab;
    let y = (a.y * b.r + b.y * a.r) / ab;
    x * x / aspect + y * y * aspect
}

/// An enclosing circle of a packing: the Bădoiu–Clarkson iteration from the bounding-box
/// centre, then the exact radius around the found centre. Deterministic and within a few
/// percent of the minimal one.
fn enclose(circles: &[Circle]) -> Circle {
    let (x0, y0, x1, y1) = extent(circles);
    let (mut x, mut y) = (f64::midpoint(x0, x1), f64::midpoint(y0, y1));
    let reach = |x: f64, y: f64, c: &Circle| (c.x - x).hypot(c.y - y) + c.r;
    for step in 1..=64 {
        let far = circles
            .iter()
            .max_by(|p, q| reach(x, y, p).total_cmp(&reach(x, y, q)))
            .copied()
            .unwrap_or(Circle { x, y, r: 0.0 });
        let d = (far.x - x).hypot(far.y - y);
        if d < 1e-12 {
            break;
        }
        // Move towards the far circle's outer point.
        let (tx, ty) = (
            far.x + (far.x - x) / d * far.r,
            far.y + (far.y - y) / d * far.r,
        );
        let t = 1.0 / (f64::from(step) + 1.0);
        x += (tx - x) * t;
        y += (ty - y) * t;
    }
    let r = circles.iter().map(|c| reach(x, y, c)).fold(0.0, f64::max);
    Circle { x, y, r }
}
