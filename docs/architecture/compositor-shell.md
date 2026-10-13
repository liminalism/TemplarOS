# Compositor core and shell

Core (`templar-comp` + `templar-window`) is an independent Smithay compositor: one workspace as an
invariant (also to XWayland), one continuous scene across outputs, geometry invariants enforced after
every drag/output/scale/resume, compositor-owned recovery chrome, typed layers, `ext-session-lock`, a
minimal fallback switcher and the window overview (`task-system.md`), which is what “show every
window” became.

`WindowService` is the semantic window registry. Application role, protocol mapped state, explicit
shelving and activation MRU live there independently from `LayerStack`, whose only job is visual
Z-order. A temporary Wayland buffer detach or XWayland unmap retains the record and `WindowId`; only
destruction removes it. Alt-Tab walks every mapped application window, parked ones included, in
semantic MRU order, and the taskbar consumes the same filtered application view through the one
grouping, `templar_window::task::units`
(`task-system.md`). What is being presented — the ordinary scene, a Home reveal, or the overview —
is one value on the service, and a fullscreen window covers passive chrome only in the first.

Window name and control tabs default to a fixed bottom-right corner. Maximized windows retain
those tabs; fullscreen windows suppress their drawing, hover deadlines and click geometry.

Monitor sleep inhibition requires fullscreen video activity. Native Wayland players provide
a Video content hint and an active surface idle inhibitor. Firefox and Chromium can instead
provide explicit video playback wake-lock reasons through the ScreenSaver compatibility API,
scoped to the bus sender's PID and a single exposed fullscreen window. Pausing releases sleep
inhibition when the client releases its playback lock. Generic fullscreen apps, audio and
presentation locks do not inhibit monitor sleep. Browser PID-only requests cannot distinguish
DOM video fullscreen from F11; ambiguous multiple exposed windows and clients without a video
activity signal remain uninhibited.

Browser PID attribution follows Firefox's [parent-process wake-lock listener](https://raw.githubusercontent.com/mozilla/gecko-dev/master/widget/gtk/nsAppShell.cpp)
and Chromium's [in-process Device Service](https://raw.githubusercontent.com/chromium/chromium/main/content/public/browser/device_service.h).
Without a Video content hint, a browser needs the ScreenSaver compatibility path; a generic
Wayland inhibitor alone is insufficient. Protocol fixtures exercise the surface and private-bus
lifecycle; live browser playback has not been exercised.

Shell (`templar-shell`) provides Home Surface drawers, the bottom taskbar (explicit groups first, then
applications grouped by canonical AppId, with per-window hover rows), launcher, window shelf (explicit
minimisation), and notification rendering. It is restartable. Complete shell snapshots cross bounded,
latest-value handoff slots so a burst of window changes cannot queue stale UI states behind the render
thread.

Notification card bodies and Home notification rows invoke a declared `default` action first.
Otherwise they activate an existing source window, including a parked terminal. The service
can attach the authenticated sender's managed instance, with bounded same-user ancestry for
terminal completion helpers; that association persists and is bound to its notification event.
Without an instance, a unique catalogue attribution selects the application's most recent
window. Kitty completion helpers also supply the standard `desktop-entry` hint for unmanaged
terminal windows. An unavailable or stale source is reported; notification text never becomes
a command to launch. Dismiss and declared action buttons retain their separate click targets.

## Standalone X11 Displays

`templar-displays` shares the pure Displays page with Settings but talks directly to the actual RandR
server, without Alpha or sessiond. `--print` is read-only inventory. Validation checks the complete
connected inventory, advertised exact timings, CRTC routes, framebuffer bounds and native transform
capability. An unchanged transform needs no `RRSetCrtcTransform` request; a driver without transforms
can still apply a supported unchanged-transform layout. Rollback preserves raw transforms, filter
parameters, borders, disabled heads and the original primary, rather than reconstructing them from
the editable profile.

Each preview starts a separately owned rollback process before any native effect. That process owns
the monotonic confirmation deadline, the exact token/revision, actual target readback and raw-state
restoration. GUI death, pipe EOF, timeout or external configuration drift cannot transfer the deadline
to the GUI. A late final restoration reply is never confirmation. Failed restoration retains ownership
and reports actual state; it is not presented as a successful rollback. Durable profiles require a
unique kernel GPU/connector/EDID identity; temporary X11 output identities remain session-only.

Identification and calibration are actual Shape input-transparent windows, with bounded lifetimes and
exact owned-window teardown. RandR readback and an Xvfb native-surface run are not KMS TEST_ONLY,
physical presentation, three-head/hybrid-GPU hotplug, protected lock coverage or compositor black
margin evidence. Those physical gates remain separate from the standalone X11 regression journey.
