# Hardware test matrix

Every supported profile must pass, on real hardware, before a kernel/driver set is promoted.

The matrix has two halves, and `lege doctor --hardware` keeps them apart on purpose. **Nothing in
the manual half has ever been executed by the tool**: those rows are reported as `not-executed` with
their steps, and only a supervised run on the reference laptop can turn one into a pass or a fail.

## Automated — bounded, read-only probes

Run by `lege doctor --hardware` (implementation: `crates/lege-hwdetect/src/probes.rs`; the same
probes back `lege-boot-health`'s boot verdict). Every one is read-only and killed at its timeout.
Each reports one of four outcomes: `pass`, `fail`, `skipped` (this machine cannot answer — no such
tool, no such hardware) or `not-executed` (nobody asked — it timed out, or `--root` points at
something that is not this machine, so running a host tool would describe the wrong system).

| Probe | What it observes | Skipped when |
|---|---|---|
| kernel-release | `proc/sys/kernel/osrelease` | the file is unreadable |
| drm-connected-output | at least one DRM connector reports `connected` | — |
| drm-modesetting | a `cardN` device exists and `nomodeset` is absent | — |
| nvidia-module | loaded module version and installed userspace share a driver branch | no NVIDIA GPU (reported as a pass: nothing to be incoherent about) |
| nvidia-module-signer | `modinfo -F signer nvidia`, plus whether a `linux-modules-nvidia-*` LRM package is installed | no NVIDIA GPU, or `modinfo` absent |
| nvidia-smi | `nvidia-smi -q`: product name and driver version | no NVIDIA GPU, or `nvidia-smi` absent |
| vulkan-devices | `vulkaninfo --summary`: the devices the loader can see | `vulkaninfo` absent |
| prime-offload | hybrid machine, NVIDIA GPU bound to `nvidia`, `prime-select` present — *availability only* | not a hybrid machine |
| cuda-device-query | CUDA's `deviceQuery`, if it happens to be on `PATH` | no NVIDIA GPU, or no `deviceQuery` (Ubuntu does not package the CUDA samples) |
| vaapi | `vainfo`: how many VA-API profiles the media driver offers | `vainfo` absent |

`prime-offload` never selects a mode and never runs `prime-select`; actually rendering a frame on
the dGPU is the manual `intel-desktop-nvidia-offload` case below. No probe changes GPU mode,
suspends, reboots, or touches a cable — by construction, not by convention.

## Manual — user-run only, never executed by the tool

Data in `tests/hardware/*.toml`; selected by profile and reported as `not-executed` with their exact
steps and expectations.

| Check | What passes |
|---|---|
| intel-desktop-nvidia-offload | session on the iGPU; `__NV_PRIME_RENDER_OFFLOAD=1 vkcube` renders on the dGPU |
| nvidia-desktop | session driven by the dGPU (mode `discrete`) reaches `lege-boot-health` healthy |
| external-monitor-each-output-path | HDMI (dGPU-wired) and USB-C/DP (iGPU-wired) hotplug: no lost windows (`lege-window` invariant), correct scale |
| suspend-resume | s2idle/deep round trip; compositor, audio, network back within 5 s; no GPU reset |
| kernel-update-and-rollback | hostd transaction: candidate boots, health probe passes → blessed; forced failure → rolled back automatically |
| driver-update-and-rollback | same for the LRM + userspace pair |
| wayland-and-xwayland-apps | identity resolution `verified-launch` for both; dock groups correctly |
| cuda-and-vulkan-where-installed | `nvidia-smi`, `vulkaninfo`, a CUDA sample run |
| hardware-video-decode | VA-API on the iGPU and NVDEC on the dGPU actually decode a video — **blocked** while the seeds declare no media player (X1/C6) |
| brightness-and-audio-keys | XF86 keys route through the compositor to `Backlight1`/`Audio1` whatever has focus (X1/C6) |
| removable-storage | insert, mount, copy, eject — **blocked in code**: `org.legeos.Files.Volumes1` has no mount or eject verb, so there is nothing to drive (X1/C6) |

The three GPU modes (hybrid, integrated, discrete) are exercised by the first two rows plus a mode
change, which is a user action: `lege doctor --hardware --check gpu-mode` only validates that the
*declared* mode in `/etc/legeos/hardware/gpu-mode` is one the detected profile supports. It never
writes that file.

Runner: `lege doctor --hardware` (`--json` for evidence, `--check <id>` for one probe or case,
`--profile <id>` to review another profile off-device).
