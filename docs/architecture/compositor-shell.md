# Compositor core and shell
Core (cosmic-comp fork + `templar-window`): one workspace as an invariant (also to XWayland), one continuous
scene across outputs, geometry invariants enforced after every drag/output/scale/resume, compositor-owned
recovery band, typed layers, `ext-session-lock`, minimal fallback switcher and "show every window". Shell
(`templar-shell`): Home Surface drawers, panel/dock (grouping by canonical AppId), launcher, window shelf
(explicit minimisation), notification renderer — restartable.
