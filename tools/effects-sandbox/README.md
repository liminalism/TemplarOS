# effects-sandbox

A standalone place to see, tune and measure `crates/templar-effects` without a session: a fake
desktop (a maximized browser over three terminals, a dock of Home cards, a notification and a
login veil) drawn with the real effect code through wgpu. Its own Cargo workspace, like
`helpers/templar-preview-codecs`, so wgpu, winit and naga never enter the TemplarOS lockfile or
any shipped binary. Nothing here is packaged.

```sh
cd tools/effects-sandbox
cargo run --release                         # interactive
cargo run --release -- --check [DIR]        # the shader gate (DIR: also write the GLSL ES 3.00)
cargo run --release -- --snapshots DIR      # PNG stills of every transition and material
cargo run --release -- --bench              # planner CPU time + per-pass GPU time
    --size WxH                              # offscreen size for --snapshots/--bench
    --grain PX                              # essence grain diameter (default 1.5)
EFFECTS_ADAPTER=low cargo run --release -- --bench   # the integrated GPU
WGPU_BACKEND=gl cargo run --release         # the GLES backend, closer to the compositor
```

Interactive keys: **D** desktop button (staggered, front to back; again to bring everything
back; mid-flight it turns around) · **M** minimize the top window into its card · **R** restore
the last one (or click its card) · drag title bars to rearrange the gaps · **L** login veil ·
**[ ]** run duration −/+25 ms (50–300) · **S** ×8 slow motion · **H** desktop button into cards
or soaking into the garden · **G** cycle the garden material · **N** notification on/off,
**1–7** its material · **F** field overlay (R covered vanish, G garden vanish, B gap glow) ·
**,** **.** grain size −/+ (or drag the slider at the bottom left; the dot beside it is one
grain at true size) · **Esc** quit. The bottom bar answers the pointer directly: a plate under
the cursor sweeps its glass sheen and picks up a slight wobble and refraction, and a click
shrinks it and washes a ring across the glass. The login veil covers the bar. `--grain PX` sets
the grain for any mode, so stills and
benches can compare sizes. The title bar shows the grain, the last plan's cost (or
`reused`), mote count and upload, and which bar plate the pointer is on.

The sandbox only redraws while something moves; the garden backdrop and the login screen are
rendered once and kept until they change.
