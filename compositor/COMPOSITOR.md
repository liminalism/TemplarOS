# lege-comp — the compositor's contract with `lege-window`

`lege-comp` is written against **Smithay** and is an ordinary member of this workspace — not a fork of
`cosmic-comp` (ADR-0012). Smithay supplies the display server: DRM/KMS with atomic modesetting and direct
scanout, EGL/GBM, libinput, libseat, XWayland's window-manager role, and server-side implementations of
every protocol below. What Smithay deliberately does not supply is *window-management policy* — it has no
workspace, tiling or placement concept at all — and that is exactly the gap `crates/lege-window` fills.

So this document is not a patch list. It is the contract: what the compositor must detect, and what it
must call. The seven properties it must uphold are:

1. **One workspace as an invariant** — exactly one workspace exposed, including to XWayland interfaces;
   multiple monitors form one continuous scene; windows belong to the scene, not to a hidden desktop.
2. **`lege-window` bridge** — `WindowService` owns geometry/identity/layer records; `repair_window` runs
   after every drag, output/scale change, resume and geometry request; the recovery band (and, for
   client-side-decorated/borderless windows, the compositor-painted recovery tab) is compositor-drawn.
3. **Identity** — `lege-comp-integration::DbusIdentityCache`; sanitized identity into the toplevel-info path.
4. **Typed layers** (`lege_window::Layer`) replacing arbitrary always-on-top; `LayerStack` is the sole
   z-order authority — the compositor's own scene-graph stacking must be driven from `WindowService::stacking_order`,
   never maintained independently.
5. **Recovery** — `lege_recovery_v1` pressure overlay + "show every window" scatter (shelved windows
   included) + minimal fallback switcher + fullscreen/modal escape + "go home", all bound to fixed key
   combinations and calling straight into `WindowService`, so all five work without the shell;
   `ext-session-lock-v1` for the lock screen.
6. **Private shell protocol** `lege_shell_v1`; the shell is a separate, restartable process.
7. **Explicit minimisation** to the window shelf — and a shelved window is *gone from the screen*:
   `WindowService::is_visible` is the one predicate `Space` mirroring (`src/space_sync.rs`), hit-testing
   and chrome consult, so a hidden window neither renders, takes clicks nor paints a band. "Go home"
   (`WindowCommand::RevealHome`) hides client windows the same way, as a view flag that changes no
   window record and ends on activation, a new window, any recovery action or the shell disconnecting.

Property tests for the geometry live in `crates/lege-window`, not here — the policy is testable without a
compositor, and that is the point of the split.

## Backends

Two, behind one trait. **winit** hosts the compositor inside an existing session: it is how the event loop
below is developed, exercised in CI and tested with no display hardware at all. **udev/KMS** is the real
one, via Smithay's `LibSeatSession`. The winit backend is not a toy — a compositor that can only be run by
logging into it is a compositor that cannot be tested.

## What the compositor must call, and when

The compositor owns one `WindowService<DbusIdentityCache>` and one `GovernorNotifier` on the render thread's
event loop, plus a `ShellStateBroadcaster` fed after every call below that can change what the shell should
show. None of the `lege-window` or `lege-comp-integration` calls below block: `WindowService` is pure Rust
over in-memory state, and every `lege-comp-integration` type hands work to a background tokio task.

| Compositor event | Call |
|---|---|
| Wayland client connects, first `xdg_toplevel` commit (map) | `peer_pid` from the client credentials; `WindowService::on_toplevel_mapped(peer_pid, client, claims, title, requested_geometry)`. `client` is `ClientKind::TrustedShell` for the one connection the compositor itself spawned as the shell, `ClientKind::Application` for every other connection — never `ClientKind::CompositorCore`, which is reserved for surfaces the compositor core creates directly with no Wayland client behind them at all (the lock screen, the recovery overlay). Every toplevel still lands on `Layer::Normal` regardless of `client` (a shell-owned about box is just a normal window), so this is purely honest bookkeeping, not a gate — see the method's doc comment. `claims` from `xdg_toplevel.app_id` / XWayland `WM_CLASS` / the sandbox app-id extension, whichever apply. This is also the session-restore path: a restored window maps with its saved geometry as `requested_geometry`, so it is validated against the topology at hand, never trusted blindly. |
| `xdg_toplevel.set_title` / XWayland `_NET_WM_NAME` change | `WindowService::set_title` |
| `xdg_toplevel` decoration negotiation completes (`zxdg_toplevel_decoration_v1` / lack of it) | `WindowService::set_decoration` — `ClientSide`/`Borderless` gets a compositor-painted recovery tab (`WindowService::recovery_affordance`, checked every time the recovery band would otherwise be drawn) |
| `xdg_toplevel.set_fullscreen` / `unset_fullscreen`, and the compositor's own modal-dialog tracking (`xdg_dialog_v1`, or transient-for heuristics for XWayland) | `WindowService::set_fullscreen` / `set_modal` |
| Toplevel destroyed (surface unmapped, client disconnects) | `WindowService::on_toplevel_destroyed`; `DbusIdentityCache::forget(peer_pid)` once no other live window shares that PID; if that was the instance's last window, `GovernorNotifier::forget_instance` |
| `lege_shell_v1.set_exclusive_zone` from the shell | Recompute that output's work area with `lege_window::work_area_for` over every live zone, set it with `Output::with_work_area`, and feed the whole topology back through `on_outputs_changed` — the same single entry point, so a dock appearing and a monitor being unplugged are the same kind of event and cannot diverge. Release the surface's zone when it is destroyed. Never validate the claim yourself: `Output::usable` already refuses one that would swallow the output or leave no room for a recovery band |
| Output added/removed/moved/rescaled, lid close, resume from suspend | Rebuild the `Vec<Output>` (logical rects + `scale_milli`) for every active output — **the only place `Output`/topology changes enter `lege-window`** — and call `WindowService::on_outputs_changed`; re-`configure` every `(WindowId, Rect)` it returns. This is the one-workspace enforcement point in the geometry sense: there is exactly one call site that changes the topology `WindowService` reasons about, so "one continuous scene across all outputs" cannot drift into per-output scene state anywhere else in the compositor. |
| Pointer button down on a window's decoration/recovery band | `WindowService::begin_drag(window_id, pointer_global)` — `pointer_global` in the one global logical coordinate space (`geometry::physical_to_logical` at the input boundary, once, using the output the pointer is currently over; never re-derive it per motion event) |
| Pointer motion during a drag | `WindowService::update_drag(pointer_global)`; move the surface to the returned `Rect` as-is — **do not** clamp or repair mid-drag, even if `on_outputs_changed` fires while dragging (it already skips the dragged window for exactly this reason) |
| Pointer button up ending a drag | `WindowService::finish_drag()`; apply the (possibly repaired) final `Rect` |
| A surface (or the shell, through `lege_shell_v1`) requests keyboard focus, and xdg-activation issued a token backed by recent input | Build an `ActivationGrant` (`had_recent_input_evidence` from the xdg-activation token's serial, not assumed; `token` a fresh, unguessable string per grant — `WindowService` enforces single-use per exact token string, so reusing one across unrelated activation requests would falsely collide) and call `WindowService::handle_command(WindowCommand::Activate{..}, CommandContext{trusted_shell, now_monotonic_ns})`; on success, send `xdg_toplevel.configure` with `activated` and call `GovernorNotifier::set_instance_state` for the newly- and previously-activated instances |
| Surface becomes visible/hidden (occlusion, minimized-to-shelf, workspace-irrelevant since there is only one) | `GovernorNotifier::set_instance_state(instance_id, foreground, visible)`, coalesced by the notifier itself — call it on every change, do not hand-roll debouncing at the call site |
| `lege_shell_v1` request from the shell connection (`close`, `move_to_visible_area`, `shelve`, `unshelve`, `escape_fullscreen_or_modal`, `show_all_windows`) | `WindowService::handle_command` with `CommandContext{trusted_shell: true, ..}` — **only** the one connection the compositor itself spawned as the shell may set `trusted_shell: true`; every other connection (there is no other way to reach this protocol) is a protocol violation, not a legitimate `false` case |
| Fixed recovery key combinations (`docs/architecture/accessibility.md`): show every window, escape fullscreen/modal, cycle the fallback switcher, go home | Call `WindowService::handle_command(WindowCommand::ShowAllWindows \| EscapeFullscreenOrModal \| RevealHome, CommandContext{trusted_shell: true, ..})` or `WindowService::cycle_focus` directly from the compositor's own keybinding handler — **not** through the wire protocol, and not gated on a shell connection existing at all; this is what makes them work with the shell dead. After a reveal starts or ends, `input::refocus_after_reveal` moves the keyboard to the Home Surface or back to the topmost visible window |
| `xdg_toplevel.set_maximized` / `unset_maximized` | `xdg_shell::apply_maximized` → `WindowService::handle_command(SetMaximized{..})`; the returned geometry (one output's `usable()`, or the repaired floating rect) goes out as the pending size + one configure. `on_outputs_changed` re-derives a maximized window from its output rather than repairing it |
| Anything that changed toplevel list, shelf contents or stacking order | `lege_comp_integration::snapshot(&service)` then `ShellStateBroadcaster::publish` — never encode/send `lege_shell_v1` events synchronously from the same call site that touched `WindowService` |

`Layer::SecureLock` and `Layer::Recovery` surfaces (the lock screen, the pressure overlay, the recovery
tab) are created by the compositor core calling `WindowService::set_layer(id, layer, ClientKind::CompositorCore)`
directly — no protocol request exists for them, by design (`interfaces/wayland/lege-recovery-v1.xml`).
