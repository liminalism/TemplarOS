// templar-effects: helpers shared by every module. `shaders.rs` concatenates this file ahead of
// each module's own source (WGSL has no includes), so nothing here declares a binding.

// Logical pixel `p` on an output whose rectangle is `screen` (x, y, w, h) to clip space.
fn to_clip(p: vec2<f32>, screen: vec4<f32>) -> vec4<f32> {
    let n = (p - screen.xy) / screen.zw;
    return vec4<f32>(n.x * 2.0 - 1.0, 1.0 - n.y * 2.0, 0.0, 1.0);
}

// The six corners of a two-triangle quad, (0, 0) top-left to (1, 1) bottom-right.
fn quad_corner(index: u32) -> vec2<f32> {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(1.0, 0.0),
    );
    return corners[index % 6u];
}

fn luma(c: vec3<f32>) -> f32 {
    return dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// Half-open, like `Rect::contains`: abutting rectangles never both claim a pixel.
fn inside_rect(p: vec2<f32>, r: vec4<f32>) -> bool {
    return all(p >= r.xy) && all(p < r.xy + r.zw);
}

// Signed distance to a rounded rectangle centred on the origin: negative inside.
fn rounded_rect_sdf(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn rotate2(p: vec2<f32>, angle: f32) -> vec2<f32> {
    let c = cos(angle);
    let s = sin(angle);
    return vec2<f32>(c * p.x - s * p.y, s * p.x + c * p.y);
}
