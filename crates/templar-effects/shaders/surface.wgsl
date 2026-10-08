// Surface materials (see `surface.rs`). One rounded panel per draw: analytic AA edge, a bevel
// the light reads, then the material. Output is premultiplied; blend One / OneMinusSrcAlpha.
//
// Noise comes from the shared 128² tile whose four channels are four octaves of smooth value
// noise, so one fetch is one fbm: the lattice is evaluated once, on the CPU, and reused.

struct Surface {
    rect: vec4<f32>,       // x, y, w, h in target px
    canvas: vec4<f32>,     // render target w, h, time (s), opacity
    tint: vec4<f32>,       // linear rgb, strength
    light: vec4<f32>,      // direction xyz (z toward the viewer), intensity
    shape: vec4<f32>,      // corner radius, bevel, backdrop lod, -
    params: vec4<f32>,     // px per pattern unit, roughness, seed, detail
    material: vec4<u32>,   // material id, -, -, -
};

@group(0) @binding(0) var<uniform> s: Surface;
@group(0) @binding(1) var noise_tex: texture_2d<f32>;
@group(0) @binding(2) var backdrop_tex: texture_2d<f32>;
@group(0) @binding(3) var repeat_sampler: sampler;
@group(0) @binding(4) var clamp_sampler: sampler;

const GLASS: u32 = 0u;
const ETCHED_GLASS: u32 = 1u;
const STAINED_GLASS: u32 = 2u;
const STONE: u32 = 3u;
const GLASSY_STONE: u32 = 4u;
const METAL_SHEEN: u32 = 5u;
const ROUGH_WOOD: u32 = 6u;

struct SurfaceOut {
    @builtin(position) position: vec4<f32>,
    @location(0) px: vec2<f32>,
};

@vertex
fn vs_surface(@builtin(vertex_index) vertex: u32) -> SurfaceOut {
    // One pixel of margin for the anti-aliased edge.
    let rect = vec4<f32>(s.rect.xy - vec2<f32>(1.0), s.rect.zw + vec2<f32>(2.0));
    let px = rect.xy + quad_corner(vertex) * rect.zw;
    var out: SurfaceOut;
    out.position = to_clip(px, vec4<f32>(0.0, 0.0, s.canvas.xy));
    out.px = px;
    return out;
}

// ---------------------------------------------------------------- noise ---------------------

// One fetch, four octaves; a second, rotated fetch breaks the tile's repetition.
fn fbm(p: vec2<f32>) -> f32 {
    let a = textureSampleLevel(noise_tex, repeat_sampler, p, 0.0);
    let b = textureSampleLevel(noise_tex, repeat_sampler, rotate2(p, 0.73) * 2.31 + vec2<f32>(0.37, 0.61), 0.0);
    return dot(a, vec4<f32>(0.42, 0.26, 0.15, 0.07)) * 1.1 + dot(b, vec4<f32>(0.0, 0.05, 0.03, 0.02));
}

fn fine(p: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(noise_tex, repeat_sampler, p, 0.0);
}

// A lattice point of the noise tile, read exactly (one smooth value per integer cell).
fn lattice(cell: vec2<i32>) -> vec4<f32> {
    let wrapped = ((cell % vec2<i32>(128)) + vec2<i32>(128)) % vec2<i32>(128);
    return textureLoad(noise_tex, wrapped, 0);
}

// Independent values per integer cell. Neighbouring texels of the smooth tile are correlated,
// which is right for fbm and wrong for scattering Voronoi sites, so those hash instead.
fn hash_u32(n: u32) -> u32 {
    var x = n;
    x ^= x >> 16u;
    x *= 0x7feb352du;
    x ^= x >> 15u;
    x *= 0x846ca68bu;
    x ^= x >> 16u;
    return x;
}

fn hash_cell(c: vec2<i32>) -> vec4<f32> {
    let n = hash_u32((u32(c.x) * 73856093u) ^ hash_u32(u32(c.y) * 19349663u));
    let m = hash_u32(n ^ 0x9e3779b9u);
    return vec4<f32>(f32(n & 65535u), f32(n >> 16u), f32(m & 65535u), f32(m >> 16u)) / 65535.0;
}

// ---------------------------------------------------------------- lighting ------------------

fn normal_from_height(h: f32, hx: f32, hy: f32, strength: f32) -> vec3<f32> {
    return normalize(vec3<f32>((h - hx) * strength, (h - hy) * strength, 1.0));
}

fn blinn(n: vec3<f32>, l: vec3<f32>, roughness: f32) -> f32 {
    let h = normalize(l + vec3<f32>(0.0, 0.0, 1.0));
    let shininess = mix(220.0, 6.0, roughness);
    return pow(max(dot(n, h), 0.0), shininess) * (shininess + 8.0) / 64.0;
}

fn fresnel(n: vec3<f32>) -> f32 {
    let c = 1.0 - clamp(n.z, 0.0, 1.0);
    return c * c * c;
}

fn backdrop(px: vec2<f32>, lod: f32) -> vec3<f32> {
    return textureSampleLevel(backdrop_tex, clamp_sampler, px / s.canvas.xy, lod).rgb;
}

// Jewel tones for the stained glass panes.
fn pane_color(h: f32) -> vec3<f32> {
    let k = floor(h * 6.0);
    if k < 1.0 { return vec3<f32>(0.62, 0.05, 0.08); }      // ruby
    if k < 2.0 { return vec3<f32>(0.05, 0.16, 0.55); }      // cobalt
    if k < 3.0 { return vec3<f32>(0.80, 0.48, 0.06); }      // amber
    if k < 4.0 { return vec3<f32>(0.06, 0.40, 0.18); }      // emerald
    if k < 5.0 { return vec3<f32>(0.32, 0.10, 0.45); }      // violet
    return vec3<f32>(0.75, 0.72, 0.55);                      // pale
}

// ---------------------------------------------------------------- materials -----------------

struct Shade {
    color: vec3<f32>,
    normal: vec3<f32>,
};

fn shade_glass(px: vec2<f32>, q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let refract = bevel_n.xy * 18.0;
    var c = backdrop(px - refract, s.shape.z);
    c = mix(c, c * s.tint.rgb, s.tint.a) + vec3<f32>(0.035);
    c += vec3<f32>(0.30) * fresnel(bevel_n) + vec3<f32>(blinn(bevel_n, l, s.params.y)) * 0.5;
    return c;
}

fn shade_etched(px: vec2<f32>, q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    // Topographic contour etching: frosted bands where fbm crosses evenly spaced levels.
    let h = fbm(q * 0.35);
    let bands = fract(h * mix(4.0, 11.0, s.params.w));
    let etched = smoothstep(0.30, 0.36, bands) * (1.0 - smoothstep(0.64, 0.70, bands));
    let frost = fine(q * 6.0).w;
    let lod = mix(s.shape.z - 3.0, s.shape.z + 0.5, etched);
    let n = normalize(bevel_n + vec3<f32>((frost - 0.5) * 0.25 * etched, (fine(q * 6.0 + 0.5).z - 0.5) * 0.25 * etched, 0.0));
    var c = backdrop(px - n.xy * 14.0, max(lod, 0.0));
    c = mix(c, c * s.tint.rgb, s.tint.a) + vec3<f32>(0.10 + 0.04 * frost) * etched + vec3<f32>(0.02);
    c += vec3<f32>(0.28) * fresnel(bevel_n) + vec3<f32>(blinn(n, l, mix(0.08, 0.6, etched))) * 0.35;
    return c;
}

fn shade_stained(px: vec2<f32>, q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    let cell = vec2<i32>(floor(q));
    let f = fract(q);
    var f1 = 8.0;
    var f2 = 8.0;
    var id = vec4<f32>(0.0);
    for (var y = -1; y <= 1; y++) {
        for (var x = -1; x <= 1; x++) {
            let o = vec2<i32>(x, y);
            let site = hash_cell(cell + o + vec2<i32>(i32(s.params.z)));
            let point = vec2<f32>(o) + vec2<f32>(0.15) + site.xy * 0.7;
            let d = length(point - f);
            if d < f1 {
                f2 = f1;
                f1 = d;
                id = site;
            } else if d < f2 {
                f2 = d;
            }
        }
    }
    let edge = f2 - f1;
    let lead_w = mix(0.03, 0.12, s.params.w);
    let lead = 1.0 - smoothstep(lead_w, lead_w + 0.03, edge);
    // Hammered glass inside each pane, a rounded came over the joins.
    let hammer = fine(q * 2.3 + id.zw * 7.0);
    let pane_n = normalize(vec3<f32>((hammer.z - 0.5) * 0.5, (hammer.w - 0.5) * 0.5, 1.0));
    let came = clamp(edge / max(lead_w, 0.001), 0.0, 1.0);
    let came_n = normalize(vec3<f32>(0.0, 0.0, 1.0) + vec3<f32>((hammer.xy - 0.5) * 0.1, 0.0) * came);
    let n = normalize(mix(pane_n, came_n, lead) + bevel_n - vec3<f32>(0.0, 0.0, 1.0));
    let tone = pane_color(id.z);
    let through = backdrop(px - n.xy * 10.0, s.shape.z);
    let glass = tone * (0.35 + 1.5 * through) + tone * 0.22 * max(dot(n, l), 0.0);
    let metal = vec3<f32>(0.10, 0.10, 0.11) * (0.5 + 0.6 * max(dot(n, l), 0.0)) + vec3<f32>(blinn(n, l, 0.35)) * 0.4;
    return mix(glass, metal, lead) + vec3<f32>(0.18) * fresnel(bevel_n);
}

fn stone_height(q: vec2<f32>) -> f32 {
    return fbm(q) * 0.7 + fbm(q * 3.1 + vec2<f32>(0.21, 0.47)) * 0.3;
}

fn shade_stone(q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> Shade {
    let e = 1.5 / s.params.x;
    let h = stone_height(q);
    let n = normal_from_height(h, stone_height(q + vec2<f32>(e, 0.0)), stone_height(q + vec2<f32>(0.0, e)), 2.2 * s.params.w);
    let speck = fine(q * 5.0);
    var albedo = s.tint.rgb * (0.70 + 0.55 * h);
    albedo *= 1.0 - 0.35 * smoothstep(0.80, 0.86, speck.w);              // dark flecks
    albedo += vec3<f32>(0.10) * smoothstep(0.84, 0.90, speck.z);          // pale flecks
    let crevice = smoothstep(0.25, 0.55, h);
    var out: Shade;
    out.color = albedo * (0.55 + 0.45 * crevice);
    out.normal = normalize(n + bevel_n - vec3<f32>(0.0, 0.0, 1.0));
    return out;
}

fn shade_glassy_stone(q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> Shade {
    // Marble: veins are where a turbulent phase crosses zero, so they wander but never close
    // into islands; a second, fainter set crosses the first.
    let turbulence = fbm(q * 0.3) * 5.0 + fbm(q * 0.8) * 0.9;
    let v1 = abs(sin((q.x * 1.6 + q.y * 0.8) * 3.0 + turbulence * 3.0));
    let v2 = abs(sin((q.y * 2.0 - q.x * 1.1) * 2.4 + turbulence * 2.0 + 1.3));
    let vein_w = mix(0.015, 0.09, s.params.w);
    let vein = 1.0 - smoothstep(0.0, vein_w, v1);
    let faint = 1.0 - smoothstep(0.0, vein_w * 0.7, v2);
    let haze = 1.0 - smoothstep(0.0, vein_w * 7.0, v1);
    let cloud = fbm(q * 1.7);
    var out: Shade;
    out.color = s.tint.rgb * (0.65 + 0.6 * cloud) + vec3<f32>(0.78, 0.70, 0.54) * vein * 0.75
        + vec3<f32>(0.42) * faint * 0.35 + vec3<f32>(0.05) * haze;
    let e = 1.0 / s.params.x;
    let n = normal_from_height(cloud, fbm((q + vec2<f32>(e, 0.0)) * 1.7), fbm((q + vec2<f32>(0.0, e)) * 1.7), 0.35);
    out.normal = normalize(n + bevel_n - vec3<f32>(0.0, 0.0, 1.0));
    return out;
}

fn shade_metal(px: vec2<f32>, q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> vec3<f32> {
    // Brushed along x: noise squeezed to streaks.
    let streak = fine(vec2<f32>(q.x * 0.05, q.y * 9.0)).w * 0.6 + fine(vec2<f32>(q.x * 0.02, q.y * 23.0)).z * 0.4;
    // A gently curved sheet, so the highlight is a band whose height follows the light.
    let v = (px.y - s.rect.y) / max(s.rect.w, 1.0) - 0.5;
    var n = normalize(vec3<f32>(0.0, v * 0.6, 1.0) + bevel_n - vec3<f32>(0.0, 0.0, 1.0));
    n = normalize(n + vec3<f32>(0.0, (streak - 0.5) * 0.06, 0.0));
    let sweep = sin(s.canvas.z * 0.8) * 0.35;
    let lt = normalize(l + vec3<f32>(sweep, 0.0, 0.0));
    let h = normalize(lt + vec3<f32>(0.0, 0.0, 1.0));
    // Ward-style anisotropy: wide along the brushing, narrow across it.
    let ax = mix(0.35, 0.9, s.params.y);
    let ay = mix(0.04, 0.2, s.params.y);
    let hn = max(dot(h, n), 0.001);
    let tx = h.x / hn;
    let ty = (h.y - n.y * hn) / hn;
    let aniso = exp(-(tx * tx / (ax * ax) + ty * ty / (ay * ay))) * 0.75;
    let base = s.tint.rgb * (0.62 + 0.3 * streak) * (0.5 + 0.5 * max(dot(n, l), 0.0));
    return base + s.tint.rgb * aniso * (0.6 + 0.8 * streak) * s.light.w + vec3<f32>(0.10) * fresnel(bevel_n);
}

fn wood_height(q: vec2<f32>) -> f32 {
    let plank = floor(q.y * 1.6);
    let grain_q = vec2<f32>(q.x * 0.22 + plank * 3.7, q.y * 5.0);
    let rings = fract(q.y * 5.0 + fbm(grain_q * 0.35) * 3.2 + plank * 0.37);
    let ring = smoothstep(0.0, 0.18, rings) * (1.0 - smoothstep(0.55, 1.0, rings));
    let pores = fine(vec2<f32>(q.x * 0.7, q.y * 11.0) + vec2<f32>(plank * 0.31, 0.0)).w;
    let bumps = fbm(q * 0.45 + vec2<f32>(plank * 0.53, 0.0)) * 0.7 + fbm(q * 1.9 + vec2<f32>(0.0, plank)) * 0.3;
    let seam = smoothstep(0.0, 0.03, fract(q.y * 1.6)) * (1.0 - smoothstep(0.97, 1.0, fract(q.y * 1.6)));
    return (ring * 0.35 + bumps * 0.55 - smoothstep(0.7, 0.9, pores) * 0.18 * s.params.w) * seam;
}

fn shade_wood(q: vec2<f32>, bevel_n: vec3<f32>, l: vec3<f32>) -> Shade {
    let e = 1.5 / s.params.x;
    let h = wood_height(q);
    let n = normal_from_height(h, wood_height(q + vec2<f32>(e, 0.0)), wood_height(q + vec2<f32>(0.0, e)), 5.0 * s.params.w);
    let plank = floor(q.y * 1.6);
    let plank_tone = 0.82 + 0.3 * lattice(vec2<i32>(i32(plank), 7)).x;
    let rings = fract(q.y * 5.0 + fbm(vec2<f32>(q.x * 0.22 + plank * 3.7, q.y * 5.0) * 0.35) * 3.2 + plank * 0.37);
    let late = smoothstep(0.55, 0.95, rings);
    var out: Shade;
    out.color = s.tint.rgb * plank_tone * mix(1.08, 0.62, late) * (0.75 + 0.35 * h);
    out.normal = normalize(n + bevel_n - vec3<f32>(0.0, 0.0, 1.0));
    return out;
}

// ---------------------------------------------------------------- the panel -----------------

@fragment
fn fs_surface(in: SurfaceOut) -> @location(0) vec4<f32> {
    let half_size = s.rect.zw * 0.5;
    let p = in.px - (s.rect.xy + half_size);
    let radius = s.shape.x;
    let d = rounded_rect_sdf(p, half_size, radius);
    let coverage = clamp(0.5 - d, 0.0, 1.0);

    // The bevel: the SDF's gradient, tilting the normal outward near the edge.
    let gx = rounded_rect_sdf(p + vec2<f32>(0.5, 0.0), half_size, radius) - rounded_rect_sdf(p - vec2<f32>(0.5, 0.0), half_size, radius);
    let gy = rounded_rect_sdf(p + vec2<f32>(0.0, 0.5), half_size, radius) - rounded_rect_sdf(p - vec2<f32>(0.0, 0.5), half_size, radius);
    let bevel = max(s.shape.y, 0.001);
    let rise = 1.0 - smoothstep(0.0, 1.0, clamp(-d / bevel, 0.0, 1.0));
    let tilt = select(0.0, 1.1 * rise, s.shape.y > 0.0);
    let bevel_n = normalize(vec3<f32>(vec2<f32>(gx, gy) * tilt, 1.0));

    let l = normalize(s.light.xyz);
    let q = (in.px + vec2<f32>(s.params.z * 97.0)) / s.params.x;

    var color: vec3<f32>;
    switch s.material.x {
        case GLASS: {
            color = shade_glass(in.px, q, bevel_n, l);
        }
        case ETCHED_GLASS: {
            color = shade_etched(in.px, q, bevel_n, l);
        }
        case STAINED_GLASS: {
            color = shade_stained(in.px, q, bevel_n, l);
        }
        case METAL_SHEEN: {
            color = shade_metal(in.px, q, bevel_n, l);
        }
        default: {
            var sh: Shade;
            if s.material.x == STONE {
                sh = shade_stone(q, bevel_n, l);
            } else if s.material.x == GLASSY_STONE {
                sh = shade_glassy_stone(q, bevel_n, l);
            } else {
                sh = shade_wood(q, bevel_n, l);
            }
            let diffuse = 0.32 + 0.68 * max(dot(sh.normal, l), 0.0);
            let spec = blinn(sh.normal, l, s.params.y) * mix(0.9, 0.08, s.params.y);
            var coat = 0.0;
            if s.material.x == GLASSY_STONE {
                // Polished: a sharp clear-coat highlight, a fresnel rim and a soft sky reflection.
                let sky = smoothstep(-0.2, 0.6, -bevel_n.y) * 0.08 + (1.0 - in.px.y / s.canvas.y) * 0.05;
                coat = fresnel(sh.normal) * 0.4 + blinn(sh.normal, l, 0.03) * 0.8 + sky;
            }
            color = sh.color * diffuse * s.light.w + vec3<f32>(spec + coat) * s.light.w;
        }
    }
    let alpha = coverage * s.canvas.w;
    return vec4<f32>(color * alpha, alpha);
}
