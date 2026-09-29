//! Rasterizing a laid-out Place body: the shapes `templar_treemap::garden` produced, in a
//! style's material, into one straight-alpha bitmap the tile blits until its key changes.
//!
//! Everything here is geometry and colour arithmetic over a private premultiplied canvas, so the
//! body can be drawn once at the size the tile has and composited over whatever the tile's shell
//! is this frame (resting, hovered, lifted) without being redrawn. Text is not here: names are
//! drawn live over the bitmap, crisp at the output's scale and coloured by state.

use pixelkit_raster::{Bitmap, FillRule, Path, RasterKernel};
use templar_treemap::CushionConfig;
use templar_treemap::garden::{GardenLayout, Item, LayoutKind, Shape};

use super::themes::{self, Grain, Style};
use crate::surface::design::{self, Palette};

/// A `width`×`height` buffer of premultiplied `0xAARRGGBB`, composited "over" as shapes land.
pub(super) struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

impl Canvas {
    pub(super) fn new(width: u32, height: u32) -> Self {
        Self {
            width: width as usize,
            height: height as usize,
            pixels: vec![0; width as usize * height as usize],
        }
    }

    /// Fill `path` at `alpha`, taking each covered pixel's colour from `colour(x, y)` (the
    /// pixel's centre, in canvas pixels), so a shaded surface costs one pass like a flat one.
    pub(super) fn fill(
        &mut self,
        kernel: &mut RasterKernel,
        path: &mut Path,
        alpha: u8,
        mut colour: impl FnMut(f32, f32) -> u32,
    ) {
        if self.width == 0 || self.height == 0 || path.is_empty() {
            return;
        }
        let (width, pixels) = (self.width, &mut self.pixels);
        path.fill_with(
            kernel,
            self.width,
            self.height,
            FillRule::NonZero,
            |y, x0, x1, coverage| {
                let row = y * width;
                for x in x0..=x1.min(width - 1) {
                    let covered = coverage[x];
                    if covered == 0 {
                        continue;
                    }
                    let a = (u32::from(covered) * u32::from(alpha) + 127) / 255;
                    let rgb = colour(x as f32 + 0.5, y as f32 + 0.5);
                    let pixel = &mut pixels[row + x];
                    *pixel = over(*pixel, rgb, a);
                }
            },
        );
    }

    /// Lay `grain` over everything drawn so far, seeded by the place's `material_seed`: the same
    /// place carries the same texture wherever and whenever it is drawn, and two places differ.
    /// Lightness only — the texture never moves a hue.
    pub(super) fn grain(&mut self, grain: &Grain, seed: u64, scale: f32) {
        if grain.fine <= 0.0 && grain.coarse <= 0.0 {
            return;
        }
        let cell_y = (grain.cell * scale).max(1.0);
        let cell_x = cell_y * grain.stretch.max(0.1);
        for y in 0..self.height {
            for x in 0..self.width {
                let pixel = &mut self.pixels[y * self.width + x];
                let alpha = *pixel >> 24;
                if alpha == 0 {
                    continue;
                }
                let factor = 1.0
                    + grain.fine * hash(x as i64, y as i64, seed)
                    + grain.coarse * value_noise(x as f32 / cell_x, y as f32 / cell_y, !seed);
                let channel = |shift: u32| {
                    let value = ((*pixel >> shift) & 0xff) as f32 * factor;
                    (value.round().clamp(0.0, alpha as f32) as u32) << shift
                };
                *pixel = (alpha << 24) | channel(16) | channel(8) | channel(0);
            }
        }
    }

    /// The canvas as the straight-alpha bitmap `Painter::blit` composites.
    pub(super) fn into_bitmap(self) -> Bitmap {
        let pixels = self
            .pixels
            .into_iter()
            .map(|pixel| {
                let alpha = pixel >> 24;
                if alpha == 0 {
                    return 0;
                }
                let channel = |shift: u32| {
                    (((((pixel >> shift) & 0xff) * 255 + alpha / 2) / alpha).min(255)) << shift
                };
                (alpha << 24) | channel(16) | channel(8) | channel(0)
            })
            .collect();
        Bitmap {
            width: self.width as u32,
            height: self.height as u32,
            pixels,
        }
    }
}

/// `rgb` at coverage `a` (0..=255) over the premultiplied `dst`.
fn over(dst: u32, rgb: u32, a: u32) -> u32 {
    let inv = 255 - a;
    let channel = |shift: u32| {
        let source = ((rgb >> shift) & 0xff) * a;
        let under = ((dst >> shift) & 0xff) * inv;
        ((source + under + 127) / 255) << shift
    };
    let alpha = a + ((dst >> 24) * inv + 127) / 255;
    (alpha << 24) | channel(16) | channel(8) | channel(0)
}

/// A stable pseudo-random value in `[-1, 1]` for one lattice point.
fn hash(x: i64, y: i64, seed: u64) -> f32 {
    let mut h = seed
        ^ (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    (h >> 40) as f32 / (1u64 << 23) as f32 - 1.0
}

/// Smooth value noise in `[-1, 1]`: the lattice hashed, interpolated with a smoothstep.
fn value_noise(x: f32, y: f32, seed: u64) -> f32 {
    let (x0, y0) = (x.floor(), y.floor());
    let (tx, ty) = (x - x0, y - y0);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let (ix, iy) = (x0 as i64, y0 as i64);
    let top = hash(ix, iy, seed) + (hash(ix + 1, iy, seed) - hash(ix, iy, seed)) * sx;
    let bottom =
        hash(ix, iy + 1, seed) + (hash(ix + 1, iy + 1, seed) - hash(ix, iy + 1, seed)) * sx;
    top + (bottom - top) * sy
}

/// `rgb` with every channel multiplied by `factor`.
fn lit(rgb: u32, factor: f32) -> u32 {
    let channel = |shift: u32| {
        let value = ((rgb >> shift) & 0xff) as f32 * factor;
        (value.round().clamp(0.0, 255.0) as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// How strongly one item is inked. A container (an item whose children are drawn over it) is
/// the lighter ground its children stand on, so the gap between siblings reads in it; a leaf is
/// inked by its depth and its share of the strongest item's weight. Size already says
/// relevance, so tone only has to separate the levels, and the spread is small.
fn strength(style: &Style, item: &Item, max_weight: f32, container: bool) -> f32 {
    let share = (item.weight / max_weight).clamp(0.0, 1.0).sqrt();
    let depth = f32::from(item.depth.min(4));
    if container {
        style.strength * (0.3 + 0.12 * depth)
    } else {
        style.strength * (0.55 + 0.1 * depth + 0.35 * share).min(1.0)
    }
}

/// The weight every item's share is measured against: the layout's strongest.
pub(super) fn max_weight(layout: &GardenLayout) -> f32 {
    layout
        .items
        .iter()
        .map(|item| item.weight)
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON)
}

/// The ink a branch stroke is drawn in: darker than any fill, heavier branches darker still.
pub(super) fn bark(palette: &Palette, style: &Style, item: &Item, max_weight: f32) -> u32 {
    design::mix(
        palette.folder_shell,
        themes::ink(palette, style, item.is_dir),
        (style.strength * 1.35 * (0.7 + 0.3 * item.weight / max_weight)).min(0.95),
    )
}

/// Draw `layout` in `style` into a `width`×`height` bitmap at `scale` physical pixels per
/// logical one. The layout is in logical pixels with its origin at the bitmap's.
#[allow(clippy::too_many_arguments)]
pub(super) fn rasterize(
    layout: &GardenLayout,
    style: &Style,
    palette: &Palette,
    width: u32,
    height: u32,
    scale: f32,
    seed: u64,
    kernel: &mut RasterKernel,
) -> Bitmap {
    let mut canvas = Canvas::new(width, height);
    let max_weight = max_weight(layout);
    let surface = palette.folder_shell;
    let light = CushionConfig::default();
    let mut path = Path::new();
    for (index, item) in layout.items.iter().enumerate() {
        // Items come parents first, so an item is a container exactly when the next one is
        // deeper than it.
        let container = layout
            .items
            .get(index + 1)
            .is_some_and(|next| next.depth > item.depth);
        let ink = themes::ink(palette, style, item.is_dir);
        let tone = design::mix(surface, ink, strength(style, item, max_weight, container));
        if layout.kind == LayoutKind::Folio {
            sheet(
                &mut canvas,
                kernel,
                &mut path,
                item,
                style,
                palette,
                scale,
                tone,
            );
            continue;
        }
        path.clear();
        shape_path(&mut path, &item.shape, scale, (0.0, 0.0), style.gap);
        match &item.shape {
            Shape::Stroke { points, widths } => {
                let bark = bark(palette, style, item, max_weight);
                canvas.fill(kernel, &mut path, 255, |_, _| bark);
                if style.leaves
                    && !item.is_dir
                    && let (Some(&(x, y)), Some(&tip)) = (points.last(), widths.last())
                {
                    let leaf = design::mix(surface, themes::ink(palette, style, false), 0.55);
                    let radius = (tip * 1.6).max(2.4) * scale;
                    path.clear();
                    path.circle(x * scale, y * scale, radius);
                    canvas.fill(kernel, &mut path, 235, |_, _| leaf);
                }
            }
            Shape::Rect {
                surface: coeffs, ..
            } if style.cushion && coeffs.iter().any(|c| c.abs() > f32::EPSILON) => {
                let [sx1, sx2, sy1, sy2] = *coeffs;
                let [lx, ly, lz] = light.light;
                canvas.fill(kernel, &mut path, 255, |x, y| {
                    let (x, y) = (x / scale, y / scale);
                    let (nx, ny) = (-(2.0 * sx2 * x + sx1), -(2.0 * sy2 * y + sy1));
                    let cos =
                        ((nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt()).max(0.0);
                    let intensity = (light.ambient + light.diffuse * cos).clamp(0.0, 1.0);
                    lit(tone, 0.8 + 0.22 * intensity)
                });
            }
            Shape::Circle { cx, cy, r } if style.relief > 0.0 => {
                let (cx, cy, r) = (cx * scale, cy * scale, (r * scale).max(1.0));
                canvas.fill(kernel, &mut path, 255, |x, y| {
                    lit(tone, relief(style.relief, (x - cx) / r, (y - cy) / r))
                });
            }
            Shape::Polygon { points } if style.relief > 0.0 && !points.is_empty() => {
                let (cx, cy) = centroid(points);
                let r = points
                    .iter()
                    .map(|&(x, y)| ((x - cx).powi(2) + (y - cy).powi(2)).sqrt())
                    .fold(0.0_f32, f32::max)
                    .max(1.0)
                    * scale;
                let (cx, cy) = (cx * scale, cy * scale);
                canvas.fill(kernel, &mut path, 255, |x, y| {
                    lit(tone, relief(style.relief, (x - cx) / r, (y - cy) / r))
                });
            }
            _ => canvas.fill(kernel, &mut path, 255, |_, _| tone),
        }
    }
    canvas.grain(&style.grain, seed, scale);
    canvas.into_bitmap()
}

/// A top-left light across a dome: brighter toward the light, a little darker away from it.
fn relief(amount: f32, dx: f32, dy: f32) -> f32 {
    1.0 + amount * (-(0.55 * dx + 0.85 * dy)).clamp(-1.2, 1.2)
}

/// One sheet of a folio: a faint shadow under it, the paper, and a hairline edge, so a pile of
/// sheets reads as a pile even where two of them are the same tone.
#[allow(clippy::too_many_arguments)]
fn sheet(
    canvas: &mut Canvas,
    kernel: &mut RasterKernel,
    path: &mut Path,
    item: &Item,
    style: &Style,
    palette: &Palette,
    scale: f32,
    tone: u32,
) {
    let Shape::Polygon { points } = &item.shape else {
        path.clear();
        shape_path(path, &item.shape, scale, (0.0, 0.0), style.gap);
        canvas.fill(kernel, path, 255, |_, _| tone);
        return;
    };
    let edge = design::mix(palette.folder_shell, palette.text, 0.28);
    let paper = design::mix(palette.well_fill, tone, 0.55);
    let shadow: Vec<(f32, f32)> = points.iter().map(|&(x, y)| (x + 0.6, y + 1.2)).collect();
    path.clear();
    polygon(path, &shadow, scale, (0.0, 0.0));
    canvas.fill(kernel, path, 34, |_, _| palette.text);
    path.clear();
    polygon(path, points, scale, (0.0, 0.0));
    canvas.fill(kernel, path, 255, |_, _| edge);
    if let Some(inner) = inset_convex(points, 0.9 / scale.max(1.0)) {
        path.clear();
        polygon(path, &inner, scale, (0.0, 0.0));
        canvas.fill(kernel, path, 255, |_, _| paper);
    }
}

fn centroid(points: &[(f32, f32)]) -> (f32, f32) {
    let n = points.len().max(1) as f32;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
    (sx / n, sy / n)
}

/// Twice the signed area (positive: clockwise on a y-down screen, the winding `Path::circle`
/// uses).
fn signed_area(points: &[(f32, f32)]) -> f32 {
    let n = points.len();
    (0..n)
        .map(|i| {
            let (a, b) = (points[i], points[(i + 1) % n]);
            a.0 * b.1 - b.0 * a.1
        })
        .sum()
}

/// A convex polygon with every edge moved `distance` inward, or `None` when that leaves
/// nothing (the edges have crossed).
fn inset_convex(points: &[(f32, f32)], distance: f32) -> Option<Vec<(f32, f32)>> {
    let n = points.len();
    if n < 3 {
        return None;
    }
    if distance <= 0.0 {
        return Some(points.to_vec());
    }
    let area = signed_area(points);
    if area.abs() < f32::EPSILON {
        return None;
    }
    // Inward normal of edge i (from point i to i+1), for this winding.
    let sign = area.signum();
    let normal = |i: usize| {
        let (a, b) = (points[i], points[(i + 1) % n]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let length = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        (-dy / length * sign, dx / length * sign)
    };
    let mut out = Vec::with_capacity(n);
    for (i, &p) in points.iter().enumerate() {
        let previous = (i + n - 1) % n;
        let (n0, n1) = (normal(previous), normal(i));
        // Where the two offset edges meet: p + distance * (n0 + n1) / (1 + n0·n1).
        let dot = n0.0 * n1.0 + n0.1 * n1.1;
        let k = distance / (1.0 + dot).max(0.05);
        out.push((p.0 + (n0.0 + n1.0) * k, p.1 + (n0.1 + n1.1) * k));
    }
    // Past the cell's inradius the offset edges cross and the polygon turns inside out, which
    // keeps its winding but reverses every edge.
    let kept = (0..n).all(|i| {
        let (from, to) = (points[i], points[(i + 1) % n]);
        let (inner_from, inner_to) = (out[i], out[(i + 1) % n]);
        (to.0 - from.0) * (inner_to.0 - inner_from.0)
            + (to.1 - from.1) * (inner_to.1 - inner_from.1)
            > 0.0
    });
    (kept && signed_area(&out) * sign > f32::EPSILON).then_some(out)
}

fn polygon(path: &mut Path, points: &[(f32, f32)], scale: f32, (ox, oy): (f32, f32)) {
    for (index, &(x, y)) in points.iter().enumerate() {
        let (x, y) = (ox + x * scale, oy + y * scale);
        if index == 0 {
            path.move_to(x, y);
        } else {
            path.line_to(x, y);
        }
    }
    path.close();
}

/// Append `shape` to `path` in physical pixels at `origin`, `gap` logical pixels narrower
/// than itself overall so neighbours stand apart.
pub(super) fn shape_path(path: &mut Path, shape: &Shape, scale: f32, origin: (f32, f32), gap: f32) {
    let (ox, oy) = origin;
    let inset = gap * 0.5;
    match shape {
        Shape::Rect { x, y, w, h, .. } => {
            let (w, h) = (w - 2.0 * inset, h - 2.0 * inset);
            if w * scale < 0.5 || h * scale < 0.5 {
                return;
            }
            path.rect(
                ox + (x + inset) * scale,
                oy + (y + inset) * scale,
                w * scale,
                h * scale,
            );
        }
        Shape::Polygon { points } => {
            if let Some(inner) = inset_convex(points, inset) {
                polygon(path, &inner, scale, origin);
            }
        }
        Shape::Circle { cx, cy, r } => {
            let r = (r - inset) * scale;
            if r > 0.3 {
                path.circle(ox + cx * scale, oy + cy * scale, r);
            }
        }
        Shape::Sector {
            cx,
            cy,
            r0,
            r1,
            a0,
            a1,
        } => sector(
            path,
            (ox + cx * scale, oy + cy * scale),
            *r0,
            *r1,
            *a0,
            *a1,
            scale,
            inset,
        ),
        Shape::Stroke { points, widths } => stroke(path, points, widths, scale, origin),
    }
}

#[allow(clippy::too_many_arguments)]
fn sector(
    path: &mut Path,
    (cx, cy): (f32, f32),
    r0: f32,
    r1: f32,
    a0: f32,
    a1: f32,
    scale: f32,
    inset: f32,
) {
    let inner = if r0 > 0.0 { (r0 + inset) * scale } else { 0.0 };
    let outer = (r1 - inset) * scale;
    if outer - inner < 0.5 {
        return;
    }
    if a1 - a0 >= std::f32::consts::TAU - 1e-3 {
        path.circle(cx, cy, outer);
        if inner > 0.0 {
            path.circle_reversed(cx, cy, inner);
        }
        return;
    }
    let trim = |radius: f32| (inset * scale / radius.max(1.0)).min((a1 - a0) * 0.45);
    let (outer_trim, inner_trim) = (trim(outer), trim(inner));
    let arc = |path: &mut Path, radius: f32, from: f32, to: f32, first: bool| {
        let steps = (((to - from).abs() * radius) / 3.0)
            .ceil()
            .clamp(2.0, 256.0) as usize;
        for step in 0..=steps {
            let angle = from + (to - from) * step as f32 / steps as f32;
            let (x, y) = (cx + radius * angle.cos(), cy + radius * angle.sin());
            if first && step == 0 {
                path.move_to(x, y);
            } else {
                path.line_to(x, y);
            }
        }
    };
    arc(path, outer, a0 + outer_trim, a1 - outer_trim, true);
    if inner > 0.0 {
        arc(path, inner, a1 - inner_trim, a0 + inner_trim, false);
    } else {
        path.line_to(cx, cy);
    }
    path.close();
}

/// A tapering stroke: one quad per segment, its two ends as wide as their vertices, and a round
/// joint at every vertex — all wound the same way, so under the non-zero rule the overlaps fill
/// once and never cut holes.
fn stroke(
    path: &mut Path,
    points: &[(f32, f32)],
    widths: &[f32],
    scale: f32,
    (ox, oy): (f32, f32),
) {
    let half = |index: usize| (widths.get(index).copied().unwrap_or(1.0) * scale * 0.5).max(0.35);
    let at = |index: usize| (ox + points[index].0 * scale, oy + points[index].1 * scale);
    for index in 0..points.len().saturating_sub(1) {
        let ((x0, y0), (x1, y1)) = (at(index), at(index + 1));
        let (dx, dy) = (x1 - x0, y1 - y0);
        let length = (dx * dx + dy * dy).sqrt();
        if length <= f32::EPSILON {
            continue;
        }
        let (nx, ny) = (-dy / length, dx / length);
        let (h0, h1) = (half(index), half(index + 1));
        let mut quad = [
            (x0 + nx * h0, y0 + ny * h0),
            (x1 + nx * h1, y1 + ny * h1),
            (x1 - nx * h1, y1 - ny * h1),
            (x0 - nx * h0, y0 - ny * h0),
        ];
        if signed_area(&quad) < 0.0 {
            quad.reverse();
        }
        path.move_to(quad[0].0, quad[0].1);
        for &(x, y) in &quad[1..] {
            path.line_to(x, y);
        }
        path.close();
    }
    for index in 0..points.len() {
        let (x, y) = at(index);
        let radius = half(index);
        if radius > 0.6 {
            path.circle(x, y, radius);
        }
    }
}

/// Whether logical point `(px, py)` is on `shape`, with `slack` logical pixels of tolerance
/// around thin strokes.
pub(super) fn contains(shape: &Shape, (px, py): (f32, f32), slack: f32) -> bool {
    match shape {
        Shape::Rect { x, y, w, h, .. } => px >= *x && py >= *y && px < x + w && py < y + h,
        Shape::Polygon { points } => {
            let mut inside = false;
            let n = points.len();
            for i in 0..n {
                let (a, b) = (points[i], points[(i + n - 1) % n]);
                if (a.1 > py) != (b.1 > py) && px < (b.0 - a.0) * (py - a.1) / (b.1 - a.1) + a.0 {
                    inside = !inside;
                }
            }
            inside
        }
        Shape::Circle { cx, cy, r } => (px - cx).powi(2) + (py - cy).powi(2) <= r * r,
        Shape::Sector {
            cx,
            cy,
            r0,
            r1,
            a0,
            a1,
        } => {
            let (dx, dy) = (px - cx, py - cy);
            let distance = (dx * dx + dy * dy).sqrt();
            if distance < *r0 || distance > *r1 {
                return false;
            }
            let angle = dy.atan2(dx);
            let from_start = (angle - a0).rem_euclid(std::f32::consts::TAU);
            from_start <= a1 - a0
        }
        Shape::Stroke { points, widths } => points.windows(2).enumerate().any(|(index, pair)| {
            let ((x0, y0), (x1, y1)) = (pair[0], pair[1]);
            let (dx, dy) = (x1 - x0, y1 - y0);
            let length = dx * dx + dy * dy;
            let t = if length > f32::EPSILON {
                (((px - x0) * dx + (py - y0) * dy) / length).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let (qx, qy) = (x0 + dx * t, y0 + dy * t);
            let reach = widths.get(index).copied().unwrap_or(1.0) * 0.5 + slack;
            (px - qx).powi(2) + (py - qy).powi(2) <= reach * reach
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A convex cell inset for grout keeps its winding and shrinks by the grout on every side;
    /// an inset larger than the cell leaves nothing rather than an inside-out shape.
    #[test]
    fn a_cell_is_inset_evenly_or_not_at_all() {
        let square = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)];
        let inner = inset_convex(&square, 1.0).expect("room for grout");
        assert_eq!(inner.len(), 4);
        for (x, y) in &inner {
            assert!((x - 1.0).abs() < 1e-4 || (x - 9.0).abs() < 1e-4, "{x}");
            assert!((y - 1.0).abs() < 1e-4 || (y - 9.0).abs() < 1e-4, "{y}");
        }
        assert!(inset_convex(&square, 6.0).is_none());
    }

    /// Hover and click follow the drawn shape, not its bounding box: a point in a sector's
    /// angular gap, or beside a stroke, is not on it.
    #[test]
    fn a_point_is_on_a_shape_only_where_it_is_drawn() {
        let quarter = Shape::Sector {
            cx: 0.0,
            cy: 0.0,
            r0: 10.0,
            r1: 20.0,
            a0: -std::f32::consts::FRAC_PI_2,
            a1: 0.0,
        };
        assert!(contains(&quarter, (10.0, -10.0), 0.0), "upper right");
        assert!(
            !contains(&quarter, (10.0, 10.0), 0.0),
            "lower right is another angle"
        );
        assert!(!contains(&quarter, (3.0, -3.0), 0.0), "inside the hole");
        let twig = Shape::Stroke {
            points: vec![(0.0, 0.0), (20.0, 0.0)],
            widths: vec![2.0, 2.0],
        };
        assert!(contains(&twig, (10.0, 2.5), 2.0));
        assert!(!contains(&twig, (10.0, 6.0), 2.0));
    }
}
