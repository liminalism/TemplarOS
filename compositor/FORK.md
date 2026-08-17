# lege-comp — the cosmic-comp fork

Upstream: pop-os/cosmic-comp (Smithay). Branches: `upstream` (tracking), `legeos` (patches). Each patch is
one of the following, and nothing else:

1. **One workspace as an invariant** — exactly one workspace exposed, including to XWayland interfaces;
   multiple monitors form one continuous scene; windows belong to the scene, not to a hidden desktop.
2. **`lege-window` bridge** — `WindowService` owns geometry/identity records; `repair_window` runs after
   every drag, output/scale change, resume and geometry request; the recovery band is compositor-drawn.
3. **Identity** — `lege-comp-integration::DbusIdentityCache`; sanitized identity into the toplevel-info path.
4. **Typed layers** (`lege_window::Layer`) replacing arbitrary always-on-top.
5. **Recovery** — `lege_recovery_v1` overlay + "show every window" + minimal fallback switcher, all working
   without the shell; `ext-session-lock-v1` for the lock screen.
6. **Private shell protocol** `lege_shell_v1`; the shell is a separate, restartable process.
7. **Explicit minimisation** to the window shelf.

Everything else stays upstream. Rebase monthly; a patch that cannot be described in one line above is a
smell. Property tests for the geometry live in `crates/lege-window`, not in the fork.
