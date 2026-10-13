//! Technocratic system marks for the bottom bar.
//!
//! One construction: a square, a constant stroke, open geometry, and a node only where a joint
//! needs a stop. Settings, Network, Account, Calendar, Terminal and Search share the same ink.
//! They are drawn here, in the shell's own ink, so a missing icon theme cannot blank the bar.
//!
//! The clock on the cluster stays words. The calendar mark belongs to the calendar the clock
//! opens, and the terminal mark is what a terminal chip wears when the theme has no picture.

use pixelkit_raster::{Painter, RasterKernel, Rect};

/// A system mark the bar can draw without a theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    Settings,
    Network,
    Account,
    Calendar,
    Terminal,
    Search,
}

impl Glyph {
    pub const ALL: [Glyph; 6] = [
        Glyph::Settings,
        Glyph::Network,
        Glyph::Account,
        Glyph::Calendar,
        Glyph::Terminal,
        Glyph::Search,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Glyph::Settings => "settings",
            Glyph::Network => "network",
            Glyph::Account => "account",
            Glyph::Calendar => "calendar",
            Glyph::Terminal => "terminal",
            Glyph::Search => "search",
        }
    }
}

/// The mark a missing theme icon should fall back to, when the application is one of ours.
///
/// Theme pictures win. This is only the chip that would otherwise be a question mark.
#[must_use]
pub fn glyph_for_app(app_id: &str) -> Option<Glyph> {
    let id = app_id.to_ascii_lowercase();
    if id.contains("terminal")
        || id.contains("kitty")
        || id.contains("konsole")
        || id.contains("alacritty")
        || id.contains("wezterm")
        || id.contains("ghostty")
    {
        Some(Glyph::Terminal)
    } else if id.contains("settings") || id.contains("control-center") {
        Some(Glyph::Settings)
    } else {
        None
    }
}

/// Draw `glyph` centred in `cell`, clipped to it.
pub fn draw(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    cell: Rect,
    glyph: Glyph,
    ink: u32,
) {
    let side = cell.w.min(cell.h);
    if side < 8 {
        return;
    }
    let origin = [
        (cell.x + (cell.w - side) / 2) as f32,
        (cell.y + (cell.h - side) / 2) as f32,
    ];
    let side = side as f32;
    let stroke = (side * 0.075).clamp(1.15, 2.6);
    painter.push_clip(cell);
    match glyph {
        Glyph::Settings => dial(painter, kernel, origin, side, stroke, ink),
        Glyph::Network => nodes(painter, kernel, origin, side, stroke, ink),
        Glyph::Account => identity(painter, kernel, origin, side, stroke, ink),
        Glyph::Calendar => plate(painter, kernel, origin, side, stroke, ink),
        Glyph::Terminal => prompt(painter, kernel, origin, side, stroke, ink),
        Glyph::Search => search(painter, kernel, origin, side, stroke, ink),
    }
    painter.pop_clip();
}

fn at(origin: [f32; 2], side: f32, u: f32, v: f32) -> [f32; 2] {
    [origin[0] + u * side, origin[1] + v * side]
}

fn line(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    a: [f32; 2],
    b: [f32; 2],
    stroke: f32,
    ink: u32,
) {
    painter.line_aa(kernel, a[0], a[1], b[0], b[1], stroke, ink, 255);
}

fn ring(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    c: [f32; 2],
    radius: f32,
    stroke: f32,
    ink: u32,
) {
    painter.stroke_circle_aa(kernel, c[0], c[1], radius, stroke, ink, 255);
}

fn dot(painter: &mut Painter<'_>, kernel: &mut RasterKernel, c: [f32; 2], radius: f32, ink: u32) {
    painter.fill_circle_aa(kernel, c[0], c[1], radius, ink, 255);
}

fn search(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    ring(
        painter,
        kernel,
        at(origin, side, 0.42, 0.42),
        side * 0.25,
        stroke,
        ink,
    );
    line(
        painter,
        kernel,
        at(origin, side, 0.60, 0.60),
        at(origin, side, 0.84, 0.84),
        stroke,
        ink,
    );
}

/// A closed rectangle whose corners meet. Butt caps stop at the endpoint, so each side runs
/// half a stroke past the corner and the joint fills.
fn frame(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    unit: [f32; 4],
    stroke: f32,
    ink: u32,
) {
    let [min_u, min_v, max_u, max_v] = unit;
    let top_left = at(origin, side, min_u, min_v);
    let top_right = at(origin, side, max_u, min_v);
    let bottom_right = at(origin, side, max_u, max_v);
    let bottom_left = at(origin, side, min_u, max_v);
    let pad = stroke * 0.5;
    let extend = |start: [f32; 2], end: [f32; 2]| {
        let dx = end[0] - start[0];
        let dy = end[1] - start[1];
        let len = dx.hypot(dy).max(0.001);
        let ux = dx / len * pad;
        let uy = dy / len * pad;
        ([start[0] - ux, start[1] - uy], [end[0] + ux, end[1] + uy])
    };
    for (start, end) in [
        (top_left, top_right),
        (top_right, bottom_right),
        (bottom_right, bottom_left),
        (bottom_left, top_left),
    ] {
        let (start, end) = extend(start, end);
        line(painter, kernel, start, end, stroke, ink);
    }
}

fn dial(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    let c = at(origin, side, 0.50, 0.52);
    ring(painter, kernel, c, side * 0.26, stroke, ink);
    ring(painter, kernel, c, side * 0.07, stroke, ink);
    for step in 0..8 {
        let angle = step as f32 * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
        let (sin, cos) = angle.sin_cos();
        let cardinal = step % 2 == 0;
        let inner = if cardinal { 0.32 } else { 0.34 };
        let outer = if cardinal { 0.46 } else { 0.42 };
        line(
            painter,
            kernel,
            [c[0] + cos * side * inner, c[1] + sin * side * inner],
            [c[0] + cos * side * outer, c[1] + sin * side * outer],
            stroke,
            ink,
        );
    }
}

fn nodes(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    let joints = [
        at(origin, side, 0.27, 0.30),
        at(origin, side, 0.73, 0.30),
        at(origin, side, 0.50, 0.74),
    ];
    line(painter, kernel, joints[0], joints[1], stroke, ink);
    line(painter, kernel, joints[1], joints[2], stroke, ink);
    line(painter, kernel, joints[2], joints[0], stroke, ink);
    let radius = (side * 0.045).max(stroke * 0.85);
    for joint in joints {
        dot(painter, kernel, joint, radius, ink);
    }
}

fn identity(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    ring(
        painter,
        kernel,
        at(origin, side, 0.50, 0.28),
        side * 0.11,
        stroke,
        ink,
    );
    // Ends sit below the joint. An arc whose ends rise reads as a smile at this size.
    let joint = at(origin, side, 0.50, 0.50);
    let ends = [
        at(origin, side, 0.50, 0.39),
        at(origin, side, 0.22, 0.78),
        at(origin, side, 0.78, 0.78),
    ];
    for end in ends {
        let dx = joint[0] - end[0];
        let dy = joint[1] - end[1];
        let len = dx.hypot(dy).max(0.001);
        let past = [
            joint[0] + dx / len * stroke * 0.5,
            joint[1] + dy / len * stroke * 0.5,
        ];
        line(painter, kernel, end, past, stroke, ink);
    }
}

fn plate(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    frame(
        painter,
        kernel,
        origin,
        side,
        [0.20, 0.22, 0.80, 0.84],
        stroke,
        ink,
    );
    line(
        painter,
        kernel,
        at(origin, side, 0.20, 0.40),
        at(origin, side, 0.80, 0.40),
        stroke,
        ink,
    );
    for u in [0.36, 0.64] {
        line(
            painter,
            kernel,
            at(origin, side, u, 0.10),
            at(origin, side, u, 0.26),
            stroke,
            ink,
        );
    }
    let radius = (side * 0.028).max(0.8);
    for v in [0.54, 0.70] {
        for u in [0.36, 0.50, 0.64] {
            dot(painter, kernel, at(origin, side, u, v), radius, ink);
        }
    }
}

fn prompt(
    painter: &mut Painter<'_>,
    kernel: &mut RasterKernel,
    origin: [f32; 2],
    side: f32,
    stroke: f32,
    ink: u32,
) {
    frame(
        painter,
        kernel,
        origin,
        side,
        [0.16, 0.22, 0.84, 0.78],
        stroke,
        ink,
    );
    let chevron = [
        at(origin, side, 0.32, 0.40),
        at(origin, side, 0.46, 0.50),
        at(origin, side, 0.32, 0.60),
    ];
    painter.stroke_polyline_aa(kernel, &chevron, stroke, ink, 255);
    line(
        painter,
        kernel,
        at(origin, side, 0.54, 0.60),
        at(origin, side, 0.70, 0.60),
        stroke,
        ink,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raster(glyph: Glyph) -> (Vec<u32>, u32) {
        let mut buffer = pixelkit_raster::WindowBuffer::new(64, 64);
        let ground = 0x002e_333d;
        let ink = 0x00f2_f2f2;
        {
            let mut painter = Painter::new(&mut buffer);
            painter.clear(ground);
            let mut kernel = RasterKernel::default();
            draw(
                &mut painter,
                &mut kernel,
                Rect::new(8, 8, 48, 48),
                glyph,
                ink,
            );
        }
        (buffer.pixels, ground)
    }

    fn mask(pixels: &[u32], ground: u32) -> Vec<bool> {
        pixels.iter().map(|px| *px != ground).collect()
    }

    #[test]
    fn each_mark_inks_the_cell_and_nothing_outside_it() {
        for glyph in Glyph::ALL {
            let (pixels, ground) = raster(glyph);
            let inked = mask(&pixels, ground);
            let inside = inked.iter().filter(|on| **on).count();
            assert!(
                (40..900).contains(&inside),
                "{} inked {inside} pixels",
                glyph.name()
            );
            for y in 0..64 {
                for x in 0..64 {
                    if !(8..56).contains(&x) || !(8..56).contains(&y) {
                        assert_eq!(
                            pixels[y * 64 + x],
                            ground,
                            "{} spilled to {x},{y}",
                            glyph.name()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_marks_are_different_pictures() {
        let masks: Vec<Vec<bool>> = Glyph::ALL
            .iter()
            .map(|glyph| {
                let (pixels, ground) = raster(*glyph);
                mask(&pixels, ground)
            })
            .collect();
        for i in 0..masks.len() {
            for j in (i + 1)..masks.len() {
                assert_ne!(
                    masks[i],
                    masks[j],
                    "{} and {} raster the same",
                    Glyph::ALL[i].name(),
                    Glyph::ALL[j].name()
                );
            }
        }
    }

    #[test]
    fn a_cramped_cell_draws_nothing() {
        let mut buffer = pixelkit_raster::WindowBuffer::new(16, 16);
        let mut painter = Painter::new(&mut buffer);
        painter.clear(0x0011_1111);
        let mut kernel = RasterKernel::default();
        draw(
            &mut painter,
            &mut kernel,
            Rect::new(0, 0, 6, 20),
            Glyph::Settings,
            0x00ff_ffff,
        );
        assert!(buffer.pixels.iter().all(|px| *px == 0x0011_1111));
    }

    #[test]
    fn known_applications_fall_back_to_their_mark() {
        assert_eq!(glyph_for_app("kitty"), Some(Glyph::Terminal));
        assert_eq!(glyph_for_app("org.kde.konsole"), Some(Glyph::Terminal));
        assert_eq!(
            glyph_for_app("org.wezfurlong.wezterm"),
            Some(Glyph::Terminal)
        );
        assert_eq!(
            glyph_for_app("org.templaros.Settings"),
            Some(Glyph::Settings)
        );
        assert_eq!(glyph_for_app("gnome-control-center"), Some(Glyph::Settings));
        assert_eq!(glyph_for_app("org.templaros.Files"), None);
        assert_eq!(glyph_for_app("org.mozilla.firefox"), None);
    }
}
