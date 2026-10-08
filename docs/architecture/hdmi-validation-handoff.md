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

## Safe next step

Resume only when a disposable test environment can expose a real DRM device with the required
EGL device extension and the test owner can isolate it from the live desktop. Confirm the device
and EGL capabilities inside that environment before starting the compositor. Then retain evidence
for primary readiness/frame delivery, secondary worker ownership and bounded failure behavior,
input/event-loop responsiveness, hotplug/reprobe and rollback. Test TV-safe formats/modes and
power/suspend separately. If the environment lacks the capability, record the limitation and stop
before mutation; do not claim a pass from software fixtures.

Do not restart, stop or kill the person's live `templar-*` services, deploy the development
session, or log out to free the host GPU. The owner controls those actions and any physical
qualification window. Preserve existing d1 evidence and test only in a disposable owned guest.
