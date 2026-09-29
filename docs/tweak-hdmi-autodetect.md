# Tweak: HDMI must just work (LegeOS)

Date: 2026-09-16
Machine: Lenovo Legion Slim 7 16IRH8 / 82Y3 (`dk-82y3`)
Status: recorded from a live rescue, not yet implemented in LegeOS
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

`templar-comp` already has the right *shape* (`udev.rs`: connector hotplug →
`scan_connectors` → `connector_connected` → `recompute_outputs`). The current *scope*
is wrong for this machine.

`compositor/templar-comp/src/backend/udev.rs` documents that every non-primary DRM
device — "e.g. the NVIDIA half of a hybrid Intel+NVIDIA laptop" — is **logged and
ignored**. HDMI on 82Y3 lives on that ignored half. The existing hardware test
`tests/hardware/output-hotplug.toml` (`profiles = ["hybrid-intel-nvidia"]`, step
"connect HDMI (dGPU path)") already names this path. The compositor does not walk it.

### Required automatic process

On boot, on DRM change, on resume, and on a repeating short probe until the first
successful HDMI enable (NVIDIA EDID can lag sysfs):

1. **Enumerate every DRM card**, not only `primary_gpu()`. Intel `card2` and NVIDIA
   `card1` are both in play on this chassis.
2. **Treat a connector as present if either** the kernel connector status is
   `connected` **or** the driving GPU's modesetting stack has EDID / a live KMS
   output. Do not take i915-only sysfs as the whole story.
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

- `compositor/templar-comp/src/backend/udev.rs` — `is_primary_gpu_dev`, the ignore
  path, `select_preferred_mode`, `connector_connected`
- `tests/hardware/output-hotplug.toml` — hybrid Intel+NVIDIA HDMI is already a
  hardware profile; it needs the compositor to drive the dGPU connector
- `tests/fixtures/topologies/laptop-plus-hdmi.json`
- `tests/fixtures/topologies/hotplug-sequence.json` (eDP → eDP+HDMI → HDMI-only → eDP)
- `docs/measurements/hardware-inventory-dk-82y3-20260907.md` — this exact chassis:
  `card1 nvidia (connected: none), card2 i915 (connected: eDP-2)` while HDMI was
  already the NVIDIA port

## Non-goals for this tweak

Do not reinstall NVIDIA, do not switch kernels, do not rewrite xorg.conf, do not
make `prime-select` ping-pong, to "fix HDMI". The GPU was fine. The output was
idle. LegeOS should enable idle connected outputs.
