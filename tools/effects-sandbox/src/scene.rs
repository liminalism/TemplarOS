//! The fake desktop: a z-ordered stack of windows over the Homegarden, a dock of Home cards,
//! a notification, a login veil — and the essence runs that move windows between them.
//!
//! The rules mirror TemplarOS: z-order is one list (bottom → top, like
//! `WindowService::stacking_order` reversed); minimize parks a window into its Home card
//! (`set_minimize_target`); the desktop button hides every client window (`RevealHome`) and
//! brings them back. Each trigger plans once and then only advances progress.

use crate::gpu::{
    Backdrop, BarDraw, Dest, EssenceDraw, Gpu, QuadDraw, QuadParams, SurfaceDraw, clear_pass,
};
use crate::paint::{self, Kind, TITLE_H};
use std::time::Instant;
use templar_effects::{
    Direction, Gesture, Material, Plan, Planner, Plate, Rect, Request, Run, Sink, Style, Surface,
    Timing,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WinKind {
    Browser,
    Terminal,
    Login,
}

#[derive(Debug)]
pub struct Win {
    pub kind: WinKind,
    pub rect: Rect,
    pub shown: bool,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    quad: QuadDraw,
    thumb: QuadDraw,
    card_glass: SurfaceDraw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Single,
    Desktop,
}

/// What a plan was made from: the same key means the same plan, so it is reused.
#[derive(Clone, Debug, PartialEq)]
struct PlanKey {
    window: Rect,
    obstacles: Vec<Rect>,
    sink: Sink,
    timing: Timing,
}

#[derive(Debug)]
pub struct Active {
    pub win: usize,
    pub run: Run,
    pub plan: Plan,
    draw: EssenceDraw,
    key: PlanKey,
    delay_ms: f32,
    group: Group,
}

#[derive(Debug)]
pub struct Scene {
    pub screen: Rect,
    pub wins: Vec<Win>,
    /// Bottom → top.
    pub z: Vec<usize>,
    pub runs: Vec<Active>,
    planner: Planner,
    /// The last finished run's plan and GPU buffers per window: minimize then restore with
    /// nothing moved costs no planning and no upload.
    cache: Vec<Option<(PlanKey, Plan, EssenceDraw)>>,
    pub reused: bool,
    pub timing: Timing,
    pub style: Style,
    pub home: bool,
    home_hidden: Vec<usize>,
    pub parked: Vec<usize>,
    pub garden: Material,
    pub notification: Option<Material>,
    pub slow: bool,
    pub show_field: bool,
    /// The grain slider is drawn and takes clicks (interactive only; stills leave it out).
    pub show_slider: bool,
    pub stagger_ms: f32,
    /// The desktop button sends each window into its own Home card (true) or lets it soak
    /// into the garden through the nearest gap (false).
    pub desktop_cards: bool,
    pub last_plan_us: f32,
    pub last_upload: usize,
    pub last_motes: u32,
    backdrop: Backdrop,
    garden_draw: SurfaceDraw,
    garden_dirty: bool,
    blit: QuadDraw,
    notif_draw: SurfaceDraw,
    bars: Vec<QuadDraw>,
    login_bg: SurfaceDraw,
    login_card: SurfaceDraw,
    login_bars: Vec<QuadDraw>,
    login_dirty: bool,
    field_quad: Option<QuadDraw>,
    drag: Option<(usize, i32, i32)>,
    pub cursor: (f32, f32),
    /// Glass plates along the bottom. The pointer eases [`gestures`]; the shader does the rest.
    plates: Vec<BarDraw>,
    gestures: [Gesture; BAR_CELLS],
    bar_down: bool,
    bar_time: f32,
    /// The dev-only grain slider: panel, track, fill, knob, a grain at true size.
    slider_quads: Vec<QuadDraw>,
    slider_drag: bool,
}

const CARD_W: i32 = 156;
const CARD_H: i32 = 96;
/// The study bar: the same five duties as the shell's system marks, in that order.
const BAR_CELLS: usize = 5;
const BAR_NAMES: [&str; BAR_CELLS] = ["settings", "network", "calendar", "terminal", "account"];
/// The grain slider's track, from the screen's bottom-left corner.
const SLIDER_X: i32 = 28;
const SLIDER_W: i32 = 220;
const SLIDER_FROM_BOTTOM: i32 = 36;

impl Scene {
    pub fn new(gpu: &Gpu, w: u32, h: u32) -> Self {
        let screen = Rect::new(0, 0, w as i32, h as i32);
        let backdrop = Backdrop::new(gpu, w, h);
        let (fw, fh) = (w as f32, h as f32);
        let r = |x: f32, y: f32, ww: f32, hh: f32| {
            Rect::new(
                (fw * x) as i32,
                (fh * y) as i32,
                (fw * ww) as i32,
                (fh * hh) as i32,
            )
        };
        let layout = [
            (WinKind::Terminal, r(0.05, 0.08, 0.36, 0.38)),
            (WinKind::Terminal, r(0.53, 0.12, 0.40, 0.42)),
            (WinKind::Terminal, r(0.22, 0.56, 0.42, 0.29)),
            (WinKind::Browser, screen),
            (WinKind::Login, screen),
        ];
        let wins: Vec<Win> = layout
            .iter()
            .enumerate()
            .map(|(seed, &(kind, rect))| make_win(gpu, &backdrop, kind, rect, seed as u32 + 1))
            .collect();
        let bars = (0..6).map(|_| gpu.quad(&gpu.white, &gpu.clamp)).collect();
        let login_bars = (0..4).map(|_| gpu.quad(&gpu.white, &gpu.clamp)).collect();
        let slider_quads = (0..5).map(|_| gpu.quad(&gpu.white, &gpu.clamp)).collect();
        let plates = (0..BAR_CELLS).map(|_| gpu.bar(&backdrop.view)).collect();
        let mut scene = Self {
            screen,
            z: (0..wins.len()).collect(),
            wins,
            runs: Vec::new(),
            planner: Planner::new(),
            cache: Vec::new(),
            reused: false,
            timing: Timing::DEFAULT,
            style: Style::DEFAULT,
            home: false,
            home_hidden: Vec::new(),
            parked: Vec::new(),
            garden: Material::RoughWood,
            notification: Some(Material::Glass),
            slow: false,
            show_field: false,
            show_slider: false,
            stagger_ms: 30.0,
            desktop_cards: true,
            last_plan_us: 0.0,
            last_upload: 0,
            last_motes: 0,
            garden_draw: gpu.surface(&gpu.white),
            garden_dirty: true,
            blit: gpu.quad(&backdrop.view, &gpu.clamp),
            notif_draw: gpu.surface(&backdrop.view),
            bars,
            login_bg: gpu.surface(&backdrop.view),
            login_card: gpu.surface(&backdrop.view),
            login_bars,
            login_dirty: true,
            backdrop,
            field_quad: None,
            drag: None,
            cursor: (0.0, 0.0),
            plates,
            gestures: [Gesture::rest(); BAR_CELLS],
            bar_down: false,
            bar_time: 0.0,
            slider_quads,
            slider_drag: false,
        };
        let login = scene.login_id();
        scene.wins[login].shown = false;
        scene
    }

    /// Rebuild for a new output size, keeping the settings.
    pub fn resized(self, gpu: &Gpu, w: u32, h: u32) -> Self {
        let mut next = Scene::new(gpu, w, h);
        next.timing = self.timing;
        next.style = self.style;
        next.garden = self.garden;
        next.notification = self.notification;
        next.slow = self.slow;
        next.show_field = self.show_field;
        next.show_slider = self.show_slider;
        next.desktop_cards = self.desktop_cards;
        next
    }

    fn login_id(&self) -> usize {
        self.wins
            .iter()
            .position(|w| w.kind == WinKind::Login)
            .expect("the scene has a login veil")
    }

    pub fn animating(&self) -> bool {
        !self.runs.is_empty() || self.gestures.iter().any(|gesture| gesture.live())
    }

    fn run_index(&self, id: usize) -> Option<usize> {
        self.runs.iter().position(|r| r.win == id)
    }

    fn card_ids(&self) -> Vec<usize> {
        (0..self.wins.len())
            .filter(|&id| self.wins[id].kind != WinKind::Login)
            .collect()
    }

    pub fn card_rect(&self, id: usize) -> Rect {
        let ids = self.card_ids();
        let n = ids.len() as i32;
        let index = ids.iter().position(|&i| i == id).unwrap_or(0) as i32;
        let gap = 14;
        let total = n * CARD_W + (n - 1) * gap;
        let x0 = (self.screen.w - total) / 2;
        Rect::new(
            x0 + index * (CARD_W + gap),
            self.screen.h - CARD_H - 20,
            CARD_W,
            CARD_H,
        )
    }

    /// Shown client windows other than `except` — what a run flows around.
    fn obstacles(&self, except: usize, below_only: bool, extra: &[usize]) -> Vec<Rect> {
        let limit = if below_only {
            self.z
                .iter()
                .position(|&id| id == except)
                .unwrap_or(self.z.len())
        } else {
            self.z.len()
        };
        self.z[..limit]
            .iter()
            .filter(|&&id| id != except && self.wins[id].kind != WinKind::Login)
            .filter(|&&id| {
                (self.wins[id].shown
                    && self
                        .run_index(id)
                        .is_none_or(|i| self.runs[i].run.direction == Direction::Depart))
                    || extra.contains(&id)
            })
            .map(|&id| self.wins[id].rect)
            .collect()
    }

    fn start(
        &mut self,
        gpu: &Gpu,
        id: usize,
        sink: Sink,
        obstacles: &[Rect],
        direction: Direction,
        delay_ms: f32,
        group: Group,
    ) {
        let key = PlanKey {
            window: self.wins[id].rect,
            obstacles: obstacles.to_vec(),
            sink,
            timing: self.timing,
        };
        self.cache.resize_with(self.wins.len(), || None);
        let (plan, draw) = match self.cache[id].take() {
            Some((cached, plan, draw)) if cached == key => {
                self.reused = true;
                self.last_plan_us = 0.0;
                self.last_upload = 0;
                (plan, draw)
            }
            _ => {
                let started = Instant::now();
                let plan = self.planner.plan(&Request {
                    screen: self.screen,
                    window: key.window,
                    obstacles,
                    sink,
                    timing: self.timing,
                });
                self.last_plan_us = started.elapsed().as_secs_f32() * 1e6;
                let draw = gpu.essence(&plan, &self.wins[id].view);
                self.reused = false;
                self.last_upload = draw.upload_bytes;
                (plan, draw)
            }
        };
        self.last_motes = draw.mote_count;
        self.field_quad = Some(gpu.quad(&draw.field_view, &gpu.nearest));
        let run = match direction {
            Direction::Depart => Run::depart(self.timing),
            Direction::Arrive => Run::arrive(self.timing),
        };
        self.runs.push(Active {
            win: id,
            run,
            plan,
            draw,
            key,
            delay_ms,
            group,
        });
    }

    fn raise(&mut self, id: usize) {
        let login = self.login_id();
        self.z.retain(|&i| i != id);
        let top = if id == login {
            self.z.len()
        } else {
            self.z
                .iter()
                .position(|&i| i == login)
                .unwrap_or(self.z.len())
        };
        self.z.insert(top, id);
    }

    /// Minimize the topmost window into its card — or turn around one on its way back.
    pub fn minimize(&mut self, gpu: &Gpu) {
        if let Some(active) = self.runs.iter_mut().rev().find(|r| {
            r.group == Group::Single
                && r.run.direction == Direction::Arrive
                && self.wins[r.win].kind != WinKind::Login
        }) {
            active.run.reverse();
            self.parked.push(active.win);
            return;
        }
        let Some(&id) = self.z.iter().rev().find(|&&id| {
            self.wins[id].shown
                && self.wins[id].kind != WinKind::Login
                && self.run_index(id).is_none()
        }) else {
            return;
        };
        let obstacles = self.obstacles(id, false, &[]);
        self.start(
            gpu,
            id,
            Sink::Card(self.card_rect(id)),
            &obstacles,
            Direction::Depart,
            0.0,
            Group::Single,
        );
        self.parked.push(id);
    }

    /// Restore `id` from its card (or turn its departure around).
    pub fn restore(&mut self, gpu: &Gpu, id: usize) {
        if let Some(index) = self.run_index(id) {
            let active = &mut self.runs[index];
            active.run.reverse();
            if active.run.direction == Direction::Arrive {
                self.parked.retain(|&i| i != id);
            } else {
                self.parked.push(id);
            }
            return;
        }
        if self.wins[id].shown {
            self.raise(id);
            return;
        }
        self.raise(id);
        let obstacles = self.obstacles(id, false, &[]);
        self.start(
            gpu,
            id,
            Sink::Card(self.card_rect(id)),
            &obstacles,
            Direction::Arrive,
            0.0,
            Group::Single,
        );
        self.parked.retain(|&i| i != id);
        if self.home {
            self.home = false;
            self.home_hidden.clear();
        }
    }

    pub fn restore_last(&mut self, gpu: &Gpu) {
        if let Some(&id) = self.parked.last() {
            self.restore(gpu, id);
        }
    }

    /// The desktop button: everything goes home, staggered front to back; pressed again,
    /// everything comes back, back to front. Pressed mid-flight, every run turns around.
    pub fn desktop(&mut self, gpu: &Gpu) {
        if self.runs.iter().any(|r| r.group == Group::Desktop) {
            self.runs.retain_mut(|r| {
                if r.group != Group::Desktop {
                    return true;
                }
                if r.delay_ms > 0.0 {
                    return false;
                }
                r.run.reverse();
                true
            });
            self.home = !self.home;
            return;
        }
        let stagger = self.stagger_ms;
        if self.home {
            let returning: Vec<usize> = self
                .home_hidden
                .iter()
                .rev()
                .copied()
                .filter(|&id| !self.wins[id].shown && self.run_index(id).is_none())
                .collect();
            for (k, &id) in returning.iter().enumerate() {
                let obstacles = self.obstacles(id, true, &returning[..k]);
                let sink = self.desktop_sink(id);
                self.start(
                    gpu,
                    id,
                    sink,
                    &obstacles,
                    Direction::Arrive,
                    k as f32 * stagger,
                    Group::Desktop,
                );
            }
            self.home = false;
        } else {
            let leaving: Vec<usize> = self
                .z
                .iter()
                .rev()
                .copied()
                .filter(|&id| {
                    self.wins[id].shown
                        && self.wins[id].kind != WinKind::Login
                        && self.run_index(id).is_none()
                })
                .collect();
            for (k, &id) in leaving.iter().enumerate() {
                let obstacles = self.obstacles(id, true, &[]);
                let sink = self.desktop_sink(id);
                self.start(
                    gpu,
                    id,
                    sink,
                    &obstacles,
                    Direction::Depart,
                    k as f32 * stagger,
                    Group::Desktop,
                );
            }
            self.home_hidden = leaving;
            self.home = true;
        }
    }

    fn desktop_sink(&self, id: usize) -> Sink {
        if self.desktop_cards {
            Sink::Card(self.card_rect(id))
        } else {
            Sink::Garden
        }
    }

    /// The login veil: shown, it lifts through the gaps between the session's windows.
    pub fn login(&mut self, gpu: &Gpu) {
        let id = self.login_id();
        if let Some(index) = self.run_index(id) {
            self.runs[index].run.reverse();
            return;
        }
        let obstacles = self.obstacles(id, false, &[]);
        let direction = if self.wins[id].shown {
            Direction::Depart
        } else {
            Direction::Arrive
        };
        self.start(
            gpu,
            id,
            Sink::Garden,
            &obstacles,
            direction,
            0.0,
            Group::Single,
        );
    }

    /// Show the veil without a transition (snapshots start from a locked screen).
    pub fn lock_now(&mut self) {
        let id = self.login_id();
        self.wins[id].shown = true;
    }

    /// Advance every run by `dt_ms` of wall time. Returns whether anything is still moving.
    pub fn tick(&mut self, dt_ms: f32) -> bool {
        let dt = if self.slow { dt_ms / 8.0 } else { dt_ms };
        self.advance_bar(dt / 1000.0);
        for active in &mut self.runs {
            if active.delay_ms > 0.0 {
                active.delay_ms -= dt;
                if active.delay_ms > 0.0 {
                    continue;
                }
                let late = -active.delay_ms;
                active.delay_ms = 0.0;
                active.run.advance(late);
            } else {
                active.run.advance(dt);
            }
        }
        let mut index = 0;
        while index < self.runs.len() {
            if !self.runs[index].run.finished() {
                index += 1;
                continue;
            }
            let done = self.runs.swap_remove(index);
            self.wins[done.win].shown = done.run.direction == Direction::Arrive;
            self.cache.resize_with(self.wins.len(), || None);
            self.cache[done.win] = Some((done.key, done.plan, done.draw));
        }
        self.animating()
    }

    /// Put every run at progress `u` with no stagger (stills).
    pub fn set_progress(&mut self, u: f32) {
        for active in &mut self.runs {
            active.delay_ms = 0.0;
            active.run.u = match active.run.direction {
                Direction::Depart => u,
                Direction::Arrive => 1.0 - u,
            };
        }
    }

    pub fn adjust_duration(&mut self, delta_ms: f32) {
        self.timing = Timing {
            duration_ms: self.timing.duration_ms + delta_ms,
            ..self.timing
        }
        .clamped();
    }

    pub fn cycle_garden(&mut self) {
        self.garden = self.garden.next();
        self.garden_dirty = true;
        self.login_dirty = true;
    }

    pub fn press(&mut self, gpu: &Gpu) {
        let (x, y) = self.cursor;
        if self.show_slider && self.slider_hit().contains(x, y) {
            self.slider_drag = true;
            self.slide_to(x);
            return;
        }
        if self.hot_bar().is_some() {
            self.bar_down = true;
            // Catch the press edge even when the button is released before the next frame.
            self.advance_bar(0.0);
            return;
        }
        for id in self.card_ids() {
            if self.card_rect(id).contains(x, y) {
                self.restore(gpu, id);
                return;
            }
        }
        let hit = self.z.iter().rev().copied().find(|&id| {
            let w = &self.wins[id];
            w.kind != WinKind::Login
                && w.shown
                && self.run_index(id).is_none()
                && w.rect.contains(x, y)
        });
        if let Some(id) = hit {
            self.raise(id);
            let rect = self.wins[id].rect;
            if (y as i32) < rect.y + TITLE_H {
                self.drag = Some((id, x as i32 - rect.x, y as i32 - rect.y));
            }
        }
    }

    pub fn release(&mut self) {
        self.drag = None;
        self.slider_drag = false;
        self.bar_down = false;
    }

    /// Pose one plate for a still. The others are left as they are.
    pub fn bar_pose(&mut self, index: usize, hover: f32, press: f32, pulse: f32) {
        if let Some(gesture) = self.gestures.get_mut(index) {
            *gesture = Gesture::pose(hover, press, pulse);
        }
        self.bar_time = 0.35;
    }

    pub fn moved(&mut self, x: f32, y: f32) -> bool {
        let was = self.hot_bar();
        self.cursor = (x, y);
        if self.slider_drag {
            self.slide_to(x);
            return true;
        }
        if let Some((id, dx, dy)) = self.drag {
            self.wins[id].rect.x = x as i32 - dx;
            self.wins[id].rect.y = y as i32 - dy;
            return true;
        }
        was != self.hot_bar()
    }

    fn hot_bar(&self) -> Option<usize> {
        let (x, y) = self.cursor;
        self.bar_rects().iter().position(|rect| rect.contains(x, y))
    }

    fn bar_rects(&self) -> [Rect; BAR_CELLS] {
        let cell = 64;
        let gap = 14;
        let total = BAR_CELLS as i32 * cell + (BAR_CELLS as i32 - 1) * gap;
        let x0 = (self.screen.w - total) / 2;
        let y = (self.screen.h - 36 - cell).max(0);
        std::array::from_fn(|index| Rect::new(x0 + index as i32 * (cell + gap), y, cell, cell))
    }

    fn advance_bar(&mut self, dt_s: f32) {
        self.bar_time += dt_s;
        let rects = self.bar_rects();
        let (x, y) = self.cursor;
        for (index, gesture) in self.gestures.iter_mut().enumerate() {
            gesture.advance(dt_s, rects[index].contains(x, y), self.bar_down);
        }
    }

    /// Grain size by a factor (the `,` and `.` keys); it applies at once, even mid-run.
    pub fn scale_grain(&mut self, factor: f32) {
        self.style.grain_px =
            (self.style.grain_px * factor).clamp(Style::MIN_GRAIN_PX, Style::MAX_GRAIN_PX);
    }

    fn slider_track(&self) -> [f32; 4] {
        [
            SLIDER_X as f32,
            (self.screen.h - SLIDER_FROM_BOTTOM) as f32 - 2.0,
            SLIDER_W as f32,
            4.0,
        ]
    }

    fn slider_hit(&self) -> Rect {
        Rect::new(
            SLIDER_X - 12,
            self.screen.h - SLIDER_FROM_BOTTOM - 14,
            SLIDER_W + 24,
            28,
        )
    }

    /// The slider is logarithmic: as much travel from 1 to 2 px as from 8 to 16.
    fn slider_t(&self) -> f32 {
        let span = (Style::MAX_GRAIN_PX / Style::MIN_GRAIN_PX).ln();
        ((self.style.grain_px / Style::MIN_GRAIN_PX).ln() / span).clamp(0.0, 1.0)
    }

    fn slide_to(&mut self, x: f32) {
        let t = ((x - SLIDER_X as f32) / SLIDER_W as f32).clamp(0.0, 1.0);
        let span = Style::MAX_GRAIN_PX / Style::MIN_GRAIN_PX;
        // Snapped to a tenth of a pixel, so the title shows exactly what is drawn.
        let px = Style::MIN_GRAIN_PX * span.powf(t);
        self.style.grain_px = ((px * 10.0).round() / 10.0).max(Style::MIN_GRAIN_PX);
    }

    fn set_slider(&self, gpu: &Gpu, size: (f32, f32)) {
        let [x, y, w, _] = self.slider_track();
        let t = self.slider_t();
        let knob = 14.0;
        let grain = self.style.grain_px;
        let parts = [
            QuadParams::solid(
                [x - 16.0, y - 16.0, w + 32.0 + 48.0, 36.0],
                [0.06, 0.07, 0.08],
                0.86,
                18.0,
            ),
            QuadParams::solid([x, y, w, 4.0], [1.0, 1.0, 1.0], 0.25, 2.0),
            QuadParams::solid([x, y, w * t, 4.0], [0.62, 0.86, 0.74], 0.9, 2.0),
            QuadParams::solid(
                [x + w * t - knob * 0.5, y + 2.0 - knob * 0.5, knob, knob],
                [0.95, 0.97, 0.96],
                1.0,
                knob * 0.5,
            ),
            // One grain at its true size, beside the track.
            QuadParams::solid(
                [
                    x + w + 24.0 - grain * 0.5,
                    y + 2.0 - grain * 0.5,
                    grain,
                    grain,
                ],
                [0.85, 1.0, 0.92],
                1.0,
                grain * 0.5,
            ),
        ];
        for (quad, p) in self.slider_quads.iter().zip(parts) {
            quad.set(&gpu.queue, size, p);
        }
    }

    pub fn status(&self) -> String {
        format!(
            "effects-sandbox — {:.0} ms{} — grain {:.1} px — desktop {} — last plan {}, {} motes, {} KiB uploaded — garden: {} — notification: {} — bar: {}",
            self.timing.duration_ms,
            if self.slow { " (×8 slow)" } else { "" },
            self.style.grain_px,
            if self.desktop_cards {
                "→ cards"
            } else {
                "→ soak"
            },
            if self.reused {
                "reused".to_owned()
            } else {
                format!("{:.0} µs", self.last_plan_us)
            },
            self.last_motes,
            self.last_upload / 1024,
            self.garden.name(),
            self.notification.map_or("off", Material::name),
            self.hot_bar().map_or("idle", |index| BAR_NAMES[index]),
        )
    }

    fn render_login(&self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder) {
        let login = &self.wins[self.login_id()];
        let (w, h) = (login.rect.w as f32, login.rect.h as f32);
        // An etched glass veil over the Homegarden, a polished stone card on it.
        let bg = Surface {
            radius: 0.0,
            bevel: 0.0,
            scale: 260.0,
            backdrop_lod: 5.5,
            ..Surface::preset(
                Material::EtchedGlass,
                Rect::new(0, 0, login.rect.w, login.rect.h),
            )
        };
        self.login_bg.set(&gpu.queue, (w, h), &bg);
        let card = Rect::new((w * 0.5 - 210.0) as i32, (h * 0.5 - 150.0) as i32, 420, 300);
        let glass = Surface {
            radius: 22.0,
            bevel: 8.0,
            ..Surface::preset(Material::GlassyStone, card)
        };
        self.login_card.set(&gpu.queue, (w, h), &glass);
        let (cx, cy) = (card.x as f32, card.y as f32);
        let parts = [
            QuadParams::solid(
                [cx + 170.0, cy + 36.0, 80.0, 80.0],
                [0.85, 0.9, 0.88],
                0.9,
                40.0,
            ),
            QuadParams::solid(
                [cx + 120.0, cy + 136.0, 180.0, 18.0],
                [1.0, 1.0, 1.0],
                0.85,
                9.0,
            ),
            QuadParams::solid(
                [cx + 60.0, cy + 186.0, 300.0, 44.0],
                [0.08, 0.1, 0.12],
                0.55,
                22.0,
            ),
            QuadParams::solid(
                [cx + 150.0, cy + 250.0, 120.0, 12.0],
                [1.0, 1.0, 1.0],
                0.5,
                6.0,
            ),
        ];
        for (quad, p) in self.login_bars.iter().zip(parts) {
            quad.set(&gpu.queue, (w, h), p);
        }
        let view = login
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut pass = clear_pass(encoder, &view, "login");
        gpu.draw_surface(&mut pass, &self.login_bg, Dest::Offscreen);
        gpu.draw_surface(&mut pass, &self.login_card, Dest::Offscreen);
        for quad in &self.login_bars {
            gpu.draw_quad(&mut pass, quad, Dest::Offscreen);
        }
    }

    /// Encode one frame into `target`.
    pub fn render(&mut self, gpu: &Gpu, target: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let size = (self.screen.w as f32, self.screen.h as f32);
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        // Retained: the garden and the login screen are redrawn only when they change.
        if self.garden_dirty {
            let garden = Surface {
                radius: 0.0,
                bevel: 0.0,
                ..Surface::preset(self.garden, self.screen)
            };
            self.garden_draw.set(&gpu.queue, size, &garden);
            self.backdrop.render(gpu, &mut encoder, &self.garden_draw);
            self.garden_dirty = false;
        }
        if self.login_dirty {
            self.render_login(gpu, &mut encoder);
            self.login_dirty = false;
        }

        self.blit.set(
            &gpu.queue,
            size,
            QuadParams::new([0.0, 0.0, size.0, size.1]),
        );
        for id in self.card_ids() {
            let card = self.card_rect(id);
            let glass = Surface {
                radius: 14.0,
                bevel: 5.0,
                ..Surface::preset(Material::Glass, card)
            };
            self.wins[id].card_glass.set(&gpu.queue, size, &glass);
            let inner = [
                card.x as f32 + 10.0,
                card.y as f32 + 10.0,
                card.w as f32 - 20.0,
                card.h as f32 - 20.0,
            ];
            let parked = !self.wins[id].shown || self.run_index(id).is_some();
            self.wins[id].thumb.set(
                &gpu.queue,
                size,
                QuadParams {
                    radius: 6.0,
                    ..QuadParams::new(inner).opacity(if parked { 1.0 } else { 0.45 })
                },
            );
        }
        for active in &self.runs {
            active
                .draw
                .set(&gpu.queue, &active.plan, active.run.u, &self.style);
        }
        for win in &self.wins {
            let r = win.rect.to_f32();
            win.quad.set(&gpu.queue, size, QuadParams::new(r));
        }
        if let Some(material) = self.notification {
            let rect = Rect::new(self.screen.w - 404, 24, 380, 104);
            let surface = Surface {
                radius: 18.0,
                bevel: 6.0,
                ..Surface::preset(material, rect)
            };
            self.notif_draw.set(&gpu.queue, size, &surface);
            let (x, y) = (rect.x as f32, rect.y as f32);
            let dark = !matches!(material, Material::Glass | Material::EtchedGlass);
            let ink = if dark {
                [0.95, 0.95, 0.93]
            } else {
                [0.08, 0.09, 0.11]
            };
            let parts = [
                QuadParams::solid(
                    [x + 20.0, y + 22.0, 60.0, 60.0],
                    [0.35, 0.62, 0.52],
                    0.95,
                    14.0,
                ),
                QuadParams::solid([x + 96.0, y + 28.0, 170.0, 14.0], ink, 0.9, 7.0),
                QuadParams::solid([x + 96.0, y + 54.0, 250.0, 9.0], ink, 0.6, 4.5),
                QuadParams::solid([x + 96.0, y + 70.0, 200.0, 9.0], ink, 0.6, 4.5),
            ];
            for (quad, p) in self.bars.iter().zip(parts) {
                quad.set(&gpu.queue, size, p);
            }
        }
        if self.show_slider {
            self.set_slider(gpu, size);
        }
        if let Some(field) = &self.field_quad {
            field.set(
                &gpu.queue,
                size,
                QuadParams::new([0.0, 0.0, size.0, size.1]).opacity(0.55),
            );
        }
        let show_bar = !self.wins[self.login_id()].shown;
        if show_bar {
            let rects = self.bar_rects();
            for (index, plate) in self.plates.iter().enumerate() {
                plate.set(
                    &gpu.queue,
                    size,
                    &Plate::glass(
                        rects[index],
                        self.gestures[index],
                        self.bar_time,
                        index as f32 * 1.7,
                    ),
                );
            }
        }

        {
            let mut pass = clear_pass(&mut encoder, target, "frame");
            gpu.draw_quad(&mut pass, &self.blit, Dest::Target);
            for id in self.card_ids() {
                gpu.draw_surface(&mut pass, &self.wins[id].card_glass, Dest::Target);
                gpu.draw_quad(&mut pass, &self.wins[id].thumb, Dest::Target);
            }
            // Window order is the one z-order; a window in transition is drawn at its own place,
            // and its motes right after it, so windows above it still cover them.
            for &id in &self.z {
                if let Some(index) = self.run_index(id) {
                    gpu.draw_body(&mut pass, &self.runs[index].draw);
                    gpu.draw_motes(&mut pass, &self.runs[index].draw);
                } else if self.wins[id].shown {
                    gpu.draw_quad(&mut pass, &self.wins[id].quad, Dest::Target);
                }
                if id == self.z[self.z.len() - 2] && self.notification.is_some() {
                    // Notifications sit above client windows, below the login veil.
                    gpu.draw_surface(&mut pass, &self.notif_draw, Dest::Target);
                    for quad in self.bars.iter().take(4) {
                        gpu.draw_quad(&mut pass, quad, Dest::Target);
                    }
                }
            }
            if self.show_field
                && let Some(field) = &self.field_quad
            {
                gpu.draw_quad(&mut pass, field, Dest::Target);
            }
            // The study bar sits over the desktop and under the dev slider. The login veil
            // covers the whole output, so the plates stay out of those stills.
            if show_bar {
                for plate in &self.plates {
                    gpu.draw_bar(&mut pass, plate, Dest::Target);
                }
            }
            // Dev tooling, over everything.
            if self.show_slider {
                for quad in &self.slider_quads {
                    gpu.draw_quad(&mut pass, quad, Dest::Target);
                }
            }
        }
        encoder.finish()
    }
}

fn make_win(gpu: &Gpu, backdrop: &Backdrop, kind: WinKind, rect: Rect, seed: u32) -> Win {
    let (w, h) = (rect.w.max(1) as u32, rect.h.max(1) as u32);
    let texture = match kind {
        WinKind::Browser | WinKind::Terminal => {
            let paint_kind = if kind == WinKind::Browser {
                Kind::Browser
            } else {
                Kind::Terminal
            };
            let canvas = paint::window(paint_kind, rect.w, rect.h, seed);
            gpu.texture("window", w, h, &canvas.px).0
        }
        WinKind::Login => gpu.render_texture("login", w, h, false),
    };
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Win {
        kind,
        rect,
        shown: true,
        quad: gpu.quad(&view, &gpu.clamp),
        thumb: gpu.quad(&view, &gpu.clamp),
        card_glass: gpu.surface(&backdrop.view),
        texture,
        view,
    }
}
