// The essence transition (see `essence.rs` for the planner and the timeline).
//
// Two draws per window per frame, sharing one bind group:
//   vs_body / fs_body   the window's own quad, dissolving: a piece over another window goes
//                       within ~50 ms, a piece over the garden holds until the essence reaches it
//   vs_mote / fs_mote   one instanced quad per piece, travelling its path of least resistance,
//                       drawn after the body and hidden wherever another window lies over the
//                       garden, so essence is only ever seen in the gaps.
//
// Colour is premultiplied throughout; blend One / OneMinusSrcAlpha. Glow is emitted as extra
// RGB over alpha, i.e. it adds light rather than covering.

struct Essence {
    screen: vec4<f32>,          // output rectangle, logical px
    window: vec4<f32>,          // the window's rectangle
    field: vec4<f32>,           // field origin xy, field extent wh (cols × cell, rows × cell)
    timing: vec4<f32>,          // u (departure progress), cell px, edge width, covered fraction
    glow: vec4<f32>,            // essence colour (linear), glow strength
    jitter: vec4<f32>,          // covered jitter, garden jitter, mote glow, mote end size
    dust: vec4<f32>,            // grain diameter px, grain density, cloud spread, -
    counts: vec4<u32>,          // obstacle count, sink (0 garden, 1 card), mote count, -
    obstacles: array<vec4<f32>, 16>,
};

@group(0) @binding(0) var<uniform> e: Essence;
@group(0) @binding(1) var window_tex: texture_2d<f32>;
@group(0) @binding(2) var field_tex: texture_2d<f32>;
@group(0) @binding(3) var noise_tex: texture_2d<f32>;
@group(0) @binding(4) var clamp_sampler: sampler;
@group(0) @binding(5) var repeat_sampler: sampler;

// Another window lies over `p`. Exact for the listed obstacles; the field's covered channel
// catches any beyond the uniform's sixteen.
fn covered_at(p: vec2<f32>, field_covered: f32) -> bool {
    var hit = field_covered > 0.99;
    let count = min(e.counts.x, 16u);
    for (var i = 0u; i < count; i++) {
        if inside_rect(p, e.obstacles[i]) {
            hit = true;
        }
    }
    return hit;
}

fn field_uv(p: vec2<f32>) -> vec2<f32> {
    return (p - e.field.xy) / e.field.zw;
}

// White noise on a lattice coordinate. Float only, no sine (Hoskins' "hash without sine"), so
// it ports to GLSL ES 1.00 as it is and agrees across drivers.
fn hash12(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.x, p.y, p.x) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

fn hash42(p: vec2<f32>) -> vec4<f32> {
    var p4 = fract(vec4<f32>(p.x, p.y, p.x, p.y) * vec4<f32>(0.1031, 0.1030, 0.0973, 0.1099));
    p4 += dot(p4, p4.wzxy + 33.33);
    return fract((p4.xxyz + p4.yzzw) * p4.zywx);
}

// ---------------------------------------------------------------- body ----------------------

struct BodyOut {
    @builtin(position) position: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) uv: vec2<f32>,
};

@vertex
fn vs_body(@builtin(vertex_index) vertex: u32) -> BodyOut {
    let corner = quad_corner(vertex);
    let px = e.window.xy + corner * e.window.zw;
    var out: BodyOut;
    out.position = to_clip(px, e.screen);
    out.px = px;
    out.uv = corner;
    return out;
}

@fragment
fn fs_body(in: BodyOut) -> @location(0) vec4<f32> {
    // Every sample first, in uniform control flow.
    let color = textureSample(window_tex, clamp_sampler, in.uv);
    let field = textureSample(field_tex, clamp_sampler, field_uv(in.px));
    // The noise tile is scaled to the grain, and every grain-sized site adds its own white
    // jitter: the dissolving edge breaks up into dust, not blotches.
    let grain = textureSample(noise_tex, repeat_sampler, in.px / max(e.dust.x * 40.0, 16.0));
    let speck = hash12(floor(in.px / e.dust.x));

    let u = e.timing.x;
    let n = grain.z * 0.4 + grain.w * 0.3 + speck * 0.3;
    var vanish: f32;
    var width: f32;
    if covered_at(in.px, field.a) {
        vanish = field.r + (n - 0.5) * e.jitter.x;
        width = e.timing.w * 0.3;
    } else {
        vanish = field.g + (n - 0.5) * e.jitter.y;
        width = e.timing.z;
    }
    // Whole at u = 0 and gone at u = 1, whatever the jitter did.
    vanish = clamp(vanish, 0.0, 1.0 - width);
    let gone = smoothstep(vanish, vanish + width, u);
    let alpha = 1.0 - gone;
    // The dissolving edge glows faintly, most where essence is entering a gap.
    let band = gone * alpha * 4.0;
    let rim = band * band;
    let glow = e.glow.rgb * (e.glow.w * rim * (0.15 + 0.6 * field.b));
    return vec4<f32>(color.rgb * alpha + glow * color.a, color.a * alpha);
}

// ---------------------------------------------------------------- motes ---------------------

struct MoteIn {
    @location(0) p01: vec4<f32>,
    @location(1) p23: vec4<f32>,
    @location(2) p45: vec4<f32>,
    @location(3) p67: vec4<f32>,
    // release, arrive, source x, source y — all normalized
    @location(4) times: vec4<f32>,
};

struct MoteOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) src_uv: vec2<f32>,
    @location(2) px: vec2<f32>,
    // path progress, envelope, grain seed
    @location(3) @interpolate(flat) life: vec4<f32>,
    // the quad's extent along and across its path, px
    @location(4) @interpolate(flat) size: vec2<f32>,
};

fn catmull_rom(p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>, p3: vec2<f32>, t: f32) -> vec2<f32> {
    let t2 = t * t;
    let t3 = t2 * t;
    return 0.5 * (2.0 * p1 + (p2 - p0) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2
        + (3.0 * p1 - p0 - 3.0 * p2 + p3) * t3);
}

fn catmull_rom_tangent(p0: vec2<f32>, p1: vec2<f32>, p2: vec2<f32>, p3: vec2<f32>, t: f32) -> vec2<f32> {
    return 0.5 * ((p2 - p0) + 2.0 * (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t
        + 3.0 * (3.0 * p1 - p0 - 3.0 * p2 + p3) * t * t);
}

@vertex
fn vs_mote(@builtin(vertex_index) vertex: u32, m: MoteIn) -> MoteOut {
    var out: MoteOut;
    let release = m.times.x;
    let span = max(m.times.y - release, 0.001);
    let progress = (e.timing.x - release) / span;
    if progress <= 0.0 || progress >= 1.0 {
        // Not yet released, or already soaked in: a zero-area quad outside the clip volume.
        out.position = vec4<f32>(-2.0, -2.0, 0.0, 1.0);
        return out;
    }
    var points = array<vec2<f32>, 8>(m.p01.xy, m.p01.zw, m.p23.xy, m.p23.zw, m.p45.xy, m.p45.zw, m.p67.xy, m.p67.zw);
    var lengths: array<f32, 7>;
    var total = 0.0;
    for (var i = 0; i < 7; i++) {
        lengths[i] = length((points[i + 1] - points[i]) * e.screen.zw);
        total += lengths[i];
    }
    // Slow to leave, quick between, settling at the end; even speed along the path whatever
    // the spacing of its control points (chord-length parameterization).
    let s = progress * progress * (3.0 - 2.0 * progress);
    var remaining = s * total;
    var segment = 0;
    for (var i = 0; i < 6; i++) {
        if remaining > lengths[segment] {
            remaining -= lengths[segment];
            segment += 1;
        }
    }
    let f = clamp(remaining / max(lengths[segment], 0.001), 0.0, 1.0);
    let p0 = points[max(segment - 1, 0)] * e.screen.zw + e.screen.xy;
    let p1 = points[segment] * e.screen.zw + e.screen.xy;
    let p2 = points[segment + 1] * e.screen.zw + e.screen.xy;
    let p3 = points[min(segment + 2, 7)] * e.screen.zw + e.screen.xy;
    let center = catmull_rom(p0, p1, p2, p3, f);
    let tangent = catmull_rom_tangent(p0, p1, p2, p3, f);

    // A piece becomes light quickly once it leaves, and only then turns to face where it is
    // going: a row of dark tilted squares reads as debris, not essence.
    let lit = smoothstep(0.0, 0.25, progress);
    let turn = lit;
    var dir = vec2<f32>(1.0, 0.0);
    let tangent_len = length(tangent);
    if tangent_len > 0.001 {
        let blended = mix(dir, tangent / tangent_len, turn);
        if length(blended) > 0.001 {
            dir = normalize(blended);
        }
    }
    let normal = vec2<f32>(-dir.y, dir.x);
    let cell = e.timing.y;
    // Once a piece has crumbled it spreads into a cloud of grains and draws out along its
    // path, so the clouds following one path overlap into a stream of dust rather than a row
    // of beads; it gathers again as it settles. A piece with nowhere to go (soaking in place)
    // only shrinks.
    let travels = clamp(total / (cell * 4.0), 0.0, 1.0);
    let settle = mix(1.08, e.jitter.w, s);
    let along = cell * settle * (1.0 + 2.0 * lit * travels * (1.0 - s * s));
    let across = cell * settle * mix(1.0, e.dust.z, lit * travels * (1.0 - s));
    let corner = quad_corner(vertex) - vec2<f32>(0.5);
    let px = center + dir * (corner.x * along) + normal * (corner.y * across);

    out.position = to_clip(px, e.screen);
    out.local = corner;
    let source = m.times.zw * e.screen.zw + e.screen.xy + (corner + vec2<f32>(0.5)) * cell;
    out.src_uv = (source - e.window.xy) / e.window.zw;
    out.px = px;
    let envelope = smoothstep(0.0, 0.08, progress) * (1.0 - smoothstep(0.55, 1.0, progress));
    // The source position is unique per piece: it seeds the piece's own scatter of grains.
    out.life = vec4<f32>(progress, envelope, m.times.zw * vec2<f32>(5113.0, 3571.0));
    out.size = vec2<f32>(along, across);
    return out;
}

@fragment
fn fs_mote(in: MoteOut) -> @location(0) vec4<f32> {
    let progress = in.life.x;
    // Square piece to soft, ragged cloud.
    let round = smoothstep(0.0, 0.35, progress);
    let square_d = max(abs(in.local.x), abs(in.local.y));
    let round_d = length(in.local) * 1.12;
    let base_d = mix(square_d, round_d, round);
    // Most of a cloud's quad is its empty corners or lies under another window: settle those
    // first, before any sample or grain. (Explicit-level samples, so they may follow a branch.)
    if base_d - 0.15 * round >= 0.5 {
        discard;
    }
    let field = textureSampleLevel(field_tex, clamp_sampler, field_uv(in.px), 0.0);
    let in_source = all(in.src_uv >= vec2<f32>(0.0)) && all(in.src_uv <= vec2<f32>(1.0));
    if !in_source || covered_at(in.px, field.a) {
        discard;
    }
    let piece = textureSampleLevel(window_tex, clamp_sampler, in.src_uv, 0.0);
    let cloud = textureSampleLevel(noise_tex, repeat_sampler, in.px / (e.timing.y * 3.0), 0.0);
    let d = base_d + (cloud.w - 0.5) * 0.3 * round;
    let soft = mix(0.015, 0.4, round);
    let shape = 1.0 - smoothstep(0.5 - soft, 0.5, d);

    // The piece crumbles into grains of `dust.x` px on a jittered lattice in the cloud's own
    // frame (x along the path), so a grain keeps its size however far the cloud is drawn out.
    // Each lane of grains drifts at its own pace, smearing the cloud into a stream.
    // It crumbles as it leaves: a half-crumbled square reads as debris, not essence.
    let crumble = smoothstep(0.0, 0.1, progress);
    let grain_px = e.dust.x;
    let spacing = grain_px * 2.2;
    let q = in.local * in.size;
    let lane = floor(q.y / spacing);
    let pace = hash12(vec2<f32>(lane, in.life.z)) - 0.5;
    let drifted = vec2<f32>(q.x + pace * in.size.x * 0.6 * crumble, q.y);
    let site = floor(drifted / spacing);
    let h = hash42(site + in.life.zw);
    let centre = (site + 0.3 + 0.4 * h.xy) * spacing;
    let r = grain_px * 0.5 * (0.7 + 0.6 * h.z);
    // Below a pixel a grain fades by its area instead of vanishing: specks of air.
    let rr = max(r, 0.5);
    let dot_cover = (1.0 - smoothstep(rr - 0.5, rr + 0.5, distance(drifted, centre)))
        * min(1.0, (r * r) / (rr * rr));
    let dust = select(0.0, dot_cover, h.w < e.dust.y);
    let solid = mix(1.0, dust, crumble);
    // Grains catch the light unevenly; where many streams meet they thicken, not white out.
    let sparkle = mix(1.0, 0.45 + 0.75 * fract(h.x + h.y), crumble);

    // Window pixels to essence: a dark piece becomes as much light as a bright one, carrying
    // only a little of its luminance.
    let g = e.jitter.z * smoothstep(0.0, 0.25, progress);
    let straight = piece.rgb / max(piece.a, 0.001);
    let essence = e.glow.rgb * (0.6 + 0.4 * luma(straight));
    let rgb = mix(piece.rgb, essence * piece.a, g);

    let k = shape * solid * in.life.y;
    // As it turns to light it adds rather than covers.
    return vec4<f32>(rgb * k * (1.0 + e.glow.w * g * 0.35) * sparkle, piece.a * k * (1.0 - 0.75 * g));
}
