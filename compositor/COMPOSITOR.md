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
   A client window's geometry is its **frame**: the band sits *above* the client surface
   (`lege_window::client_area`), never over its pixels, and carries park, maximize and close controls
   (`chrome::layout::band_buttons`); the surface is mapped at, and configured to, `WindowService::client_area`
   — the one place the band is subtracted. A fullscreen window and every shell surface have no frame.
3. **Identity** — `lege-comp-integration::DbusIdentityCache`; sanitized identity into the toplevel-info path.
4. **Typed layers** (`lege_window::Layer`) replacing arbitrary always-on-top; `LayerStack` is the sole
   z-order authority — the compositor's own scene-graph stacking must be driven from `WindowService::stacking_order`,
   never maintained independently. A **window group is one thing to stack**: activating any member raises
   every member together (`LayerStack::raise_many`), keeping their relative order with the activated one on
   top, so nothing outside the group is left stacked between two of its windows. That raise changes rank and
   nothing else, which is why grouping cannot disturb the recovery invariant.
   The stacking order governs the *frame's element order* too (`src/scene.rs`): a window's
   band and grip are part of that window, emitted immediately above its own surface and below every window in
   front of it, so a higher window occludes a lower window's title exactly as it occludes its content. Only
   whole-output chrome — the lock screen, the pressure overlay, the fallback switcher, the "show every window"
   scatter — sits above every window. Collecting chrome as one flat list ahead of all window content instead
   painted a buried window's title over the translucent terminal in front of it, at full opacity.
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
| `xdg_toplevel` decoration negotiation completes (`zxdg_toplevel_decoration_v1` / lack of it) | Answer `ServerSide` — the frame is the compositor's — unless the client insists on `ClientSide`; `WindowService::set_decoration` either way. `ClientSide`/`Borderless` gets the compositor-painted recovery tab colour (`WindowService::recovery_affordance`), still above the client, so a client that draws its own title bar shows two rather than losing one under the band |
| A mapped client commits a size other than the one it was last given, having acknowledged that one (`ToplevelSurface::with_committed_state`) | The frame follows: `WindowService::set_geometry(frame_around(committed size at the current client position))`. Never mid-resize, never for a maximized or fullscreen window — a commit that still carries the older size is the client catching up, not choosing |
| `xdg_toplevel.set_fullscreen` / `unset_fullscreen`, and the compositor's own modal-dialog tracking (`xdg_dialog_v1`, or transient-for heuristics for XWayland) | `WindowService::set_fullscreen` / `set_modal` |
| Toplevel destroyed (surface unmapped, client disconnects) | `WindowService::on_toplevel_destroyed`; `DbusIdentityCache::forget(peer_pid)` once no other live window shares that PID; if that was the instance's last window, `GovernorNotifier::forget_instance` |
| `lege_shell_v1.set_exclusive_zone` from the shell | Recompute that output's work area with `lege_window::work_area_for` over every live zone, set it with `Output::with_work_area`, and feed the whole topology back through `on_outputs_changed` — the same single entry point, so a dock appearing and a monitor being unplugged are the same kind of event and cannot diverge. Release the surface's zone when it is destroyed. Never validate the claim yourself: `Output::usable` already refuses one that would swallow the output or leave no room for a recovery band |
| Output added/removed/moved/rescaled, lid close, resume from suspend | Rebuild the `Vec<Output>` (logical rects + `scale_milli`) for every active output — **the only place `Output`/topology changes enter `lege-window`** — and call `WindowService::on_outputs_changed`; re-`configure` every `(WindowId, Rect)` it returns. This is the one-workspace enforcement point in the geometry sense: there is exactly one call site that changes the topology `WindowService` reasons about, so "one continuous scene across all outputs" cannot drift into per-output scene state anywhere else in the compositor. |
| Pointer button down on a window's decoration/recovery band | `WindowService::begin_drag(window_id, pointer_global)` — `pointer_global` in the one global logical coordinate space (`geometry::physical_to_logical` at the input boundary, once, using the output the pointer is currently over; never re-derive it per motion event). On one of the band's controls instead (`chrome::layout::band_button_at`, the same rectangles chrome drew): close is `WindowCommand::Close` then `xdg_toplevel.close`, park is `WindowCommand::Shelve` with the keyboard handed on, maximize toggles `WindowCommand::SetMaximized`; the press is consumed and no client sees it |
| Pointer button down on the corner grip, or within `RESIZE_MARGIN` *outside* a framed window's edge (`lege_window::resize_edges_at`) | `WindowService::begin_resize(window_id, pointer_global, edges)`; the window on top takes its edge zone before the window beneath sees the press. Refused for a maximized window, and the press falls through to whatever is under it |
| Pointer motion during a drag | `WindowService::update_drag(pointer_global)`; move the surface to the returned `Rect` as-is — **do not** clamp or repair mid-drag, even if `on_outputs_changed` fires while dragging (it already skips the dragged window for exactly this reason) |
| Pointer button up ending a drag | `WindowService::finish_drag()`; apply the (possibly repaired) final `Rect`. Then, for a group drag, `WindowService::move_group(anchor, final_rect)` — the invariant is enforced once, at the end, for every member at once |
| Pointer motion during a **band** drag, over another window's band | `window_groups::band_drop_target` (the stacking order, top-first, skipping the dragged window): that window becomes `LegeCompState::drop_target` and its band draws as a drop target. Only a band is a drop target — an ordinary drop anywhere else, including on top of another window, still just moves a window (planning doc §2 rule 6) |
| Pointer button up ending a band drag over another window's band | `window_groups::group_windows(target, dragged)` — group and *place* (`WindowCommand::Group`), after the drag has settled, so `place_beside` starts from where the window was let go. The target is the anchor, so its group's identity survives a merge |
| A band drag on a grouped window | The whole group moves, at the offsets recorded when the gesture began (`window_groups::drag_followers`); `Shift` held at press time moves that member alone. Followers are moved without repair during the drag, exactly like the dragged window, and repaired once at the end |
| Drawing a window's chrome (`chrome::collect_window_chrome`) | A grouped window's band carries a 4-logical-pixel stripe in the group's accent and the group's name before the title, and the group's **bottom-most visible member** draws the outline: four 2-logical-pixel bars in the accent, just outside the union of the group's unshelved windows (`WindowService::group_outline`, `chrome::layout::outline_bars`). Outside the union so it never covers a member's own pixels; window chrome rather than output chrome so it survives the shell dying *and* so an unrelated window stacked above the group occludes it, exactly as it occludes that member's band |
| A surface (or the shell, through `lege_shell_v1`) requests keyboard focus, and xdg-activation issued a token backed by recent input | Build an `ActivationGrant` (`had_recent_input_evidence` from the xdg-activation token's serial, not assumed; `token` a fresh, unguessable string per grant — `WindowService` enforces single-use per exact token string, so reusing one across unrelated activation requests would falsely collide) and call `WindowService::handle_command(WindowCommand::Activate{..}, CommandContext{trusted_shell, now_monotonic_ns})`; on success, send `xdg_toplevel.configure` with `activated` and call `GovernorNotifier::set_instance_state` for the newly- and previously-activated instances |
| Surface becomes visible/hidden (occlusion, minimized-to-shelf, workspace-irrelevant since there is only one) | `GovernorNotifier::set_instance_state(instance_id, foreground, visible)`, coalesced by the notifier itself — call it on every change, do not hand-roll debouncing at the call site |
| `lege_shell_v1` request from the shell connection (`close`, `move_to_visible_area`, `shelve`, `unshelve`, `escape_fullscreen_or_modal`, `show_all_windows`) | `WindowService::handle_command` with `CommandContext{trusted_shell: true, ..}` — **only** the one connection the compositor itself spawned as the shell may set `trusted_shell: true`; every other connection (there is no other way to reach this protocol) is a protocol violation, not a legitimate `false` case |
| `lege_shell_toplevel_v1` group request (`join_group`, `leave_group`, `activate_group`, `shelve_group`, `unshelve_group`, `set_group_name`) | `src/window_groups.rs` — the one place every route into the group model goes, so the chord, the protocol request and the band drop cannot drift. `join_group(other)` makes `other` the anchor whose group identity survives a merge, and *places* the joining window beside it (`WindowCommand::Group`, planning doc §2 rule 6); `activate_group` takes the same trusted `ActivationGrant` as `activate`, because a group is not a way around the focus-stealing checks. A request naming a window or group that no longer exists is ignored, not an error: the shell's view arrives by event and is allowed to be one frame stale |
| Anything that changes group membership, a group's name, or which window is in one | `lege_shell_toplevel_v1.group` after `identity` on every sync (`protocols_shell::send_group`): `group_id` 0 for a loose window, otherwise the id, the name and the accent every surface draws that group with. The compositor is the sole source of truth for groups; this event is the shell's only view of them |
| `lege_shell_manager_v1.set_terminal` | Store the canonical app id (`ShellProtocolState::terminal_app_id`) for `Super+grave` (§4.6). Kept when the shell disconnects — the chord still toggles an existing terminal window with the shell dead; only *launching* one needs the shell back |
| `Super+grave`, the drop-down terminal | "The terminal" is the topmost client window whose **resolved** identity (`CachedIdentity`, from the cgroup — never the client's own `app_id`) is that stored id. Shelved or unfocused: unshelve and activate. Focused: `WindowCommand::Shelve` and hand the keyboard back to `previous_focus`, then to the topmost window still on screen, then to Home. Absent: `lege_shell_toplevel_v1`'s manager gets `terminal_requested`, once per press, and the shell launches the provider through the ordinary launch chain. **Never closed**: an accidental press must cost nothing |
| Window-group chords: `Alt+Tab`/`Alt+Shift+Tab`, `Super+Tab`/`Super+Shift+Tab`, `Super+G`, `Super+Shift+G` | `Alt+Tab` is `WindowService::cycle_within_group` — inside the focused window's group, with the loose set counting as a group, so with no group anywhere it is the switcher it has always been. `Super+Tab` is `cycle_groups`, landing on each group's `last_active` member. `Super+G` groups the focused window with the previously focused one (`LegeCompState::previous_focus`, tracked in `input::activate_window` because activation is a flag and the previous holder is gone by the time anything could ask) and sets it down beside it; `Super+Shift+G` takes it out. **No `Ctrl+Tab`**: it is the tab chord in every browser, terminal and editor, and a compositor-reserved chord wins (planning doc §2.1). Shift+Tab arrives as `ISO_Left_Tab`, which both switchers accept |
| Fixed recovery key combinations (`docs/architecture/accessibility.md`): show every window, escape fullscreen/modal, cycle the fallback switcher, go home | Call `WindowService::handle_command(WindowCommand::ShowAllWindows \| EscapeFullscreenOrModal \| RevealHome, CommandContext{trusted_shell: true, ..})` or `WindowService::cycle_focus` directly from the compositor's own keybinding handler — **not** through the wire protocol, and not gated on a shell connection existing at all; this is what makes them work with the shell dead. After a reveal starts or ends, `input::refocus_after_reveal` moves the keyboard to the Home Surface or back to the topmost visible window |
| `xdg_toplevel.set_maximized` / `unset_maximized` | `xdg_shell::apply_maximized` → `WindowService::handle_command(SetMaximized{..})`; the returned geometry (one output's `usable()`, or the repaired floating rect) goes out as the pending size + one configure. `on_outputs_changed` re-derives a maximized window from its output rather than repairing it |
| Any input event at all — keyboard, pointer, touch — before it is dispatched anywhere | `LegeCompState::note_activity` (`src/idle.rs`): wake a screen that idle-blanking turned off, and restart the countdown. The compositor is the session's **only** honest idle source — logind's `IdleHint` is driven by nothing here and no client can see the seat — so this is where "nothing has happened for a while" is known rather than guessed. The delay itself is `org.legeos.Power1`'s `screen_off_after`, which the shell reads and relays through `lege_shell_v1.set_idle_timeout` (0 = never); a live `zwp_idle_inhibit_manager_v1` inhibitor suspends the countdown entirely. On expiry the backend turns every output off (DPMS off on udev, a black frame nested) and the next input turns them back on. Turning the screen off is **not** locking it and not dimming it: `Power1`'s `lock_after` and `dim_after` are separate intervals, neither is enforced, and `src/idle.rs`'s module doc says where each would attach |
| Anything that changed toplevel list, shelf contents or stacking order | `lege_comp_integration::snapshot(&service)` then `ShellStateBroadcaster::publish` — never encode/send `lege_shell_v1` events synchronously from the same call site that touched `WindowService` |

`Layer::SecureLock` and `Layer::Recovery` surfaces (the lock screen, the pressure overlay, the recovery
tab) are created by the compositor core calling `WindowService::set_layer(id, layer, ClientKind::CompositorCore)`
directly — no protocol request exists for them, by design (`interfaces/wayland/lege-recovery-v1.xml`).
