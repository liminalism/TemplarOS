//! Convex-polygon arithmetic in `f64`, screen coordinates (y down). Polygons are kept with a
//! positive shoelace sum, which is clockwise on screen.

pub(super) type P = (f64, f64);

/// Twice the signed area (shoelace sum); positive for clockwise-on-screen polygons.
pub(super) fn shoelace(poly: &[P]) -> f64 {
    let n = poly.len();
    (0..n)
        .map(|i| {
            let (x0, y0) = poly[i];
            let (x1, y1) = poly[(i + 1) % n];
            x0 * y1 - x1 * y0
        })
        .sum()
}

pub(super) fn area(poly: &[P]) -> f64 {
    shoelace(poly).abs() / 2.0
}

/// Area centroid; the vertex mean for degenerate polygons.
pub(super) fn centroid(poly: &[P]) -> P {
    let n = poly.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let twice = shoelace(poly);
    if twice.abs() < 1e-9 {
        let (sx, sy) = poly
            .iter()
            .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
        return (sx / n as f64, sy / n as f64);
    }
    let (mut cx, mut cy) = (0.0, 0.0);
    for i in 0..n {
        let (x0, y0) = poly[i];
        let (x1, y1) = poly[(i + 1) % n];
        let cross = x0 * y1 - x1 * y0;
        cx += (x0 + x1) * cross;
        cy += (y0 + y1) * cross;
    }
    (cx / (3.0 * twice), cy / (3.0 * twice))
}

/// Keep the part of a convex polygon where `a·x + b·y ≤ c` (Sutherland–Hodgman, one edge).
pub(super) fn clip(poly: &[P], a: f64, b: f64, c: f64) -> Vec<P> {
    let n = poly.len();
    let mut out = Vec::with_capacity(n + 1);
    for i in 0..n {
        let p = poly[i];
        let q = poly[(i + 1) % n];
        let dp = a * p.0 + b * p.1 - c;
        let dq = a * q.0 + b * q.1 - c;
        if dp <= 0.0 {
            out.push(p);
        }
        if (dp < 0.0 && dq > 0.0) || (dp > 0.0 && dq < 0.0) {
            let t = dp / (dp - dq);
            out.push((p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1)));
        }
    }
    if out.len() < 3 { Vec::new() } else { out }
}

/// Shrink a clockwise-on-screen convex polygon by `d` on every edge; `None` if nothing is left.
pub(super) fn inset(poly: &[P], d: f64) -> Option<Vec<P>> {
    let n = poly.len();
    let mut out = poly.to_vec();
    for i in 0..n {
        let p = poly[i];
        let q = poly[(i + 1) % n];
        let (dx, dy) = (q.0 - p.0, q.1 - p.1);
        let len = dx.hypot(dy);
        if len < 1e-9 {
            continue;
        }
        // Interior normal of a positive-shoelace polygon.
        let (nx, ny) = (-dy / len, dx / len);
        out = clip(&out, -nx, -ny, -(nx * p.0 + ny * p.1) - d);
        if out.is_empty() {
            return None;
        }
    }
    Some(out)
}

pub(super) fn contains(poly: &[P], p: P) -> bool {
    let n = poly.len();
    n >= 3
        && (0..n).all(|i| {
            let a = poly[i];
            let b = poly[(i + 1) % n];
            (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0) >= 0.0
        })
}

/// The x-extent of a convex polygon along the horizontal line `y`.
pub(super) fn chord(poly: &[P], y: f64) -> Option<(f64, f64)> {
    let n = poly.len();
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for i in 0..n {
        let p = poly[i];
        let q = poly[(i + 1) % n];
        if (p.1 - y) * (q.1 - y) > 0.0 || (p.1 - q.1).abs() < 1e-12 {
            continue;
        }
        let t = (y - p.1) / (q.1 - p.1);
        let x = p.0 + t * (q.0 - p.0);
        lo = lo.min(x);
        hi = hi.max(x);
    }
    (hi > lo).then_some((lo, hi))
}

pub(super) fn to_f32(poly: &[P]) -> Vec<(f32, f32)> {
    poly.iter().map(|&(x, y)| (x as f32, y as f32)).collect()
}

pub(super) fn rect(x: f64, y: f64, w: f64, h: f64) -> Vec<P> {
    vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)]
}
