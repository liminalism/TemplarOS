varying vec2 v_coords;
uniform sampler2D e_window_tex;
uniform vec4 e_bounds;
uniform float alpha;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif
void main() {
    vec2 px=e_bounds.xy+v_coords*e_bounds.zw;
    if (!inside(px,e_window)) discard;
    vec2 uv=(px-e_window.xy)/e_window.zw;
    vec4 color=texture2D(e_window_tex,uv);
    vec4 field=texture2D(e_field_tex,field_uv(px));
    vec4 grain=texture2D(e_noise_tex,px/max(e_dust.x*40.0,16.0));
    float speck=hash12(floor(px/e_dust.x));
    float n=grain.z*0.4+grain.w*0.3+speck*0.3;
    float vanish;
    float width;
    if (covered(px,field.a)) { vanish=field.r+(n-0.5)*e_jitter.x; width=e_timing.w*0.3; }
    else { vanish=field.g+(n-0.5)*e_jitter.y; width=e_timing.z; }
    vanish=clamp(vanish,0.0,1.0-width);
    float gone=smoothstep(vanish,vanish+width,e_timing.x);
    float a=1.0-gone;
    float band=gone*a*4.0;
    float rim=band*band;
    vec3 glow=e_glow.rgb*(e_glow.w*rim*(0.15+0.6*field.b));
    gl_FragColor=vec4(color.rgb*a+glow*color.a,color.a*a)*alpha;
}
