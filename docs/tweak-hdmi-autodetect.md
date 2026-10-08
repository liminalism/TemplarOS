# Tweak: HDMI must just work (LegeOS)

Date: 2026-09-16
Machine: Lenovo Legion Slim 7 16IRH8 / 82Y3 (`dk-82y3`)
Status: bounded native HDMI software path implemented in source; physical Intel/NVIDIA/TV qualification remains required
For: `Rust-projects/LegeOS` — compositor / output hotplug

This is an OS job. Windows lights the TV when you plug HDMI in. LegeOS must do the same.
Nobody should SSH in and run `xrandr` to get a picture.

## What happened

The Legion's internal LCD still draws, but the backlight is dead, so the laptop is unusable
without an external display. A Samsung TV was on HDMI. Ubuntu 25.10 (SDDM + LXQt + Openbox,
X11, `prime-select nvidia`) booted to a graphical session on the internal panel and sent
**no signal** to the TV.

The session itself was healthy. The TV was electrically present. Linux simply never enabled
the HDMI output.

## Topology (do not guess this)

Hybrid graphics. HDMI is physically wired to NVIDIA, the panel to Intel.

| Piece | What it actually was |
|---|---|
| iGPU | Intel Raptor Lake Iris Xe (`i915`), DRM `card2` |
| dGPU | NVIDIA GeForce RTX 4060 Max-Q (`nvidia` 610.43.02), DRM `card1` |
| Internal panel | Intel `eDP-2` / XRandR `eDP-1-2`, 2560x1600@240, BOE NE160QDM-NZ3, connected and enabled |
| HDMI port | NVIDIA `card1-HDMI-A-1` / XRandR `HDMI-0` |
| TV | Samsung EDID (`SAMSUNG`), physical size ~1872x1053 mm, preferred 3840x2160@30, also offers 1920x1080@60 and 3840x2160@59.94 |
| Session | X11 `DISPLAY=:0`, SDDM autologin user `dk`, LXQt/Openbox |
| PRIME | `prime-select` = `nvidia`. Providers: `NVIDIA-0` (Source Output) + `modesetting` (Sink Output, Source Offload), already associated |

Two views of the same plug disagreed:

- Kernel sysfs `/sys/class/drm/card1-HDMI-A-1/status` stayed `disconnected`, EDID size 0.
- NVIDIA's X driver reported `HDMI-0 connected` with a full Samsung EDID, but **no CRTC**
  (connected and dark). `nvidia-smi` `Disp.A` was Off / `display_active=Disabled`.
  `CurrentMetaMode` was `NULL`.

That split is the bug class. An OS that only trusts i915 sysfs, or only the "primary" DRM
card, will conclude there is no TV.

## Process that actually lit the TV

Diagnose first. Do not reboot, do not reinstall drivers, do not rewrite xorg.conf.

1. Confirm the graphical session (user, seat, `Type=x11`, `Display=:0`, Xauthority from
   the session process — here `/tmp/xauth_HZYmzb`). SSH has none of that by default.
2. Inspect DRM connectors **and** the compositor/X driver's outputs. They can disagree.
3. Record the original layout (`xrandr --query`) so it can be undone.
4. Enable the HDMI output the driver already sees:

   ```bash
   xrandr --output HDMI-0 --auto
   ```

   That assigned CRTC 1 and set NVIDIA metamode `DPY-2: nvidia-auto-select @3840x2160`.
   Layout became two overlapping screens at `+0+0` (internal still primary).
5. Make HDMI the only useful screen:

   ```bash
   xrandr --output HDMI-0 --auto --primary --output eDP-1-2 --off
   ```

6. 4K@30 (the EDID preferred mode) configured cleanly but is a bad hotel-TV handshake.
   Drop to 1080p60. That is what produced a visible picture and flipped NVIDIA display
   active:

   ```bash
   xrandr --output HDMI-0 --mode 1920x1080 --rate 60 --primary --output eDP-1-2 --off
   ```

   After this: `nvidia-smi` `Disp.A` **On**, `display_active=Enabled`, GPU left P8-idle
   and actually scanned out. TV showed the LXQt session.

Output names are not stable across PRIME modes. On this boot they were `HDMI-0` and
`eDP-1-2`. Older LXQt saved layouts still said `HDMI-1-0` / `eDP-2`. Match live names;
do not replay a stored name.

### Persistence on this Ubuntu install (workaround, not the OS)

Until LegeOS owns this:

- `/home/dk/.local/bin/enable-hdmi-tv.sh` — wait for X, find any live `HDMI*`, prefer
  1920x1080@60, fall back to `--auto`, turn `eDP*` off. Retry ~30s for late EDID.
- LXQt autostart `~/.config/autostart/enable-hdmi-tv.desktop` (3s delay).
- SDDM `DisplayCommand` wrapper `/usr/local/sbin/sddm-xsetup-hdmi.sh` so autologin /
  greeter is not stuck on the dead panel.
- `lxqt-config-monitor -l` Hidden, because it would re-apply the stale `HDMI-1-0` layout
  after login and undo the fix.

Undo: `/home/dk/.local/bin/restore-internal-display.sh`, remove the autostart and the
SDDM drop-in `/etc/sddm.conf.d/hdmi-tv.conf`.

This is a bandage. LegeOS must not ship a world where the user needs it.

## What LegeOS must do automatically

Windows behaviour to copy: plug HDMI in (or boot with it already in) → picture on the
TV without a settings panel, without SSH, without knowing GPU names.

`templar-comp` now enumerates the seat's DRM cards and selects the primary
deterministically (boot VGA, otherwise render-capable, otherwise first sorted card).
The primary GPU remains the scene renderer and DMA-BUF feedback source. A secondary
connector gets a bounded isolated owner rather than the old logged-and-ignored path.
Secondary sysfs/EDID observations, DRM admission, GBM/EGL setup, scanout, readback and
cleanup do not run on input dispatch. The historical rescue above establishes the
hardware need; it does not qualify this implementation on that hardware.

### Required automatic process

On boot, on a settled DRM change, and on resume: debounce for 750 ms and admit only
one 1-second late-presence/EDID reprobe per device observation lifetime, not a repeating
open/modeset loop. A stalled owner stays admitted until its actual cleanup returns.

1. **Enumerate every DRM card**, not only `primary_gpu()`. Intel `card2` and NVIDIA
   `card1` are both in play on this chassis.
2. **Treat a connector as present if either** the kernel connector status is
   `connected` **or** the driving GPU's modesetting stack has EDID / a live KMS
   output. EDID presence requires nonempty kernel bytes within the shared 32768-byte
   limit. Worker rescans use this same presence rule to retain or retire surfaces,
   including loss of alternate evidence without a connector-status transition.
   Presence alone supplies no invented timing. Do not take i915-only sysfs as the
   whole story.
3. **Read EDID.** If the sink is a TV (here: Samsung) and the preferred mode is
   4K@30, do not blindly scan that out first. Try a TV-safe mode that the EDID
   also lists: `1920x1080@60`, then preferred, then first usable. Preferred-mode
   first is how we almost shipped a dark 4K30 desktop.
4. **Enable it.** Assign a CRTC, set a mode, start scanout. "Connected" with no CRTC
   is still no signal.
5. **Pick a primary.** If the internal panel is the only connected eDP but is not a
   usable display (lid closed, backlight dead, or user policy "prefer external"),
   make HDMI primary and turn eDP off or leave it as a non-primary clone. Default
   for a working laptop + working TV can stay "extend to the right"
   (`laptop-plus-hdmi.json`, `hotplug-sequence.json`). Default for "internal is
   unusable" is HDMI-only, which is what rescued this session.
6. **Keep doing it.** Hotplug after login, unplug, replug, lid events, NVIDIA coming
   out of runtime D3. One enable at greeter is not enough if the compositor later
   rescans and drops the NVIDIA connector again.
7. **Do not depend on output name strings** (`HDMI-0` vs `HDMI-A-1` vs `HDMI-1-0`).
   Identify by connector type + DRM device + EDID hash.

Success looks like: cable in, TV on the right input, Linux desktop on the TV, no
manual `xrandr`. Failure looks like this rescue: TV says No Signal, internal panel
is the only enabled monitor, NVIDIA `Disp.A` Off.

## Pointers in-tree

- `compositor/templar-comp/src/backend/udev.rs`, `native_policy.rs` — deterministic
  primary selection, bounded settling, TV-safe advertised mode selection and feedback
- `native_worker.rs`, `native_device_worker.rs`, `native_outputs.rs`, `native_fence.rs`
  in that directory — asynchronous native transactions and owned cross-GPU transport
- `services/templar-hostd/src/display_devices.rs`, `display_recovery.rs` — authenticated
  secondary descriptor lifetime and shared session/greeter boot-local crash breaker
- `interfaces/dbus/org.templaros.Host.DisplayRecovery1.xml` — additive FD lease contract
- `tests/hardware/output-hotplug.toml` — physical qualification plan, not a passed test
- `tests/fixtures/topologies/laptop-plus-hdmi.json`
- `tests/fixtures/topologies/hotplug-sequence.json` (eDP → eDP+HDMI → HDMI-only → eDP)
- `docs/measurements/hardware-inventory-dk-82y3-20260907.md` — this exact chassis:
  `card1 nvidia (connected: none), card2 i915 (connected: eDP-2)` while HDMI was
  already the NVIDIA port

## Non-goals for this tweak

Do not reinstall NVIDIA, do not switch kernels, do not rewrite xorg.conf, do not
make `prime-select` ping-pong, to "fix HDMI". The GPU was fine. The output was
idle. LegeOS should enable idle connected outputs.

## Software behavior and qualification limits

Secondary activation is lazy: dormant metadata observations do not create a DRM,
GBM or EGL owner. An advertised connected sink and an active, unblanked seat are
required before acquisition. Hostd accepts only the installed root-owned compositor
inode in the exact session service cgroup, or an actual active logind greeter on that
seat. It resolves opaque GPU identity against trusted seat cards, never an arbitrary
device path, and never transfers its deterministic primary card. Process start,
unique bus name, actual logind session and active-seat changes revoke a lease.
An unchanged sink with no usable advertised timings receives only its one late-EDID
probe; failure then releases the secondary context and lease instead of keeping it
awake or inventing a mode. A healthy greeter-to-user transition is bound to the actual
logind session, not merely a surviving greeter PID. The old root owner is retired;
new acquisition waits asynchronously and boundedly for its real cleanup, never
replacing a still-occupied slot.

The marker is durably written before open/rearm. Root driver owners are capped at
eight, one per seat/GPU, with one outstanding operation or unpublished FD handoff.
The unpublished FD guard remains alive through D-Bus reply serialization/send and
returns its duplicate to that same owner for close. Existing owned leases can clean
up after seat deactivation or an installed-binary upgrade without gaining new grants.
Compositor secondary owners are also capped at eight; driver command admission is
bounded and nonblocking. Root open/rearm has a three-second request deadline;
compositor owner/TEST_ONLY/frame admission has a five-second watchdog. A deadline
does **not** cancel kernel work: a timed-out owner retains its slot and in-flight
resources until real driver cleanup returns. No blocking join or synchronous
secondary-device fallback is used. Message serials and device/frame/transaction
epochs prevent late opens, presentation reports or cleanup from replacing a newer
live admission. The next session or greeter remains primary-only with an explanatory
notice after an unfinished secondary attempt; that notice is not a GPU diagnosis.

HDMI chooses advertised progressive 1920×1080 near 60 Hz before preferred 4K30.
Cross-GPU transport and secondary scanout are explicitly 8-bit, using common linear
DMA-BUF formats and bounded native-fence admission, never a hidden CPU copy or an
untimed fence wait. Unsupported import/fence/atomic KMS capabilities are reported.
When the connector advertises `max bpc`, eight is applied and checked again on the
first real presented frame and resume. Without that property, the 8-bit buffer format
does not prove the physical HDMI link depth.

Transactions TEST_ONLY before effects, retain old cross-device outputs until every
enabled target really presents, then break and read back. Readback compares signal
dimensions/rate/interlace rather than a strict mode-description string. A stalled
secondary does not queue endless rollback attempts; available saved primary-GPU
targets can be rearmed independently while the **full** rollback remains an explicit
failure/unknown state. No physical darkness is inferred from a timeout, owner exit
or a failed ioctl. Frame callbacks follow the surface's primary output, not every
output's refresh. Logical primary is selected among connected, enabled, mapped
heads: a valid explicit primary wins, otherwise stable display identity selects
the fallback. Inventory, Home placement and capture use this same projection.
Removing the old primary therefore promotes a surviving mapped head without
rewriting the scanout owner's configuration or weakening frame admission.
Home previews run on the primary GPU's logical-primary pass,
including external-only layouts; secondary EGL/KMS owners never capture them.
Hidden Home demand is removed and rechecked before rendering, so hidden wells
schedule no new capture work.

This isolates secondary driver entry, not primary renderer/kernel faults, shared
driver memory pressure or system-wide OOM. The October 7 installed watchdog abort
was from a single-GPU binary, with a fence/page-wait stack coinciding with global
OOM/no swap; it is **not** evidence that HDMI or this multi-GPU path caused it.
Actual NVIDIA fence/import behavior, Samsung signal/link depth, dGPU runtime suspend
after release, NVIDIA preserve-video-memory configuration, suspend/resume and
greeter restart still require physical qualification. No such qualification, build
or runtime check was performed by this source-only implementation task.

## Owned hardware smoke plan

Use a recoverable lab seat with an independent SSH path, not the live owner desktop.
Record kernel/driver, connector EDID, mode/depth readback and compositor/hostd logs.

1. Boot with HDMI absent: primary/feedback/Home must remain deterministic; observe
   that no secondary DRM/GBM/EGL lease is created and measure actual dGPU runtime D3.
2. Attach the Samsung TV on the dGPU port. Exercise cable bounce, late EDID and boot
   with the TV already connected; expect one settled activation, advertised 1080p60
   and 8-bit buffers, with actual signal and advertised link-depth readback recorded.
3. Move a window between the high-refresh panel and 60-Hz TV. Check primary-output
   callback pacing, pointer continuity, scale/drag anchors and Home placement.
   Exercise external-only Home previews, then hide/lock Home and measure zero new
   well captures; no preview work may enter the secondary EGL/KMS owner.
4. Preview panel-off/HDMI-only and revert/expire it. Prove the new target presents
   before the old output is disabled and that final native routing/timing matches.
   Exercise unsupported import/fence and an isolated stalled-owner fault; healthy
   primary input must continue, ownership counts remain bounded, and unknown
   secondary/full-rollback state must never be reported as success or darkness.
5. Switch away from the active seat, drop the compositor's bus name, and end its
   process. Confirm revocation and refusal of stale rearm/open/publication. Interrupt
   activation before first presentation; both the next user session and greeter must
   stay primary-only with the boot-local recovery notice rather than restart-loop.
6. Unplug/replug and inspect actual descriptor/context cleanup and runtime suspend.
   Repeat suspend/resume with the driver's required NVIDIA video-memory preservation
   settings; qualify missing/stale events and same-signal readback independently.

Parent integration checks should compile compositor/hostd and run focused
`native_policy`, `native_fence`, `native_worker`, display-control, `display_devices`
and `display_recovery` regressions plus the additive D-Bus ABI baseline. Software
checks do not substitute for the six physical steps above.
