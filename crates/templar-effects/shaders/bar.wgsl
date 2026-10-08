// Bar plates (see `bar.rs`). One rounded glass cell per draw, for a bottom-bar chip under the
// pointer. Output is premultiplied; blend One / OneMinusSrcAlpha.
//
// The quad is a few pixels larger than the plate so a slight wobble and the anti-aliased edge
// stay inside it. At rest the wobble amplitude and the refraction are zero, so a settled plate
// does not depend on time and the host can keep the pixels. Hover sweeps the sheen, adds a
// bounded wobble and a bounded refraction; a click shrinks the plate and rings it.

struct Bar {
    rect: vec4<f32>,     // x, y, w, h in target px
    canvas: vec4<f32>,   // render target w, h, time (s), opacity
    tint: vec4<f32>,     // linear rgb, strength
    shape: vec4<f32>,    // corner radius, bevel, backdrop lod, seed
    motion: vec4<f32>,   // hover, press, pulse, wobble px
    optics: vec4<f32>,   // distort px, sheen position, sheen gain, -
};

@group(0) @binding(0) var<uniform> s: Bar;
@group(0) @binding(1) var noise_tex: texture_2d<f32>;
@group(0) @binding(2) var backdrop_tex: texture_2d<f32>;
@group(0) @binding(3) var repeat_sampler: sampler;
@group(0) @binding(4) var clamp_sampler: sampler;

struct BarOut {
    @builtin(position) position: vec4<f32>,
    @location(0) px: vec2<f32>,
};

@vertex
fn vs_bar(@builtin(vertex_index) vertex: u32) -> BarOut {
    // Room for the wobble (at most a couple of pixels) and one pixel of edge antialiasing.
    let margin = 4.0;
    let rect = vec4<f32>(s.rect.xy - vec2<f32>(margin), s.rect.zw + vec2<f32>(margin * 2.0));
    let px = rect.xy + quad_corner(vertex) * rect.zw;
    var out: BarOut;
    out.position = to_clip(px, vec4<f32>(0.0, 0.0, s.canvas.xy));
    out.px = px;
    return out;
}

fn backdrop(px: vec2<f32>, lod: f32) -> vec3<f32> {
    let uv = clamp(px / s.canvas.xy, vec2<f32>(0.0), vec2<f32>(1.0));
    return textureSampleLevel(backdrop_tex, clamp_sampler, uv, lod).rgb;
}

fn fresnel(n: vec3<f32>) -> f32 {
    let c = 1.0 - clamp(n.z, 0.0, 1.0);
    return c * c * c;
}

@fragment
fn fs_bar(in: BarOut) -> @location(0) vec4<f32> {
    let amp = s.motion.w;
    let wobble = vec2<f32>(
        sin(s.canvas.z * 7.2 + s.shape.w) * amp,
        cos(s.canvas.z * 5.6 + s.shape.w * 1.7) * amp * 0.65,
    );
    let half_size = s.rect.zw * 0.5;
    let centre = s.rect.xy + half_size;
    let p = in.px - centre - wobble;
    // A click settles the plate a pixel and a half into itself.
    let inset = s.motion.y * 1.6;
    let half = max(half_size - vec2<f32>(inset), vec2<f32>(1.0));
    let radius = max(s.shape.x - inset, 0.0);
    let d = rounded_rect_sdf(p, half, radius);
    let coverage = clamp(0.5 - d, 0.0, 1.0);

    let gx = rounded_rect_sdf(p + vec2<f32>(0.5, 0.0), half, radius)
        - rounded_rect_sdf(p - vec2<f32>(0.5, 0.0), half, radius);
    let gy = rounded_rect_sdf(p + vec2<f32>(0.0, 0.5), half, radius)
        - rounded_rect_sdf(p - vec2<f32>(0.0, 0.5), half, radius);
    let bevel = max(s.shape.y, 0.001);
    let rise = 1.0 - smoothstep(0.0, 1.0, clamp(-d / bevel, 0.0, 1.0));
    let tilt = select(0.0, 1.05 * rise, s.shape.y > 0.0);
    let bevel_n = normalize(vec3<f32>(vec2<f32>(gx, gy) * tilt, 1.0));

    // Refraction grows toward the rim and picks up a slow ripple while the pointer is down
    // or over the plate. Both terms are zero when `optics.x` is zero.
    let radial = length(p) / max(length(half), 1.0);
    let ang = atan2(p.y, p.x);
    let falloff = smoothstep(0.15, 0.95, radial);
    let offset = vec2<f32>(cos(ang), sin(ang)) * s.optics.x * falloff;
    let ripple = vec2<f32>(
        sin(p.y * 0.18 + s.canvas.z * 4.0 + s.shape.w),
        cos(p.x * 0.18 + s.canvas.z * 3.2 + s.shape.w),
    ) * s.optics.x * 0.35;
    let sample_at = in.px - offset - ripple - bevel_n.xy * 8.0;
    let straight = backdrop(sample_at, s.shape.z);
    let shifted = backdrop(sample_at + vec2<f32>(s.optics.x * 0.35, 0.0), s.shape.z);
    let fringe = clamp(s.optics.x / 6.0, 0.0, 1.0) * 0.55;
    var color = mix(straight, vec3<f32>(shifted.r, straight.g, straight.b), fringe);
    color = mix(color, color * s.tint.rgb, s.tint.a) + vec3<f32>(0.03);
    color += vec3<f32>(0.26) * fresnel(bevel_n);

    // A quiet sheen at rest, swept across the plate as hover rises. A little time drift while
    // the pointer is over it, so the highlight is not a sticker.
    let nx = p.x / max(half.x, 1.0);
    let ny = p.y / max(half.y, 1.0);
    let diag = nx * 0.85 + ny * 0.45;
    let along = diag * 0.5 + 0.5;
    let sheen_at = s.optics.y + sin(s.canvas.z * 1.4 + s.shape.w) * 0.045 * s.motion.x;
    let band = along - sheen_at;
    let sheen = exp(-band * band / 0.012) * s.optics.z;
    color += vec3<f32>(0.82, 0.90, 1.0) * sheen;

    // The click is a ring that opens and fades. It lives inside the plate.
    let pulse = s.motion.z;
    let rad = length(p);
    let ring_r = mix(0.12, 1.02, 1.0 - pulse) * length(half);
    let ring = exp(-abs(rad - ring_r) * 0.5) * pulse;
    color += vec3<f32>(0.72, 0.88, 1.0) * ring * 0.9;

    // Fine grain, only enough to keep the glass from looking like a flat fill.
    let grain = textureSampleLevel(noise_tex, repeat_sampler, in.px / 128.0, 0.0).w;
    color += vec3<f32>((grain - 0.5) * 0.04);

    color *= 1.0 - s.motion.y * 0.14;
    let alpha = coverage * s.canvas.w;
    return vec4<f32>(color * alpha, alpha);
}
