# Compositor core and shell

Core (`templar-comp` + `templar-window`) is an independent Smithay compositor: one workspace as an
invariant (also to XWayland), one continuous scene across outputs, geometry invariants enforced after
every drag/output/scale/resume, compositor-owned recovery chrome, typed layers, `ext-session-lock`, a
minimal fallback switcher and “show every window”.

`WindowService` is the semantic window registry. Application role, protocol mapped state, explicit
shelving and activation MRU live there independently from `LayerStack`, whose only job is visual
Z-order. A temporary Wayland buffer detach or XWayland unmap retains the record and `WindowId`; only
destruction removes it. Alt-Tab uses the semantic application MRU and the taskbar consumes the same
filtered application view, including an MRU rank for each chip's hover-row windows.

Shell (`templar-shell`) provides Home Surface drawers, the bottom taskbar (explicit groups first, then
applications grouped by canonical AppId, with per-window hover rows), launcher, window shelf (explicit
minimisation), and notification rendering. It is restartable. Complete shell snapshots cross bounded,
latest-value handoff slots so a burst of window changes cannot queue stale UI states behind the render
thread.
