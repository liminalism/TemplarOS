//! Rectangular kinds: the ordered strip treemap, the stable squarified cushion treemap and the
//! icicle partition. Siblings are laid out in id order, so for any two siblings `i < j` the
//! rectangle of `j` lies entirely right of or entirely below the rectangle of `i`, whatever
//! their weights.

use super::{Ctx, Item, LABEL_LINE, LABEL_MIN_W, Shape, rect_label};
use crate::layout::{add_ridge, squarify};

/// Gap between a directory's edge and its children.
const NEST_PAD: f64 = 2.0;
/// Cushion ridge height of the top level and its per-level falloff (the Files defaults).
const CUSHION_HEIGHT: f32 = 0.8;
const CUSHION_FALLOFF: f32 = 0.75;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tiling {
    Strip,
    Squarified,
}

pub(super) fn strip(ctx: &Ctx<'_>) -> Vec<Item> {
    nested(ctx, Tiling::Strip)
}

pub(super) fn squarified(ctx: &Ctx<'_>) -> Vec<Item> {
    nested(ctx, Tiling::Squarified)
}

fn nested(ctx: &Ctx<'_>, tiling: Tiling) -> Vec<Item> {
    let mut items = Vec::with_capacity(ctx.forest.nodes.len());
    let b = ctx.bounds;
    tile(
        ctx,
        tiling,
        &ctx.forest.roots,
        (
            f64::from(b.x),
            f64::from(b.y),
            f64::from(b.w),
            f64::from(b.h),
        ),
        [0.0; 4],
        CUSHION_HEIGHT,
        &mut items,
    );
    items
}

fn tile(
    ctx: &Ctx<'_>,
    tiling: Tiling,
    siblings: &[usize],
    (x, y, w, h): (f64, f64, f64, f64),
    parent_surface: [f32; 4],
    cushion: f32,
    items: &mut Vec<Item>,
) {
    if siblings.is_empty() || w <= 0.0 || h <= 0.0 {
        return;
    }
    let total: f64 = siblings.iter().map(|&i| ctx.forest.weight(i)).sum();
    let scale = w * h / total;
    let areas: Vec<f64> = siblings
        .iter()
        .map(|&i| ctx.forest.weight(i) * scale)
        .collect();
    let placed: Vec<(f64, f64, f64, f64)> = match tiling {
        Tiling::Strip => strips(&areas, x, y, w, h),
        Tiling::Squarified => squarify(&areas, x, y, w, h)
            .into_iter()
            .map(|p| (p.x, p.y, p.w, p.h))
            .collect(),
    };
    // Clamp float drift so every rectangle stays inside its parent.
    for (&index, &(rx, ry, rw, rh)) in siblings.iter().zip(&placed) {
        let rx0 = rx.clamp(x, x + w);
        let ry0 = ry.clamp(y, y + h);
        let rw = (rx + rw).clamp(x, x + w) - rx0;
        let rh = (ry + rh).clamp(y, y + h) - ry0;
        let (rx, ry) = (rx0, ry0);
        let (fx, fy, fw, fh) = (rx as f32, ry as f32, rw as f32, rh as f32);
        let surface = if tiling == Tiling::Squarified {
            let [mut sx1, mut sx2, mut sy1, mut sy2] = parent_surface;
            add_ridge(fx, fx + fw, cushion, &mut sx1, &mut sx2);
            add_ridge(fy, fy + fh, cushion, &mut sy1, &mut sy2);
            [sx1, sx2, sy1, sy2]
        } else {
            [0.0; 4]
        };
        let children = &ctx.forest.nodes[index].children;
        let inner = interior(rx, ry, rw, rh);
        let has_header = !children.is_empty() && inner.is_some_and(|(_, iy, _, _)| iy > ry);
        items.push(ctx.item(
            index,
            Shape::Rect {
                x: fx,
                y: fy,
                w: fw,
                h: fh,
                surface,
            },
            rect_label(fx, fy, fw, fh, has_header),
        ));
        if let Some(inner) = inner {
            tile(
                ctx,
                tiling,
                children,
                inner,
                surface,
                cushion * CUSHION_FALLOFF,
                items,
            );
        }
    }
}

/// The part of a directory's rectangle its children tile: inset by [`NEST_PAD`], minus a label
/// line on top when the rectangle is tall enough to spare one.
fn interior(x: f64, y: f64, w: f64, h: f64) -> Option<(f64, f64, f64, f64)> {
    let pad = if w.min(h) > 6.0 * NEST_PAD {
        NEST_PAD
    } else {
        0.0
    };
    let line = f64::from(LABEL_LINE);
    let header = if h > 4.0 * line && w > f64::from(LABEL_MIN_W) {
        line
    } else {
        0.0
    };
    let (iw, ih) = (w - 2.0 * pad, h - 2.0 * pad - header);
    (iw > 0.0 && ih > 0.0).then_some((x + pad, y + pad + header, iw, ih))
}

/// Ordered strip treemap (Bederson, Shneiderman & Wattenberg, *Ordered and Quantum Treemaps*,
/// 2002): items fill strips along the longer side in order; a strip closes when adding the next
/// item would worsen its mean aspect ratio.
fn strips(areas: &[f64], x: f64, y: f64, w: f64, h: f64) -> Vec<(f64, f64, f64, f64)> {
    let rows = w >= h;
    let (along, across) = if rows { (w, h) } else { (h, w) };

    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = 0;
    let mut sum = 0.0;
    for (i, &a) in areas.iter().enumerate() {
        if i > start {
            let before = mean_aspect(&areas[start..i], sum, along);
            let after = mean_aspect(&areas[start..=i], sum + a, along);
            if after > before {
                groups.push(start..i);
                start = i;
                sum = 0.0;
            }
        }
        sum += a;
    }
    groups.push(start..areas.len());

    let mut out = Vec::with_capacity(areas.len());
    let mut offset_across = 0.0;
    let last_group = groups.len() - 1;
    for (g, range) in groups.into_iter().enumerate() {
        let group_sum: f64 = areas[range.clone()].iter().sum();
        let thickness = if g == last_group {
            across - offset_across
        } else {
            group_sum / along
        };
        let last = range.end - 1;
        let mut offset_along = 0.0;
        for i in range {
            let length = if i == last {
                along - offset_along
            } else {
                areas[i] / group_sum * along
            };
            out.push(if rows {
                (x + offset_along, y + offset_across, length, thickness)
            } else {
                (x + offset_across, y + offset_along, thickness, length)
            });
            offset_along += length;
        }
        offset_across += thickness;
    }
    out
}

fn mean_aspect(areas: &[f64], sum: f64, along: f64) -> f64 {
    let thickness = sum / along;
    areas
        .iter()
        .map(|&a| {
            let length = a / thickness;
            (length / thickness).max(thickness / length)
        })
        .sum::<f64>()
        / areas.len() as f64
}

/// Top-down icicle: one band per depth, each node spanning its share of its parent's width.
/// Leaves run down to the bottom edge (flame-graph style), so the partition fills the bounds.
pub(super) fn icicle(ctx: &Ctx<'_>) -> Vec<Item> {
    let b = ctx.bounds;
    let band = f64::from(b.h) / ctx.forest.levels() as f64;
    let mut items = Vec::with_capacity(ctx.forest.nodes.len());
    partition(
        ctx,
        &ctx.forest.roots,
        f64::from(b.x),
        f64::from(b.w),
        f64::from(b.y),
        band,
        &mut items,
    );
    items
}

fn partition(
    ctx: &Ctx<'_>,
    siblings: &[usize],
    x: f64,
    w: f64,
    y: f64,
    band: f64,
    items: &mut Vec<Item>,
) {
    let total: f64 = siblings.iter().map(|&i| ctx.forest.weight(i)).sum();
    let right = x + w;
    let mut cursor = x;
    for (k, &index) in siblings.iter().enumerate() {
        let next = if k + 1 == siblings.len() {
            right
        } else {
            (cursor + ctx.forest.weight(index) / total * w).min(right)
        };
        let leaf = ctx.forest.nodes[index].children.is_empty();
        let bottom = f64::from(ctx.bounds.bottom());
        let height = if leaf { bottom - y } else { band };
        let (fx, fy, fw, fh) = (
            cursor as f32,
            y as f32,
            (next - cursor) as f32,
            height as f32,
        );
        items.push(ctx.item(
            index,
            Shape::Rect {
                x: fx,
                y: fy,
                w: fw,
                h: fh,
                surface: [0.0; 4],
            },
            // In the node's own band, level with its siblings' names.
            rect_label(fx, fy, fw, band as f32, false),
        ));
        partition(
            ctx,
            &ctx.forest.nodes[index].children,
            cursor,
            next - cursor,
            y + band,
            band,
            items,
        );
        cursor = next;
    }
}
