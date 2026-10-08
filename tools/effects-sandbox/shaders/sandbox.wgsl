// Sandbox-only: a textured quad (windows, thumbnails, the garden blit, mip downsampling, the
// field overlay). Prepended with templar-effects' common.wgsl. Premultiplied output.

struct Quad {
    screen: vec4<f32>,   // target rectangle (0, 0, w, h)
    rect: vec4<f32>,     // where to draw
    uv: vec4<f32>,       // uv at the top-left (xy) and bottom-right (zw)
    color: vec4<f32>,    // premultiplied multiplier
    misc: vec4<f32>,     // mip level, corner radius px, -, -
};

@group(0) @binding(0) var<uniform> q: Quad;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct QuadOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) local: vec2<f32>,
};

@vertex
fn vs_quad(@builtin(vertex_index) vertex: u32) -> QuadOut {
    let c = quad_corner(vertex);
    var out: QuadOut;
    out.position = to_clip(q.rect.xy + c * q.rect.zw, q.screen);
    out.uv = mix(q.uv.xy, q.uv.zw, c);
    out.local = (c - vec2<f32>(0.5)) * q.rect.zw;
    return out;
}

@fragment
fn fs_quad(in: QuadOut) -> @location(0) vec4<f32> {
    let c = textureSampleLevel(tex, samp, in.uv, q.misc.x);
    let d = rounded_rect_sdf(in.local, q.rect.zw * 0.5, q.misc.y);
    let coverage = select(1.0, clamp(0.5 - d, 0.0, 1.0), q.misc.y > 0.0);
    return c * q.color * coverage;
}
