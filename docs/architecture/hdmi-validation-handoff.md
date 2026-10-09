# HDMI validation handoff

The current HDMI work remains **unqualified**. The authoritative project record is
[`legeos.work.pdf-homegarden-hdmi-20261007/1`](../../.akr/records/legeos/work.akr), whose
`hdmi-safety` acceptance check is still open. Its latest owned dual-DRM observation is
[`legeos.evidence.owned-native-dual-drm-capability-20261008/1`](../../.akr/records/legeos/evidence.akr).

## What the owned runtime established

The October 8 `d1` test used a disposable QEMU guest with two virtual DRM devices and the
current pinned compositor, host daemon, session daemon and shell binaries. The real guest
session, PAM identity and production service ownership started correctly. Both virtual GPUs
lacked `EGL_EXT_device_drm`; render-node discovery therefore refused the unsupported path.
The production compositor and secondary worker reached GBM/EGL setup but produced no healthy
primary frame or ready output. The guarded hotplug observer correctly refused device removal
before sending a QMP mutation because readiness was false. The exact guest was exported and
stopped successfully.

This is an **inconclusive capability boundary**, not a successful HDMI journey and not evidence
of a defect on the reference Intel/NVIDIA hardware. There is no observed secondary presentation,
input/event-loop isolation, hotplug recovery, TV mode, suspend or power result from this run.
Focused policy, fence, recovery and primary-presence tests remain useful but do not substitute for
the missing owned production-worker journey or physical qualification.

## October 8 live observation (read-only, installed session)

Recorded as
[`legeos.evidence.live-hdmi-plug-installed-single-gpu-20261008/1`](../../.akr/records/legeos/evidence.akr).
The person plugged a TV into HDMI while a passive sampler read `/proc`, `/sys`, udev and the
journal. Nothing on the live session was started, stopped or opened.

- `HDMI-A-1` belongs to the NVIDIA card (`card2`); the panel `eDP-1` is on i915 (`card1`). The
  installed compositor (`templaros-comp …g1853e26`) drives only the primary GPU and ignores
  non-primary devices, so it cannot light the TV at all. The multi-GPU worker exists only in
  the uninstalled checkout.
- The plug woke the NVIDIA GPU from runtime suspend, the `nv_drm_handle_hotplug_event` work item
  hogged a CPU, and the unlit TV link flapped: 128 hotplug events in about 140 s.
- The compositor stayed responsive (main thread `S`/`ep_poll`, never `D`), available memory stayed
  4.7–8.2 GiB, and the panel kept presenting. No freeze.
- The only earlier logged plug (2026-10-07 05:55 UTC) froze the panel under critical memory
  pressure: the compositor blocked in `D`/`folio_wait_bit_common`, the watchdog aborted it after
  30 s, and `systemd-coredump` was still collecting five minutes later, so the session was never
  restarted. The kernel, NetworkManager, filed and logind kept working and the power key produced
  an orderly shutdown. That was a compositor stall under memory exhaustion, not a system hang;
  HDMI was coincident or contributory, not shown causal.

The checkout now addresses the core-collection delay described below. The host's lack of swap
and RAM-backed build storage remain separate resource follow-ups; neither was changed here.

## October 8 watchdog recovery change (source and owned subprocess only)

When an enabled watchdog reaches its existing abort deadline, it reads memory availability and
full memory PSI once. At critical pressure it exits with status 70 through `_exit`, bypassing core
collection and potentially stuck cleanup. The threshold is available RAM at or below 2% of total
(clamped to 256 MiB–1.5 GiB), or full PSI avg10 at least 2%. At ordinary pressure it retains the
existing abort and diagnostic core. Missing measurements do not imply pressure. No global core
policy, swap setting or installed service was changed.

Native focused watchdog tests passed (10 tests). An owned subprocess exited with status 70 and
neither a terminating signal nor a core-dump flag. These checks establish the source decision and
exit mechanism, not display-manager recovery or a hardware fix. The change does not bound an
uninterruptible kernel wait and does not show that HDMI caused the original memory stall. The
installed compositor still needs owner-controlled installation and physical qualification.

The coherent native package set `0.1.0~dev1791505403+gebc54d2.dirty` is now
prepared and inspected after workspace formatting and the display helper
extraction, with verification completed October 9 UTC. Its actual packaged
compositor passes `--check` in an
owned headless environment; this checks protocol/socket construction and does
not exercise a GPU worker or HDMI. Nothing was installed. The
[package cohort](../measurements/index-packages-2026-10-08-wp17.md) retains the
exact artifact hashes for an owner-controlled qualification window.

The October 9 post-format native run also passes all ten watchdog tests,
including the owned subprocess that exits with status 70, no terminating signal
and no core flag. This is a fresh observation of the current exit mechanism and
pressure decision. It does not exercise installed recovery or GPU hotplug. Exact
commands and source hashes are in the [refreshed cohort](../measurements/index-formatting-2026-10-09-wp17.md).

A read-only October 9 package database query still reports installed compositor
and filed at `0.1.0~dev1791205563+g1853e26`. This is package identity only, without
checking the running process or exercising the TV. The new prepared package set
remains separate from installed qualification.

## October 9 connected-TV diagnosis (read-only)

At 01:02–01:05 UTC the owner's powered TV is detected on NVIDIA
`card2-HDMI-A-1`: connected, a 256-byte Samsung EDID, and advertised modes up to
3840×2160. The kernel reports the connector enabled, which does not establish
compositor presentation. Intel `card1-eDP-1` remains enabled at 2560×1600.

Running compositor PID 2446, started October 8 at 02:01 UTC, has exactly the same
SHA256 as `/usr/bin/templar-comp` (`163334c1086aad5893cd6ae0e998b9bb951deb255c40698bb3d64760296c3ca5`).
The installed package remains `0.1.0~dev1791205563+g1853e26`; that binary contains
the single-primary-GPU exclusion. The live session has no Displays1 object.
Its journal records 134 NVIDIA device-change notices between 00:59:08 and
01:03:08. No TV frame or new secondary worker was observed.

The loaded and on-disk NVIDIA module versions both report `580.173.02` under
kernel `7.0.0-31-generic`. The missing update is compositor userspace: restarting
the existing installation would reload the same old binary. The owner's next
step is to install the prepared native packages, then log out and log back into
TemplarOS. This does not require a kernel-module update or reboot for the new
compositor code to load; physical HDMI functionality remains unqualified.

From the repository, as the ordinary desktop user:

```sh
bash scripts/reinstall-templaros-desktop.sh --skip-build --skip-nm-tray2
```

The same invocation with `--dry-run` completed without installation. Its 14
selected native packages all match build-set
`0.1.0~dev1791505403+gebc54d2.dirty` by controls and recorded sizes; hashes are
retained in the observation artifact. The agent did not install or restart
anything. Exact observations are recorded in
`legeos.evidence.connected-samsung-tv-old-runtime-20261009/1`.

## October 8 capability probe

Recorded as
[`legeos.evidence.hdmi-egl-device-capability-probe-20261008/1`](../../.akr/records/legeos/evidence.akr).
Host QEMU 10.2.1 has `virtio-vga-gl`/`virtio-gpu-gl-pci`, `egl-headless` and virglrenderer, which
would most likely give a guest a render node with `EGL_EXT_device_drm`. Its only backing is the
host `renderD128`, the Intel GPU that scans out the live desktop, and the d1 fixture boundary
forbids host GPU access. The owned journey was not re-run and nothing was mutated.

## Safe next step

Resume only when a disposable test environment can expose a real DRM device with the required
EGL device extension and the test owner can isolate it from the live desktop. Confirm the device
and EGL capabilities inside that environment before starting the compositor. Then retain evidence
for primary readiness/frame delivery, secondary worker ownership and bounded failure behavior,
input/event-loop responsiveness, hotplug/reprobe and rollback. Test TV-safe formats/modes and
power/suspend separately. If the environment lacks the capability, record the limitation and stop
before mutation; do not claim a pass from software fixtures.

Two ways to obtain that environment, both owner decisions: approve backing a disposable virgl
guest with the shared host `renderD128` (accepting that a guest-triggered GPU hang could reset the
live desktop's GPU context), or provide a separate machine. A real TV journey also needs the
multi-GPU compositor installed by the owner; the installed build cannot drive the NVIDIA HDMI port.

Do not restart, stop or kill the person's live `templar-*` services, deploy the development
session, or log out to free the host GPU. The owner controls those actions and any physical
qualification window. Preserve existing d1 evidence and test only in a disposable owned guest.
