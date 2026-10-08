//! The wgpu side: one device, five pipelines, and small "draw" objects that pair a uniform
//! buffer with its bind group. Everything templar-effects decides arrives here as bytes.

use templar_effects::essence::{MOTE_BYTES, Plan, Style};
use templar_effects::{Plate, Surface, noise, shaders};
use wgpu::util::DeviceExt;

pub const SANDBOX_WGSL: &str = include_str!("../shaders/sandbox.wgsl");
/// The garden backdrop, the login screen and offscreen targets.
pub const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

#[derive(Debug)]
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub adapter: String,
    pub timestamps: bool,
    pub clamp: wgpu::Sampler,
    pub repeat: wgpu::Sampler,
    pub nearest: wgpu::Sampler,
    pub noise: wgpu::TextureView,
    pub white: wgpu::TextureView,
    quad_layout: wgpu::BindGroupLayout,
    essence_layout: wgpu::BindGroupLayout,
    surface_layout: wgpu::BindGroupLayout,
    quad_target: wgpu::RenderPipeline,
    quad_offscreen: wgpu::RenderPipeline,
    surface_target: wgpu::RenderPipeline,
    surface_offscreen: wgpu::RenderPipeline,
    bar_target: wgpu::RenderPipeline,
    bar_offscreen: wgpu::RenderPipeline,
    body: wgpu::RenderPipeline,
    motes: wgpu::RenderPipeline,
}

/// Where a draw lands: the presentable target or an offscreen texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dest {
    Target,
    Offscreen,
}

pub async fn adapter(
    instance: &wgpu::Instance,
    surface: Option<&wgpu::Surface<'static>>,
) -> Result<wgpu::Adapter, String> {
    instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            // EFFECTS_ADAPTER=low picks the integrated GPU, which is where cost matters most.
            power_preference: if std::env::var("EFFECTS_ADAPTER").is_ok_and(|v| v == "low") {
                wgpu::PowerPreference::LowPower
            } else {
                wgpu::PowerPreference::HighPerformance
            },
            force_fallback_adapter: false,
            compatible_surface: surface,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|error| format!("no usable GPU adapter: {error}"))
}

fn uniform_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn texture_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

impl Gpu {
    pub async fn new(
        adapter: &wgpu::Adapter,
        target_format: wgpu::TextureFormat,
    ) -> Result<Self, String> {
        let timestamps = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
        let required_features = if timestamps {
            wgpu::Features::TIMESTAMP_QUERY
        } else {
            wgpu::Features::empty()
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("effects sandbox"),
                required_features,
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::default(),
            })
            .await
            .map_err(|error| format!("request device: {error}"))?;
        let info = adapter.get_info();
        let adapter_name = format!("{} ({:?})", info.name, info.backend);

        let sampler = |label, address, mip| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                address_mode_u: address,
                address_mode_v: address,
                address_mode_w: address,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: mip,
                ..Default::default()
            })
        };
        let clamp = sampler(
            "clamp",
            wgpu::AddressMode::ClampToEdge,
            wgpu::MipmapFilterMode::Linear,
        );
        let repeat = sampler(
            "repeat",
            wgpu::AddressMode::Repeat,
            wgpu::MipmapFilterMode::Nearest,
        );
        let nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nearest"),
            ..Default::default()
        });

        let noise_size = noise::NOISE_SIZE;
        let noise = upload(
            &device,
            &queue,
            "noise",
            noise_size,
            noise_size,
            &noise::noise_rgba(noise_size),
            wgpu::TextureFormat::Rgba8Unorm,
        )
        .create_view(&wgpu::TextureViewDescriptor::default());
        let white = upload(
            &device,
            &queue,
            "white",
            1,
            1,
            &[255, 255, 255, 255],
            OFFSCREEN_FORMAT,
        )
        .create_view(&wgpu::TextureViewDescriptor::default());

        let vf = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let frag = wgpu::ShaderStages::FRAGMENT;
        let quad_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[
                uniform_entry(0, vf),
                texture_entry(1, frag),
                sampler_entry(2, frag),
            ],
        });
        let essence_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("essence"),
            entries: &[
                uniform_entry(0, vf),
                texture_entry(1, frag),
                texture_entry(2, frag),
                texture_entry(3, frag),
                sampler_entry(4, frag),
                sampler_entry(5, frag),
            ],
        });
        let surface_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("surface"),
            entries: &[
                uniform_entry(0, vf),
                texture_entry(1, frag),
                texture_entry(2, frag),
                sampler_entry(3, frag),
                sampler_entry(4, frag),
            ],
        });

        let module = |label, source: String| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            })
        };
        let quad_module = module("sandbox", format!("{}{}", shaders::COMMON, SANDBOX_WGSL));
        let essence_module = module("essence", shaders::ESSENCE.source.to_owned());
        let surface_module = module("surface", shaders::SURFACE.source.to_owned());
        let bar_module = module("bar", shaders::BAR.source.to_owned());

        let mote_attributes = wgpu::vertex_attr_array![0 => Unorm16x4, 1 => Unorm16x4, 2 => Unorm16x4, 3 => Unorm16x4, 4 => Unorm16x4];
        let mote_buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: MOTE_BYTES as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &mote_attributes,
        })];
        let make = |label,
                    layout: &wgpu::BindGroupLayout,
                    module: &wgpu::ShaderModule,
                    vs,
                    fs,
                    format,
                    buffers: &[Option<wgpu::VertexBufferLayout>]| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module,
                    entry_point: Some(vs),
                    compilation_options: Default::default(),
                    buffers,
                },
                fragment: Some(wgpu::FragmentState {
                    module,
                    entry_point: Some(fs),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let quad_target = make(
            "quad",
            &quad_layout,
            &quad_module,
            "vs_quad",
            "fs_quad",
            target_format,
            &[],
        );
        let quad_offscreen = make(
            "quad offscreen",
            &quad_layout,
            &quad_module,
            "vs_quad",
            "fs_quad",
            OFFSCREEN_FORMAT,
            &[],
        );
        let surface_target = make(
            "surface",
            &surface_layout,
            &surface_module,
            "vs_surface",
            "fs_surface",
            target_format,
            &[],
        );
        let surface_offscreen = make(
            "surface offscreen",
            &surface_layout,
            &surface_module,
            "vs_surface",
            "fs_surface",
            OFFSCREEN_FORMAT,
            &[],
        );
        let bar_target = make(
            "bar",
            &surface_layout,
            &bar_module,
            "vs_bar",
            "fs_bar",
            target_format,
            &[],
        );
        let bar_offscreen = make(
            "bar offscreen",
            &surface_layout,
            &bar_module,
            "vs_bar",
            "fs_bar",
            OFFSCREEN_FORMAT,
            &[],
        );
        let body = make(
            "essence body",
            &essence_layout,
            &essence_module,
            "vs_body",
            "fs_body",
            target_format,
            &[],
        );
        let motes = make(
            "essence motes",
            &essence_layout,
            &essence_module,
            "vs_mote",
            "fs_mote",
            target_format,
            &mote_buffers,
        );

        Ok(Self {
            device,
            queue,
            adapter: adapter_name,
            timestamps,
            clamp,
            repeat,
            nearest,
            noise,
            white,
            quad_layout,
            essence_layout,
            surface_layout,
            quad_target,
            quad_offscreen,
            surface_target,
            surface_offscreen,
            bar_target,
            bar_offscreen,
            body,
            motes,
        })
    }

    pub fn texture(
        &self,
        label: &str,
        w: u32,
        h: u32,
        rgba: &[u8],
    ) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = upload(
            &self.device,
            &self.queue,
            label,
            w,
            h,
            rgba,
            OFFSCREEN_FORMAT,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// A texture both rendered into and sampled, optionally with a full mip chain.
    pub fn render_texture(&self, label: &str, w: u32, h: u32, mips: bool) -> wgpu::Texture {
        let levels = if mips {
            (32 - w.max(h).leading_zeros()).min(8)
        } else {
            1
        };
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        })
    }

    pub fn quad(&self, view: &wgpu::TextureView, sampler: &wgpu::Sampler) -> QuadDraw {
        let buffer = self.uniform_buffer("quad", 80);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad"),
            layout: &self.quad_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        });
        QuadDraw { buffer, bind }
    }

    pub fn surface(&self, backdrop: &wgpu::TextureView) -> SurfaceDraw {
        let buffer = self.uniform_buffer("surface", templar_effects::surface::UNIFORM_BYTES as u64);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("surface"),
            layout: &self.surface_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.noise),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(backdrop),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.repeat),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.clamp),
                },
            ],
        });
        SurfaceDraw { buffer, bind }
    }

    /// One bottom-bar plate. The bind group matches a surface: uniform, noise, backdrop.
    pub fn bar(&self, backdrop: &wgpu::TextureView) -> BarDraw {
        let buffer = self.uniform_buffer("bar", templar_effects::bar::UNIFORM_BYTES as u64);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("bar"),
            layout: &self.surface_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&self.noise),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(backdrop),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.repeat),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.clamp),
                },
            ],
        });
        BarDraw { buffer, bind }
    }

    /// Everything a planned run needs on the GPU, uploaded once.
    pub fn essence(&self, plan: &Plan, window: &wgpu::TextureView) -> EssenceDraw {
        let field = upload(
            &self.device,
            &self.queue,
            "essence field",
            plan.cols,
            plan.rows,
            &plan.field,
            wgpu::TextureFormat::Rgba8Unorm,
        );
        let field_view = field.create_view(&wgpu::TextureViewDescriptor::default());
        let mut mote_bytes = plan.mote_bytes();
        let mote_count = plan.motes.len() as u32;
        if mote_bytes.is_empty() {
            mote_bytes.resize(MOTE_BYTES, 0);
        }
        let motes = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("essence motes"),
                contents: &mote_bytes,
                usage: wgpu::BufferUsages::VERTEX,
            });
        let uniform =
            self.uniform_buffer("essence", templar_effects::essence::UNIFORM_BYTES as u64);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("essence"),
            layout: &self.essence_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(window),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&field_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&self.noise),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::Sampler(&self.clamp),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::Sampler(&self.repeat),
                },
            ],
        });
        EssenceDraw {
            uniform,
            bind,
            motes,
            mote_count,
            field_view,
            upload_bytes: plan.field.len() + mote_bytes.len(),
        }
    }

    fn uniform_buffer(&self, label: &str, size: u64) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn draw_quad(&self, pass: &mut wgpu::RenderPass<'_>, quad: &QuadDraw, dest: Dest) {
        pass.set_pipeline(if dest == Dest::Target {
            &self.quad_target
        } else {
            &self.quad_offscreen
        });
        pass.set_bind_group(0, &quad.bind, &[]);
        pass.draw(0..6, 0..1);
    }

    pub fn draw_surface(&self, pass: &mut wgpu::RenderPass<'_>, surface: &SurfaceDraw, dest: Dest) {
        pass.set_pipeline(if dest == Dest::Target {
            &self.surface_target
        } else {
            &self.surface_offscreen
        });
        pass.set_bind_group(0, &surface.bind, &[]);
        pass.draw(0..6, 0..1);
    }

    pub fn draw_bar(&self, pass: &mut wgpu::RenderPass<'_>, plate: &BarDraw, dest: Dest) {
        pass.set_pipeline(if dest == Dest::Target {
            &self.bar_target
        } else {
            &self.bar_offscreen
        });
        pass.set_bind_group(0, &plate.bind, &[]);
        pass.draw(0..6, 0..1);
    }

    pub fn draw_body(&self, pass: &mut wgpu::RenderPass<'_>, essence: &EssenceDraw) {
        pass.set_pipeline(&self.body);
        pass.set_bind_group(0, &essence.bind, &[]);
        pass.draw(0..6, 0..1);
    }

    pub fn draw_motes(&self, pass: &mut wgpu::RenderPass<'_>, essence: &EssenceDraw) {
        if essence.mote_count == 0 {
            return;
        }
        pass.set_pipeline(&self.motes);
        pass.set_bind_group(0, &essence.bind, &[]);
        pass.set_vertex_buffer(0, essence.motes.slice(..));
        pass.draw(0..6, 0..essence.mote_count);
    }
}

fn upload(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    w: u32,
    h: u32,
    rgba: &[u8],
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        texture.as_image_copy(),
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w.max(1) * 4),
            rows_per_image: Some(h.max(1)),
        },
        wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
    );
    texture
}

#[derive(Debug)]
pub struct QuadDraw {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

/// A quad's parameters: where, which part of the texture, a premultiplied colour multiplier.
#[derive(Clone, Copy, Debug)]
pub struct QuadParams {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub lod: f32,
    pub radius: f32,
}

impl QuadParams {
    pub fn new(rect: [f32; 4]) -> Self {
        Self {
            rect,
            uv: [0.0, 0.0, 1.0, 1.0],
            color: [1.0; 4],
            lod: 0.0,
            radius: 0.0,
        }
    }

    /// Premultiplied solid colour (with the 1×1 white texture).
    pub fn solid(rect: [f32; 4], rgb: [f32; 3], alpha: f32, radius: f32) -> Self {
        Self {
            color: [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha],
            radius,
            ..Self::new(rect)
        }
    }

    pub fn opacity(mut self, alpha: f32) -> Self {
        self.color = [alpha; 4];
        self
    }
}

impl QuadDraw {
    pub fn set(&self, queue: &wgpu::Queue, target: (f32, f32), p: QuadParams) {
        let mut values = vec![0.0, 0.0, target.0, target.1];
        values.extend_from_slice(&p.rect);
        values.extend_from_slice(&p.uv);
        values.extend_from_slice(&p.color);
        values.extend_from_slice(&[p.lod, p.radius, 0.0, 0.0]);
        queue.write_buffer(&self.buffer, 0, &floats(&values));
    }
}

#[derive(Debug)]
pub struct SurfaceDraw {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

impl SurfaceDraw {
    pub fn set(&self, queue: &wgpu::Queue, target: (f32, f32), surface: &Surface) {
        queue.write_buffer(&self.buffer, 0, &surface.uniform(target.0, target.1));
    }
}

#[derive(Debug)]
pub struct BarDraw {
    buffer: wgpu::Buffer,
    bind: wgpu::BindGroup,
}

impl BarDraw {
    pub fn set(&self, queue: &wgpu::Queue, target: (f32, f32), plate: &Plate) {
        queue.write_buffer(&self.buffer, 0, &plate.uniform(target.0, target.1));
    }
}

#[derive(Debug)]
pub struct EssenceDraw {
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    motes: wgpu::Buffer,
    pub mote_count: u32,
    pub field_view: wgpu::TextureView,
    pub upload_bytes: usize,
}

impl EssenceDraw {
    pub fn set(&self, queue: &wgpu::Queue, plan: &Plan, u: f32, style: &Style) {
        queue.write_buffer(&self.uniform, 0, &plan.uniform(u, style));
    }
}

/// The garden, rendered once into a mip chain: glass reads its blur from the levels.
#[derive(Debug)]
pub struct Backdrop {
    pub view: wgpu::TextureView,
    levels: Vec<wgpu::TextureView>,
    downsample: Vec<QuadDraw>,
    pub size: (u32, u32),
}

impl Backdrop {
    pub fn new(gpu: &Gpu, w: u32, h: u32) -> Self {
        let texture = gpu.render_texture("garden backdrop", w, h, true);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let levels: Vec<wgpu::TextureView> = (0..texture.mip_level_count())
            .map(|level| {
                texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: level,
                    mip_level_count: Some(1),
                    ..Default::default()
                })
            })
            .collect();
        let downsample = levels
            .iter()
            .take(levels.len() - 1)
            .map(|source| gpu.quad(source, &gpu.clamp))
            .collect();
        Self {
            view,
            levels,
            downsample,
            size: (w, h),
        }
    }

    /// Render the garden into level 0, then halve it down the chain.
    pub fn render(&self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, garden: &SurfaceDraw) {
        {
            let mut pass = clear_pass(encoder, &self.levels[0], "garden");
            gpu.draw_surface(&mut pass, garden, Dest::Offscreen);
        }
        for (level, quad) in self.downsample.iter().enumerate() {
            let w = (self.size.0 >> (level + 1)).max(1) as f32;
            let h = (self.size.1 >> (level + 1)).max(1) as f32;
            quad.set(&gpu.queue, (w, h), QuadParams::new([0.0, 0.0, w, h]));
            let mut pass = clear_pass(encoder, &self.levels[level + 1], "downsample");
            gpu.draw_quad(&mut pass, quad, Dest::Offscreen);
        }
    }
}

pub fn clear_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    label: &str,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(label),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// Tightly packed RGBA bytes of a single-level `OFFSCREEN_FORMAT` texture.
pub fn read_rgba(gpu: &Gpu, texture: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (texture.width(), texture.height());
    let row = w * 4;
    let padded = row.div_ceil(256) * 256;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("readback"),
        });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    let _ = gpu.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let mapped = slice.get_mapped_range().expect("a mapped readback buffer");
    let mut out = Vec::with_capacity((row * h) as usize);
    for y in 0..h {
        let start = (y * padded) as usize;
        out.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    out
}
