attribute vec2 corner;
attribute vec4 p01;
attribute vec4 p23;
attribute vec4 p45;
attribute vec4 p67;
attribute vec4 times;
uniform vec4 e_bounds;
varying vec2 v_local;
varying vec2 v_uv;
varying vec2 v_px;
varying vec4 v_life;
varying vec2 v_size;
vec2 point(int i) {
    if(i==0) return p01.xy; if(i==1) return p01.zw;
    if(i==2) return p23.xy; if(i==3) return p23.zw;
    if(i==4) return p45.xy; if(i==5) return p45.zw;
    if(i==6) return p67.xy; return p67.zw;
}
vec2 cr(vec2 a,vec2 b,vec2 c,vec2 d,float t) {
    return 0.5*(2.0*b+(c-a)*t+(2.0*a-5.0*b+4.0*c-d)*t*t+(3.0*b-a-3.0*c+d)*t*t*t);
}
vec2 tangent(vec2 a,vec2 b,vec2 c,vec2 d,float t) {
    return 0.5*((c-a)+2.0*(2.0*a-5.0*b+4.0*c-d)*t+3.0*(3.0*b-a-3.0*c+d)*t*t);
}
void main() {
    float progress=(e_timing.x-times.x)/max(times.y-times.x,0.001);
    v_local=vec2(0.0);v_uv=vec2(-1.0);v_px=vec2(0.0);v_life=vec4(0.0);v_size=vec2(1.0);
    if(progress<=0.0 || progress>=1.0) { gl_Position=vec4(-2.0,-2.0,0.0,1.0);return; }
    float total=0.0;
    for(int i=0;i<7;i++) total+=length((point(i+1)-point(i))*e_screen.zw);
    float s=progress*progress*(3.0-2.0*progress);
    float remaining=s*total;int segment=0;
    for(int i=0;i<6;i++) {
        float len=length((point(segment+1)-point(segment))*e_screen.zw);
        if(remaining>len) {remaining-=len;segment++;}
    }
    float len=length((point(segment+1)-point(segment))*e_screen.zw);
    float f=clamp(remaining/max(len,0.001),0.0,1.0);
    vec2 a=point(segment>0?segment-1:0)*e_screen.zw+e_screen.xy;
    vec2 b=point(segment)*e_screen.zw+e_screen.xy;
    vec2 c=point(segment+1)*e_screen.zw+e_screen.xy;
    vec2 d=point(segment+2<7?segment+2:7)*e_screen.zw+e_screen.xy;
    vec2 center=cr(a,b,c,d,f);vec2 t=tangent(a,b,c,d,f);
    float lit=smoothstep(0.0,0.25,progress);vec2 dir=vec2(1.0,0.0);
    if(length(t)>0.001) {vec2 blend=mix(dir,normalize(t),lit);if(length(blend)>0.001)dir=normalize(blend);}
    vec2 normal=vec2(-dir.y,dir.x);
    float cell=e_timing.y;
    float travels=clamp(total/(cell*4.0),0.0,1.0);
    float settle=mix(1.08,e_jitter.w,s);
    float along=cell*settle*(1.0+2.0*lit*travels*(1.0-s*s));
    float across=cell*settle*mix(1.0,e_dust.z,lit*travels*(1.0-s));
    vec2 local=corner-vec2(0.5);
    vec2 px=center+dir*(local.x*along)+normal*(local.y*across);
    vec2 clip=(px-e_bounds.xy)/e_bounds.zw*2.0-1.0;
    gl_Position=vec4(clip.x,clip.y,0.0,1.0);
    v_local=local;
    vec2 source=times.zw*e_screen.zw+e_screen.xy+corner*cell;
    v_uv=(source-e_window.xy)/e_window.zw;v_px=px;
    float envelope=smoothstep(0.0,0.08,progress)*(1.0-smoothstep(0.55,1.0,progress));
    v_life=vec4(progress,envelope,times.zw*vec2(5113.0,3571.0));v_size=vec2(along,across);
}
