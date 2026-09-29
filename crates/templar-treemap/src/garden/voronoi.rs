//! Weighted Voronoi treemap: each sibling group splits its parent's (convex) cell into power-
//! diagram cells whose areas converge to the siblings' weight shares (after Nocaj & Brandes,
//! *Computing Voronoi Treemaps*, EuroVis 2012): Lloyd steps keep the cells round, and a Newton
//! step on the power weights (the area Jacobian is a graph Laplacian over shared edges) fits
//! the areas.
//!
//! Sites start at the previous layout's centroids when there is one, else at a point seeded by
//! `seed ^ node.id`. A group seeded entirely from the previous layout only refits its weights:
//! its sites stay at the old centroids, so cells keep their places while weights drift (each
//! re-layout is then one gentle Lloyd step).

use super::geometry::{self, P};
use super::{Ctx, Item, LABEL_LINE, Shape, centred_label};

/// Iteration bound per sibling group.
const MAX_ITERATIONS: usize = 40;
/// Stop once every cell is within this share of its target area.
const CONVERGED: f64 = 0.01;
/// Lloyd steps a freshly seeded group always takes, so its cells round out.
const MIN_RELAX: usize = 8;
/// Gap between a cell's edge and its children.
const NEST_PAD: f64 = 2.0;

pub(super) fn layout(ctx: &Ctx<'_>) -> Vec<Item> {
    let b = ctx.bounds;
    let clip = geometry::rect(
        f64::from(b.x),
        f64::from(b.y),
        f64::from(b.w),
        f64::from(b.h),
    );
    let mut items = Vec::with_capacity(ctx.forest.nodes.len());
    split(ctx, &ctx.forest.roots, &clip, &mut items);
    items
}

fn split(ctx: &Ctx<'_>, siblings: &[usize], clip: &[P], items: &mut Vec<Item>) {
    let cells = if siblings.len() == 1 {
        vec![clip.to_vec()]
    } else {
        cells(ctx, siblings, clip)
    };
    for (&index, cell) in siblings.iter().zip(cells) {
        if cell.len() < 3 {
            continue;
        }
        let children = &ctx.forest.nodes[index].children;
        let label = label(&cell, !children.is_empty());
        items.push(ctx.item(
            index,
            Shape::Polygon {
                points: geometry::to_f32(&cell),
            },
            label,
        ));
        if children.is_empty() {
            continue;
        }
        let inner = geometry::inset(&cell, NEST_PAD)
            .filter(|inner| geometry::area(inner) > 1.0)
            .unwrap_or(cell);
        split(ctx, children, &inner, items);
    }
}

/// Converge a power diagram of `siblings` inside `clip` to their weight shares.
fn cells(ctx: &Ctx<'_>, siblings: &[usize], clip: &[P]) -> Vec<Vec<P>> {
    let n = siblings.len();
    let clip_area = geometry::area(clip);
    let total: f64 = siblings.iter().map(|&i| ctx.forest.weight(i)).sum();
    let targets: Vec<f64> = siblings
        .iter()
        .map(|&i| ctx.forest.weight(i) / total * clip_area)
        .collect();
    let (mut sites, remembered) = initial_sites(ctx, siblings, clip);
    let mut weights = vec![0.0; n];
    let epsilon = clip_area * 1e-9;
    let min_iterations = if remembered { 0 } else { MIN_RELAX };

    let mut polygons = power_diagram(&sites, &weights, clip);
    for iteration in 0..MAX_ITERATIONS {
        if iteration >= min_iterations && converged(&polygons, &targets) {
            break;
        }
        if !remembered {
            // Lloyd step: every site to its cell's centroid.
            for (site, cell) in sites.iter_mut().zip(&polygons) {
                if cell.len() >= 3 {
                    *site = geometry::centroid(cell);
                }
            }
            fix_overweighted(&sites, &mut weights, epsilon);
            polygons = power_diagram(&sites, &weights, clip);
        }
        let deltas = newton_step(&sites, &weights, &polygons, &targets);
        for (weight, delta) in weights.iter_mut().zip(deltas) {
            *weight += delta;
        }
        fix_overweighted(&sites, &mut weights, epsilon);
        polygons = power_diagram(&sites, &weights, clip);
    }
    polygons
}

/// Solve `M·δ = T − A` for the weight change δ, where `M` is the Jacobian of the cell areas in
/// the power weights: raising `w_i` by `δ` pushes the bisector with neighbour `j` by
/// `δ / (2·d_ij)`, trading `L_ij·δ / (2·d_ij)` of area (`L_ij` the shared edge's length). `M`
/// is a weighted graph Laplacian; grounding site 0 (`δ_0 = 0`, weights are shift-invariant)
/// makes it solvable.
fn newton_step(sites: &[P], weights: &[f64], polygons: &[Vec<P>], targets: &[f64]) -> Vec<f64> {
    let n = sites.len();
    let mut coupling = vec![vec![0.0; n]; n];
    let scale = polygons
        .iter()
        .flatten()
        .fold(0.0_f64, |m, p| m.max(p.0.abs()).max(p.1.abs()))
        .max(1.0);
    let tolerance = scale * 1e-7;
    for (i, cell) in polygons.iter().enumerate() {
        for e in 0..cell.len() {
            let (p, q) = (cell[e], cell[(e + 1) % cell.len()]);
            let length = (q.0 - p.0).hypot(q.1 - p.1);
            if length <= tolerance {
                continue;
            }
            for j in (0..n).filter(|&j| j != i) {
                let (a, b, c) = bisector(sites[i], weights[i], sites[j], weights[j]);
                let norm = a.hypot(b);
                if norm < 1e-12 {
                    continue;
                }
                let on = |r: P| ((a * r.0 + b * r.1 - c) / norm).abs() <= tolerance;
                if on(p) && on(q) {
                    // `norm` is 2·d_ij.
                    coupling[i][j] += length / norm;
                    break;
                }
            }
        }
    }
    // Reduced Laplacian over sites 1..n, symmetrised.
    let m = n - 1;
    let mut matrix = vec![vec![0.0; m + 1]; m];
    for r in 0..m {
        let i = r + 1;
        for j in 0..n {
            if j == i {
                continue;
            }
            let c = f64::midpoint(coupling[i][j], coupling[j][i]);
            matrix[r][r] += c;
            if j > 0 {
                matrix[r][j - 1] -= c;
            }
        }
        matrix[r][m] = targets[i] - geometry::area(&polygons[i]);
    }
    let mut deltas = vec![0.0; n];
    if let Some(solution) = solve(matrix) {
        deltas[1..].copy_from_slice(&solution);
    }
    deltas
}

/// `a·x + b·y ≤ c` is where site `i` has the lower power distance than site `j`.
fn bisector(si: P, wi: f64, sj: P, wj: f64) -> (f64, f64, f64) {
    (
        2.0 * (sj.0 - si.0),
        2.0 * (sj.1 - si.1),
        (sj.0 * sj.0 + sj.1 * sj.1) - (si.0 * si.0 + si.1 * si.1) - wj + wi,
    )
}

/// Gaussian elimination with partial pivoting on an augmented `m × (m + 1)` matrix.
fn solve(mut a: Vec<Vec<f64>>) -> Option<Vec<f64>> {
    let m = a.len();
    for col in 0..m {
        let pivot = (col..m).max_by(|&p, &q| a[p][col].abs().total_cmp(&a[q][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        for row in col + 1..m {
            let f = a[row][col] / a[col][col];
            if f != 0.0 {
                for k in col..=m {
                    a[row][k] -= f * a[col][k];
                }
            }
        }
    }
    let mut x = vec![0.0; m];
    for row in (0..m).rev() {
        let tail: f64 = (row + 1..m).map(|k| a[row][k] * x[k]).sum();
        x[row] = (a[row][m] - tail) / a[row][row];
    }
    Some(x)
}

fn converged(polygons: &[Vec<P>], targets: &[f64]) -> bool {
    polygons
        .iter()
        .zip(targets)
        .all(|(cell, &target)| (geometry::area(cell) - target).abs() <= CONVERGED * target)
}

/// Keep every site's power weight from swallowing a neighbour: `w_i - w_j < |s_i - s_j|²`.
fn fix_overweighted(sites: &[P], weights: &mut [f64], epsilon: f64) {
    for _ in 0..16 {
        let mut fixed = false;
        for i in 0..sites.len() {
            for j in i + 1..sites.len() {
                let (heavy, light) = if weights[i] > weights[j] {
                    (i, j)
                } else {
                    (j, i)
                };
                let d2 = (sites[i].0 - sites[j].0).powi(2) + (sites[i].1 - sites[j].1).powi(2);
                let over = weights[heavy] - weights[light] - d2;
                if over > 0.0 {
                    weights[heavy] -= over + epsilon;
                    fixed = true;
                }
            }
        }
        if !fixed {
            break;
        }
    }
}

/// Power-diagram cell of every site, clipped to `clip`.
fn power_diagram(sites: &[P], weights: &[f64], clip: &[P]) -> Vec<Vec<P>> {
    (0..sites.len())
        .map(|i| {
            let (si, wi) = (sites[i], weights[i]);
            let mut cell = clip.to_vec();
            for (j, (&sj, &wj)) in sites.iter().zip(weights).enumerate() {
                if j == i || cell.is_empty() {
                    continue;
                }
                // |p - si|² - wi ≤ |p - sj|² - wj
                let a = 2.0 * (sj.0 - si.0);
                let b = 2.0 * (sj.1 - si.1);
                let c = (sj.0 * sj.0 + sj.1 * sj.1) - (si.0 * si.0 + si.1 * si.1) - wj + wi;
                if a.abs() < 1e-12 && b.abs() < 1e-12 {
                    continue;
                }
                cell = geometry::clip(&cell, a, b, c);
            }
            cell
        })
        .collect()
}

/// Previous centroids where they still fall inside `clip`, else points seeded per node; nudged
/// apart so no two sites coincide. The flag tells whether every site came from the previous
/// layout.
fn initial_sites(ctx: &Ctx<'_>, siblings: &[usize], clip: &[P]) -> (Vec<P>, bool) {
    let (x0, y0, x1, y1) = clip.iter().fold(
        (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    );
    let fallback = geometry::centroid(clip);
    let mut sites: Vec<P> = Vec::with_capacity(siblings.len());
    let mut remembered = true;
    for &index in siblings {
        let id = ctx.forest.nodes[index].id;
        let previous = ctx.previous_shape(id).and_then(|shape| match shape {
            Shape::Polygon { points } if points.len() >= 3 => {
                let poly: Vec<P> = points
                    .iter()
                    .map(|&(x, y)| (f64::from(x), f64::from(y)))
                    .collect();
                Some(geometry::centroid(&poly))
            }
            _ => None,
        });
        let mut rng = ctx.rng(id);
        let previous = previous.filter(|&p| geometry::contains(clip, p));
        remembered &= previous.is_some();
        let site = previous
            .or_else(|| {
                (0..32)
                    .map(|_| (rng.range(x0, x1), rng.range(y0, y1)))
                    .find(|&p| geometry::contains(clip, p))
            })
            .unwrap_or(fallback);
        sites.push(site);
    }
    // Separate coincident sites deterministically, towards the clip centroid.
    let tiny = ((x1 - x0) + (y1 - y0)) * 1e-4;
    for i in 1..sites.len() {
        for step in 1..=8 {
            let clash = sites[..i]
                .iter()
                .any(|s| (s.0 - sites[i].0).abs() + (s.1 - sites[i].1).abs() < tiny);
            if !clash {
                break;
            }
            let t = 0.1 * f64::from(step);
            let (cx, cy) = fallback;
            let (sx, sy) = sites[i];
            sites[i] = (
                sx + (cx - sx) * t + tiny * i as f64,
                sy + (cy - sy) * t + tiny * f64::from(step),
            );
        }
    }
    (sites, remembered)
}

fn label(cell: &[P], has_children: bool) -> Option<(f32, f32, f32)> {
    let (cx, cy) = geometry::centroid(cell);
    let half = f64::from(LABEL_LINE) / 2.0;
    let y = if has_children {
        // Near the top of the cell, above most of the children.
        let top = cell.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        top + half * 1.5 + NEST_PAD
    } else {
        cy
    };
    let (a0, a1) = geometry::chord(cell, y - half)?;
    let (b0, b1) = geometry::chord(cell, y + half)?;
    let (lo, hi) = (a0.max(b0), a1.min(b1));
    if hi <= lo {
        return None;
    }
    let mid = if has_children {
        f64::midpoint(lo, hi)
    } else {
        cx
    };
    let width = 2.0 * (mid - lo).min(hi - mid);
    centred_label(mid as f32, y as f32, width as f32)
}
