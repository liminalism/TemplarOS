//! A folio: the Place's top-level entries as sheets of paper spread on a desk, in a stable grid
//! (id order) where neighbouring sheets overlap a little. Each entry's sheet is sized ∝ √weight
//! (so its area follows its weight, with a floor that keeps faint sheets readable) and slightly
//! rotated by `seed ^ node.id`; its descendants (preorder, id order) fan out beneath it as a
//! pile. The whole spread is then fitted to the bounds.
//!
//! Items are in paint order: pile by pile in id order (later piles overlap earlier ones), each
//! pile from its bottom sheet up to the entry's own sheet.

use super::{Ctx, Item, LABEL_LINE, Shape, centred_label};

/// Sheet height over width (ISO 216 portrait).
const ASPECT: f64 = std::f64::consts::SQRT_2;
/// Heaviest sheet's size relative to its grid cell, across and down; above 1, neighbours
/// overlap (rows more than columns, like papers spread on a desk).
const SPREAD_ACROSS: f64 = 1.05;
const SPREAD_DOWN: f64 = 1.35;
/// Lightest sheet's size relative to the heaviest.
const MIN_SIZE: f64 = 0.55;
/// Sheet centres wander this share of a cell from the cell centre.
const WANDER: f64 = 0.08;
/// Rotation jitter of a top sheet and the fan step of the sheets beneath it, radians.
const TOP_TILT: f64 = 0.06;
const FAN_STEP: f64 = 0.07;
/// Offset of a whole fan beneath its top sheet, as a share of that sheet's width.
const FAN_OFFSET: f64 = 0.22;
/// Smallest sheet beneath a top sheet relative to it.
const MIN_UNDER: f64 = 0.6;
/// The spread may be enlarged up to this factor to fill the bounds.
const MAX_ZOOM: f64 = 1.6;

type P = (f64, f64);

struct Sheet {
    index: usize,
    quad: Vec<P>,
    /// Top sheets carry their label room: centre and width before fitting.
    label: Option<(P, f64, f64)>,
}

pub(super) fn layout(ctx: &Ctx<'_>) -> Vec<Item> {
    let forest = ctx.forest;
    let b = ctx.bounds;
    let (bx, by, bw, bh) = (
        f64::from(b.x),
        f64::from(b.y),
        f64::from(b.w),
        f64::from(b.h),
    );
    let n = forest.roots.len();
    // The column count that gives the largest sheets.
    let sheet_width = |cols: usize| {
        let rows = n.div_ceil(cols);
        (bw / cols as f64 * SPREAD_ACROSS).min(bh / rows as f64 * SPREAD_DOWN / ASPECT)
    };
    let cols = (1..=n)
        .max_by(|&a, &c| sheet_width(a).total_cmp(&sheet_width(c)).then(c.cmp(&a)))
        .unwrap_or(1);
    let (cw, ch) = (bw / cols as f64, bh / n.div_ceil(cols) as f64);
    let base_width = sheet_width(cols);
    let max_weight = forest
        .roots
        .iter()
        .map(|&r| forest.weight(r))
        .fold(0.0, f64::max);

    let mut sheets = Vec::with_capacity(forest.nodes.len());
    for (slot, &root) in forest.roots.iter().enumerate() {
        let mut rng = ctx.rng(forest.nodes[root].id);
        let centre = (
            bx + ((slot % cols) as f64 + 0.5 + rng.range(-WANDER, WANDER)) * cw,
            by + ((slot / cols) as f64 + 0.5 + rng.range(-WANDER, WANDER)) * ch,
        );
        let size = (forest.weight(root) / max_weight)
            .sqrt()
            .clamp(MIN_SIZE, 1.0);
        let width = base_width * size;
        pile(
            ctx,
            root,
            centre,
            width,
            rng.range(-TOP_TILT, TOP_TILT),
            &mut sheets,
        );
    }

    // Fit the spread into the bounds about their centre.
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for &(px, py) in sheets.iter().flat_map(|s| &s.quad) {
        x0 = x0.min(px);
        y0 = y0.min(py);
        x1 = x1.max(px);
        y1 = y1.max(py);
    }
    let scale = (bw / (x1 - x0)).min(bh / (y1 - y0)).min(MAX_ZOOM) * (1.0 - 1e-5);
    let (mx, my) = (f64::midpoint(x0, x1), f64::midpoint(y0, y1));
    let (tx, ty) = (bx + bw / 2.0, by + bh / 2.0);
    let place = |(px, py): P| (tx + (px - mx) * scale, ty + (py - my) * scale);

    sheets
        .into_iter()
        .map(|sheet| {
            let label = sheet.label.and_then(|(centre, width, height)| {
                let (cx, cy) = place(centre);
                let (width, height) = (width * scale, height * scale);
                // On the sheet's upper part, where a sheet overlapping from below leaves it free.
                (height >= f64::from(LABEL_LINE) * 2.5)
                    .then(|| centred_label(cx as f32, (cy - height * 0.25) as f32, width as f32))
                    .flatten()
            });
            ctx.item(
                sheet.index,
                Shape::Polygon {
                    points: sheet
                        .quad
                        .iter()
                        .map(|&p| {
                            let (x, y) = place(p);
                            (x as f32, y as f32)
                        })
                        .collect(),
                },
                label,
            )
        })
        .collect()
}

/// One entry's pile, bottom sheet first: its descendants fanned beneath, its own sheet on top.
fn pile(ctx: &Ctx<'_>, root: usize, centre: P, width: f64, tilt: f64, out: &mut Vec<Sheet>) {
    let forest = ctx.forest;
    let mut under = Vec::new();
    descendants(ctx, root, &mut under);
    let root_weight = forest.weight(root);
    let steps = under.len().max(1) as f64;
    // The farthest sheet beneath lies at the bottom.
    for (k, &index) in under.iter().enumerate().rev() {
        let mut rng = ctx.rng(forest.nodes[index].id);
        let offset = FAN_OFFSET * width * (k + 1) as f64 / steps;
        let size = width
            * (forest.weight(index) / root_weight)
                .sqrt()
                .clamp(MIN_UNDER, 1.0);
        let side = if k % 2 == 0 { 1.0 } else { -1.0 };
        let turn = tilt + side * FAN_STEP * (k / 2 + 1) as f64 + rng.range(-TOP_TILT, TOP_TILT);
        // Align the sheet's top-right corner with the top sheet's, then fan it further out, so
        // every sheet beneath peeks out past the top one.
        let shift = (width - size) / 2.0;
        let c = (
            centre.0 + shift + offset * 0.6,
            centre.1 - shift * ASPECT - offset * 0.4,
        );
        out.push(Sheet {
            index,
            quad: quad(c, size, turn),
            label: None,
        });
    }
    out.push(Sheet {
        index: root,
        quad: quad(centre, width, tilt),
        label: Some((centre, width, width * ASPECT)),
    });
}

/// Descendants of `index` in preorder (id order).
fn descendants(ctx: &Ctx<'_>, index: usize, out: &mut Vec<usize>) {
    for &child in &ctx.forest.nodes[index].children {
        out.push(child);
        descendants(ctx, child, out);
    }
}

/// A sheet of width `width` centred on `c`, rotated by `tilt`, clockwise on screen.
fn quad(c: P, width: f64, tilt: f64) -> Vec<P> {
    let (hw, hh) = (width / 2.0, width * ASPECT / 2.0);
    let (s, k) = tilt.sin_cos();
    [(-hw, -hh), (hw, -hh), (hw, hh), (-hw, hh)]
        .iter()
        .map(|&(dx, dy)| (c.0 + dx * k - dy * s, c.1 + dx * s + dy * k))
        .collect()
}
