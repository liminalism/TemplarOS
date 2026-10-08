//! The essence transition: a window deconstructing into essence that flows, through the gaps
//! between the windows that stay, to the Homegarden — and the exact reverse to bring it back.
//!
//! # What the planner decides, once per trigger
//!
//! The screen is divided into a coarse **field** (cells of at least [`MIN_CELL_PX`], never more
//! than [`MAX_FIELD_CELLS`] along the long edge, so the work is bounded at any resolution). A
//! cell is *covered* when another visible window is over it and *garden* when the Homegarden
//! shows there. Every cell gets a cost — garden cheap, covered expensive, garden hugging a window
//! edge slightly dearer so streams run down the middle of a gap — and one Dijkstra pass from the
//! sink gives every cell its **path of least resistance** as a parent pointer.
//!
//! * [`Sink::Garden`] (the desktop button): every garden cell is a sink, the deeper into a gap
//!   the cheaper, so essence over another window pours to the nearest gap and soaks in there.
//! * [`Sink::Card`] (minimize): the window's Home card is the sink; essence runs through the gaps
//!   into it.
//!
//! The departing window breaks into **motes**, one per cell-sized piece. Each mote's path is the
//! parent chain from its cell, resampled to [`PATH_POINTS`] control points. The timeline, in
//! departure progress `u ∈ [0, 1]` over [`Timing::duration_ms`]:
//!
//! * a piece over another window vanishes within [`Timing::covered_ms`] (about 50 ms), the
//!   pieces nearest a gap first; its mote travels hidden by the windows it passes and emerges
//!   only where the garden shows;
//! * a piece over the garden *holds* until the essence from upstream has reached its gap, then
//!   dissolves itself and follows (with [`Sink::Card`], pieces nearer the card leave last).
//!
//! Arriving is the same plan run backwards: `u` from 1 to 0. A run can be reversed at any
//! moment ([`Run::reverse`]) without a jump, which is what makes rapid minimize/restore cheap
//! and calm: no new plan, no restart.
//!
//! # What the GPU does, every frame
//!
//! One uniform write (the progress) and two draws: the window's own quad through
//! `essence_body.wgsl` (one window sample, one field sample, one noise sample per pixel) and the
//! motes, one instanced quad each, through `essence_motes.wgsl`. Nothing is recomputed per
//! frame; finished motes collapse to zero-area quads, and nothing at all is drawn once the run
//! ends. [`Plan::bounds`] is the damage rectangle for the whole run.

use crate::geometry::Rect;
use std::cmp::Reverse;
use std::collections::VecDeque;

/// Control points per mote path (Catmull-Rom in the vertex shader).
pub const PATH_POINTS: usize = 8;
/// Bytes per mote instance: [`PATH_POINTS`] screen-normalized `unorm16x2` points, then
/// `unorm16x4` (release, arrive, source x, source y).
pub const MOTE_BYTES: usize = PATH_POINTS * 4 + 8;
/// The smallest field cell, in logical pixels — also the size of one mote.
pub const MIN_CELL_PX: u32 = 16;
/// The most field cells along the screen's long edge.
pub const MAX_FIELD_CELLS: u32 = 160;
/// The most motes one window breaks into: past this, pieces grow to 2×2 (3×3, …) field cells.
/// Planning cost and instance count both scale with it.
pub const MAX_MOTES: usize = 4096;
/// The most obstacle rectangles the shaders test exactly; the field covers the rest coarsely.
pub const MAX_OBSTACLES: usize = 16;
/// Bytes of the `Essence` uniform block (`shaders/essence.wgsl`).
pub const UNIFORM_BYTES: usize = 128 + MAX_OBSTACLES * 16;

/// The final fraction of the run in which the last essence soaks into the garden.
const TAIL: f32 = 0.12;
/// Cost per cell, in tenths: garden, covered. Covered is dear enough that essence leaves a
/// window by its nearest edge and never takes a short cut under one when a gap goes round:
/// a detour of fifty garden cells is cheaper than crossing a single covered one.
const COST_GARDEN: u32 = 10;
const COST_COVERED: u32 = 500;
/// Garden within this many cells of a window edge costs more, and a garden sink this close
/// to an edge is a worse place to soak in.
const CLEAR_MAX: u8 = 3;
const GARDEN_DEPTH_COST: u32 = 8;
/// How fast a held gap's hold time falls off per cell away from where essence enters it.
const HOLD_DECAY: f32 = 0.035;
const NONE: u32 = u32::MAX;
const UNSET: u32 = u32::MAX - 1;

/// Piece edge for a window: one field cell, or a few when that would exceed [`MAX_MOTES`].
fn piece_size(grid: Grid, window: Rect) -> f32 {
    let cells = (window.w.max(0) as f32 * grid.inv_cell).ceil()
        * (window.h.max(0) as f32 * grid.inv_cell).ceil();
    let stride = (cells / MAX_MOTES as f32).sqrt().ceil().max(1.0);
    grid.cell * stride
}

/// Where the essence goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sink {
    /// Into the Homegarden wherever it shows (the desktop button, "go home").
    Garden,
    /// Into the window's card on Home (minimize/park), in screen logical pixels.
    Card(Rect),
}

/// How long a run takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    /// The whole run, clamped to `[MIN_MS, MAX_MS]`.
    pub duration_ms: f32,
    /// How quickly a piece over another window vanishes.
    pub covered_ms: f32,
}

impl Timing {
    pub const MIN_MS: f32 = 50.0;
    pub const MAX_MS: f32 = 300.0;
    pub const DEFAULT: Timing = Timing {
        duration_ms: 220.0,
        covered_ms: 50.0,
    };

    /// The same timing within its limits: a run between 50 and 300 ms, and the covered
    /// vanish never more than half of it.
    #[must_use]
    pub fn clamped(self) -> Self {
        let duration_ms = finite_or(self.duration_ms, Self::DEFAULT.duration_ms)
            .clamp(Self::MIN_MS, Self::MAX_MS);
        let covered_ms =
            finite_or(self.covered_ms, Self::DEFAULT.covered_ms).clamp(8.0, duration_ms * 0.5);
        Self {
            duration_ms,
            covered_ms,
        }
    }

    /// The covered vanish as a fraction of the run.
    #[must_use]
    pub fn covered_fraction(self) -> f32 {
        let t = self.clamped();
        t.covered_ms / t.duration_ms
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::DEFAULT
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// Everything the planner needs: where the window is and what stays on screen around it.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    /// The output, in global logical pixels.
    pub screen: Rect,
    /// The window leaving (or, run backwards, arriving).
    pub window: Rect,
    /// Every other window visible while it leaves. Windows *below* it are what it flows
    /// around; windows above it hide it anyway, and listing them is harmless.
    pub obstacles: &'a [Rect],
    pub sink: Sink,
    pub timing: Timing,
}

/// One piece of the window and the path it takes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mote {
    /// Control points, screen-normalized (`0..=65535` across the output).
    pub path: [[u16; 2]; PATH_POINTS],
    /// Departure progress at which the piece leaves the window, and at which it has soaked in.
    pub release: u16,
    pub arrive: u16,
    /// The top-left of the piece's source square, screen-normalized.
    pub source: [u16; 2],
}

/// A planned run: the field texture, the motes and the bounds. Built by [`Planner::plan`].
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub screen: Rect,
    pub window: Rect,
    pub sink: Option<Sink>,
    pub timing: Timing,
    /// Field cell edge in logical pixels.
    pub cell_px: u32,
    /// Edge of one piece of the window (one mote): a whole number of field cells.
    pub piece_px: u32,
    pub cols: u32,
    pub rows: u32,
    /// `cols × rows` RGBA8 texels, row-major: R = when a covered piece vanishes, G = when a
    /// garden piece vanishes (both as departure progress, defined in every cell so linear
    /// filtering stays smooth), B = essence entering a gap (glow), A = covered.
    pub field: Vec<u8>,
    pub motes: Vec<Mote>,
    /// The obstacles the shaders test exactly, at most [`MAX_OBSTACLES`].
    pub obstacles: Vec<Rect>,
    /// Everything the run can touch: the window and every mote path. The damage rectangle.
    pub bounds: Rect,
}

impl Plan {
    /// The motes as the instance vertex buffer (`MOTE_BYTES` each, little-endian).
    #[must_use]
    pub fn mote_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.motes.len() * MOTE_BYTES);
        for mote in &self.motes {
            for point in &mote.path {
                out.extend_from_slice(&point[0].to_le_bytes());
                out.extend_from_slice(&point[1].to_le_bytes());
            }
            for value in [mote.release, mote.arrive, mote.source[0], mote.source[1]] {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        out
    }

    /// The `Essence` uniform block for departure progress `u`.
    #[must_use]
    pub fn uniform(&self, u: f32, style: &Style) -> [u8; UNIFORM_BYTES] {
        let mut w = UniformWriter::default();
        w.vec4(self.screen.to_f32());
        w.vec4(self.window.to_f32());
        let cell = self.cell_px as f32;
        let piece = self.piece_px as f32;
        w.vec4([
            self.screen.x as f32,
            self.screen.y as f32,
            cell * self.cols as f32,
            cell * self.rows as f32,
        ]);
        let fast = self.timing.covered_fraction();
        w.vec4([u.clamp(0.0, 1.0), piece, style.edge, fast]);
        w.vec4([
            style.glow[0],
            style.glow[1],
            style.glow[2],
            style.glow_strength,
        ]);
        w.vec4([
            fast * 0.3,
            style.free_jitter,
            style.mote_glow,
            style.mote_shrink,
        ]);
        w.vec4([
            finite_or(style.grain_px, Style::DEFAULT.grain_px)
                .clamp(Style::MIN_GRAIN_PX, Style::MAX_GRAIN_PX),
            finite_or(style.dust_density, Style::DEFAULT.dust_density).clamp(0.0, 1.0),
            finite_or(style.dust_spread, Style::DEFAULT.dust_spread).max(0.0),
            0.0,
        ]);
        let sink = match self.sink {
            Some(Sink::Card(_)) => 1,
            _ => 0,
        };
        w.uvec4([
            self.obstacles.len() as u32,
            sink,
            self.motes.len() as u32,
            0,
        ]);
        for index in 0..MAX_OBSTACLES {
            let rect = self.obstacles.get(index).copied().unwrap_or_default();
            w.vec4(rect.to_f32());
        }
        w.finish()
    }
}

/// The look of a run, independent of its geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    /// Essence colour, linear RGB.
    pub glow: [f32; 3],
    /// How strongly a dissolving edge and a travelling mote glow.
    pub glow_strength: f32,
    /// The width of a dissolving edge, in progress.
    pub edge: f32,
    /// Noise jitter on a garden piece's vanish time, in progress.
    pub free_jitter: f32,
    /// How far a mote turns from window pixels to essence by the end of its path.
    pub mote_glow: f32,
    /// How small a mote is at the end of its path, relative to a cell.
    pub mote_shrink: f32,
    /// Diameter of one grain of essence, in logical pixels. A mote leaves the window as its
    /// piece and crumbles into grains this size; the dissolving edge of the window is grained
    /// at the same scale. Not a user setting: it is fixed here, and tuned in the sandbox.
    pub grain_px: f32,
    /// The share of grain sites that hold a grain once a piece has crumbled, `0..=1`.
    pub dust_density: f32,
    /// How much wider than its piece a travelling cloud of grains spreads, as a multiple.
    pub dust_spread: f32,
}

impl Style {
    /// The range `grain_px` is held to: below a pixel a grain is a faint shimmer in the air,
    /// above a piece it is a chunk.
    pub const MIN_GRAIN_PX: f32 = 0.5;
    pub const MAX_GRAIN_PX: f32 = 24.0;

    pub const DEFAULT: Style = Style {
        glow: [0.62, 0.86, 0.74],
        glow_strength: 0.6,
        edge: 0.07,
        free_jitter: 0.12,
        mote_glow: 0.85,
        mote_shrink: 0.4,
        grain_px: 1.5,
        dust_density: 0.65,
        dust_spread: 1.25,
    };
}

impl Default for Style {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Which way a run is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Leaving: progress rises to 1.
    Depart,
    /// Coming back: progress falls to 0.
    Arrive,
}

/// A run in flight: the progress to feed [`Plan::uniform`] and how it advances.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Run {
    pub u: f32,
    pub direction: Direction,
    pub duration_ms: f32,
}

impl Run {
    #[must_use]
    pub fn depart(timing: Timing) -> Self {
        Self {
            u: 0.0,
            direction: Direction::Depart,
            duration_ms: timing.clamped().duration_ms,
        }
    }

    #[must_use]
    pub fn arrive(timing: Timing) -> Self {
        Self {
            u: 1.0,
            direction: Direction::Arrive,
            duration_ms: timing.clamped().duration_ms,
        }
    }

    /// Advance by `dt_ms` of wall time. Returns whether the run is still going.
    pub fn advance(&mut self, dt_ms: f32) -> bool {
        let step = finite_or(dt_ms, 0.0).max(0.0) / self.duration_ms;
        self.u = match self.direction {
            Direction::Depart => (self.u + step).min(1.0),
            Direction::Arrive => (self.u - step).max(0.0),
        };
        !self.finished()
    }

    /// Turn around from where it is: a window called back mid-departure returns along the same
    /// paths from exactly the state it had reached.
    pub fn reverse(&mut self) {
        self.direction = match self.direction {
            Direction::Depart => Direction::Arrive,
            Direction::Arrive => Direction::Depart,
        };
    }

    #[must_use]
    pub fn finished(&self) -> bool {
        match self.direction {
            Direction::Depart => self.u >= 1.0,
            Direction::Arrive => self.u <= 0.0,
        }
    }
}

/// The planner with its scratch buffers: after the first plan of a given size, planning again
/// allocates nothing beyond what the returned [`Plan`] already holds.
#[derive(Debug, Default)]
pub struct Planner {
    covered: Vec<bool>,
    clearance: Vec<u8>,
    cover_depth: Vec<u16>,
    dist: Vec<u32>,
    parent: Vec<u32>,
    chain: Vec<u32>,
    anchor: Vec<u32>,
    buckets: Vec<Vec<u32>>,
    queue: VecDeque<u32>,
    rects: Vec<Rect>,
    garden: Vec<bool>,
    /// Summed-area table of covered cells, `(cols + 1) × (rows + 1)`.
    covered_sum: Vec<u32>,
    covered_v: Vec<f32>,
    free_v: Vec<f32>,
    hold: Vec<f32>,
    known: Vec<bool>,
    nearest: Vec<u16>,
    no_garden: bool,
    walks: Vec<Walk>,
    points: Vec<(f32, f32)>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Walk {
    path: [(f32, f32); PATH_POINTS],
    length: f32,
    covered_run: f32,
    entry: u32,
    start_cell: u32,
    covered: bool,
    source: (f32, f32),
}

/// The field geometry for one screen: every index computation in one place.
#[derive(Clone, Copy, Debug)]
struct Grid {
    screen: Rect,
    cell: f32,
    inv_cell: f32,
    inv_w: f32,
    inv_h: f32,
    cols: u32,
    rows: u32,
}

impl Grid {
    fn new(screen: Rect) -> Self {
        let long = screen.w.max(screen.h).max(1) as u32;
        let cell = MIN_CELL_PX.max(long.div_ceil(MAX_FIELD_CELLS));
        let cols = (screen.w.max(1) as u32).div_ceil(cell);
        let rows = (screen.h.max(1) as u32).div_ceil(cell);
        Self {
            screen,
            cell: cell as f32,
            inv_cell: 1.0 / cell as f32,
            inv_w: 1.0 / screen.w.max(1) as f32,
            inv_h: 1.0 / screen.h.max(1) as f32,
            cols,
            rows,
        }
    }

    fn len(self) -> usize {
        (self.cols * self.rows) as usize
    }

    fn center(self, index: u32) -> (f32, f32) {
        let col = index % self.cols;
        let row = index / self.cols;
        (
            self.screen.x as f32 + (col as f32 + 0.5) * self.cell,
            self.screen.y as f32 + (row as f32 + 0.5) * self.cell,
        )
    }

    /// The cell's centre, pulled inside the screen: the last row and column overhang the
    /// output, and what they show is what the screen's own edge shows.
    fn clamped_center(self, index: u32) -> (f32, f32) {
        let (x, y) = self.center(index);
        (
            x.min(self.screen.right() as f32 - 0.5),
            y.min(self.screen.bottom() as f32 - 0.5),
        )
    }

    fn cell_at(self, x: f32, y: f32) -> u32 {
        let col = ((x - self.screen.x as f32) * self.inv_cell)
            .floor()
            .clamp(0.0, (self.cols - 1) as f32) as u32;
        let row = ((y - self.screen.y as f32) * self.inv_cell)
            .floor()
            .clamp(0.0, (self.rows - 1) as f32) as u32;
        row * self.cols + col
    }

    /// The 8-neighbourhood with step lengths in tenths (10 straight, 14 diagonal).
    fn neighbours(self, index: u32) -> impl Iterator<Item = (u32, u32)> {
        const STEPS: [(i64, i64, u32); 8] = [
            (-1, 0, 10),
            (1, 0, 10),
            (0, -1, 10),
            (0, 1, 10),
            (-1, -1, 14),
            (1, -1, 14),
            (-1, 1, 14),
            (1, 1, 14),
        ];
        let col = i64::from(index % self.cols);
        let row = i64::from(index / self.cols);
        let (cols, rows) = (i64::from(self.cols), i64::from(self.rows));
        STEPS.into_iter().filter_map(move |(dx, dy, step)| {
            let (c, r) = (col + dx, row + dy);
            (c >= 0 && r >= 0 && c < cols && r < rows).then(|| ((r * cols + c) as u32, step))
        })
    }

    fn normalize(self, (x, y): (f32, f32)) -> [u16; 2] {
        [
            unorm16((x - self.screen.x as f32) * self.inv_w),
            unorm16((y - self.screen.y as f32) * self.inv_h),
        ]
    }
}

fn unorm16(value: f32) -> u16 {
    (value.clamp(0.0, 1.0) * 65535.0).round() as u16
}

fn unorm8(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// A small, stable integer hash: the same cell always jitters the same way.
fn hash01(value: u32) -> f32 {
    let mut x = value.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    (x >> 8) as f32 / 16_777_216.0
}

impl Planner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Plan a run. Never fails: a degenerate request (an empty window, a window off screen)
    /// yields a plan with no motes, which draws nothing.
    pub fn plan(&mut self, request: &Request<'_>) -> Plan {
        let mut plan = Plan::default();
        self.plan_into(request, &mut plan);
        plan
    }

    /// [`Self::plan`] into an existing plan, reusing its buffers.
    pub fn plan_into(&mut self, request: &Request<'_>, plan: &mut Plan) {
        let timing = request.timing.clamped();
        let screen = if request.screen.is_empty() {
            Rect::new(0, 0, 1, 1)
        } else {
            request.screen
        };
        let grid = Grid::new(screen);
        plan.screen = screen;
        plan.window = request.window;
        plan.sink = Some(request.sink);
        plan.timing = timing;
        plan.cell_px = grid.cell as u32;
        plan.cols = grid.cols;
        plan.rows = grid.rows;
        plan.motes.clear();
        plan.obstacles.clear();
        plan.field.clear();
        plan.field.resize(grid.len() * 4, 0);

        self.classify(grid, request);
        self.route(grid, request.sink);
        self.anchor.clear();
        self.anchor.resize(grid.len(), UNSET);
        let piece = piece_size(grid, request.window.intersect(grid.screen));
        plan.piece_px = piece as u32;
        self.walk(grid, request, piece);
        self.schedule(grid, request.sink, timing, plan);
        self.write_field(grid, plan);
        // With no garden anywhere nothing counts as covered, exactly too: the shaders must
        // agree with the field, or the whole window would take the 50 ms path.
        if !self.no_garden {
            choose_obstacles(request, plan);
        }
    }

    /// Covered cells, garden clearance from window edges, and depth under cover.
    fn classify(&mut self, grid: Grid, request: &Request<'_>) {
        let n = grid.len();
        self.covered.clear();
        self.covered.resize(n, false);
        self.rects.clear();
        self.rects.extend(
            request
                .obstacles
                .iter()
                .map(|rect| rect.intersect(grid.screen))
                .filter(|rect| !rect.is_empty()),
        );
        let mut any_garden = false;
        for index in 0..n as u32 {
            let (x, y) = grid.clamped_center(index);
            let covered = self.rects.iter().any(|rect| rect.contains(x, y));
            self.covered[index as usize] = covered;
            any_garden |= !covered;
        }
        // A screen with no garden anywhere (a maximized window behind): the essence soaks in
        // where it is, which is a plain dissolve.
        self.no_garden = !any_garden;
        if self.no_garden {
            self.covered.fill(false);
        }
        // Clearance: garden cells' distance (in cells, capped) from the nearest covered cell.
        bfs_distance(
            grid,
            &self.covered,
            &mut self.queue,
            &mut self.nearest,
            u16::from(CLEAR_MAX),
        );
        self.clearance.clear();
        self.clearance.extend(
            self.nearest
                .iter()
                .map(|&d| d.min(u16::from(CLEAR_MAX)) as u8),
        );
        // Depth under cover: covered cells' distance from the nearest garden cell.
        self.garden.clear();
        self.garden.extend(self.covered.iter().map(|&c| !c));
        bfs_distance(
            grid,
            &self.garden,
            &mut self.queue,
            &mut self.cover_depth,
            u16::MAX,
        );
        // Covered-cell counts over any box in O(1), for the line tests.
        let (cols, rows) = (grid.cols as usize, grid.rows as usize);
        self.covered_sum.clear();
        self.covered_sum.resize((cols + 1) * (rows + 1), 0);
        for row in 0..rows {
            let mut line = 0;
            for col in 0..cols {
                line += u32::from(self.covered[row * cols + col]);
                self.covered_sum[(row + 1) * (cols + 1) + col + 1] =
                    self.covered_sum[row * (cols + 1) + col + 1] + line;
            }
        }
    }

    /// Covered cells in the inclusive cell box spanned by two cells.
    fn covered_in_box(&self, grid: Grid, a: u32, b: u32) -> (u32, u32) {
        let (ac, ar) = (a % grid.cols, a / grid.cols);
        let (bc, br) = (b % grid.cols, b / grid.cols);
        let (c0, c1) = (ac.min(bc) as usize, ac.max(bc) as usize + 1);
        let (r0, r1) = (ar.min(br) as usize, ar.max(br) as usize + 1);
        let w = grid.cols as usize + 1;
        let sum = &self.covered_sum;
        let count = sum[r1 * w + c1] + sum[r0 * w + c0] - sum[r0 * w + c1] - sum[r1 * w + c0];
        (count, ((c1 - c0) * (r1 - r0)) as u32)
    }

    fn cell_cost(&self, index: usize) -> u32 {
        if self.covered[index] {
            return COST_COVERED;
        }
        match self.clearance[index] {
            1 => COST_GARDEN + 6,
            2 => COST_GARDEN + 2,
            _ => COST_GARDEN,
        }
    }

    /// One Dijkstra pass from the sink: `dist` and the parent forest.
    fn route(&mut self, grid: Grid, sink: Sink) {
        let n = grid.len();
        self.dist.clear();
        self.dist.resize(n, u32::MAX);
        self.parent.clear();
        self.parent.resize(n, NONE);
        // Dial's algorithm: edge costs are small integers, so a ring of buckets one longer than
        // the dearest edge replaces the binary heap.
        let ring = (14 * COST_COVERED / 10 + 1) as usize;
        self.buckets.resize_with(ring, Vec::new);
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        let mut queued = 0usize;
        match sink {
            Sink::Garden => {
                for index in 0..n {
                    if !self.covered[index] {
                        let shallow = u32::from(CLEAR_MAX.saturating_sub(self.clearance[index]));
                        self.dist[index] = shallow * GARDEN_DEPTH_COST;
                        self.buckets[self.dist[index] as usize % ring].push(index as u32);
                        queued += 1;
                    }
                }
            }
            Sink::Card(card) => {
                for index in 0..n as u32 {
                    let (x, y) = grid.clamped_center(index);
                    if card.contains(x, y) {
                        self.dist[index as usize] = 0;
                        self.buckets[0].push(index);
                        queued += 1;
                    }
                }
                if queued == 0 {
                    let (x, y) = card.center();
                    let index = grid.cell_at(x, y);
                    self.dist[index as usize] = 0;
                    self.buckets[0].push(index);
                    queued += 1;
                }
            }
        }
        let mut dist = 0u32;
        while queued > 0 {
            let slot = dist as usize % ring;
            while let Some(index) = self.buckets[slot].pop() {
                queued -= 1;
                if self.dist[index as usize] != dist {
                    continue;
                }
                let here = self.cell_cost(index as usize);
                for (next, step) in grid.neighbours(index) {
                    let candidate = dist + step * (here + self.cell_cost(next as usize)) / 20;
                    if candidate < self.dist[next as usize] {
                        self.dist[next as usize] = candidate;
                        self.parent[next as usize] = index;
                        self.buckets[candidate as usize % ring].push(next);
                        queued += 1;
                    }
                }
            }
            dist += 1;
        }
    }

    /// Any-angle straightening of the parent forest (Theta*-style), on demand: a cell's anchor
    /// is its parent's anchor when the straight line there is clear, otherwise its parent. Only
    /// cells some mote's path actually reaches are ever resolved, each once; a mote then
    /// follows a few anchors instead of a staircase of cells.
    fn anchor_of(&mut self, grid: Grid, cell: u32) -> u32 {
        let mut at = cell;
        self.chain.clear();
        while self.anchor[at as usize] == UNSET {
            let parent = self.parent[at as usize];
            if parent == NONE {
                self.anchor[at as usize] = NONE;
                break;
            }
            self.chain.push(at);
            at = parent;
        }
        while let Some(next) = self.chain.pop() {
            let parent = self.parent[next as usize];
            let skip = self.anchor[parent as usize];
            let clear = skip != NONE && self.line_clear(grid, next, skip);
            self.anchor[next as usize] = if clear { skip } else { parent };
        }
        self.anchor[cell as usize]
    }

    /// Whether the segment between two cell centres keeps to the rule every path keeps: it may
    /// start under cover, but once it reaches the garden it never goes under a window again.
    /// Most segments are settled by the box they span alone — no cover in it, or nothing but
    /// cover — and only the rest are walked, one sample per cell.
    fn line_clear(&self, grid: Grid, from: u32, to: u32) -> bool {
        let starts_covered = self.covered[from as usize];
        let (count, area) = self.covered_in_box(grid, from, to);
        if count == 0 || (starts_covered && count == area) {
            return true;
        }
        if !starts_covered && self.covered[to as usize] {
            return false;
        }
        let (a, b) = (grid.center(from), grid.center(to));
        let mut under_cover = starts_covered;
        let steps = (distance(a, b) / grid.cell).ceil().max(1.0) as u32;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let cell = grid.cell_at(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            if self.covered[cell as usize] {
                if !under_cover {
                    return false;
                }
            } else {
                under_cover = false;
            }
        }
        true
    }

    /// Where the segment `a → b`, starting under cover, first reaches the garden: the cell and
    /// the distance along the segment.
    fn cover_exit(&self, grid: Grid, a: (f32, f32), b: (f32, f32)) -> Option<(u32, f32)> {
        let length = distance(a, b);
        let steps = (length / grid.cell).ceil().max(1.0) as u32;
        (0..=steps).find_map(|step| {
            let t = step as f32 / steps as f32;
            let cell = grid.cell_at(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            (!self.covered[cell as usize]).then_some((cell, length * t))
        })
    }

    /// Follow every mote's anchors once and fit its control points.
    fn walk(&mut self, grid: Grid, request: &Request<'_>, piece: f32) {
        self.walks.clear();
        let window = request.window.intersect(grid.screen);
        if window.is_empty() {
            return;
        }
        let mcols = (window.w as f32 / piece).ceil() as i32;
        let mrows = (window.h as f32 / piece).ceil() as i32;
        let card = match request.sink {
            Sink::Card(card) => Some(card),
            Sink::Garden => None,
        };
        let (wx, wy) = (request.window.x as f32, request.window.y as f32);
        let (ww, wh) = (
            request.window.w.max(1) as f32,
            request.window.h.max(1) as f32,
        );
        for row in 0..mrows {
            for col in 0..mcols {
                let x0 = window.x as f32 + col as f32 * piece;
                let y0 = window.y as f32 + row as f32 * piece;
                let x1 = (x0 + piece).min(window.right() as f32);
                let y1 = (y0 + piece).min(window.bottom() as f32);
                let start = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
                // A piece of several cells is centred on a cell corner: settle which cell it
                // starts in by leaning half a pixel toward its origin.
                let start_cell = grid.cell_at(start.0 - 0.5, start.1 - 0.5);
                self.points.clear();
                self.points.push(start);
                let covered = self.covered[start_cell as usize];
                let mut covered_run = 0.0;
                let mut entry = if covered { NONE } else { start_cell };
                let mut length = 0.0;
                let mut at = start_cell;
                let mut previous = start;
                loop {
                    let next = self.anchor_of(grid, at);
                    if next == NONE {
                        break;
                    }
                    at = next;
                    let point = grid.center(at);
                    if entry == NONE
                        && let Some((cell, along)) = self.cover_exit(grid, previous, point)
                    {
                        entry = cell;
                        covered_run = length + along;
                    }
                    length += distance(previous, point);
                    previous = point;
                    self.points.push(point);
                }
                // Into the card, each piece lands where it sits in the thumbnail: the window
                // reassembles small rather than collapsing to a point.
                if let Some(card) = card {
                    let target = (
                        card.x as f32 + (start.0 - wx) / ww * card.w as f32,
                        card.y as f32 + (start.1 - wy) / wh * card.h as f32,
                    );
                    length += distance(previous, target);
                    self.points.push(target);
                }
                if entry == NONE {
                    entry = at;
                    covered_run = length;
                }
                let path = fit_points(&mut self.points);
                self.walks.push(Walk {
                    path,
                    length,
                    covered_run,
                    entry,
                    start_cell,
                    covered,
                    source: (x0, y0),
                });
            }
        }
    }

    /// Every mote path's extent, padded by how far a travelling mote is drawn out along it.
    fn path_bounds(&self, piece_px: u32) -> Rect {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for walk in &self.walks {
            for &(x, y) in &walk.path {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        if x0 > x1 {
            return Rect::default();
        }
        let pad = piece_px as f32 * 2.5;
        Rect::new(
            (x0 - pad) as i32,
            (y0 - pad) as i32,
            (x1 - x0 + 2.0 * pad) as i32,
            (y1 - y0 + 2.0 * pad) as i32,
        )
    }

    /// Release and arrival times for every mote, the hold of every gap, the vanish times of
    /// every cell.
    fn schedule(&mut self, grid: Grid, sink: Sink, timing: Timing, plan: &mut Plan) {
        let n = grid.len();
        let fast = timing.covered_fraction();
        let span = (1.0 - TAIL - fast).max(0.2);
        let diagonal = (grid.screen.w as f32).hypot(grid.screen.h as f32);
        let reference = self
            .walks
            .iter()
            .map(|w| w.length)
            .fold(0.25 * diagonal, f32::max);
        let max_depth = self
            .walks
            .iter()
            .filter(|w| w.covered)
            .map(|w| self.cover_depth[w.start_cell as usize])
            .max()
            .unwrap_or(1)
            .max(1);

        // Covered vanish: the cells nearest a gap pour out first. This is when a piece *starts*
        // to go; with the shader's ±0.15 jitter and 0.3 dissolve width it is gone by `fast`.
        self.covered_v.clear();
        self.covered_v.extend((0..n).map(|index| {
            let depth = f32::from(self.cover_depth[index].min(max_depth)) / f32::from(max_depth);
            fast * (0.05 + 0.5 * depth)
        }));

        // Covered motes, and the time each gap must hold until.
        self.hold.clear();
        self.hold.resize(n, 0.0);
        plan.bounds = plan
            .window
            .intersect(grid.screen)
            .union(self.path_bounds(plan.piece_px));
        for (index, walk) in self.walks.iter().enumerate() {
            if !walk.covered {
                continue;
            }
            let jitter = (hash01(index as u32) - 0.5) * 0.2 * fast;
            let release =
                (self.covered_v[walk.start_cell as usize] + 0.15 * fast + jitter).clamp(0.0, fast);
            let travel = (span * walk.length / reference).max(0.06);
            let arrive = (release + travel).min(1.0);
            let entry = walk.entry as usize;
            let passes = match sink {
                Sink::Garden => arrive,
                Sink::Card(_) => {
                    release + travel * (walk.covered_run / walk.length.max(1.0)) + 0.08
                }
            };
            self.hold[entry] = self.hold[entry].max(passes);
            plan.motes.push(mote(grid, walk, release, arrive));
        }

        // A held gap holds its neighbourhood too, falling off with distance.
        spread_max(grid, &mut self.hold, &self.covered, HOLD_DECAY);

        // Garden vanish.
        let max_dist = self
            .walks
            .iter()
            .filter(|w| !w.covered)
            .map(|w| self.dist[w.start_cell as usize])
            .max()
            .unwrap_or(1)
            .max(1) as f32;
        self.free_v.clear();
        self.free_v.resize(n, 0.0);
        self.known.clear();
        self.known.resize(n, false);
        for index in 0..n {
            if self.covered[index] {
                continue;
            }
            let base = match sink {
                Sink::Garden => 0.40 + 0.10 * hash01(index as u32 ^ 0x5bd1),
                Sink::Card(_) => {
                    let closeness = 1.0 - (self.dist[index] as f32 / max_dist).min(1.0);
                    0.28 + 0.42 * closeness
                }
            };
            let held = if self.hold[index] > 0.0 {
                self.hold[index] + 0.03
            } else {
                0.0
            };
            self.free_v[index] = base.max(held).clamp(fast + 0.05, 1.0 - TAIL);
            self.known[index] = true;
        }
        spread_nearest(grid, &mut self.free_v, &self.known, &mut self.nearest);

        // Garden motes leave when their piece dissolves.
        for walk in self.walks.iter().filter(|w| !w.covered) {
            let release = self.free_v[walk.start_cell as usize];
            let travel = (span * walk.length / reference).max(0.10);
            let arrive = (release + travel).min(1.0);
            plan.motes.push(mote(
                grid,
                walk,
                release,
                arrive.max(release + 0.02).min(1.0),
            ));
        }
    }

    fn write_field(&self, grid: Grid, plan: &mut Plan) {
        for index in 0..grid.len() {
            let texel = &mut plan.field[index * 4..index * 4 + 4];
            texel[0] = unorm8(self.covered_v[index]);
            texel[1] = unorm8(self.free_v[index]);
            texel[2] = unorm8(if self.covered[index] {
                0.0
            } else {
                self.hold[index].min(1.0)
            });
            texel[3] = if self.covered[index] { 255 } else { 0 };
        }
    }
}

fn mote(grid: Grid, walk: &Walk, release: f32, arrive: f32) -> Mote {
    let mut path = [[0; 2]; PATH_POINTS];
    for (slot, point) in path.iter_mut().zip(walk.path) {
        *slot = grid.normalize(point);
    }
    Mote {
        path,
        release: unorm16(release),
        arrive: unorm16(arrive),
        source: grid.normalize(walk.source),
    }
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    (dx * dx + dy * dy).sqrt()
}

/// Exactly [`PATH_POINTS`] control points for a polyline, keeping its corners: the spline
/// passes through every control point, so a corner kept is a gap entrance honoured. Too many
/// vertices: drop the one whose removal changes the shape least (Visvalingam). Too few: split
/// the longest segment. The shader parameterizes by chord length, so uneven spacing does not
/// change the speed.
fn fit_points(points: &mut Vec<(f32, f32)>) -> [(f32, f32); PATH_POINTS] {
    while points.len() > PATH_POINTS {
        let (drop, _) = (1..points.len() - 1)
            .map(|i| (i, triangle_area(points[i - 1], points[i], points[i + 1])))
            .fold((1, f32::INFINITY), |best, candidate| {
                if candidate.1 < best.1 {
                    candidate
                } else {
                    best
                }
            });
        points.remove(drop);
    }
    let mut out = [points[0]; PATH_POINTS];
    let segments = points.len() - 1;
    if segments == 0 {
        return out;
    }
    // Hand each spare point to the segment whose pieces would otherwise be longest.
    let mut lengths = [0.0f32; PATH_POINTS];
    let mut splits = [0usize; PATH_POINTS];
    for i in 0..segments {
        lengths[i] = distance(points[i], points[i + 1]);
    }
    for _ in 0..PATH_POINTS - points.len() {
        let mut best = 0;
        for i in 1..segments {
            if lengths[i] / (splits[i] + 1) as f32 > lengths[best] / (splits[best] + 1) as f32 {
                best = i;
            }
        }
        splits[best] += 1;
    }
    let mut at = 0;
    for i in 0..segments {
        let (a, b) = (points[i], points[i + 1]);
        let pieces = splits[i] + 1;
        for j in 0..pieces {
            let t = j as f32 / pieces as f32;
            out[at] = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            at += 1;
        }
    }
    out[PATH_POINTS - 1] = points[segments];
    out
}

fn triangle_area(a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> f32 {
    ((b.0 - a.0) * (c.1 - a.1) - (c.0 - a.0) * (b.1 - a.1)).abs() * 0.5
}

/// Multi-source BFS (8-neighbourhood): every cell's step distance from the nearest `source`
/// cell, 0 at a source and capped at `cap`. With no source at all, every cell gets `cap`.
fn bfs_distance(
    grid: Grid,
    sources: &[bool],
    queue: &mut VecDeque<u32>,
    out: &mut Vec<u16>,
    cap: u16,
) {
    let n = grid.len();
    out.clear();
    out.resize(n, cap);
    queue.clear();
    for (index, _) in sources.iter().enumerate().filter(|(_, source)| **source) {
        out[index] = 0;
        queue.push_back(index as u32);
    }
    while let Some(index) = queue.pop_front() {
        let next_depth = out[index as usize].saturating_add(1);
        if next_depth >= cap {
            continue;
        }
        for (next, _) in grid.neighbours(index) {
            if out[next as usize] > next_depth {
                out[next as usize] = next_depth;
                queue.push_back(next);
            }
        }
    }
}

/// Two-pass chamfer max-propagation over garden cells: `value[c] = max(value[c],
/// value[n] - decay × steps)`.
fn spread_max(grid: Grid, values: &mut [f32], covered: &[bool], decay: f32) {
    let (cols, rows) = (grid.cols as usize, grid.rows as usize);
    let diagonal = decay * std::f32::consts::SQRT_2;
    let relax = |values: &mut [f32], index: usize, other: usize, cost: f32| {
        if !covered[index] && !covered[other] {
            let candidate = values[other] - cost;
            if candidate > values[index] {
                values[index] = candidate;
            }
        }
    };
    for row in 0..rows {
        for col in 0..cols {
            let index = row * cols + col;
            if col > 0 {
                relax(values, index, index - 1, decay);
            }
            if row > 0 {
                relax(values, index, index - cols, decay);
                if col > 0 {
                    relax(values, index, index - cols - 1, diagonal);
                }
                if col + 1 < cols {
                    relax(values, index, index - cols + 1, diagonal);
                }
            }
        }
    }
    for row in (0..rows).rev() {
        for col in (0..cols).rev() {
            let index = row * cols + col;
            if col + 1 < cols {
                relax(values, index, index + 1, decay);
            }
            if row + 1 < rows {
                relax(values, index, index + cols, decay);
                if col + 1 < cols {
                    relax(values, index, index + cols + 1, diagonal);
                }
                if col > 0 {
                    relax(values, index, index + cols - 1, diagonal);
                }
            }
        }
    }
    for value in values.iter_mut() {
        *value = value.max(0.0);
    }
}

/// Fill every unknown cell with the value of its nearest known cell (two-pass chamfer), so
/// the field stays smooth under linear filtering across a window edge.
fn spread_nearest(grid: Grid, values: &mut [f32], known: &[bool], steps: &mut Vec<u16>) {
    let (cols, rows) = (grid.cols as usize, grid.rows as usize);
    steps.clear();
    steps.extend(known.iter().map(|&k| if k { 0 } else { u16::MAX }));
    if !known.iter().any(|&k| k) {
        return;
    }
    let relax = |values: &mut [f32], steps: &mut [u16], index: usize, other: usize, cost: u16| {
        let candidate = steps[other].saturating_add(cost);
        if candidate < steps[index] {
            steps[index] = candidate;
            values[index] = values[other];
        }
    };
    for row in 0..rows {
        for col in 0..cols {
            let index = row * cols + col;
            if col > 0 {
                relax(values, steps, index, index - 1, 10);
            }
            if row > 0 {
                relax(values, steps, index, index - cols, 10);
                if col > 0 {
                    relax(values, steps, index, index - cols - 1, 14);
                }
                if col + 1 < cols {
                    relax(values, steps, index, index - cols + 1, 14);
                }
            }
        }
    }
    for row in (0..rows).rev() {
        for col in (0..cols).rev() {
            let index = row * cols + col;
            if col + 1 < cols {
                relax(values, steps, index, index + 1, 10);
            }
            if row + 1 < rows {
                relax(values, steps, index, index + cols, 10);
                if col + 1 < cols {
                    relax(values, steps, index, index + cols + 1, 14);
                }
                if col > 0 {
                    relax(values, steps, index, index + cols - 1, 14);
                }
            }
        }
    }
}

/// The obstacles the shaders test exactly: those overlapping the run's bounds most.
fn choose_obstacles(request: &Request<'_>, plan: &mut Plan) {
    let mut ranked: Vec<(i64, Rect)> = request
        .obstacles
        .iter()
        .map(|rect| rect.intersect(plan.screen))
        .filter(|rect| !rect.is_empty())
        .map(|rect| {
            let overlap = rect.intersect(plan.bounds);
            (
                i64::from(overlap.w.max(0)) * i64::from(overlap.h.max(0)),
                rect,
            )
        })
        .filter(|(area, _)| *area > 0)
        .collect();
    ranked.sort_by_key(|(area, _)| Reverse(*area));
    plan.obstacles
        .extend(ranked.into_iter().take(MAX_OBSTACLES).map(|(_, rect)| rect));
}

#[derive(Debug)]
struct UniformWriter {
    bytes: [u8; UNIFORM_BYTES],
    at: usize,
}

impl Default for UniformWriter {
    fn default() -> Self {
        Self {
            bytes: [0; UNIFORM_BYTES],
            at: 0,
        }
    }
}

impl UniformWriter {
    fn vec4(&mut self, values: [f32; 4]) {
        for value in values {
            self.bytes[self.at..self.at + 4].copy_from_slice(&value.to_le_bytes());
            self.at += 4;
        }
    }

    fn uvec4(&mut self, values: [u32; 4]) {
        for value in values {
            self.bytes[self.at..self.at + 4].copy_from_slice(&value.to_le_bytes());
            self.at += 4;
        }
    }

    fn finish(self) -> [u8; UNIFORM_BYTES] {
        debug_assert_eq!(self.at, UNIFORM_BYTES);
        self.bytes
    }
}
