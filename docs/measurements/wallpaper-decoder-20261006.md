# Wallpaper decoder probe — 2026-10-06

## Observed result

Native Linux 7.0.0-31-generic, x86_64. `/usr/bin/cjpeg` and `/usr/bin/djpeg`
reported **libjpeg-turbo 2.1.5 (build 20260406)**. A disposable generated
7680×4320 RGB source was encoded at JPEG quality 80, then `jpegtran -progressive`
produced a progressive version of the same image. Decoder output was streamed to
`/dev/null`, not displayed or retained by the observer.

Command per source: `djpeg -scale 1/4 IMAGE.jpg`; output dimensions 1920×1080.
After one initial decode of each, three ordered baseline/progressive pairs ran.
The observer sampled the child's `smaps_rollup` only after `/proc/PID/comm` became
`djpeg`, sleeping 2 ms between reads, then reaped the child with `wait4`.

| Source/run | Elapsed seconds | Samples | Sampled peak RSS KiB | Sampled peak PSS KiB |
|---|---:|---:|---:|---:|
| baseline 1 | 0.0262 | 11 | 2472 | 667 |
| progressive 1 | 0.1231 | 40 | 99764 | 97959 |
| baseline 2 | 0.0250 | 11 | 2540 | 735 |
| progressive 2 | 0.1283 | 41 | 99716 | 97915 |
| baseline 3 | 0.0252 | 11 | 2476 | 671 |
| progressive 3 | 0.1209 | 42 | 99772 | 97967 |

All six child exit codes were 0 and all were reaped. These are sampled lower bounds
on peak memory, not allocator/cgroup-certified peaks or statistically stable p95.
Elapsed times include observer sampling/reaping overhead; no cold-cache claim.
The images were decoded sequentially, with no graphics backend or wallpaper process.

`wait4.ru_maxrss` for baseline was 37524 KiB, including the Python launcher's
pre-exec high-water memory; it is **not** reported as decoder-only RSS. Progressive
wait4 peaks were 99196–99216 KiB. `/usr/bin/time` was unavailable; post-exec smaps
sampling avoided interpreting launcher memory as decoder residency.

Fixture hashes:

- baseline JPEG: `39de3b96dd933906d04cf4d0a35c4b35b775ab36c53bcd30e7fd744db1072075`
- progressive JPEG: `c456f76402484ad78e34335e5faebf61be152c4e3552b23388560b514669c7f8`

## Interpretation and limits

A small decoded output does not imply small internal decoder memory. The
[libjpeg-turbo memory documentation](https://raw.githubusercontent.com/libjpeg-turbo/libjpeg-turbo/main/doc/libjpeg.txt)
explicitly describes a full-image coefficient array for multiscan/progressive
JPEG: approximately 3 source bytes/pixel for ordinary 4:2:0, up to 6 for 4:4:4,
before application output. It also warns that two-pass palette quantization can
add a full RGB image and lookup table. The current upstream document is a
reference, not the version of the executable used in this measurement.

A temporary process confines this memory to the change and releases it on exit;
it does **not** eliminate the switching peak. The design budgets progressive
sources separately from the common baseline-JPEG path. No idle utility-memory,
Variety comparison, small-JPEG enlargement, palette-quality, Wayland/X11 buffer,
GPU or compositor-memory result was obtained by this probe.

## Reproduction recipe

Use a disposable directory. No live display, user bus, services, global cache
flush or root privileges are needed. The source PPM is about 95 MiB on disk;
write one row at a time so fixture creation itself does not hold the entire image.
For every `(x,y)` use:

```
R = floor(x * 255 / 7679)
G = floor(y * 255 / 4319)
B = (floor(x / 32) + floor(y / 32)) mod 256
```

Write P6 header `P6\n7680 4320\n255\n`, followed by those packed RGB rows.
Run `cjpeg -quality 80 -outfile baseline-8k.jpg source-8k.ppm`, then
`jpegtran -progressive -outfile progressive-8k.jpg baseline-8k.jpg`.
Decode both with `djpeg -scale 1/4` to `/dev/null`. Use an owned child PID and
post-exec `/proc/PID/smaps_rollup` RSS/PSS samples, not inherited launcher
high-water RSS; tolerate process exit during a sample and always wait/reap.
Repeat the paired warm runs. Remove all temporary image fixtures afterward.

The exact observed table above is retained as the evidence artifact; temporary
fixtures/observer files are not implementation code or permanent tests.
