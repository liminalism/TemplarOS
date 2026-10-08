# Low-memory wallpaper switcher

Status: **proposed design, not an implemented switcher**. Planning authority:
`@legeos.work.low-memory-wallpaper-switcher`. Source: the owner's September 11,
2026 report, registered as `low-memory-background-switcher-20260911`; the original
JSON export is registered separately as `wallpaper-report-export-20260911`.
Its title and timestamp are in the export. A chat title or direct conversation URL
has not been verified; local prompt-history searches found only the current request.

## Decision and scope

Add **`crates/templar-wallpaper` as a member of the LegeOS Cargo workspace**.
Cargo packages are peers in this repository; do not nest a second workspace or put
OS wallpaper policy in PixelKit, the image viewer, or the PDF-processing crate.
The larger owner is TemplarOS session/appearance. Existing `templar-sessiond`
hosts rotation/settings; `templar-comp` owns the displayed pixels. A temporary
`templar-wallpaper-decode` executable in the new package owns decoding.

Deliver startup restore, timed rotation, next/previous/pause/resume, local files
and directories, all six sizing modes, per-output overrides, bounded selection,
and an optional texture-resolution ceiling. Target **Variety's responsiveness
for these operations**, not its entire download/quotes/effects feature set. Those
extra features were not requested. No claim of measured parity is made yet.

Defaults: disabled, as `shell.wallpaper.enabled` already specifies; fill mode;
15-minute rotation when enabled; no fades, preload, decoded cache, animated-image
playback, plugin framework, or periodic directory crawl. Rust matches the existing
workspace. The report's C recommendation is not a requirement to rewrite the OS
in C; small codec-only C adapters are acceptable when the safe Rust API does not
expose the needed memory behavior.

## Ownership and package boundary

| Component | Owns | Must not retain |
|---|---|---|
| `templar-wallpaper` library | configuration, selection, rotation transitions, geometry, allocation plans | decoded pixels, GPU context, toolkit state |
| `templar-sessiond` wallpaper host | committed settings, current selection, one timer, one worker, status | image cache, full pixel body in JSON/D-Bus |
| `templar-wallpaper-decode` | probe, decode, resize/crop, color/alpha conversion, sealed output | anything after process exit |
| `templar-comp` wallpaper renderer | texture per necessary render context, placement, presentation generation | original decode, unnecessary CPU mirror |
| Settings and `templar wallpaper` CLI | render state, forward actions | policy authority or independent scheduler |

Planned library modules: `config`, `selection`, `rotation`, `geometry`, `budget`.
Planned binary modules: `decode`, `raster_wire`. Codec dependencies are optional
package features required by the decoder executable; sessiond and compositor use
the library with default features disabled. No image/PDF/GPU/Wayland/winit/Rayon
runtime is needed by the policy core. Wire types follow `templar-types` conventions.

The existing PDF preview helper is **not** a wallpaper decoder: it is PDF-only,
file-based, and limited to a 768-pixel edge. Filed's thumbnail path fully decodes,
encodes PNG, transfers it, and decodes again at the client. Reuse supervision and
sealed-FD mechanics, not that image pipeline or its 2-GiB address-space allowance.

## Native control and memory lifecycle

Use the existing authenticated sessiond-to-compositor `CompositorControl` endpoint.
Do not relay policy through the shell or create another daemon/unit. Its current
bounded JSON framing carries no descriptors: implementation must add bounded
SCM_RIGHTS transfer with unambiguous frame/descriptor association, ownership,
close-on-error, partial read/write handling, and the existing peer authentication.
A numeric FD in JSON is not an FD transfer. No wallpaper payload travels as base64.

Proposed lifecycle:

1. A committed settings change, session start, user command or rotation deadline
   selects a pathname. Open the source once and pass the owned read-only descriptor
   to the worker; do not make the worker reopen a raceable filename.
2. Obtain output/topology and renderer limits. Compute source crop, texture dimensions,
   color format and **bytes**, with checked arithmetic. Select one active job; one
   latest pending request replaces older pending requests rather than queuing images.
3. Worker produces one finalized tightly packed opaque RGBA8 raster in a memfd.
   Metadata includes wire version, request/config/topology generations, dimensions,
   stride, length and explicit channel/alpha interpretation. Unmap all writable
   views before applying WRITE/GROW/SHRINK/SEAL seals. Supervise timeout, exit and
   exact output length; kill/reap only this owned child on cancellation.
4. Sessiond passes metadata and the sealed read-only FD without reading the pixel
   body. Compositor validates seals, actual file length, format, overflow, driver
   texture limits and generation before mapping. Socket I/O/probing/decoding stays
   off the render thread; the bounded import occurs with the correct GLES renderer.
5. Import using Smithay `TextureBuffer::from_memory`, then create a texture render
   element with crop/destination geometry. Its GLES texture does not retain the
   borrowed CPU byte slice. Do not use `MemoryRenderBuffer` for wallpaper: that path
   retains CPU pixels plus renderer texture caches. Drop mappings and FDs after import.
6. Swap only successfully imported current-generation textures. Failed/canceled/stale
   jobs leave the last presented wallpaper intact. Reap the worker and drop decoder,
   original, resized raster and cache allocations. A failed output import is reported
   per output; cross-GPU presentation is not called atomic.
7. Keep current source identity/placement, not source pixels. Recreate a lost GPU
   context from the source. If the source is gone, retain surviving valid textures
   and use the normal solid ground on contexts that cannot be restored; expose the
   failure. Never fabricate a successful applied state.

The old texture and one candidate can overlap during transactional replacement.
**No fades** does not mean zero old/new overlap. Bound that overlap explicitly; share
textures across outputs only on a compatible render context and identical image
representation. Distinct GPUs may require distinct allocations. Prepare outputs
serially to bound worker scratch. Upload chunks or double-buffering must not quietly
create a full-size second CPU image.

Settings remain authoritative desired configuration; `accepted_generation` and
per-output `presented_generation` describe actual rendering. A control reply after
import is not proof of a presented frame. Failure status is observable, not a
rollback of an already committed settings transaction. Set/Reset, History undo,
startup restore and owner restart all reconcile through the same host path.

## Making the wallpaper actually visible

Observed: Home is a nontransparent full-output window. `FieldScreen::draw` clears
its buffer to opaque ground; `occlude_windows` also paints ground over covered
Home objects. A texture below that surface is invisible.

When a valid wallpaper is active, Home must paint objects over genuinely transparent
ground through **one** alpha-capable presenter. Preserve Home's input region, so
transparent ground does not disable selection or become accidentally click-through.
Covered-client areas erase Home objects/previews to transparent ground: translucent
clients may reveal wallpaper, but not hidden Home tiles or their own thumbnails.
No black/color-key transparency; produce proper premultiplied alpha/coverage.

Reuse the masked Wayland presentation protocol machinery, but do not merely add it
beside the current SoftbufferPresenter. Replace the active Home presenter, retire
its obsolete storage, and avoid a separate full-output coverage allocation where
alpha already encodes coverage. Keep release-safe buffering. Count shell SHM and
backing buffers in the memory comparison; they can erase the proposed saving.
A normal solid compositor ground stays below the image; failed enable/import never
requires an empty or unreadable Home. Disabling drops texture resources and restores
the ordinary ground presentation. This is a required implementation slice, not an
optional polish task.

Both native DRM and nested backends currently use GLES. Cargo's Pixman feature does
not establish a production software backend. No ordinary layer-shell global was
found; the private shell layer request is not standard layer-shell. The native
integration therefore uses compositor textures, not a promised working `swaybg`
backend. `wp_viewporter` exists but is unnecessary for compositor-internal textures.

Append wallpaper at the bottom of the unlocked shared scene. Keep the lock early
return: no wallpaper in locked scene/capture, no change to safe-frame barriers.
Whole-output capture includes the actual unlocked scene; window thumbnails do not.
Use existing output geometry/calibration/mirroring transforms once, and recompute
placement on scale/rotation/inset/topology changes. Remove output/context-only
resources on unplug. Fully occluded wallpaper causes no autonomous redraw and must
not prevent opaque fullscreen direct scanout. Rotation never resets user activity,
unblanks DPMS, or revives a paused/locked/sleeping session.

## Geometry and small JPEGs

Treat sizing and stored texture resolution as separate choices. Perform EXIF
orientation before geometry. Use consistent sRGB output and flatten alpha over the
chosen ground color in the worker. Keep HDR/color-management behavior explicit;
never treat different transfer functions as equivalent bytes.

| Mode | Geometry |
|---|---|
| fill | preserve aspect; cover destination; center-crop excess |
| fit | preserve aspect; contain; center; solid ground in bars |
| stretch | independent X/Y scaling, intentionally distorts |
| center | exact source pixel size centered; clip oversized source |
| actual | exact source pixel size from top-left origin; clip |
| tile | exact source pixel size repeated; no implicit scaling |

Use physical destination pixels for exact-size modes, then map through output scale,
rotation and calibration. `center` and `actual` never silently become fit. Preserve
that geometry when using a smaller texture; do not redefine tile period in texture
pixels. Oversized center/tile sources decode only what geometry needs where possible.

For a small JPEG displayed at 4K, native fill/fit/stretch can keep the source-sized
texture and scale during composition. A 640×360 RGBA texture is **0.879 MiB**, not
31.641 MiB. This does not create detail; bilinear upscaling is intentionally soft.
The decoder process exits; the compositor still retains that small texture. For
backends requiring a full-output bitmap, stream resize directly into the final
buffer and release worker memory after handoff. Such a backend still retains full
output-sized pixels; a small JPEG on disk is not proof of a small framebuffer.

Texture modes: `native` (no artificial downscale ceiling; do not upscale source
storage just to display it larger), or explicit `max-edge`/`max-pixels`. Default is
native quality. Offer a 1080p-class budget as an opt-in, not silent degradation.
1920×1080 at 4 bytes/pixel is **7.910 MiB** versus **31.641 MiB** at 4K: 75% less
source-texture payload. Count stride/alignment, context replicas, staging, previous
texture and driver allocation overhead separately. Scanout/swapchain and screenshot
targets remain full resolution.

For minification choose decoder reduction before resampling; use area filtering
rather than aliasing-heavy bilinear reduction for large ratios. Bilinear enlargement
needs only a few source rows. A separable row/area accumulator bounds ordinary
resizing scratch by row width, not source height. EXIF quarter-turns, interlacing,
multiscan JPEGs and codec-internal allocations need explicit bounded staging/spooling
paths; two-row scratch is not a universal total-memory promise.

The implementation is **entirely Rust**, including codecs and WuDP. Native-size
JPEG uses `zune-jpeg::decode_into`: ordinary RGBA goes directly into the final
mapped raster; EXIF 2–8 uses one mapped 3N RGB staging raster, dropped after the
permutation. Actual reductions use `jpeg-decoder`'s reduced IDCT. Progressive
coefficients remain budgeted at source size; neither path raises the 192-MiB ceiling.
Rust PNG rows bound ordinary resizing scratch; Adam7/orientation needs explicit
staging. Rust WebP's full-frame workspace is budgeted, not described as streaming.
ICC/HDR/non-sRGB/CMYK input is refused rather than silently misinterpreted.

Set a worker allocation envelope before decoding, accounting for codec process,
source sampling/coefficients, orientation staging, output and old/new import overlap.
Initial worker ceiling: 192 MiB for supported 8-bit input; reject an image whose
conservative planned requirements exceed it, with a clear reason. It is a configurable
worker limit, not a 55-MB whole-system promise. Do not cap the entire sessiond/compositor
cgroup for a wallpaper. Common-case low memory and pathological-source refusal are
separate published results. A pre-resized source may bring the same image within budget.

## Rotation and control contract

Native settings use existing Settings1 persistence/Reset/History. Extend
`shell.wallpaper.*` for sources, recursive traversal, interval, order, sizing,
ground color, output overrides, texture ceiling and worker budget. The existing
`enabled` key remains the enable gate; no duplicate TOML authority in TemplarOS.

Proposed wallpaper state/action interface: Inspect (config revision, capabilities,
current/previous source, per-output desired/accepted/presented generations, paused
state, next deadline and last error), Next, Previous, Pause, Resume, and state Changed.
Use existing snapshot/subscription and owner-recovery conventions. Commands and
Settings share the same state; do not offer controls the backend cannot apply.

Restore the last successful source at session start; select the first usable source
when no selection exists. Rotation intervals initially accept 5 minutes through
1 hour, plus explicit disabled rotation. One monotonic deadline; no 1-second loop.
On lock/sleep, stop dispatching new decode jobs and preserve displayed resources;
on resume/unlock coalesce all missed intervals into at most one rotation. Configure
change/user Next cancels stale work through generation checks; Pause prevents timed
rotation but still permits explicit selection. Scheduling restarts from successful
application; failed attempts do not advance selection/history.

Sources: explicit files and directories, optional recursion, no symlink-directory
following or background indexing by default. Refresh candidates at rotation or explicit
refresh, not continuous polling. Empty/offline directories leave last wallpaper
and an error; file removal does not discard a valid texture. Preserve Unix paths as
bytes using the existing lossless path conventions; never interpolate a shell command.

Default random/no-immediate-repeat uses reservoir sampling with bounded traversal
state. Empty list is an error; one candidate may repeat. Sequential selection uses
bounded scans and deterministic byte-path ordering. Exact shuffle-without-repetition
uses a capped metadata list, spilling to a private on-disk permutation/index when
needed; mmap is not called zero physical memory. A visited filename set that grows
without bound is forbidden. Persist a small PRNG/cursor/last-success state; Previous
returns the preceding successful selection without retaining its pixels. Bound
candidate traversal/work per job and cancellation; don't block the session/render
thread scanning a million-file tree. Missing/corrupt files get a bounded attempt
set per rotation, never an infinite retry loop.

## Compatibility backends

Share geometry/selection contracts, not a forced universal pixel sink.

- **X11:** one-shot setter plus session-scoped user timer; zero resident setter
  memory between changes. Root-depth pixmap, 32–64-row MIT-SHM stripes, checked
  upload completion before stripe reuse, ordinary chunked PutImage when SHM is
  unavailable. Honor byte order, visual masks, scanline padding and request limits;
  screen depth alone is not bytes/pixel. Finish candidate pixmap before publishing
  root background, clear/repaint, and `_XROOTPMAP_ID`/`ESETROOT_PMAP_ID`. Retain only
  owned wallpaper resources. Never kill an arbitrary client from untrusted/stale
  root properties; clean up only provably owned resources. Server pixmap remains
  full-sized on a normal true-color root. XWayland root is not Wayland wallpaper.
- **Generic layer-shell Wayland:** persistent minimal connection; no Cairo/toolkit
  required. Probe layer-shell, formats and viewporter. `wp_viewport` allows smaller
  buffer plus configured logical destination; without it, report reduced-texture
  mode unavailable. Configure/ack/attach ordering, empty input region, no keyboard
  focus, correct output scale. After upload unmap the client's readable pixels when
  possible; shared backing/compositor pixels still exist and must be counted.
  Do not modify/punch holes in storage still used by the compositor.
- **Buffer-release correction:** Wayland does not promise prompt release of a current
  buffer. Never wait indefinitely to reuse it. Allocate at most one candidate inside
  the byte budget; if old+new does not fit, reject/defer with an explicit status while
  preserving the current wallpaper. Release-starvation is a required test scenario.
  Lower resolution can make transactional overlap fit; it is not silently forced.
- **Portal/desktop owner:** delegate without decoding when the desktop exposes the
  wallpaper portal. It does not expose exact per-output sizing/low-res residency;
  advertise those controls unavailable. Success means request accepted by the owner,
  not verified rendered pixels. `swaybg` may be an explicit measured compatibility
  option, not an automatic cure for absent layer-shell.

Native TemplarOS is the first integration; compatibility adapters are separately
runnable deliverables of the proposal. No root session daemon, hardcoded display
socket, live-bus development service, or timer surviving session logout.

## Quantization: 256-color default, explicit opt-out

The owner identifies **WuDP (`wudpquantizer`) as a separate color quantizer** and
reports that it did better than their speed-forked NeuQuant in prior testing.
That qualitative result is accepted as owner-reported evidence; no numerical
speed/quality result or wallpaper-memory result is inferred. WuDP is the preferred
candidate to recover and evaluate, not another name for NeuQuant.

The owner's 2026-10-06 default-policy instruction supersedes the earlier opt-in
decision: modest one-time load overhead is acceptable because wallpaper changes
are infrequent. New/default and reset policies use **256-color WuDP**. Set
`quantize_colors: null` in policy JSON, or `off` in Appearance, for ordinary RGBA;
explicit persisted opt-outs are not silently overwritten. Omitted policy/worker
fields inherit defaults. This is lossy: smooth skies and gradients can band.

**WuDP sources recovered on the external `red` drive**, under
`/run/media/dk/red/red/Lege/Preproduction/lepers/lepers-donotuse/`:

- `wudbquantizer.h`: filename uses **b**, but declares `WuDPQuantizer`. It explicitly
  labels itself conceptual and substitutes a frequency-sort placeholder for WuDP;
  this wrapper is not the algorithm to benchmark or ship.
- `WUDB.cpp`: substantive histogram → LAB merge → weighted PCA → 256 projection
  bins → dynamic-programming palette → CIEDE2000 pixel assignment implementation,
  including `WuDPQuantizer` methods and a CLI `main`.
- `wudb.h`: declares `WuDPQuantizer(K, histogram_slots, probe_limit=10)`,
  `build_palette_from_image_cv(img, use_sampling=false, sample_fraction=0.1)`,
  `quantize_image_cv(img)` and `get_palette_srgb()`.

Immutable AKR copies: `wudp-implementation-20261006` (SHA256 starts `2a1cdfbd`),
`wudp-interface-20261006` (`e1cf903d`) and `wudp-conceptual-header-20261006`
(`1dbc02c5`). Originals remain unchanged. This identifies the distinct algorithm
family, not which revision produced the owner's earlier comparison.

The recovered copy is **not a drop-in build**: include filenames differ from the
stored companions; `ProjectionBin` fields/constructors differ between header and
implementation. Dependencies include OpenCV, Eigen, vivid and the local histogram/
CIELAB/PCA headers; these three source files contain no explicit license grant.
The requested port reimplements the mathematical pipeline in Rust rather than
repairing this C++ build. Preserve source provenance and the absence of an original
license notice; do not invent a grant or attribute the new workspace license to
the archived originals.

The **NeuQuant comparator**, not WuDP, exists at
`../Lege-ecosystem/lege-process/colorquant/neuquant.rs`:
`NeuQuant::new(samplefac, colors, &[u8])`, `index_of`, `color_map_rgba`.
Its core does not retain the borrowed source. Its vectors at K=256 have about
12 KiB element payload, plus allocation/sort overhead. Its existing PNG wrapper
is not low-peak: RGB (3N), collected RGBA (4N) and indices (N) coexist, about
8N image payload, or 63.3 MiB at 4K. These observations do not describe WuDP.

WuDP's current mapping API takes `CV_8UC3` BGR and allocates another 3N-byte BGR
image; it does **not** produce retained R8 indices. Its histogram starts at 262144
slots in the CLI and grows; histogram exports, merged LAB maps/vectors, and the
two mapping caches also consume memory proportional to encountered colors.
The actual slot ABI and allocation peaks must be measured, not copied from
NeuQuant's 12-KiB figure. The uncapped caches cannot enter a resident wallpaper host.

The DP tables have `(K+1)*(M+1)` float/int cells apiece, with M=256; the code's
`K >= M` branch bypasses DP and uses occupied-bin centroids. Thus K=256 is not a
measurement of the dynamic-programming path. Keep the owner's prior result;
declare K, histogram/probe limits, sampling and projection bins for new experiments.

The dependency-light **Rust WuDP core** is linked only into the optional temporary
decoder path. It retains LAB conversion, weighted PCA, projection bins, contiguous
dynamic programming and CIEDE2000 mapping. Deterministic RGB buckets replace elastic
probing and tolerance-based hash equality; prefix moments and rolling DP costs
replace the large cost tables. A bounded per-occupied-bucket CIEDE2000 LUT emits
indices into caller-owned storage, without an expanded BGR result or per-pixel
palette search. There is no random sampling, OpenCV/Eigen/vivid, C++ adapter or
unbounded mapping cache. RGB bucketing is an explicit approximation, not bit-identical
C++ output. Defaults: K=256, M=512, histogram bits=5/channel; no sampling/probing.
Default planned scratch is 1,406,520 bytes including 32 KiB stack allowance; maximum
supported K=256/M=1024/bits=6 plans 9,266,744 bytes. Caller RGB/index storage is extra.
The distinct private DjVu NeuQuant copy remains a comparator, not the implementation.

| Representation | RAM/texture implication |
|---|---|
| indexed PNG expanded to ordinary RGBA | disk may shrink; final pixels do not; extra index cache can increase RAM |
| 8-bit indices + RGBA palette retained by CPU | N+1024 bytes at 256 colors, but no saving over a baseline retaining zero CPU raster |
| compositor R8 indices + palette shader, no RGBA mirror | genuine candidate: ~75% source-texture payload saving versus 4N |
| RGB565 negotiated output | 2N packed-color pixels; not palette quantization; existing GLES memory importer does not provide it |
| supported BC/ETC/ASTC texture | possible stronger alternative; actual support, encoding peak and latency require measurement |
| half width/height ordinary texture | ~N bytes at four bytes/texel; same ideal payload as full-size R8 with a different quality tradeoff |

Smithay's ordinary memory import does not support indexed textures; native wallpaper
uses its custom R8+palette import/shader/scene path. Index texture sampling must be
exact/nearest: interpolating numeric indices is wrong. Smooth scaling resolves
neighboring indices to colors **before** interpolation. Palette and index generations
replace together. Quantize the worker texture, enforce 1..256 colors for an R8 index
and flatten source alpha before WuDP's RGB input.
For the NeuQuant comparator specifically, input is packed RGBA; small-image sampling
can learn zero samples, learned alpha cannot establish wallpaper opacity, and higher
samplefac means fewer samples. These constraints must not be attributed to WuDP.
Dithering off initially.

Experiment after worker adaptation: 12 fixed photos/gradients/flat art/line art,
1080p and 4K, same geometry and color treatment. Compare **WuDP first**, the
speed-forked NeuQuant comparator (K=256, samplefac=10), full-size RGBA, reduced-size
RGBA, real R8+palette when available, indexed PNG-to-RGBA control, and at most one
supported block-compressed format. Record actual WuDP parameters rather than using
NeuQuant's parameters by analogy. One worker, three initial screening repeats (not
stable p95), then 100 switches for release gates. Do not rerun the owner's prior
comparison merely to confirm their report, or label an offline index-vector
experiment GPU verification.

The earlier strict default-promotion screen (Variety parity and SSIM >=0.98 without
objectionable banding) is no longer a prerequisite for the owner's default-policy
choice. Optimized real 4K worker measurements found median JPEG decode times of
39.8–58.7 ms for RGBA and 152.0–193.3 ms for WuDP-256: about 93–150 ms extra per
change. A synthetic 4K PNG gradient took 387.9 versus 505.9 ms. Three runs per mode
are a load-time screen, not stable p95. Native logical texture payload is 33,177,600
bytes RGBA versus 8,295,424 bytes indexed plus palette (31.64 versus 7.91 MiB).
Visual review found retained photographic detail but visible smooth-background
banding, including at 256 colors; quantization is not lossless or universally
better-looking. Dithering remains off. Full quality/comparator and physical-GPU
accounting remain release observations to collect, not claims of this default.
See [the optimized default-policy measurements](../measurements/wallpaper-quantization-default-20261006.json).

## Measurement and release gates

One 4-byte image payload: 1080p 7.910 MiB; 1440p 14.063; 4K 31.641;
5K 56.250; 8K 126.563. These are **not minimum utility RSS**, and are not total
compositor RAM. The reduced texture does not reduce all final output pixels.

Record worker RSS/PSS, persistent-host incremental PSS, shell buffers, shared-memory
backing, server/compositor incremental memory, real GPU allocations where observable,
and source/render-target byte counts. Shared PSS is proportional: do not simply sum
RSS for the same mapping, or subtract a whole buffer from one process's PSS as if it
were wholly charged there. Shared memfd lifetime/unmapped pages need explicit allocation
accounting even when absent from utility RSS. If GPU accounting is unavailable, report
that gap and counted payload; do not claim a measured whole-system saving.

Variety comparison: same machine, actual version/backend/helper processes, images,
outputs, startup state, sizing/filter behavior and warm/cold-file conditions; disable
network sources, effects and fades on both. Compare complete process trees and
incremental graphics allocations, not Variety's whole process against only our
scheduler. Include scan/select/decode/upload/first presented frame; screenshot hashes
or confirmation of setter acceptance alone are not presentation latency. Do not drop
the global page cache on the person's desktop; use a disposable environment/cold files.

Proposed gates, fixed before collecting release results:

- startup and rotation p95 no worse than 1.10× the matched Variety baseline; median
  no worse than baseline. Present raw times and at least 100 samples; no use of the
  report's 'finishes before five minutes' as a performance-parity gate;
- idle aggregate utility incremental PSS <5 MiB and <50% of matched Variety utility
  PSS (native excludes existing host baseline, but publishes host totals too);
  X11 one-shot setter has no resident PID between changes;
- common-case baseline-JPEG 4K worker peak PSS <20 MiB, separately measured from
  output/import storage. Progressive/Adam7/rotation sources obey the declared
  192-MiB worker envelope or give a clear refusal; publish their actual peaks;
- native idle source texture payload no larger than the selected budget times
  necessary context replicas; old+new/staging fit declared transition allocation;
- combined attributable storage must improve when claiming reduced/indexed modes;
  memory transferred into shell/graphics cannot count as eliminated;
- over 100 replacements, all workers reaped, obsolete mappings/textures released,
  no growing resource count; steady PSS stays within a 1-MiB noise envelope with
  no sustained upward trend after warm-up;
- no periodic wallpaper-originated wake during a 60-second idle window away from
  deadlines; actual output topology/owner events remain legitimate wakes;
- real isolated desktop journeys cover startup, small JPEG-to-4K, every sizing mode,
  timed/manual/paused rotation, empty/corrupt/deleted sources, stale jobs, owner
  restart, context loss, multi-output/scale/calibration, Home transparency/input,
  translucent/fullscreen clients, screenshot, unplug, lock and sleep transitions;
- generic Wayland release starvation cannot hang indefinitely; X11 repeated changes
  do not accumulate retained pixmaps or affect another client's resources.

Use `/proc/*/smaps_rollup` plus worker/owned-cgroup high-water accounting for peaks;
2-ms sampling alone can miss spikes. Use allocation tracing/Massif including mmap
for attribution, not as a substitute for resident accounting. Hardware DRM/GPU gates
remain owner-run unless a separate safe test session is available. Never touch the
person's live compositor/services/session.

## Observations made during this design

A native disposable decoder experiment actually ran, documented in
`docs/measurements/wallpaper-decoder-20261006.md`. Libjpeg-turbo 2.1.5 decoded
7680×4320 baseline and progressive versions of the same source to 1920×1080 with
`djpeg -scale 1/4`; output went to `/dev/null`. Across three warm runs, sampled RSS
was 2472–2540 KiB for baseline and 99716–99772 KiB for progressive. Both exited and
were reaped. This supports budgeting progressive coefficients, not a switcher,
Variety parity, resizing-to-4K, quantizer, compositor or GPU claim.

## Rust implementation and isolated observations

The workspace implementation now includes `crates/templar-wallpaper`, the temporary
`templar-wallpaper-decode`, and independent `templar-wallpaper` compatibility commands.
Native policy runs in existing sessiond; bottom-scene textures are compositor-owned
in nested and DRM/GLES paths. CLI, Appearance, Settings1 Reset/History and Wallpaper1
share desired/applied state. Home uses one premultiplied-alpha-capable bounded SHM
presenter, including the opt-in PixelKit raster support; ordinary opaque painters
retain their existing behavior. Codec and compatibility features are not resident
native-policy dependencies. Packaging installs the decoder beside sessiond.

Actual isolated runs used a private bus, owned Xvfb/nested GLES and private labwc.

Descriptors, sampled process counters, pixel checks and compatibility-owner detail
are retained in [the machine-readable observation artifact](../measurements/wallpaper-rust-20261006.json).

- Native RGBA and real R8+palette displayed matching RG/BW pixels. A 128×96 source
  occupied 49,152 RGBA bytes versus 13,312 indexed texture/palette bytes; explicit
  32×24 reduction occupied 1,792 bytes while preserving original physical geometry.
  All six modes were captured, not inferred from descriptor acceptance.
- Manual controls, non-UTF-8 source paths, compositor-owner restart, corrupt-source
  last-good preservation, and real GUI Pause/Resume/error readback were exercised.
  Home revealed the applied image and returned to opaque theme ground when disabled;
  the actual `--render-home-work` wide/HiDPI/narrow journeys also passed.
- Opt-in 300-second rotation actually advanced between two raw-path sources twice;
  worker children were empty after presentation. Observation times include polling
  delay and are not calibrated deadline-latency or p95 measurements.
- Across 100 native refreshes, source mappings returned to zero, worker children
  were reaped and descriptor counts remained unchanged. Compositor anonymous PSS
  changed by +304 KiB and sessiond by +20 KiB. Total/file PSS increased; this is not
  whole-system savings or a stable-total-PSS claim. Programs are now cached per
  renderer context rather than recompiled per image.
- Stale sealed-FD requests no longer poison current diagnostics; an immediately
  disconnected 16-MiB request cannot replace a newer live request. Impossible
  backing+texture overlap is refused before mapping. These were failing-before,
  passing-after actual transport scenarios.
- Real isolated session-lock protocol coverage hid wallpaper from protected capture.
  Explicit disable now releases background resources even while locked, without
  changing the protected scene; the old locked-clear rejection left a stale texture.
  Lock/unlock now publishes activity immediately: queued policy recovered after
  unlocking without an activity query, new subscription or unrelated input.
- Real 3840×2160 baseline/progressive native JPEG workers stayed within the declared
  192-MiB address-space ceiling. Native EXIF-6 decoded to 2160×3840 with sampled
  PSS 60,120 KiB and one RGB staging raster; all eight orientations passed actual
  RGBA/indexed pixel checks. Progressive reduced decode still accounts coefficients.
  Corrupt/truncated input and an inadequate 16-MiB envelope failed clearly.
- X11 completed 100 one-shot replacements and freed previous owned pixmaps/markers.
  Native 24-bit MIT-SHM and 16-bit RGB565 PutImage fallback both displayed real pixels.
  Labwc layer-shell displayed six modes, bounded live buffers to two, and immediately
  refused 19 of 20 burst changes under release starvation. The pixman owner darkened
  outermost viewport-scaled edge pixels; interiors matched. One output/scale only.
- Real private KDE portal inspection exposed version 1 and its unsupported native
  controls. Initialization previously remained blocked after 469.7 seconds; the
  same real stalled owner now produces an explicit timeout in 30.006 seconds.
  Initialization shares a 30-second deadline, method invocation has 30 seconds,
  response has 120 seconds and cancellation is bounded to five seconds. With a
  fresh real permission store and permission approval on owned Xvfb, the owner
  returned response 2; no acceptance or presentation success is claimed.
- Full affected suites passed 1,298 tests with one existing ignored sessiond test,
  including all 119 PixelKit raster tests. Later full compositor coverage passed
  186 tests, and final decode/compatibility coverage passed all 26 wallpaper tests.
  Clippy still reports pedantic and unsafe-FFI warnings; this is not a clean lint gate.

The initial implementation observations above used an off-by-default policy. The
subsequent owner-approved default is WuDP-256, based on the optimized load-time
screen above, with explicit `null`/`off` retained. Smooth-gradient banding remains a
known quality tradeoff. Timings in the initial observations are debug runs and
2-ms PSS sampling can miss peaks. Physical DRM/GPU storage, multi-output/fractional
transforms, full-resolution 4K scanout, suspend/resume, matched Variety medians/p95
and the complete WuDP/NeuQuant/representation comparison remain release evidence
to collect. The original six release gates remain proposed; these observations
do not satisfy the whole-system claim.
