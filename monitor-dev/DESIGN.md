# Templar Displays: a smarter monitor utility

## The product in one sentence

Choose what you want the screens to do; the application handles enabling,
positioning, safe mode selection, reconnection and recovery.

The current Monitor-style checkbox workflow asks the user to manage output
state. The replacement should ask for intent: **Extend**, **Duplicate**,
**Show only on 1**, or **Show only on 2**. A connected-but-disabled television is
still a connected television and must remain visible in the interface.

This document describes both the intended complete utility and its staged
implementation. README.md is authoritative about what the delivered alpha does
and does not implement. In particular, compositor changes and NVIDIA-specific
picture fitting are not included as working implementations in this delivery.

## The everyday experience

### Connect a TV

The application displays two labeled cards immediately after discovery:
“1 · Laptop display” and “2 · Samsung TV · HDMI”. A driver-provided manufacturer
or product name is preferable to a connector string alone. The connector stays
available as useful secondary information.

On a new screen combination, keep the laptop active and extend onto the TV at
an advertised recommended mode. This avoids the dangerous default of silently
moving the entire desktop to a television that might be on the wrong input.
The app does not require opening the window to process a connection once its
session helper has been enabled at login.

The UI distinguishes **connected**, **enabled**, and **configured successfully**.
It must not claim that EDID detection proves the TV is actually showing a picture.
A TV can remain electrically connected while off or showing another input.

### Choose TV only

“Show only on 2” constructs the whole requested configuration: TV enabled,
supported mode selected, position set, TV made main where the backend supports
that concept, and laptop disabled. The user does not assemble those steps with
checkboxes. Apply starts a guarded preview. Keep confirms it; Escape, Revert or
timer expiry restores the previous usable configuration.

Do not make the laptop's disabled state a permanent policy that survives the
loss of the screen it was disabled in favor of.

### Unplug the TV

The helper observes that the former usable screen has disappeared. It enumerates
what is connected now, enables the laptop at a supported recommended mode, and
places it at the desktop origin. An old profile pointing at the removed TV is
not treated as an executable recovery command.

This fallback is temporary. It must not overwrite the user's confirmed
“when this TV is connected, use TV only” preference. Reconnecting the same TV
can restore that confirmed preference. The alpha uses conservative connector-
specific identities; moving a television to a new port is treated as a new setup.

Application-window relocation is the window manager/compositor's responsibility.
On TemplarOS it should be explicit: migrate unreachable windows and panels,
move keyboard/pointer focus to a usable output, and keep the settings confirmation
surface reachable. The current X11 alpha relies on the existing window manager
for this behavior and does not implement a separate window-moving policy.

### Keep manual controls without requiring manual setup

The main page contains the four intent buttons, an arrangement canvas, a selected
screen's resolution and refresh controls, orientation, and a main-screen action
where supported. Advanced settings must not compete with the main flow.

Use the actual advertised refresh modes. Keep 59.94 and 60.00 distinct rather
than rounding both to an ambiguous “60 Hz”. A recommendation is a default, not
a lock: a user can still choose a lower resolution or another refresh rate.

A full product should preserve stable screen numbering and give an Identify
overlay on each real output. The alpha labels the arrangement and selection
cards, but does not yet create physical identify overlays.

## “Fit the picture to the TV” is a separate control

### Separate three concepts

**Signal resolution** is the HDMI timing sent to the television, for example
1920 × 1080 at 60 Hz. **Desktop/UI scale** determines the logical size of text,
windows and controls. **Picture fitting** determines the rectangle within the
signal that contains the visible desktop. They are not interchangeable.

A typical overscan correction can keep a 1920 × 1080 signal while fitting the
whole desktop inside an 1824 × 1026 rectangle, inset 48 pixels horizontally and
27 vertically. This preserves the 16:9 picture ratio. Changing the advertised
resolution is neither necessary nor always helpful.

Where a TV offers a real pixel-for-pixel mode, that is preferable because GPU
shrinking can soften text. But the utility should still offer genuine GPU or
compositor fitting rather than send the user to another application.

### Intended interaction

Choose Fit TV picture. The eventual full-screen target shows a border, corner
markers and a readable center control. Drag corners inward until every edge is
visible. Provide keyboard nudging because an initially cropped corner may not
be reachable, and provide Reset. The center controls must remain inside a safe
area even before calibration is complete.

Default to symmetric margins with aspect ratio locked. Unlocking independent
edges is advanced functionality, not the default. Apply should use the same
15-second guarded preview as any other display change. Save a calibration only
after confirmation, keyed to the display and the relevant signal mode.

The delivered alpha has draggable corners on a miniature calibration preview,
symmetric horizontal/vertical border controls, reset, and guarded application
to a driver exposing RandR underscan. It does not yet provide a full-screen
target, independent offsets, or continuous live application while dragging.

### Supported mechanisms, not workarounds

On appropriate X11 drivers, use their advertised underscan properties. On the
proprietary NVIDIA X driver, ViewPortOut and ViewPortIn are documented facilities
for defining and scaling the visible picture. A future NV-CONTROL adapter should
preserve the complete existing MetaMode, change only the intended output, and
restore the complete snapshot after failure. It must resolve the actual output
owner; merely detecting an installed NVIDIA GPU is not sufficient on a hybrid
laptop. That adapter is not implemented in this alpha.

On TemplarOS, the compositor can own a supported safe-area transform in its
normal composition path. It can attempt a hardware plane/scaler path where
available, and otherwise use normal GPU composition with black margins. The
setting belongs to output configuration, not to an application that grabs KMS
control away from the compositor.

Do not implement the feature with arbitrary modelines, fake EDIDs, oversized
panning desktops, UI zoom, scripted root configuration rewrites, or a generic
matrix that accidentally samples a neighboring extended screen.

When a backend cannot implement fitting reliably, display a reason. Do not show
an enabled slider that changes nothing, and do not assume support from the GPU
vendor name alone.

## Architecture and safety ownership

The front-end is replaceable. This alpha uses egui as a Rust test front-end; a
PixelKit front-end can share the typed model and local control interface.
Display policy must not move into whichever window happens to be open.

The session helper owns configuration requests, validation, optimistic state
revisions, profiles, the unconfirmed journal, preview timeout and hotplug policy.
There is one helper per graphical session, protected by a lifetime file lock,
with a private local socket. It does not run as root or change system-wide
boot/login display settings.

The X11 adapter uses RandR through xrandr. The Wayland adapter uses
wlr-output-management through wlr-randr. Those command transports are intentionally
replaceable by native Rust bindings. They use argument arrays, not a shell.
The initial implementation favors an inspectable compatibility boundary over
inventing a display server inside the application.

The preview transaction is:

1. Refresh inventory and reject a stale UI draft.
2. Validate every output, mode, position, capability and safety invariant.
3. Test the backend configuration where supported.
4. Persist the previous state before any mutation.
5. Apply, read back the actual configuration, and reject unexpected results.
6. Await explicit Keep with a service-owned deadline.
7. On failure or expiry, restore what remains applicable, otherwise compute a
   new connected-screen fallback. Preserve the journal when recovery itself fails.

RandR dry-run is not advertised as an atomic hardware transaction. The alpha
excludes property writes from its dry-run argv. A successful logical test or
readback still does not prove that a person can see the picture.

A production TemplarOS compositor should make user-preview rollback independent
of both the UI and the external helper. The compositor should own the final
lease/deadline and restore on control-client loss. The alpha instead supplies a
separate helper, a recovery journal, and optional systemd crash-restart supervision.
An unsupervised helper cannot execute recovery after it has itself been killed.

## Policies that must not get mixed together

Display topology, screen blanking, laptop lid/suspend behavior, brightness,
HDMI audio and graphics-driver recovery are different responsibilities.

The utility must not wake a deliberately sleeping display on every polling
iteration. Its fallback reacts to startup, topology loss and lid reopening,
not merely to every blank/powered-down state. It does not change logind policy
or install a suspend inhibitor. A missing lid hint is not authoritative proof
of an open or closed lid; the alpha uses it only as a best-effort recovery hint.

In TemplarOS, assign one output-policy owner and make the app its client. Under
Lubuntu, avoid competing automatic profile restorers. Keep the old interactive
utility installed until the new one passes testing; do not disable the entire
power manager just to stop one display-layout policy.

## What makes the alpha ready to replace Monitor

Compilation alone is not the acceptance gate. The important tests use the real
laptop and TV: connect while already logged in, start with HDMI already connected,
choose TV only, close the app, unplug, reconnect, cancel a resolution change,
kill the UI during preview, restart the supervised helper during preview, and
repeat with lid/suspend transitions. Confirm that resolution, refresh and fitting
survive only the intended confirmed profile transitions.

The first release should be declared ready for a particular hardware/session
combination, not universally ready because it happened to work in a demo.
See TESTING.md for the exact checklist and the delivered validation status.

## Next implementation order

First compile and fix any integration errors, then qualify the X11 hotplug and
rollback paths on the actual Lubuntu machine. Next wire TemplarOS's compositor
output-management interface and compositor-owned fallback. Then add its safe-area
rendering/input transform and the NVIDIA-specific adapter. Native event-driven
Rust transports, physical identify overlays and full-screen calibration should
follow before polishing cosmetics. Preserve the same readable four-choice flow
throughout; none of these backend improvements require a more complicated main UI.
