//! Fake window contents, painted on the CPU once per window size: premultiplied sRGB RGBA with
//! rounded, anti-aliased corners. Enough structure (title bars, text-like lines, images) that
//! the transition can be judged on something that reads like real windows.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Browser,
    Terminal,
}

pub const TITLE_H: i32 = 34;
pub const RADIUS: f32 = 10.0;

pub struct Canvas {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u8>,
}

impl Canvas {
    fn new(w: i32, h: i32) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w.max(1) * h.max(1) * 4) as usize],
        }
    }

    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, c: [u8; 3]) {
        let (x0, y0) = (x.max(0), y.max(0));
        let (x1, y1) = ((x + w).min(self.w), (y + h).min(self.h));
        for yy in y0..y1 {
            for xx in x0..x1 {
                let at = ((yy * self.w + xx) * 4) as usize;
                self.px[at..at + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
    }

    fn gradient(&mut self, x: i32, y: i32, w: i32, h: i32, a: [u8; 3], b: [u8; 3]) {
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let t = ((xx - x) as f32 / w.max(1) as f32 * 0.6
                    + (yy - y) as f32 / h.max(1) as f32 * 0.4)
                    .clamp(0.0, 1.0);
                let mix =
                    |i: usize| (f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t) as u8;
                let at = ((yy * self.w + xx) * 4) as usize;
                self.px[at..at + 4].copy_from_slice(&[mix(0), mix(1), mix(2), 255]);
            }
        }
    }

    fn pill(&mut self, x: i32, y: i32, w: i32, h: i32, c: [u8; 3]) {
        let r = h as f32 * 0.5;
        for yy in y.max(0)..(y + h).min(self.h) {
            for xx in x.max(0)..(x + w).min(self.w) {
                let px = xx as f32 + 0.5 - x as f32;
                let py = yy as f32 + 0.5 - y as f32;
                let cx = px.clamp(r, w as f32 - r);
                let d = (px - cx).hypot(py - r) - r;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    let at = ((yy * self.w + xx) * 4) as usize;
                    for (slot, &target) in self.px[at..at + 3].iter_mut().zip(&c) {
                        let current = f32::from(*slot);
                        *slot = (current + (f32::from(target) - current) * cov) as u8;
                    }
                    self.px[at + 3] = 255;
                }
            }
        }
    }

    /// Cut the window's rounded corners, premultiplying the edge.
    fn round_corners(&mut self, radius: f32) {
        let (w, h) = (self.w as f32, self.h as f32);
        for yy in 0..self.h {
            for xx in 0..self.w {
                let px = xx as f32 + 0.5;
                let py = yy as f32 + 0.5;
                let cx = px.clamp(radius, w - radius);
                let cy = py.clamp(radius, h - radius);
                let d = (px - cx).hypot(py - cy) - radius;
                let cov = (0.5 - d).clamp(0.0, 1.0);
                if cov < 1.0 {
                    let at = ((yy * self.w + xx) * 4) as usize;
                    for i in 0..4 {
                        self.px[at + i] = (f32::from(self.px[at + i]) * cov) as u8;
                    }
                }
            }
        }
    }
}

struct Lcg(u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0 >> 8
    }
    fn range(&mut self, lo: i32, hi: i32) -> i32 {
        lo + (self.next() % (hi - lo).max(1) as u32) as i32
    }
}

pub fn window(kind: Kind, w: i32, h: i32, seed: u32) -> Canvas {
    let mut c = Canvas::new(w, h);
    let mut rng = Lcg(seed.wrapping_mul(2_654_435_761) | 1);
    match kind {
        Kind::Browser => browser(&mut c, &mut rng),
        Kind::Terminal => terminal(&mut c, &mut rng),
    }
    c.round_corners(RADIUS);
    c
}

fn browser(c: &mut Canvas, rng: &mut Lcg) {
    let (w, h) = (c.w, c.h);
    c.fill(0, 0, w, h, [250, 250, 252]);
    c.fill(0, 0, w, TITLE_H + 40, [233, 233, 238]);
    c.pill(10, 8, 220, TITLE_H - 10, [255, 255, 255]);
    c.pill(240, 8, 180, TITLE_H - 10, [222, 222, 228]);
    for (i, col) in [[237, 106, 94], [245, 191, 79], [98, 197, 84]]
        .iter()
        .enumerate()
    {
        c.pill(w - 90 + i as i32 * 26, 11, 14, 14, *col);
    }
    c.pill(16, TITLE_H + 6, w - 32, 28, [255, 255, 255]);
    c.pill(30, TITLE_H + 15, 260, 10, [170, 170, 180]);
    let top = TITLE_H + 40;
    c.gradient(0, top, w, 220.min(h - top), [44, 92, 168], [36, 170, 160]);
    c.pill(60, top + 70, (w / 3).min(520), 34, [255, 255, 255]);
    c.pill(60, top + 122, (w / 4).min(380), 16, [210, 236, 240]);
    let side = (w / 4).clamp(160, 320);
    let mut y = top + 260;
    while y < h - 40 {
        if rng.range(0, 6) == 0 {
            let iw = rng.range(200, 420).min(w - side - 120);
            c.gradient(60, y, iw, 120, [214, 140, 74], [242, 205, 120]);
            y += 150;
            continue;
        }
        c.pill(60, y, rng.range(160, 360), 18, [40, 40, 52]);
        y += 34;
        for _ in 0..rng.range(2, 5) {
            let lw = rng.range((w - side) / 3, w - side - 120).max(40);
            c.pill(60, y, lw, 9, [150, 150, 160]);
            y += 20;
        }
        y += 22;
    }
    c.fill(w - side, top + 230, 1, h, [225, 225, 232]);
    let mut y = top + 260;
    while y < h - 60 {
        c.pill(w - side + 24, y, side - 60, 54, [238, 240, 246]);
        y += 72;
    }
}

fn terminal(c: &mut Canvas, rng: &mut Lcg) {
    let (w, h) = (c.w, c.h);
    c.fill(0, 0, w, h, [22, 24, 29]);
    c.fill(0, 0, w, TITLE_H, [43, 45, 50]);
    c.pill(w / 2 - 70, 12, 140, 10, [120, 122, 130]);
    for (i, col) in [[237, 106, 94], [245, 191, 79], [98, 197, 84]]
        .iter()
        .enumerate()
    {
        c.pill(12 + i as i32 * 22, 10, 13, 13, *col);
    }
    let colors = [
        [120, 220, 140],
        [210, 214, 222],
        [110, 190, 230],
        [230, 190, 110],
        [150, 152, 160],
    ];
    let mut y = TITLE_H + 14;
    while y < h - 18 {
        let mut x = 14;
        if rng.range(0, 4) == 0 {
            c.fill(x, y, 18, 10, [120, 220, 140]);
            x += 26;
        }
        for _ in 0..rng.range(1, 7) {
            let word = rng.range(3, 12) * 7;
            if x + word > w - 14 {
                break;
            }
            let col = colors[rng.range(0, colors.len() as i32) as usize];
            c.fill(x, y, word, 10, col);
            x += word + 7;
        }
        y += 19;
    }
}
