# templar-effects

The TemplarOS effect library: WGSL shaders and the pure planning behind them. No dependencies,
no GPU state — a host (`tools/effects-sandbox` or the compositor) owns devices and
pipelines; this crate decides what to draw and packs the bytes it is drawn with.

## The essence transition

The planner supports minimize, restore and the desktop button. A window deconstructs into
essence that flows,
through the gaps between the windows that stay, into its Home card (`Sink::Card`) or into the
Homegarden wherever it shows (`Sink::Garden`), and comes back the same way.

- **Pieces over another window go fast** (`Timing::covered_ms`, 50 ms by default; start,
  jitter and dissolve width are sized so they are *gone* by then).
- **Pieces over the garden hold** until the essence from upstream has reached their gap, then
  dissolve and follow; into a card, the pieces nearest it leave last.
- **Essence is only ever seen in the gaps**: motes are masked wherever another window lies over
  the garden (exactly for 16 obstacles, by the field beyond), and they leave a window by its
  nearest edge — a covered cell costs 50 garden cells, so a path never takes a short cut under a
  window when a gap goes round (`tests/essence.rs::paths_take_the_gap_in_a_wall`).
- **Into a card, each piece lands where it sits in the thumbnail**: the window reassembles
  small instead of collapsing to a point.
- **Essence is dust.** Each piece crumbles as it leaves into grains of `Style::grain_px`
  (library default 1.5 logical px; supported library range 0.5–24) that spread into a cloud and
  stream along its path, and the window's dissolving edge is grained at the same size. The
  grain is drawn, not planned: changing it replans nothing. The compositor defaults to
  6 logical px and exposes a 1–24 size setting under Appearance & Motion. The sandbox also
  offers a slider, `,` `.`, and `--grain`.
- **Arriving is the same plan run backwards**, and `Run::reverse` turns a run around mid-flight
  from exactly where it is, so rapid minimize/restore never replans or jumps.

### Once per trigger (CPU, `Planner::plan`)

A coarse field over the output (cells ≥ 16 px, ≤ 160 along the long edge, so the work is
bounded at any resolution): covered/garden classification, a Dial bucket-queue shortest-path
pass from the sink, Theta*-style any-angle straightening resolved lazily (with a summed-area
table settling most line tests in O(1)), then one walk per piece. A large window breaks into
2×2-cell pieces so it never exceeds `MAX_MOTES` (4096). Scratch buffers are reused: after the
first plan of a size, planning allocates nothing beyond the returned `Plan`.

Measured with `effects-sandbox --bench` (release, maximized window over three terminals, this
laptop under load):

| | 1080p garden | 1080p card | 4K garden | 4K card |
|---|---|---|---|---|
| plan, median | 0.93 ms | 1.56 ms | 1.75 ms | 3.14 ms |
| motes / upload | 2040 / 111 KiB | 2040 / 111 KiB | 3600 / 196 KiB | 3600 / 196 KiB |

A cached plan (same window, obstacles, sink and timing) costs nothing: the sandbox reuses the
plan *and* its GPU buffers for restore-after-minimize.

### Every frame in the sandbox (GPU)

One uniform write and two draws, sharing one bind group (`shaders/essence.wgsl`):
`vs_body/fs_body` (the window's own quad: one window sample, one field sample, one noise
sample, one hash) and `vs_mote/fs_mote` (one instanced quad per piece, 40 bytes each; pieces
not yet released or already soaked in collapse to zero-area quads; each quad's grains come
from a float hash on a jittered lattice, so dust costs no instances, and a fragment in a
cloud's empty corner or under another window is discarded before any sample). Nothing is drawn once a run ends.
`Plan::bounds` is the damage rectangle for the whole run.

Sandbox per-pass GPU time, median µs (timestamp queries; the window drawn plainly for comparison):

| | plain window | essence body | essence motes (peak) |
|---|---|---|---|
| RTX 4060, 1080p | 24 | 67 | 107 |
| RTX 4060, 4K | 133 | 259 | 426 |
| Iris Xe, 1080p | 100 | 230–345 | 436 |
| Iris Xe, 4K | 550 | 909–1741 | 1770 |

Dust about doubled the motes' cost against the earlier solid pieces on the RTX 4060 (54 →
107 µs at 1080p, 191 → 426 µs at 4K, same plain-window baseline): a cloud of grains covers
more of the screen than a thread did. The Iris Xe rows were re-measured on an unloaded
machine, so they do not compare with the earlier, loaded figures. `Style::dust_spread` (1.25)
is the lever; the grain size itself costs nothing.

## Surfaces

These material shaders remain sandbox features; the window-transition integration does not
apply them to the shipped interface.

`Material`: glass, etched glass, stained glass, stone, glassy stone, metal sheen, rough wood
(`shaders/surface.wgsl`, one uniform block, a material switch). Each is a rounded panel with an
analytic AA edge and a bevel the light reads. The glass family reads a backdrop the host has
blurred into a mip chain; the rest read only the noise tile. Surfaces are meant to be retained
— redrawn only when size, material or light change (`Material::animated` is true only for the
metal's sheen).

## Bar plates

These shader experiments remain separate from the shell's existing gesture-rail motion.

`Gesture` is one chip on a bottom bar. The host says whether the pointer is over it and whether
the button is down; `Gesture::advance` eases hover and press and starts a pulse on the press
edge. `Gesture::optics` turns that into a wobble (at most 1.6 px), a refraction (at most 5.4 px)
and a glass sheen. At rest the wobble and the refraction are exactly zero, so a settled bar does
not need another frame (`Gesture::live`).

`shaders/bar.wgsl` draws one rounded glass plate from those numbers: the sheen sits quiet until
hover sweeps it, the plate wobbles and refracts only while the pointer is engaged, and a click
shrinks the plate and washes a ring across it. The sandbox keeps five plates along the bottom
(settings, network, calendar, terminal, account) and drives them with the pointer. Stills:
`bar_rest`, `bar_hover`, `bar_press`.

## Noise

`noise::noise_rgba`: a 128² tileable RGBA8 texture whose four channels are four octaves of
smooth value noise, so one bilinear fetch is one fbm. This is the transferable result of the
Bend staged-effects experiments (lattice reuse; a *full-resolution* cached basis lost to
recomputation, hence a small repeating tile).

## The gate

`cargo run --manifest-path tools/effects-sandbox/Cargo.toml -- --check` parses and validates
every module with naga 30 and translates every entry point to GLSL ES 3.0 and SPIR-V. Run it
after any WGSL edit; `just check` covers only the Rust.

## Compositor integration

`compositor/templar-comp/src/motion.rs` selects Essence, Simple or Off for park/restore,
using the same `set_minimize_target` geometry as the existing simple transition. It builds
one planner result from the window, Home target and visible application obstacles, retains
that plan in an `Arc`, and uses `Run::reverse` for rapid reversals. The effect occupies the
window's original scene slot; it does not become an overlay above unrelated windows.

The production renderer in `compositor/templar-comp/src/essence_renderer.rs` uses hand-ported
GLSL ES 1.00 body and batched-mote shaders in the existing GLES context. Per-transition window
snapshots stay on the GPU. Texture dimensions are bounded by `GL_MAX_TEXTURE_SIZE`, and
snapshot storage is capped at 128 MiB per GLES context. No CPU readback or second wgpu device
is introduced. Shader initialization, texture support or snapshot-readiness failures fall
back to the existing 200 ms simple motion.

The shell propagates `shell.appearance.window-transition` (`essence`, `simple`, `off`) and
`shell.appearance.particle-size` through the shell/compositor protocol. Particle size updates
each frame's style uniform without recomputing the plan. The existing
`shell.home-field.reduced-motion` overrides transition choices; enabling it or choosing Off
cancels an active visual transition while preserving the already-committed window state.
Material and bar WGSL shaders are not part of this production path.

The tables above measure the sandbox's WGSL renderer, not compositor performance. Production
shader/rendering fixtures qualify the GLES path; installed-session feel and integrated-GPU
performance still require observation on the target machine.
