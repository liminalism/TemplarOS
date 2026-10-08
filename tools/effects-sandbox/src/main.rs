//! effects-sandbox: a standalone place to see, tune and measure templar-effects without a
//! session. No compositor, no shell, no D-Bus — a fake desktop and the real effect code.
//!
//! ```text
//! effects-sandbox                     interactive (keys below)
//! effects-sandbox --check [DIR]       validate every WGSL module; translate to GLSL ES 3.0 and SPIR-V
//! effects-sandbox --snapshots DIR     render stills of each transition and every material to PNG
//! effects-sandbox --bench             planner CPU time and per-pass GPU time (timestamp queries)
//!     --size WxH                      offscreen size for --snapshots/--bench (default 1920x1080)
//!     --grain PX                      essence grain diameter (default `Style::DEFAULT.grain_px`)
//! EFFECTS_ADAPTER=low                 use the integrated (low-power) GPU
//! ```
//!
//! Keys: D desktop button · M minimize top window · R restore last · click a card to restore ·
//! drag title bars · L login veil · [ ] duration −/+25 ms · S ×8 slow motion · G cycle garden
//! material · N notification on/off · 1–7 notification material · F field overlay ·
//! H desktop button sends windows to their cards / lets them soak into the garden ·
//! , . grain size −/+ (or drag the slider, bottom left) · Esc quit.
//! The bottom bar is always there (except under the login veil): moving onto a plate sweeps its
//! glass sheen and starts a slight wobble and refraction; clicking shrinks it and washes a ring.

mod check;
mod gpu;
mod paint;
mod scene;

use gpu::{Backdrop, Dest, Gpu, OFFSCREEN_FORMAT, QuadParams, clear_pass, read_rgba};
use scene::Scene;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use templar_effects::{Material, Planner, Rect, Request, Sink, Style, Surface, Timing};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let size = args
        .iter()
        .position(|a| a == "--size")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .unwrap_or((1920, 1080));
    let grain = args
        .iter()
        .position(|a| a == "--grain")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(Style::DEFAULT.grain_px)
        .clamp(Style::MIN_GRAIN_PX, Style::MAX_GRAIN_PX);
    let style = Style {
        grain_px: grain,
        ..Style::DEFAULT
    };
    let result = match args.first().map(String::as_str) {
        Some("--check") => {
            let (report, ok) = check::run(args.get(1).map(Path::new));
            print!("{report}");
            if ok {
                Ok(())
            } else {
                Err("shader gate failed".to_owned())
            }
        }
        Some("--snapshots") => match args.get(1) {
            Some(dir) => snapshots(Path::new(dir), size, style),
            None => Err("--snapshots needs a directory".to_owned()),
        },
        Some("--bench") => bench(size, style),
        Some(other) if other.starts_with("--") && other != "--size" && other != "--grain" => {
            Err(format!("unknown option {other}"))
        }
        _ => interactive(style),
    };
    if let Err(error) = result {
        eprintln!("effects-sandbox: {error}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- offscreen -----------------

fn offscreen_gpu() -> Result<Gpu, String> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = pollster::block_on(gpu::adapter(&instance, None))?;
    pollster::block_on(Gpu::new(&adapter, OFFSCREEN_FORMAT))
}

fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

fn snapshots(dir: &Path, (w, h): (u32, u32), style: Style) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let gpu = offscreen_gpu()?;
    println!("adapter: {}", gpu.adapter);
    println!("grain: {:.1} px", style.grain_px);
    let target = gpu.render_texture("snapshot", w, h, false);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let steps = [0.0, 0.06, 0.12, 0.2, 0.3, 0.45, 0.6, 0.75, 0.9];
    let shoot = |scene: &mut Scene, name: &str| -> Result<(), String> {
        for &u in &steps {
            scene.set_progress(u);
            let commands = scene.render(&gpu, &view);
            gpu.queue.submit([commands]);
            let path: PathBuf = dir.join(format!("{name}_{:03}.png", (u * 100.0).round() as u32));
            write_png(&path, w, h, &read_rgba(&gpu, &target))?;
        }
        println!("{name}: {} stills", steps.len());
        Ok(())
    };

    // The desktop button staggers its windows, so it is shot on the real clock: a still every
    // 30 ms, as the interactive sandbox would play it.
    let timed = |scene: &mut Scene, name: &str| -> Result<(), String> {
        let mut frame = 0;
        loop {
            let commands = scene.render(&gpu, &view);
            gpu.queue.submit([commands]);
            write_png(
                &dir.join(format!("{name}_{:03}ms.png", frame * 30)),
                w,
                h,
                &read_rgba(&gpu, &target),
            )?;
            frame += 1;
            if !scene.tick(30.0) {
                break;
            }
        }
        println!("{name}: {frame} stills, 30 ms apart");
        Ok(())
    };
    let mut scene = Scene::new(&gpu, w, h);
    scene.style = style;
    scene.notification = None;
    scene.desktop(&gpu);
    timed(&mut scene, "desktop")?;

    let mut scene = Scene::new(&gpu, w, h);
    scene.style = style;
    scene.notification = None;
    scene.desktop_cards = false;
    scene.desktop(&gpu);
    timed(&mut scene, "soak")?;

    let mut scene = Scene::new(&gpu, w, h);
    scene.style = style;
    scene.notification = None;
    scene.minimize(&gpu);
    shoot(&mut scene, "minimize")?;

    let mut scene = Scene::new(&gpu, w, h);
    scene.style = style;
    scene.notification = None;
    scene.lock_now();
    scene.login(&gpu);
    shoot(&mut scene, "login")?;

    surfaces_sheet(&gpu, dir, (w, h))?;
    bar_stills(&gpu, dir, (w, h))?;
    Ok(())
}

/// The bottom bar at rest, under the pointer, and on a click. One still each: the poses are
/// set directly so a frame does not depend on the easer having caught up.
fn bar_stills(gpu: &Gpu, dir: &Path, (w, h): (u32, u32)) -> Result<(), String> {
    let target = gpu.render_texture("bar", w, h, false);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let shoot = |scene: &mut Scene, name: &str| -> Result<(), String> {
        let commands = scene.render(gpu, &view);
        gpu.queue.submit([commands]);
        write_png(
            &dir.join(format!("{name}.png")),
            w,
            h,
            &read_rgba(gpu, &target),
        )?;
        println!("{name}");
        Ok(())
    };
    let mut scene = Scene::new(gpu, w, h);
    scene.notification = None;
    shoot(&mut scene, "bar_rest")?;
    scene.bar_pose(2, 1.0, 0.0, 0.0);
    shoot(&mut scene, "bar_hover")?;
    scene.bar_pose(2, 0.0, 0.0, 0.0);
    scene.bar_pose(3, 1.0, 1.0, 0.85);
    shoot(&mut scene, "bar_press")?;
    Ok(())
}

/// Every material as a panel over the garden, plus each as the garden itself.
fn surfaces_sheet(gpu: &Gpu, dir: &Path, (w, h): (u32, u32)) -> Result<(), String> {
    let size = (w as f32, h as f32);
    let backdrop = Backdrop::new(gpu, w, h);
    let target = gpu.render_texture("surfaces", w, h, false);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    for garden_material in [Material::RoughWood, Material::Stone] {
        let garden = gpu.surface(&gpu.white);
        garden.set(
            &gpu.queue,
            size,
            &Surface {
                radius: 0.0,
                bevel: 0.0,
                ..Surface::preset(garden_material, Rect::new(0, 0, w as i32, h as i32))
            },
        );
        let blit = gpu.quad(&backdrop.view, &gpu.clamp);
        blit.set(
            &gpu.queue,
            size,
            QuadParams::new([0.0, 0.0, size.0, size.1]),
        );
        let (cols, pad) = (4, 40.0);
        let pw = (size.0 - pad * (cols as f32 + 1.0)) / cols as f32;
        let ph = (size.1 - pad * 3.0) / 2.0;
        let panels: Vec<_> = Material::ALL
            .iter()
            .enumerate()
            .map(|(i, &material)| {
                let (col, row) = ((i % cols) as f32, (i / cols) as f32);
                let rect = Rect::new(
                    (pad + col * (pw + pad)) as i32,
                    (pad + row * (ph + pad)) as i32,
                    pw as i32,
                    ph as i32,
                );
                let draw = gpu.surface(&backdrop.view);
                draw.set(
                    &gpu.queue,
                    size,
                    &Surface {
                        radius: 22.0,
                        bevel: 8.0,
                        ..Surface::preset(material, rect)
                    },
                );
                draw
            })
            .collect();
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("surfaces"),
            });
        backdrop.render(gpu, &mut encoder, &garden);
        {
            let mut pass = clear_pass(&mut encoder, &view, "surfaces");
            gpu.draw_quad(&mut pass, &blit, Dest::Target);
            for panel in &panels {
                gpu.draw_surface(&mut pass, panel, Dest::Target);
            }
        }
        gpu.queue.submit([encoder.finish()]);
        let name = format!(
            "surfaces_on_{}.png",
            garden_material.name().replace(' ', "_")
        );
        write_png(&dir.join(&name), w, h, &read_rgba(gpu, &target))?;
        println!(
            "{name}: {} materials (glass, etched, stained, stone, glassy stone, metal, wood)",
            Material::ALL.len()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- bench ---------------------

fn percentile(values: &mut [f64], p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 - 1.0) * p).round() as usize;
    values[index.min(values.len() - 1)]
}

fn bench((w, h): (u32, u32), style: Style) -> Result<(), String> {
    let screen = Rect::new(0, 0, w as i32, h as i32);
    let (fw, fh) = (w as f32, h as f32);
    let terminals = [
        Rect::new(
            (fw * 0.05) as i32,
            (fh * 0.08) as i32,
            (fw * 0.36) as i32,
            (fh * 0.38) as i32,
        ),
        Rect::new(
            (fw * 0.53) as i32,
            (fh * 0.12) as i32,
            (fw * 0.40) as i32,
            (fh * 0.42) as i32,
        ),
        Rect::new(
            (fw * 0.22) as i32,
            (fh * 0.56) as i32,
            (fw * 0.42) as i32,
            (fh * 0.29) as i32,
        ),
    ];
    let card = Rect::new(screen.w / 2 - 78, screen.h - 116, 156, 96);
    println!("scene: {w}x{h}, a maximized window over three terminals");
    if cfg!(debug_assertions) {
        println!("note: a debug build — CPU timings are not representative; use --release");
    }

    let mut planner = Planner::new();
    let mut plan = templar_effects::Plan::default();
    for (label, sink) in [("garden", Sink::Garden), ("card", Sink::Card(card))] {
        let request = Request {
            screen,
            window: screen,
            obstacles: &terminals,
            sink,
            timing: Timing::DEFAULT,
        };
        planner.plan_into(&request, &mut plan);
        let mut times: Vec<f64> = (0..300)
            .map(|_| {
                let started = Instant::now();
                planner.plan_into(&request, &mut plan);
                started.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        println!(
            "plan  {label:<6}  median {:>7.1} µs  p95 {:>7.1} µs  max {:>7.1} µs   field {}x{} (cell {} px), {} motes, {} KiB to upload",
            percentile(&mut times, 0.5),
            percentile(&mut times, 0.95),
            percentile(&mut times, 1.0),
            plan.cols,
            plan.rows,
            plan.cell_px,
            plan.motes.len(),
            (plan.field.len() + plan.mote_bytes().len()) / 1024
        );
    }

    let gpu = offscreen_gpu()?;
    println!("adapter: {}", gpu.adapter);
    if !gpu.timestamps {
        println!("gpu: this adapter has no timestamp queries; GPU timing skipped");
        return Ok(());
    }
    let canvas = paint::window(paint::Kind::Browser, w as i32, h as i32, 4);
    let (_window_texture, window_view) = gpu.texture("bench window", w, h, &canvas.px);
    let target = gpu.render_texture("bench target", w, h, false);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let plain = gpu.quad(&window_view, &gpu.clamp);
    plain.set(&gpu.queue, (fw, fh), QuadParams::new(screen.to_f32()));
    planner.plan_into(
        &Request {
            screen,
            window: screen,
            obstacles: &terminals,
            sink: Sink::Card(card),
            timing: Timing::DEFAULT,
        },
        &mut plan,
    );
    let essence = gpu.essence(&plan, &window_view);

    // Per progress: plain window | body | motes, each its own timed pass over a loaded target.
    let progresses = [0.03_f32, 0.15, 0.3, 0.5, 0.7, 0.9];
    let passes = 3;
    let queries = (progresses.len() * passes * 2) as u32;
    let set = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: Some("bench"),
        ty: wgpu::QueryType::Timestamp,
        count: queries,
    });
    // Each progress resolves into its own aligned slice of the buffer.
    let stride = wgpu::QUERY_RESOLVE_BUFFER_ALIGNMENT;
    let resolve_size = progresses.len() as u64 * stride;
    let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("resolve"),
        size: resolve_size,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: resolve_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let period = f64::from(gpu.queue.get_timestamp_period());
    let mut samples = vec![Vec::new(); progresses.len() * passes];
    for iteration in 0..60 {
        for (pi, &u) in progresses.iter().enumerate() {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("bench"),
                });
            essence.set(&gpu.queue, &plan, u, &style);
            for pass_kind in 0..passes {
                let index = ((pi * passes + pass_kind) * 2) as u32;
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("timed"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                        query_set: &set,
                        beginning_of_pass_write_index: Some(index),
                        end_of_pass_write_index: Some(index + 1),
                    }),
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                match pass_kind {
                    0 => gpu.draw_quad(&mut pass, &plain, Dest::Target),
                    1 => gpu.draw_body(&mut pass, &essence),
                    _ => gpu.draw_motes(&mut pass, &essence),
                }
            }
            let first = (pi * passes * 2) as u32;
            encoder.resolve_query_set(
                &set,
                first..first + (passes * 2) as u32,
                &resolve,
                pi as u64 * stride,
            );
            gpu.queue.submit([encoder.finish()]);
        }
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("copy"),
            });
        encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, resolve_size);
        gpu.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = gpu.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        });
        {
            let data = slice
                .get_mapped_range()
                .map_err(|e| format!("map timestamps: {e:?}"))?;
            if iteration >= 10 {
                for pi in 0..progresses.len() {
                    let base = pi * stride as usize;
                    let stamps: Vec<u64> = data[base..base + passes * 16]
                        .chunks_exact(8)
                        .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
                        .collect();
                    for (k, pair) in stamps.chunks_exact(2).enumerate() {
                        samples[pi * passes + k]
                            .push(pair[1].saturating_sub(pair[0]) as f64 * period / 1000.0);
                    }
                }
            }
        }
        readback.unmap();
    }
    println!(
        "gpu   median µs per pass (50 frames after 10 warm-up), {} motes, grain {:.1} px, minimize into a card:",
        plan.motes.len(),
        style.grain_px
    );
    println!("      progress   plain window   essence body   essence motes");
    for (pi, &u) in progresses.iter().enumerate() {
        let mut cell = |k: usize| percentile(&mut samples[pi * passes + k], 0.5);
        let (plain_us, body_us, motes_us) = (cell(0), cell(1), cell(2));
        println!("      {u:>8.2}   {plain_us:>12.1}   {body_us:>12.1}   {motes_us:>13.1}");
    }
    Ok(())
}

// ---------------------------------------------------------------- interactive ---------------

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    gpu: Gpu,
    scene: Option<Scene>,
    last: Instant,
    title_at: Instant,
}

#[derive(Default)]
struct App {
    state: Option<State>,
    error: Option<String>,
    style: Style,
}

fn interactive(style: Style) -> Result<(), String> {
    println!(
        "keys: D desktop · M minimize · R restore · L login veil · [ ] duration · S slow · G garden · N/1-7 notification · F field · H desktop cards/soak · , . grain (or the slider) · Esc"
    );
    let event_loop = EventLoop::new().map_err(|e| e.to_string())?;
    let mut app = App {
        style,
        ..App::default()
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.error.map_or(Ok(()), Err)
}

impl State {
    fn new(event_loop: &ActiveEventLoop, style: Style) -> Result<Self, String> {
        let attributes = Window::default_attributes()
            .with_title("effects-sandbox")
            .with_inner_size(winit::dpi::PhysicalSize::new(1600, 900));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| e.to_string())?;
        let adapter = pollster::block_on(gpu::adapter(&instance, Some(&surface)))?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the surface is unsupported on this adapter")?;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 2;
        let view_format = config.format.add_srgb_suffix();
        if view_format != config.format {
            config.view_formats.push(view_format);
        }
        let gpu = pollster::block_on(Gpu::new(&adapter, view_format))?;
        surface.configure(&gpu.device, &config);
        println!("adapter: {}", gpu.adapter);
        let mut scene = Scene::new(&gpu, config.width, config.height);
        scene.style = style;
        scene.show_slider = true;
        Ok(Self {
            window,
            surface,
            config,
            view_format,
            gpu,
            scene: Some(scene),
            last: Instant::now(),
            title_at: Instant::now(),
        })
    }

    fn scene(&mut self) -> &mut Scene {
        self.scene.as_mut().expect("scene")
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w == self.config.width && h == self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.gpu.device, &self.config);
        let scene = self.scene.take().expect("scene");
        self.scene = Some(scene.resized(&self.gpu, w, h));
    }

    fn key(&mut self, key: &Key) -> bool {
        let gpu = &self.gpu;
        let scene = self.scene.as_mut().expect("scene");
        match key {
            Key::Named(NamedKey::Escape) => return false,
            Key::Character(c) => match c.as_str() {
                "d" | "D" => scene.desktop(gpu),
                "m" | "M" => scene.minimize(gpu),
                "r" | "R" => scene.restore_last(gpu),
                "l" | "L" => scene.login(gpu),
                "[" => scene.adjust_duration(-25.0),
                "]" => scene.adjust_duration(25.0),
                "s" | "S" => scene.slow = !scene.slow,
                "g" | "G" => scene.cycle_garden(),
                "f" | "F" => scene.show_field = !scene.show_field,
                "h" | "H" => scene.desktop_cards = !scene.desktop_cards,
                "," => scene.scale_grain(1.0 / 1.25),
                "." => scene.scale_grain(1.25),
                "n" | "N" => {
                    scene.notification = if scene.notification.is_some() {
                        None
                    } else {
                        Some(Material::Glass)
                    }
                }
                digit => {
                    if let Some(material) = digit
                        .parse::<usize>()
                        .ok()
                        .and_then(|d| Material::ALL.get(d.wrapping_sub(1)))
                    {
                        scene.notification = Some(*material);
                    }
                }
            },
            _ => {}
        }
        true
    }

    fn frame(&mut self) {
        let dt = self.last.elapsed().as_secs_f32() * 1000.0;
        self.last = Instant::now();
        self.scene().tick(dt.min(100.0));
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.gpu.device, &self.config);
                return;
            }
            _ => return,
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        let scene = self.scene.as_mut().expect("scene");
        let commands = scene.render(&self.gpu, &view);
        self.gpu.queue.submit([commands]);
        self.gpu.queue.present(frame);
        if self.title_at.elapsed().as_millis() > 250 {
            self.title_at = Instant::now();
            self.window
                .set_title(&self.scene.as_ref().expect("scene").status());
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_none() {
            match State::new(event_loop, self.style) {
                Ok(state) => self.state = Some(state),
                Err(error) => {
                    self.error = Some(error);
                    event_loop.exit();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                state.resize(size.width, size.height);
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                if !state.key(&event.logical_key) {
                    event_loop.exit();
                }
                state.last = Instant::now();
                state.window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                if state.scene().moved(position.x as f32, position.y as f32) {
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: button_state,
                button: MouseButton::Left,
                ..
            } => {
                if button_state == ElementState::Pressed {
                    let gpu = &state.gpu;
                    state.scene.as_mut().expect("scene").press(gpu);
                    state.last = Instant::now();
                } else {
                    state.scene().release();
                }
                state.window.request_redraw();
            }
            WindowEvent::RedrawRequested => state.frame(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Idle costs nothing: a frame is requested only while something is moving.
        if let Some(state) = self.state.as_mut()
            && state.scene().animating()
        {
            state.window.request_redraw();
        }
    }
}
