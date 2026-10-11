//! Context-local, GPU-only Essence snapshots and a two-draw GLES 2 transition.
//! The planner and static mote batch are retained across reversal. No readback/device exists here.
#![allow(unsafe_code)]

use crate::{motion::EssenceMotion, wallpaper::WallpaperRenderer};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ContextId, Frame, ImportMem, Offscreen, Renderer, Texture,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Element, Id, Kind, RenderElement,
                surface::WaylandSurfaceRenderElement,
            },
            gles::{
                Capability, GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexture,
                Uniform, UniformName, UniformType, ffi,
            },
            utils::{CommitCounter, OpaqueRegions},
        },
    },
    desktop::Window,
    utils::{Buffer, Physical, Rectangle, Scale, Transform, user_data::UserDataMap},
};
use std::{
    collections::HashMap,
    ffi::CString,
    sync::{Arc, Weak},
};
use templar_effects::essence::{Plan, Style, UNIFORM_BYTES};
use templar_window::{Rect, WindowId};

const COMMON: &str = include_str!("essence_shaders/common.glsl");
const BODY: &str = include_str!("essence_shaders/body.frag");
const VERTEX: &str = include_str!("essence_shaders/motes.vert");
const FRAGMENT: &str = include_str!("essence_shaders/motes.frag");
const NAMES: [&str; 8] = [
    "e_screen", "e_window", "e_field", "e_timing", "e_glow", "e_jitter", "e_dust", "e_counts",
];
const MAX_BACKING_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
struct Programs {
    body: GlesPixelProgram,
    motes: u32,
    noise: GlesTexture,
}
#[derive(Debug)]
struct Context {
    id: ContextId<GlesTexture>,
    programs: Result<Programs, String>,
    entries: HashMap<WindowId, Entry>,
    failed: HashMap<WindowId, Weak<Plan>>,
    retired_buffers: Vec<u32>,
}
#[derive(Debug)]
struct Entry {
    plan: Arc<Plan>,
    snapshot: GlesTexture,
    field: GlesTexture,
    output: GlesTexture,
    vertices: u32,
    vertex_count: i32,
    bounds: Rect,
    id: Id,
    commit: CommitCounter,
    failed: bool,
    last: Option<(u32, u32)>,
    backing_bytes: u64,
    scale_bits: u64,
}
#[derive(Debug, Default)]
pub struct EssenceRenderer {
    contexts: Vec<Context>,
}

impl EssenceRenderer {
    /// Drop snapshots immediately when output policy stops ordinary rendering.
    /// Texture drops use Smithay's context-owned deletion queue. Raw VBO deletion
    /// waits until its owner's next make-current, including multi-GPU wakeup.
    pub fn clear(&mut self) {
        for cache in &mut self.contexts {
            cache
                .retired_buffers
                .extend(cache.entries.drain().map(|(_, entry)| entry.vertices));
            cache.failed.clear();
        }
    }

    pub fn retire(&mut self, renderer: &mut GlesRenderer, active: impl Fn(WindowId) -> bool) {
        let context = renderer.context_id();
        for cache in &mut self.contexts {
            cache.failed.retain(|id, _| active(*id));
            let retired: Vec<_> = cache
                .entries
                .keys()
                .copied()
                .filter(|id| !active(*id))
                .collect();
            for id in retired {
                if let Some(entry) = cache.entries.remove(&id) {
                    cache.retired_buffers.push(entry.vertices);
                }
            }
            if cache.id == context {
                delete_retired_buffers(renderer, cache);
            }
        }
    }

    pub fn element<R: WallpaperRenderer>(
        &mut self,
        renderer: &mut R,
        window_id: WindowId,
        window: &Window,
        motion: &EssenceMotion,
        style: Style,
        now_ns: u64,
        output: Rect,
        scale: f64,
    ) -> Option<EssenceElement> {
        // A plan belongs to one output. Never apply its coordinates to a neighboring scene.
        let screen = motion.plan.screen;
        if Rect::new(screen.x, screen.y, screen.w, screen.h) != output {
            return None;
        }
        let gl = renderer.wallpaper_gles();
        let context_id = gl.context_id();
        let index = if let Some(index) = self
            .contexts
            .iter()
            .position(|cache| cache.id == context_id)
        {
            index
        } else {
            let programs = Programs::new(gl).map_err(|failure| {
                let failure = failure.to_string();
                tracing::warn!(%failure, "Essence GLES initialization failed; using simple motion");
                failure
            });
            self.contexts.push(Context {
                id: context_id,
                programs,
                entries: HashMap::new(),
                failed: HashMap::new(),
                retired_buffers: Vec::new(),
            });
            self.contexts.len() - 1
        };
        let cache = &mut self.contexts[index];
        delete_retired_buffers(gl, cache);
        if cache.programs.is_err() {
            return None;
        }
        if cache
            .failed
            .get(&window_id)
            .and_then(Weak::upgrade)
            .is_some_and(|plan| Arc::ptr_eq(&plan, &motion.plan))
        {
            return None;
        }
        cache.failed.remove(&window_id);
        let replace = cache.entries.get(&window_id).is_none_or(|entry| {
            !Arc::ptr_eq(&entry.plan, &motion.plan) || entry.scale_bits != scale.to_bits()
        });
        if replace {
            if let Some(old) = cache.entries.remove(&window_id) {
                let _ = gl.with_context(|api| unsafe {
                    api.DeleteBuffers(1, &old.vertices);
                });
            }
            let resident: u64 = cache
                .entries
                .values()
                .map(|entry| entry.backing_bytes)
                .sum();
            match Entry::new(
                gl,
                window,
                motion.plan.clone(),
                output,
                scale,
                MAX_BACKING_BYTES.saturating_sub(resident),
            ) {
                Ok(entry) => {
                    cache.entries.insert(window_id, entry);
                }
                Err(failure) => {
                    // Remember rejection for the lifetime of this plan, including reversals.
                    tracing::warn!(%failure, ?window_id, "Essence snapshot unavailable; using simple motion");
                    // A rejected snapshot does not retain any failed allocations.
                    cache.failed.insert(window_id, Arc::downgrade(&motion.plan));
                    return None;
                }
            }
        }
        let programs = cache.programs.as_ref().ok()?;
        let entry = cache.entries.get_mut(&window_id)?;
        if entry.failed {
            return None;
        }
        let u = motion.at(now_ns).u;
        let key = (u.to_bits(), style.grain_px.to_bits());
        if entry.last != Some(key) {
            if let Err(failure) = entry.render(gl, programs, u, style) {
                tracing::warn!(%failure, ?window_id, "Essence draw failed; using simple motion");
                entry.failed = true;
                return None;
            }
            entry.last = Some(key);
            entry.commit.increment();
        }
        Some(EssenceElement {
            texture: entry.output.clone(),
            id: entry.id.clone(),
            commit: entry.commit,
            geometry: Rectangle::new(
                (
                    ((entry.bounds.x - output.x) as f64 * scale).round() as i32,
                    ((entry.bounds.y - output.y) as f64 * scale).round() as i32,
                )
                    .into(),
                (entry.output.width() as i32, entry.output.height() as i32).into(),
            ),
        })
    }
}

impl Programs {
    fn new(renderer: &mut GlesRenderer) -> anyhow::Result<Self> {
        let names = uniform_names();
        let body = renderer.compile_custom_pixel_shader(format!("{COMMON}\n{BODY}"), &names)?;
        let noise = renderer.import_memory(
            &templar_effects::noise::noise_rgba(128),
            Fourcc::Abgr8888,
            (128, 128).into(),
            false,
        )?;
        renderer.with_context(|gl| unsafe {
            let mut bound = 0;
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut bound);
            gl.BindTexture(ffi::TEXTURE_2D, noise.tex_id());
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::REPEAT as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::REPEAT as i32);
            gl.BindTexture(ffi::TEXTURE_2D, bound as u32);
        })?;
        let motes = renderer.with_context(|gl| compile_motes(gl))??;
        Ok(Self { body, motes, noise })
    }
}
impl Entry {
    fn new(
        renderer: &mut GlesRenderer,
        window: &Window,
        plan: Arc<Plan>,
        screen: Rect,
        scale: f64,
        budget: u64,
    ) -> anyhow::Result<Self> {
        let margin = plan.piece_px.saturating_mul(4).min(i32::MAX as u32) as i32;
        let b = plan.bounds;
        let bounds = Rect::new(
            b.x.saturating_sub(margin),
            b.y.saturating_sub(margin),
            b.w.saturating_add(margin * 2),
            b.h.saturating_add(margin * 2),
        )
        .intersection(screen)
        .ok_or_else(|| anyhow::anyhow!("effect bounds outside output"))?;
        // Capture the entire committed tree, including shadows/subsurfaces and
        // tracked popups. The planner uses these same bounds in space_sync.
        let geometry = window.bbox_with_popups();
        let size = |w: i32, h: i32| {
            (
                ((f64::from(w) * scale).ceil() as i32).max(1),
                ((f64::from(h) * scale).ceil() as i32).max(1),
            )
        };
        let snapshot_size = size(geometry.size.w, geometry.size.h);
        let effect_size = size(bounds.width, bounds.height);
        let bytes = (snapshot_size.0 as u64 * snapshot_size.1 as u64
            + effect_size.0 as u64 * effect_size.1 as u64)
            * 4;
        anyhow::ensure!(
            bytes <= budget,
            "effect snapshot and backing exceed bounded budget"
        );
        let mut max = 0;
        renderer.with_context(|gl| unsafe {
            gl.GetIntegerv(ffi::MAX_TEXTURE_SIZE, &mut max);
        })?;
        anyhow::ensure!(
            [
                snapshot_size.0,
                snapshot_size.1,
                effect_size.0,
                effect_size.1
            ]
            .into_iter()
            .all(|side| side <= max),
            "effect exceeds GLES texture limit"
        );
        let mut snapshot = <GlesRenderer as Offscreen<GlesTexture>>::create_buffer(
            renderer,
            Fourcc::Abgr8888,
            snapshot_size.into(),
        )?;
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
            renderer,
            (
                (-f64::from(geometry.loc.x) * scale).round() as i32,
                (-f64::from(geometry.loc.y) * scale).round() as i32,
            )
                .into(),
            Scale::from(scale),
            1.0,
        );
        anyhow::ensure!(!elements.is_empty(), "window has no committed surface tree");
        let mut tracker = OutputDamageTracker::new(snapshot_size, scale, Transform::Normal);
        let mut framebuffer = renderer.bind(&mut snapshot)?;
        tracker.render_output(
            renderer,
            &mut framebuffer,
            0,
            &elements,
            [0.0, 0.0, 0.0, 0.0],
        )?;
        drop(framebuffer);
        let field = renderer.import_memory(
            &plan.field,
            Fourcc::Abgr8888,
            (plan.cols as i32, plan.rows as i32).into(),
            false,
        )?;
        let output = <GlesRenderer as Offscreen<GlesTexture>>::create_buffer(
            renderer,
            Fourcc::Abgr8888,
            effect_size.into(),
        )?;
        let vertex_bytes = expanded_motes(&plan);
        let vertices = renderer.with_context(|gl| unsafe {
            let mut old = 0;
            gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut old);
            let mut vbo = 0;
            gl.GenBuffers(1, &mut vbo);
            gl.BindBuffer(ffi::ARRAY_BUFFER, vbo);
            gl.BufferData(
                ffi::ARRAY_BUFFER,
                vertex_bytes.len() as isize,
                vertex_bytes.as_ptr().cast(),
                ffi::STATIC_DRAW,
            );
            let failure = gl.GetError();
            gl.BindBuffer(ffi::ARRAY_BUFFER, old as u32);
            if failure != ffi::NO_ERROR {
                gl.DeleteBuffers(1, &vbo);
                Err(anyhow::anyhow!("GLES mote upload error {failure:x}"))
            } else {
                Ok(vbo)
            }
        })??;
        Ok(Self {
            vertex_count: (plan.motes.len() * 6) as i32,
            plan,
            snapshot,
            field,
            output,
            vertices,
            bounds,
            id: Id::new(),
            commit: CommitCounter::default(),
            failed: false,
            last: None,
            backing_bytes: bytes,
            scale_bits: scale.to_bits(),
        })
    }
    fn render(
        &mut self,
        renderer: &mut GlesRenderer,
        programs: &Programs,
        u: f32,
        style: Style,
    ) -> anyhow::Result<()> {
        let packed = self.plan.uniform(u, &style);
        let data = unpack_uniform(&packed);
        let bounds = [
            self.bounds.x as f32,
            self.bounds.y as f32,
            self.bounds.width as f32,
            self.bounds.height as f32,
        ];
        let uniforms = uniform_values(&data, bounds);
        let size = (self.output.width() as i32, self.output.height() as i32);
        let damage = [Rectangle::from_size(size.into())];
        let mut framebuffer = renderer.bind(&mut self.output)?;
        let mut frame = renderer.render(&mut framebuffer, size.into(), Transform::Normal)?;
        frame.clear([0.0, 0.0, 0.0, 0.0].into(), &damage)?;
        let textures = [
            self.snapshot.tex_id(),
            self.field.tex_id(),
            programs.noise.tex_id(),
        ];
        let saved = frame.with_context(|gl| bind_textures(gl, textures))?;
        let instancing = frame.capabilities().contains(&Capability::Instancing);
        let body = frame.render_pixel_shader_to(
            &programs.body,
            Rectangle::from_size(size.into()).to_f64(),
            Rectangle::from_size(size.into()),
            size.into(),
            Some(&damage),
            1.0,
            &uniforms,
        );
        let motes = if body.is_ok() {
            frame
                .with_context(|gl| {
                    draw_motes(
                        gl,
                        programs.motes,
                        self.vertices,
                        self.vertex_count,
                        &data,
                        bounds,
                        instancing,
                    )
                })?
                .map_err(anyhow::Error::msg)
        } else {
            Err(anyhow::anyhow!("Essence body draw failed"))
        };
        frame.with_context(|gl| restore_textures(gl, saved))?;
        body?;
        motes?;
        // The result texture is consumed next on this same GLES context; command
        // ordering supplies visibility without a blocking CPU fence wait.
        let _fence = frame.finish()?;
        Ok(())
    }
}

fn delete_retired_buffers(renderer: &mut GlesRenderer, cache: &mut Context) {
    if cache.retired_buffers.is_empty() {
        return;
    }
    if renderer
        .with_context(|gl| unsafe {
            gl.DeleteBuffers(
                cache.retired_buffers.len() as i32,
                cache.retired_buffers.as_ptr(),
            );
        })
        .is_ok()
    {
        cache.retired_buffers.clear();
    }
}

fn uniform_names() -> Vec<UniformName<'static>> {
    let mut names: Vec<_> = NAMES
        .into_iter()
        .map(|name| UniformName::new(name, UniformType::_4f))
        .collect();
    names.extend((0..16).map(|i| UniformName::new(format!("e_obstacles[{i}]"), UniformType::_4f)));
    names.push(UniformName::new("e_bounds", UniformType::_4f));
    names.extend(
        ["e_window_tex", "e_field_tex", "e_noise_tex"]
            .into_iter()
            .map(|name| UniformName::new(name, UniformType::_1i)),
    );
    names
}
fn unpack_uniform(bytes: &[u8; UNIFORM_BYTES]) -> [[f32; 4]; 24] {
    let mut values = [[0.0; 4]; 24];
    for (i, chunk) in bytes.chunks_exact(16).enumerate() {
        for j in 0..4 {
            let raw: [u8; 4] = chunk[j * 4..j * 4 + 4]
                .try_into()
                .expect("four-byte uniform");
            values[i][j] = if i == 7 {
                u32::from_le_bytes(raw) as f32
            } else {
                f32::from_le_bytes(raw)
            };
        }
    }
    values
}
fn uniform_values(data: &[[f32; 4]; 24], bounds: [f32; 4]) -> Vec<Uniform<'static>> {
    let mut uniforms: Vec<_> = NAMES
        .into_iter()
        .enumerate()
        .map(|(i, name)| Uniform::new(name, data[i]))
        .collect();
    uniforms.extend((0..16).map(|i| Uniform::new(format!("e_obstacles[{i}]"), data[i + 8])));
    uniforms.push(Uniform::new("e_bounds", bounds));
    for (i, name) in ["e_window_tex", "e_field_tex", "e_noise_tex"]
        .into_iter()
        .enumerate()
    {
        uniforms.push(Uniform::new(name, i as i32));
    }
    uniforms
}
fn expanded_motes(plan: &Plan) -> Vec<u8> {
    let packed = plan.mote_bytes();
    let mut bytes = Vec::with_capacity(plan.motes.len() * 6 * 48);
    for mote in packed.chunks_exact(40) {
        for corner in [
            [0.0f32, 0.0f32],
            [1.0, 0.0],
            [0.0, 1.0],
            [0.0, 1.0],
            [1.0, 0.0],
            [1.0, 1.0],
        ] {
            for value in corner {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
            bytes.extend_from_slice(mote);
        }
    }
    bytes
}

fn compile_motes(gl: &ffi::Gles2) -> anyhow::Result<u32> {
    compile_mote_sources(gl, VERTEX, FRAGMENT)
}

fn compile_mote_sources(
    gl: &ffi::Gles2,
    vertex_source: &str,
    fragment_source: &str,
) -> anyhow::Result<u32> {
    unsafe {
        let shader = |kind, source: String| -> anyhow::Result<u32> {
            let source = CString::new(source)?;
            let shader = gl.CreateShader(kind);
            gl.ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null());
            gl.CompileShader(shader);
            let mut success = 0;
            gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut success);
            if success == 0 {
                let mut log = [0i8; 4096];
                gl.GetShaderInfoLog(shader, 4096, std::ptr::null_mut(), log.as_mut_ptr());
                gl.DeleteShader(shader);
                return Err(anyhow::anyhow!(
                    "Essence shader: {}",
                    std::ffi::CStr::from_ptr(log.as_ptr()).to_string_lossy()
                ));
            }
            Ok(shader)
        };
        let vertex = shader(
            ffi::VERTEX_SHADER,
            format!("#version 100\n{COMMON}\n{vertex_source}"),
        )?;
        let fragment = match shader(
            ffi::FRAGMENT_SHADER,
            format!("#version 100\n{COMMON}\n{fragment_source}"),
        ) {
            Ok(shader) => shader,
            Err(error) => {
                gl.DeleteShader(vertex);
                return Err(error);
            }
        };
        let program = gl.CreateProgram();
        gl.AttachShader(program, vertex);
        gl.AttachShader(program, fragment);
        for (i, name) in [c"corner", c"p01", c"p23", c"p45", c"p67", c"times"]
            .into_iter()
            .enumerate()
        {
            gl.BindAttribLocation(program, i as u32, name.as_ptr());
        }
        gl.LinkProgram(program);
        gl.DeleteShader(vertex);
        gl.DeleteShader(fragment);
        let mut success = 0;
        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut success);
        if success == 0 {
            gl.DeleteProgram(program);
            anyhow::bail!("Essence mote shader link failed");
        }
        Ok(program)
    }
}
fn bind_textures(gl: &ffi::Gles2, textures: [u32; 3]) -> (i32, [i32; 3]) {
    unsafe {
        let mut active = 0;
        let mut previous = [0; 3];
        gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active);
        for (i, texture) in textures.into_iter().enumerate() {
            gl.ActiveTexture(ffi::TEXTURE0 + i as u32);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut previous[i]);
            gl.BindTexture(ffi::TEXTURE_2D, texture);
            // Smithay sets these in render_texture, but custom pixel shaders
            // and our raw batch bypass that path. Level-zero imports/FBOs are
            // incomplete with GL's default mipmapped minifier (black samples).
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            if i < 2 {
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_S,
                    ffi::CLAMP_TO_EDGE as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_WRAP_T,
                    ffi::CLAMP_TO_EDGE as i32,
                );
            }
        }
        gl.ActiveTexture(ffi::TEXTURE0);
        (active, previous)
    }
}
fn restore_textures(gl: &ffi::Gles2, saved: (i32, [i32; 3])) {
    unsafe {
        for (i, texture) in saved.1.into_iter().enumerate() {
            gl.ActiveTexture(ffi::TEXTURE0 + i as u32);
            gl.BindTexture(ffi::TEXTURE_2D, texture as u32);
        }
        gl.ActiveTexture(saved.0 as u32);
    }
}

fn draw_motes(
    gl: &ffi::Gles2,
    program: u32,
    vertices: u32,
    count: i32,
    data: &[[f32; 4]; 24],
    bounds: [f32; 4],
    instancing: bool,
) -> Result<(), String> {
    unsafe {
        let mut previous_program = 0;
        let mut buffer = 0;
        let mut blend = [0; 4];
        let mut equations = [0; 2];
        gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut previous_program);
        gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut buffer);
        for (i, key) in [
            ffi::BLEND_SRC_RGB,
            ffi::BLEND_DST_RGB,
            ffi::BLEND_SRC_ALPHA,
            ffi::BLEND_DST_ALPHA,
        ]
        .into_iter()
        .enumerate()
        {
            gl.GetIntegerv(key, &mut blend[i]);
        }
        gl.GetIntegerv(ffi::BLEND_EQUATION_RGB, &mut equations[0]);
        gl.GetIntegerv(ffi::BLEND_EQUATION_ALPHA, &mut equations[1]);
        let enabled = gl.IsEnabled(ffi::BLEND);
        let scissor = gl.IsEnabled(ffi::SCISSOR_TEST);
        let mut attrs = [[0; 7]; 6];
        let mut pointers = [std::ptr::null_mut(); 6];
        for i in 0..6 {
            for (j, key) in [
                ffi::VERTEX_ATTRIB_ARRAY_ENABLED,
                ffi::VERTEX_ATTRIB_ARRAY_SIZE,
                ffi::VERTEX_ATTRIB_ARRAY_TYPE,
                ffi::VERTEX_ATTRIB_ARRAY_NORMALIZED,
                ffi::VERTEX_ATTRIB_ARRAY_STRIDE,
                ffi::VERTEX_ATTRIB_ARRAY_BUFFER_BINDING,
            ]
            .into_iter()
            .enumerate()
            {
                gl.GetVertexAttribiv(i as u32, key, &mut attrs[i][j]);
            }
            gl.GetVertexAttribPointerv(
                i as u32,
                ffi::VERTEX_ATTRIB_ARRAY_POINTER,
                &mut pointers[i],
            );
            if instancing {
                gl.GetVertexAttribiv(i as u32, ffi::VERTEX_ATTRIB_ARRAY_DIVISOR, &mut attrs[i][6]);
            }
        }
        gl.UseProgram(program);
        gl.BindBuffer(ffi::ARRAY_BUFFER, vertices);
        gl.Enable(ffi::BLEND);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.BlendEquation(ffi::FUNC_ADD);
        gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
        for i in 0..6 {
            gl.EnableVertexAttribArray(i);
            if instancing {
                gl.VertexAttribDivisor(i, 0);
            }
            let (size, kind, normalized, offset) = if i == 0 {
                (2, ffi::FLOAT, ffi::FALSE, 0)
            } else {
                (4, ffi::UNSIGNED_SHORT, ffi::TRUE, 8 + (i - 1) * 8)
            };
            gl.VertexAttribPointer(i, size, kind, normalized, 48, (offset as usize) as *const _);
        }
        let location = |name: &str| {
            let name = CString::new(name).expect("static uniform name");
            gl.GetUniformLocation(program, name.as_ptr())
        };
        for (i, name) in NAMES.into_iter().enumerate() {
            gl.Uniform4fv(location(name), 1, data[i].as_ptr());
        }
        for i in 0..16 {
            gl.Uniform4fv(
                location(&format!("e_obstacles[{i}]")),
                1,
                data[i + 8].as_ptr(),
            );
        }
        gl.Uniform4fv(location("e_bounds"), 1, bounds.as_ptr());
        for (i, name) in ["e_window_tex", "e_field_tex", "e_noise_tex"]
            .into_iter()
            .enumerate()
        {
            gl.Uniform1i(location(name), i as i32);
        }
        gl.DrawArrays(ffi::TRIANGLES, 0, count);
        let failure = gl.GetError();
        for i in 0..6 {
            gl.BindBuffer(ffi::ARRAY_BUFFER, attrs[i][5] as u32);
            gl.VertexAttribPointer(
                i as u32,
                attrs[i][1],
                attrs[i][2] as u32,
                attrs[i][3] as u8,
                attrs[i][4],
                pointers[i],
            );
            if attrs[i][0] == 0 {
                gl.DisableVertexAttribArray(i as u32);
            } else {
                gl.EnableVertexAttribArray(i as u32);
            }
            if instancing {
                gl.VertexAttribDivisor(i as u32, attrs[i][6] as u32);
            }
        }
        gl.BindBuffer(ffi::ARRAY_BUFFER, buffer as u32);
        gl.UseProgram(previous_program as u32);
        gl.BlendEquationSeparate(equations[0] as u32, equations[1] as u32);
        gl.BlendFuncSeparate(
            blend[0] as u32,
            blend[1] as u32,
            blend[2] as u32,
            blend[3] as u32,
        );
        if enabled == ffi::FALSE {
            gl.Disable(ffi::BLEND);
        }
        if scissor != ffi::FALSE {
            gl.Enable(ffi::SCISSOR_TEST);
        }
        if failure == ffi::NO_ERROR {
            Ok(())
        } else {
            Err(format!("Essence mote draw error {failure:x}"))
        }
    }
}

#[derive(Debug)]
pub struct EssenceElement {
    texture: GlesTexture,
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<i32, Physical>,
}
impl Element for EssenceElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit
    }
    fn geometry(&self, _: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.texture.size()).to_f64()
    }
    fn opaque_regions(&self, _: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        1.0
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}
impl EssenceElement {
    pub(crate) fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            &[],
            Transform::Normal,
            1.0,
            None,
            &[],
        )
    }
}
impl<R: WallpaperRenderer> RenderElement<R> for EssenceElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _: &[Rectangle<i32, Physical>],
        _: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_essence(self, frame, src, dst, damage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::backend::{
        egl::{EGLContext, EGLDisplay, native::EGLSurfacelessDisplay},
        renderer::ExportMem,
    };
    use templar_effects::{
        essence::{Planner, Request, Sink, Timing},
        geometry::Rect as EffectRect,
    };

    /// This explicit qualification owns a surfaceless context and temporary textures.
    /// Readback is test-only, never used by the production transition renderer.
    #[test]
    #[ignore = "owned surfaceless GLES qualification; run explicitly with --ignored"]
    fn production_essence_shaders_render_endpoints_grain_and_obstacle_masks() {
        let display =
            unsafe { EGLDisplay::new(EGLSurfacelessDisplay) }.expect("surfaceless EGL unavailable");
        let context = EGLContext::new(&display).expect("surfaceless EGL context unavailable");
        let mut renderer =
            unsafe { GlesRenderer::new(context) }.expect("GLES renderer unavailable");
        assert!(
            renderer
                .with_context(|gl| compile_mote_sources(gl, "invalid shader", FRAGMENT))
                .unwrap()
                .is_err(),
            "shader initialization failure must return an error without poisoning the context"
        );
        let programs = Programs::new(&mut renderer)
            .expect("production Essence shaders failed to compile/link");
        let screen = EffectRect::new(0, 0, 128, 128);
        let window = EffectRect::new(0, 0, 64, 64);
        let obstacle = EffectRect::new(80, 24, 16, 80);
        let plan = Arc::new(Planner::new().plan(&Request {
            screen,
            window,
            obstacles: &[obstacle],
            sink: Sink::Card(EffectRect::new(108, 108, 20, 20)),
            timing: Timing::DEFAULT,
        }));
        let mut pixels = vec![0; 64 * 64 * 4];
        for y in 0..64 {
            for x in 0..64 {
                let at = (y * 64 + x) * 4;
                pixels[at] = if x < 32 { 230 } else { 20 };
                pixels[at + 1] = if y < 32 { 190 } else { 30 };
                pixels[at + 2] = 50;
                pixels[at + 3] = 255;
            }
        }
        let snapshot = renderer
            .import_memory(&pixels, Fourcc::Abgr8888, (64, 64).into(), false)
            .unwrap();
        let field = renderer
            .import_memory(
                &plan.field,
                Fourcc::Abgr8888,
                (plan.cols as i32, plan.rows as i32).into(),
                false,
            )
            .unwrap();
        let output = <GlesRenderer as Offscreen<GlesTexture>>::create_buffer(
            &mut renderer,
            Fourcc::Abgr8888,
            (128, 128).into(),
        )
        .unwrap();
        let bytes = expanded_motes(&plan);
        let vertices = renderer
            .with_context(|gl| unsafe {
                let mut old = 0;
                gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut old);
                let mut buffer = 0;
                gl.GenBuffers(1, &mut buffer);
                gl.BindBuffer(ffi::ARRAY_BUFFER, buffer);
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    bytes.len() as isize,
                    bytes.as_ptr().cast(),
                    ffi::STATIC_DRAW,
                );
                gl.BindBuffer(ffi::ARRAY_BUFFER, old as u32);
                buffer
            })
            .unwrap();
        let mut entry = Entry {
            vertex_count: (plan.motes.len() * 6) as i32,
            plan,
            snapshot,
            field,
            output,
            vertices,
            bounds: Rect::new(0, 0, 128, 128),
            id: Id::new(),
            commit: CommitCounter::default(),
            failed: false,
            last: None,
            backing_bytes: 81920,
            scale_bits: 1.0_f64.to_bits(),
        };
        let read = |renderer: &mut GlesRenderer, entry: &Entry| {
            let mapping = renderer
                .copy_texture(
                    &entry.output,
                    Rectangle::from_size((128, 128).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap().to_vec()
        };
        entry
            .render(
                &mut renderer,
                &programs,
                0.0,
                Style {
                    grain_px: 6.0,
                    ..Style::DEFAULT
                },
            )
            .unwrap();
        let whole = read(&mut renderer, &entry);
        assert_eq!(
            &whole[(8 * 128 + 8) * 4..(8 * 128 + 8) * 4 + 4],
            &[230, 190, 50, 255],
            "top-left snapshot orientation"
        );
        assert_eq!(
            &whole[(40 * 128 + 40) * 4..(40 * 128 + 40) * 4 + 4],
            &[20, 30, 50, 255],
            "bottom-right snapshot orientation"
        );
        assert_eq!(
            whole
                .chunks_exact(4)
                .filter(|pixel| pixel[3] == 255)
                .count(),
            64 * 64
        );
        entry
            .render(
                &mut renderer,
                &programs,
                1.0,
                Style {
                    grain_px: 6.0,
                    ..Style::DEFAULT
                },
            )
            .unwrap();
        assert!(
            read(&mut renderer, &entry)
                .chunks_exact(4)
                .all(|pixel| pixel[3] == 0)
        );
        entry
            .render(
                &mut renderer,
                &programs,
                0.45,
                Style {
                    grain_px: 4.0,
                    ..Style::DEFAULT
                },
            )
            .unwrap();
        let fine = read(&mut renderer, &entry);
        entry
            .render(
                &mut renderer,
                &programs,
                0.45,
                Style {
                    grain_px: 12.0,
                    ..Style::DEFAULT
                },
            )
            .unwrap();
        let coarse = read(&mut renderer, &entry);
        assert_ne!(
            fine, coarse,
            "grain size changes rendered dust without a new plan"
        );
        assert!(
            coarse.chunks_exact(4).enumerate().any(|(i, pixel)| {
                let (x, y) = (i % 128, i / 128);
                (x >= 64 || y >= 64) && pixel[3] > 0
            }),
            "the batched motes must actually leave the source window"
        );
        // Normal offscreen projection stores increasing logical y in increasing GL rows.
        for y in 24..104 {
            for x in 80..96 {
                assert_eq!(
                    coarse[(y * 128 + x) * 4 + 3],
                    0,
                    "motes covered an obstacle"
                );
            }
        }
        renderer
            .with_context(|gl| unsafe {
                gl.DeleteBuffers(1, &vertices);
                gl.DeleteProgram(programs.motes);
            })
            .unwrap();
    }
}
