precision highp float;
uniform vec4 e_screen;
uniform vec4 e_window;
uniform vec4 e_field;
uniform vec4 e_timing;
uniform vec4 e_glow;
uniform vec4 e_jitter;
uniform vec4 e_dust;
uniform vec4 e_counts;
uniform vec4 e_obstacles[16];
uniform sampler2D e_field_tex;
uniform sampler2D e_noise_tex;
bool inside(vec2 p, vec4 r) { return all(greaterThanEqual(p,r.xy)) && all(lessThan(p,r.xy+r.zw)); }
bool covered(vec2 p, float field) {
    bool hit = field > 0.99;
    for (int i=0;i<16;i++) { if (float(i)<e_counts.x && inside(p,e_obstacles[i])) hit=true; }
    return hit;
}
vec2 field_uv(vec2 p) { return (p-e_field.xy)/e_field.zw; }
float hash12(vec2 p) {
    vec3 p3=fract(vec3(p.x,p.y,p.x)*0.1031);
    p3+=dot(p3,p3.yzx+33.33);
    return fract((p3.x+p3.y)*p3.z);
}
vec4 hash42(vec2 p) {
    vec4 p4=fract(vec4(p.x,p.y,p.x,p.y)*vec4(0.1031,0.1030,0.0973,0.1099));
    p4+=dot(p4,p4.wzxy+33.33);
    return fract((p4.xxyz+p4.yzzw)*p4.zywx);
}
