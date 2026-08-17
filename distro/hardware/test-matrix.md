# Hardware test matrix

Every supported profile must pass, on real hardware, before a kernel/driver set is promoted:

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

Runner: `lege doctor --hardware` (checks are data in `tests/hardware/`).
