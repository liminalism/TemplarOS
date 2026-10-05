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

Shell (`templar-shell`) provides Home Surface drawers, the bottom taskbar (explicit groups first, then
applications grouped by canonical AppId, with per-window hover rows), launcher, window shelf (explicit
minimisation), and notification rendering. It is restartable. Complete shell snapshots cross bounded,
latest-value handoff slots so a burst of window changes cannot queue stale UI states behind the render
thread.
