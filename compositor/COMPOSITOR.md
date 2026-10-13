# templar-comp — the compositor's contract with `templar-window`

`templar-comp` is written against **Smithay** and is an ordinary member of this workspace — not a fork of
`cosmic-comp` (ADR-0012). Smithay supplies the display server: DRM/KMS with atomic modesetting and direct
scanout, EGL/GBM, libinput, libseat, XWayland's window-manager role, and server-side implementations of
every protocol below. What Smithay deliberately does not supply is *window-management policy* — it has no
workspace, tiling or placement concept at all — and that is exactly the gap `crates/templar-window` fills.

So this document is not a patch list. It is the contract: what the compositor must detect, and what it
must call. The seven properties it must uphold are:

1. **One workspace as an invariant** — exactly one workspace exposed, including to XWayland interfaces;
   multiple monitors form one continuous scene; windows belong to the scene, not to a hidden desktop.
2. **`templar-window` bridge** — `WindowService` owns a semantic record for every live toplevel:
   role, protocol mapped state, activation recency, geometry, identity and presentation flags.
   Those fields do not come from `LayerStack`: being an application, being switchable and being
   taskbar-visible are independent of where the window currently draws. `repair_window` runs
   after every drag, output/scale change, resume and geometry request; the tag strip (and, for
   client-side-decorated/borderless windows, the compositor-painted recovery tab colour) is compositor-drawn.
   A client window's geometry is its **frame**: the tag strip sits *below* the client surface
   (`templar_window::client_area`), never over its pixels, and carries a **name tag** at its left (the title,
   the group stripe; grab to move, drop target for grouping — `chrome::layout::name_tag`) and contiguous
   **control tags** at its right (park, maximize, close — `chrome::layout::control_tags`); the surface is
   mapped at, and configured to, `WindowService::client_area` — the one place the strip is subtracted.
   Every shell surface, and every client window **filling its output** — maximized or fullscreen
   (`WindowRecord::covers_screen`) — has no frame (`WindowService::has_frame`). The frame is a strip
   *below* the client area, so on a window filling the screen it is a black row across the bottom of
   the display, permanently, in exchange for chrome nobody asked to see; such a window shows its tag
   and its controls over itself on a dwell instead ("A screen-filling window's own chrome", below).
   There is no corner grip: resize is the margin ring outside every edge and corner.
3. **Identity** — `templar-comp-integration::DbusIdentityCache`; sanitized identity into the toplevel-info path.
   A **click on a name tag pins the window** (`WindowService::toggle_always_on_top`) and its name is
   then drawn in italic; a press that moves is the drag it always was (`input::CLICK_SLOP` separates
   the two on release). A pin is a sort key, not a layer: the window stays on `Layer::Normal` and is
   *ordered* as if it were on `AppOnTop`, so `layer_of` — and everything written in terms of it —
   reads a pinned window as the ordinary window it is.
4. **Typed layers** (`templar_window::Layer`) replacing arbitrary always-on-top; `LayerStack` is the sole
   z-order authority. A layer's **discriminant is its wire value and its `depth()` is its place on the
   screen**, and since `Layer::Presentation` (Taskfield's chrome, which had to be appended on the wire but
   belongs between `SystemModal` and `ShellOverlay`) those are different numbers: order layers with `Ord`
   or `depth()`, never with `as u8` — the compositor's own scene-graph stacking must be driven from `WindowService::stacking_order`,
   never maintained independently. A **window group is one thing to stack**: activating any member raises
   every member together (`LayerStack::raise_many`), keeping their relative order with the activated one on
   top, so nothing outside the group is left stacked between two of its windows. That raise changes rank and
   nothing else, which is why grouping cannot disturb the recovery invariant.
   The stacking order governs the *frame's element order* too (`src/scene.rs`): a window's
   tags are part of that window, emitted immediately above its own surface and below every window in
   front of it, so a higher window occludes a lower window's title exactly as it occludes its content. Only
   whole-output chrome — the lock screen, the pressure overlay, the fallback switcher, the window overview's
   chrome — sits above every window. Collecting chrome as one flat list ahead of all window content instead
   painted a buried window's title over the translucent terminal in front of it, at full opacity.
5. **Recovery** — `templar_recovery_v1` pressure overlay + the window overview (Taskfield, shelved windows
   included) + minimal fallback switcher + fullscreen/modal escape + "go home", all bound to fixed key
   combinations and calling straight into `WindowService`, so all five work without the shell;
   `ext-session-lock-v1` for the lock screen. What is being presented is **one value**,
   `WindowService::presentation()` (`Normal` | `HomeReveal` | `Taskfield`), and it carries the invariant
   that replaces every fullscreen special case: **a fullscreen window covers passive chrome only while
   `Normal`.** Passive chrome (the island, Home, tags) yields to fullscreen; explicit navigation
   (Taskfield, the lock screen, recovery) yields to nothing. `is_visible`, `home_is_covered` and the
   `stacking_order` exception each consult that one value, so a new mode cannot be added without
   answering how it relates to the others. In Taskfield the compositor draws each window's surface tree
   at `WindowService::presentation_rect(id)` instead of its real rectangle: **no `xdg_toplevel.configure`
   is sent**, clients keep painting at their real sizes, and leaving is a render change rather than a
   geometry change — which is why it works from fullscreen, Wayland and XWayland alike, every time. The
   layout comes from `templar_window::taskfield::layout` and is thrown away on exit: Taskfield is a
   *view*, HomeGarden is the *place*, and no card position is ever stored against a window.
6. **Private shell protocol** `templar_shell_v2`; the shell is a separate, restartable process.
7. **Explicit minimisation** to the window shelf — and a shelved window is *gone from the screen*:
   `WindowService::is_visible` is the one predicate `Space` mirroring (`src/space_sync.rs`), hit-testing
   and chrome consult, so a hidden window neither renders, takes clicks nor paints a band. "Go home"
   (`WindowCommand::RevealHome`) hides client windows the same way, as a view flag that changes no
   window record and ends on activation, a new window, any recovery action or the shell disconnecting.
   The **Home surface** is the other side of that predicate: it is the bottom layer and is visible by
   default, hidden only while a visible fullscreen or maximized client window covers its output
   (`WindowService::home_is_covered`, per output) and shown regardless during a reveal. It used to be
   hidden unless revealed, which made a fresh login a black screen until the user pressed Super; the
   accepted cost of the current rule is that a translucent client window blends with Home.
   `WindowService::stacking_order` carries the one exception to the fixed layer order: while a visible
   fullscreen client window's output is not being peeked, that window sorts **above** `ShellOverlay` and
   `Notification` — otherwise the bar would draw over every fullscreen video, since `ShellOverlay` is
   above `Normal` by design — and never above `SystemModal`, `Recovery` or `SecureLock`. The `Layer`
   discriminants are untouched; the exception is a sort key, and suspending it for one output
   (`set_peek_output`) is half of the bottom-edge peek — the other half is `bar_peeked`, which tells
   the shell the edge was asked for at all. Fullscreen only, deliberately: promoting a *maximized*
   window over `ShellOverlay` too would put the launcher, the switcher and every context menu behind
   the window the user is working in, which is the common case rather than the rare one. Because
   `stacking_order` is the single z-order authority, the render order, the hit-test mirror and the
   shell's snapshot all follow it.

Property tests for the geometry live in `crates/templar-window`, not here — the policy is testable without a
compositor, and that is the point of the split.

## Backends

Two, behind one trait. **winit** hosts the compositor inside an existing session: it is how the event loop
below is developed, exercised in CI and tested with no display hardware at all. **udev/KMS** is the real
one, via Smithay's `LibSeatSession`. The winit backend is not a toy — a compositor that can only be run by
logging into it is a compositor that cannot be tested.

## Watchdog

A frozen display with nothing in the journal is undiagnosable, so both backends start `src/watchdog.rs`
before anything else: a `comp-watchdog` thread pings a calloop source on the main loop every 5 s (one
wakeup, two atomic stores, no lock). A probe unanswered for 5 s is logged as `compositor event loop
stalled` with the main thread's `/proc` state, wchan and syscall plus every other thread not asleep,
repeated at 15, 30 and 60 s and then every minute; the answer is logged once as `compositor event loop
recovered`. On udev every CRTC's queued page flip is tracked too, and one outstanding after 2 s is
`page flip overdue` (an idle blank or a VT switch cancels the flip rather than raising it).
`TEMPLAR_COMP_WATCHDOG_ABORT_AFTER_SECS=n` turns an n-second stall into an abort for a core dump; it is
off by default. These lines, and the idle blank's `outputs blanked after idle timeout` / `outputs woken
from idle blank`, are what `templar doctor freeze` reads.

The main loop's repaint watchdog also **recovers** a pending flip older than 2 s. It calls
`DrmCompositor::clear` synchronously to disable every plane and clear Smithay's pending frame;
only after that succeeds does it reset state/damage and queue a fresh render. A failed clear
retains ownership of the pending buffer and retries on the next watchdog pass. A timestamp fence
rejects delayed events from before the clear, so an old completion cannot free the replacement
frame. There is no synthetic `frame_submitted` and no attempt to reuse a buffer DRM still owns.
Blanked outputs and paused sessions are excluded. `TEMPLAROS_TEST_DROP_PAGEFLIP_ONCE=1` is an
explicit hardware-lab fault injection: each newly created CRTC drops its first completion and
must log `overdue page flip recovered` before continuing. Never enable it in an installed session.
The state/fence regression is `cargo test -p templar-comp flip_recovery`; hardware smoothness
and real-driver clear behavior remain separate DRM observations.

For automated nested input, `--backend winit --test-input-socket /private/input.sock` creates
an opt-in Unix datagram control socket (0600, parent directory 0700). No public protocol global
or host input device exists. `scripts/nested-input.py` sends keyboard evdev down/up, pointer
motion/button, ASCII typing and read-only snapshots through the same compositor input dispatch
used by winit and udev. The option is refused on udev. Snapshots expose the compositor's switcher
selection, activation/shelf state and shell-published Home card rectangles for behavioral journeys.
The `motion X Y` producer accepts physical canvas pixels, converts them to normalized
`AbsolutePositionEvent` coordinates, and uses the production viewport transform; it never
directly assigns pointer/focus state. Snapshots additionally expose transient-parent IDs,
native fullscreen/geometry, actual peer PID and stacking for owned parent-handle readback.
The endpoint remains private and nested-only; none of these fields is a new public protocol.

Absolute input uses `Backend::absolute_input_transform`: physical backends keep the output's
device transform, while winit uses normal displayed-canvas axes. Its `Flipped180` corrects the
GLES framebuffer origin, not the host pointer. Native owned Xvfb/winit readback on 2026-10-06
observed actual host motion `(842, 653)` at seat `(842, 653)` on a 1200×800 canvas, and private
native motion `(200, 137)` at seat `(200, 137)`, without inverse fixture coordinates. The
compositor and its adopted XWayland child were reaped. This establishes nested coordinate
mapping, not physical panel rotation, client-button acceptance or hotplug.

Popup input resolves the actual surface tree before geometry-only window chrome and activation.
An `xdg_popup` or its subsurface belongs to its existing tracked toplevel, even when a menu row
extends beyond that toplevel's geometry. Clicking that row neither raises the window behind it
nor starts its resize affordance. On 2026-10-06, real Firefox Flatpak/LXQt HTML Save As reproduced
the old picker focus loss; the corrected native host row click preserved active picker stacking
and selected HTML-only before saving the exact 69-byte fixture. This is private nested proof,
not installed-session or physical-output acceptance.


## What the compositor must call, and when

The compositor owns one `WindowService<DbusIdentityCache>` and one `GovernorNotifier` on the render thread's
event loop, plus a `ShellStateBroadcaster` fed after every call below that can change what the shell should
show. None of the `templar-window` or `templar-comp-integration` calls below block: `WindowService` is pure Rust
over in-memory state, and every `templar-comp-integration` type hands work to a background tokio task.

| Compositor event | Call |
|---|---|
| Wayland client connects, first `xdg_toplevel` commit (map) | `peer_pid` from the client credentials; `WindowService::on_toplevel_mapped(peer_pid, client, claims, title, requested_geometry)`. `client` is `ClientKind::TrustedShell` for the one connection the compositor itself spawned as the shell, `ClientKind::Application` for every other connection — never `ClientKind::CompositorCore`, which is reserved for surfaces the compositor core creates directly with no Wayland client behind them at all (the lock screen, the recovery overlay). Every toplevel still lands on `Layer::Normal` regardless of `client` (a shell-owned about box is just a normal window), so this is purely honest bookkeeping, not a gate — see the method's doc comment. `claims` from `xdg_toplevel.app_id` / XWayland `WM_CLASS` / the sandbox app-id extension, whichever apply. This is also the session-restore path: a restored window maps with its saved geometry as `requested_geometry`, so it is validated against the topology at hand, never trusted blindly.. Then, if `WindowService::should_activate_on_map` holds — a client window, and no visible activated **fullscreen** client window on the same output — `input::activate_window`: the window a person just opened is the window they meant to use, and nothing else in the map sequence activates one (a launch through `Launcher1` carries no xdg-activation token), so without it an application opened from the Home search bar maps unfocused under a Home surface that still owns the keyboard. |
| `xdg_toplevel.set_title` / XWayland `_NET_WM_NAME` change | `WindowService::set_title` |
| `xdg_toplevel` decoration negotiation completes (`zxdg_toplevel_decoration_v1` / lack of it) | Always answer `ServerSide` — the frame is the compositor's; a client asking for `ClientSide` is configured server-side anyway (`xdg_shell::granted_mode`), so no application themes its own chrome. `WindowService::set_decoration` records the grant. `ClientSide`/`Borderless` gets the compositor-painted recovery tab colour (`WindowService::recovery_affordance`), still below the client, so a client that draws its own title bar keeps it and gains the compositor's tags underneath |
| A mapped client commits a size other than the one it was last given, having acknowledged that one (`ToplevelSurface::with_committed_state`) | The frame follows: `WindowService::set_geometry(frame_around(committed size at the current client position))`. Never mid-resize, never for a maximized or fullscreen window — a commit that still carries the older size is the client catching up, not choosing |
| `xdg_toplevel.set_fullscreen` / `unset_fullscreen`, and the compositor's own modal-dialog tracking (`xdg_dialog_v1`, or transient-for heuristics for XWayland) | `xdg_shell::apply_fullscreen` (both shells, and the escape route) → `WindowService::set_fullscreen`, which gives the window the output's **`logical`** rectangle — not `usable()`: genuinely full screen means over the bar's exclusive zone, not politely beside it — and remembers the geometry to come back to in a slot of its own (`fullscreen_restore`, never `restore_geometry`, so maximize-then-fullscreen restores both correctly). That size is then configured onto the client; a fullscreen window that is only a cleared flag is a window that has merely lost its frame. `on_outputs_changed` re-derives it from the nearest output and never *repairs* it: `is_recoverable` measures a tag strip against `usable()` and a fullscreen window has no strip, so repairing it is exactly what would pull it off the full output. Its recovery route is the escape chord, which restores through the same slot. `set_modal` is unchanged |
| `xdg_toplevel.set_parent` and public standard `zxdg_exporter_v2` / `zxdg_importer_v2` | `src/xdg_foreign.rs` validates the exporter's own xdg-toplevel and importer's own child, rejects cycles, and records presentation-only `WindowService::set_transient_parent`. All concurrent imports/children are tracked; export object, exported surface or exporting connection loss sends `destroyed` to every import and detaches children. Import destruction detaches only children still bound through that import. Ordinary child dialogs sort above their ordinary parent, including fullscreen, without changing their assigned layer, identity or focus. Protected shell/lock/overlay roles never lend authority. Invalid/expired handles yield an independent visible portal window with a diagnostic. |
| Pointer comes to *rest* (`input::DWELL_REST_SLOP`) within `input::PEEK_BAND` of the bottom edge of an output that has a visible client window filling it — the bottom-left corner included — for `input::DWELL_MS` (`input::update_dwell`), **while the gesture gate is disabled** (`set_gesture_gate` `enabled` 0 — the default, and what a disconnected shell leaves) | `WindowService::set_peek_output(Some(output))`, then `space_sync::sync_after_change`, which sends `templar_shell_manager_v2.bar_peeked`. That corner carries the one thing on screen that says the route exists: `chrome::collect_peek_affordance` paints `input::peek_affordance` — `input::PEEK_AFFORDANCE` logical pixels tall, in the recovery tab's colour — whenever a window fills the output, and nothing at all otherwise, because on an ordinary desktop the island is already there and a permanent mark under it would be furniture for a route nobody needs. Compositor-drawn for the same reason the recovery band is: under a fullscreen window the shell is not on screen to draw anything. The peek must be the compositor's rule: a window filling the screen owns the pointer there, so the shell never learns the edge was reached — which is why the *event* matters as much as the stacking. Nothing about the bar surface changes — it stays mapped and keeps its zone, so neither the peek nor the fullscreen entry reflows a window — only `stacking_order` does, and only for a fullscreen window. Ends (`set_peek_output(None)`) at once, with no dwell, when the pointer is neither in the band nor on a visible `ShellOverlay` surface (`input::holding_peek` — the zone is 0, so the strip the shell *reserves* is nothing to hold on to), or when the window enters or leaves fullscreen or maximized |
| Pointer comes to rest in the same peek band **again**, with that output's island already raised (gesture gate disabled, as above) | `WindowService::enter_taskfield(TaskFilter::All)` — Peek's promotion, and `taskfield.open`'s pointer route (`window_groups::POINTER_GESTURES`). One corner and one gesture at two depths: reach once for "what is running", reach again for "show me", so the pointer never has to learn a second place and a fullscreen window has a complete route out that does not go through the keyboard. `set_peek_output` answering `false` *is* "the island is already up for this output", so the second dwell is recognisable without a second piece of state |
| Pointer comes to rest for `input::DWELL_MS` in the `input::CORNER_SQUARE` square at an output's **top-left** (`input::update_dwell`) | `TemplarCompState::reveal_home(RevealHome::Show)` — the same route as `Super+H`, so the overview and switcher are torn down, the `Space` mirror follows and the keyboard moves somewhere visible. It was the *bottom*-left corner until Peek took that one: the peek affordance has to sit where the island starts, or the pointer travels away from the thing it is reaching for, and two dwells cannot share a square. Home loses nothing by moving — its primary route is the bare `Super` tap, of which this is only the pointer spelling — and it gains a corner no window's chrome ever lands in, so unlike the other two this dwell needs no "already showing" guard. Suppressed while the session is locked, while a pointer button is held (smithay's `ClickGrab` makes `is_grabbed` true for the whole press, so a window thrown at the corner is not a request for Home), and while a visible `ShellOverlay` surface is under the pointer. Leaving the corner does **not** hide Home again: revealing is a deliberate act and `set_home_revealed`'s own rules end it |
| Pointer motion (relative or absolute) while the gesture gate is **enabled** (`templar_shell_manager_v2.set_gesture_gate`) | `input::update_gesture_gate` feeds `gesture_gate::GestureGate` the segment from the previous to the new position, global logical, on the output under the new one — after `update_dwell`, never instead of delivery: the gate is compositor state, not a surface, takes no input and installs no grab. The gate is `width` × `height` logical pixels centred on the output's bottom edge. **Arm**: the segment enters it (any edge, or a single segment jumping across it — Liang–Barsky clip, not a point test) having been above `top − gesture_gate::ABOVE_MARGIN` within `ABOVE_MEMORY_MS`, not starting inside it, not ending above it, no button held (`pointer.is_grabbed()`), and the topmost visible client window under the pointer's resolved app id not in `exceptions`. **Open**: once `COMMIT_DISTANCE` sideways from the entry point, whatever it did vertically; the side is latched. **Disarm**: rising `RISE_CANCEL` above the top, a button, `ARM_TIMEOUT_MS` uncommitted, another output. After any close (and any disarm but rising) the pointer must be above the gate again before it can arm. Opening sets `WindowService::set_peek_output(Some(output))` + `space_sync::sync_after_change` (the rail's peek — `update_dwell` leaves it alone), sends `rail(output, side)` then `rail_pointer`, and every further motion sends `rail_pointer` in that output's local logical coordinates. `tracing::debug!` records arm, open, disarm (with its reason) and close (with its reason). With `learning`, `chrome::collect_gesture_gate` outlines the gate on every output, faint, in the focus accent while armed or open there. |
| An open rail ends: the shell's `close_rail`, a pointer press whose target (`surface_under`) is not the trusted shell's, Escape (still forwarded), the session locking, Taskfield entering, `set_gesture_gate` disabling it, the shell's manager being destroyed (which also resets the gate to disabled) | `TemplarCompState::close_rail` — the one path: lowers the peek if it is the rail's, sends `rail("", none)`, and puts the recognizer back in its needs-to-be-above state. A press on any shell surface never closes it; whether acting on the rail does is the shell's call. |
| A live Wayland toplevel detaches its buffer, or XWayland reports `unmapped_window` | `WindowIndex::mark_unmapped` and `WindowService::set_mapped(false)`. Keep the `WindowId`, identity, geometry, flags and activation serial; remove only its Smithay element from `Space`, exclude it from switching/taskbar snapshots, and move keyboard focus to the next semantic-MRU application (or Home). A later buffer attach / X11 map calls `set_mapped(true)` and reuses that same record. |
| Toplevel destroyed (xdg role destroyed or XWayland `destroyed_window`; client disconnects) | `WindowIndex::remove` and `WindowService::on_toplevel_destroyed`; this, not a temporary unmap, is the lifecycle boundary that spends the stable `WindowId`. `DbusIdentityCache::forget(peer_pid)` once no other live window shares that PID; if that was the instance's last window, `GovernorNotifier::forget_instance` |
| `templar_shell_v2.set_exclusive_zone` from the shell | Recompute that output's work area with `templar_window::work_area_for` over every live zone, set it with `Output::with_work_area`, and feed the whole topology back through `on_outputs_changed` — the same single entry point, so a dock appearing and a monitor being unplugged are the same kind of event and cannot diverge. Release the surface's zone when it is destroyed — and **only** then: a claim of size 0 is a live claim that anchors the surface to its edge and reserves nothing, so shrinking a bar to a hint must not unplace it. Never validate the claim yourself: `Output::usable` already refuses one that would swallow the output or leave no room for a window's tag strip. A bottom zone also takes that output's maximized windows' frames away (`WindowService::has_frame`): their controls are the shell's bar's, and the compositor draws their tags again the moment nothing is reserved there (a size-0 claim, or no claim at all). The zone is a floor on the surface's thickness and *nothing at all* about its extent along the edge: `shell_surface_placement` anchors the surface flush to its edge at the claim's `Alignment` along it — `Start` unless `set_edge_alignment` said otherwise, which is placement and never reservation — and gives it its own committed width (or height), clamped to the output. A chip strip narrower than the screen therefore reserves exactly what a full-width row reserves, and growing one into the other reflows nothing — the same reason the thickness is a floor. It also means the compositor never configures a shell surface to the output's extent, so a shell that wants the full edge supplies that number itself |
| `templar_shell_v2.set_edge_alignment` from the shell | Record it against the surface and, if it already holds a claim on that edge, put it on the claim and re-place through `recompute_outputs` — the same one entry point a zone change takes. The two requests are unordered, so an alignment for an edge the surface has not claimed yet is kept until a claim on that edge arrives; one for an edge it never claims is never applied. `Start` is what a surface that never sends it gets. Alignment moves a surface *along* its edge and changes no reservation, so a centred island reflows nothing on the way — and centring it is what clears the bottom-left corner the hot corner owns |
| Output added/removed/moved/rescaled, lid close, resume from suspend | Rebuild the `Vec<Output>` (logical rects + `scale_milli`) for every active output — **the only place `Output`/topology changes enter `templar-window`** — and call `WindowService::on_outputs_changed`; re-`configure` every `(WindowId, Rect)` it returns. This is the one-workspace enforcement point in the geometry sense: there is exactly one call site that changes the topology `WindowService` reasons about, so "one continuous scene across all outputs" cannot drift into per-output scene state anywhere else in the compositor. |
| Pointer button down on a window's name tag | `WindowService::begin_drag(window_id, pointer_global)` — `pointer_global` in the one global logical coordinate space (`geometry::physical_to_logical` at the input boundary, once, using the output the pointer is currently over; never re-derive it per motion event). On one of its control tags instead (`chrome::layout::control_tag_at`, the same rectangles chrome drew): close is `WindowCommand::Close` then `xdg_toplevel.close`, park is `WindowCommand::Shelve` with the keyboard handed on, maximize toggles `WindowCommand::SetMaximized`; the press is consumed and no client sees it. In the gap between the two tags there are no pixels of this window, and the press falls through to whatever is beneath |
| Pointer button down within `RESIZE_MARGIN` *outside* a framed window's edge, any edge or corner (`templar_window::resize_edges_at`) | `WindowService::begin_resize(window_id, pointer_global, edges)`; the window on top takes its edge zone before the window beneath sees the press. Refused for a maximized window, and the press falls through to whatever is under it |
| Pointer motion during a drag | `WindowService::update_drag(pointer_global)`; move the surface to the returned `Rect` as-is — **do not** clamp or repair mid-drag, even if `on_outputs_changed` fires while dragging (it already skips the dragged window for exactly this reason) |
| Pointer button up ending a drag | `WindowService::finish_drag()`; apply the (possibly repaired) final `Rect`. Then, for a group drag, `WindowService::move_group(anchor, final_rect)` — the invariant is enforced once, at the end, for every member at once |
| Pointer motion during a **name-tag** drag, over another window's name tag | `window_groups::tag_drop_target` (the stacking order, top-first, skipping the dragged window): that window becomes `TemplarCompState::drop_target` and its name tag draws as a drop target. Only a name tag is a drop target — an ordinary drop anywhere else, including on top of another window, still just moves a window (planning doc §2 rule 6) |
| Pointer button up ending a name-tag drag over another window's name tag | `window_groups::group_windows(target, dragged)` — group and *place* (`WindowCommand::Group`), after the drag has settled, so `place_beside` starts from where the window was let go. The target is the anchor, so its group's identity survives a merge |
| A name-tag drag on a grouped window | The whole group moves, at the offsets recorded when the gesture began (`window_groups::drag_followers`); `Shift` held at press time moves that member alone. Followers are moved without repair during the drag, exactly like the dragged window, and repaired once at the end |
| Drawing a window's chrome (`chrome::collect_window_chrome`) | A grouped window's name tag carries a 4-logical-pixel stripe in the group's accent and the group's name before the title, and the group's **bottom-most visible member** draws the outline: four 2-logical-pixel bars in the accent, just outside the union of the group's unshelved windows (`WindowService::group_outline`, `chrome::layout::outline_bars`). Outside the union so it never covers a member's own pixels; window chrome rather than output chrome so it survives the shell dying *and* so an unrelated window stacked above the group occludes it, exactly as it occludes that member's tags |
| A surface (or the shell, through `templar_shell_v2`) requests keyboard focus, and xdg-activation issued a token backed by recent input | Build an `ActivationGrant` (`had_recent_input_evidence` from the xdg-activation token's serial, not assumed; `token` a fresh, unguessable string per grant — `WindowService` enforces single-use per exact token string, so reusing one across unrelated activation requests would falsely collide) and call `WindowService::handle_command(WindowCommand::Activate{..}, CommandContext{trusted_shell, now_monotonic_ns})`; on success, send `xdg_toplevel.configure` with `activated` and publish aggregated real peer-PID process state plus the separate anonymous-instance fallback |
| Surface becomes visible/hidden (occlusion, minimized-to-shelf, workspace-irrelevant since there is only one) | OR every sibling window's activation and `mapped && !shelved` visibility by actual nonzero peer PID, then `GovernorNotifier::set_process_state(pid, foreground, visible)`. Anonymous records use aggregated `set_instance_state` separately. Notifier publication is coalesced; neither application-id matching nor one arbitrary sibling is workload truth. |
| `templar_shell_v2` request from the shell connection (`close`, `move_to_visible_area`, `shelve`, `unshelve`, `escape_fullscreen_or_modal`, `show_all_windows`) | `WindowService::handle_command` with `CommandContext{trusted_shell: true, ..}` — **only** the one connection the compositor itself spawned as the shell may set `trusted_shell: true`; every other connection (there is no other way to reach this protocol) is a protocol violation, not a legitimate `false` case |
| `templar_shell_toplevel_v1` group request (`join_group`, `leave_group`, `activate_group`, `shelve_group`, `unshelve_group`, `set_group_name`) | `src/window_groups.rs` — the one place every route into the group model goes, so the chord, the protocol request and the band drop cannot drift. `join_group(other)` makes `other` the anchor whose group identity survives a merge, and *places* the joining window beside it (`WindowCommand::Group`, planning doc §2 rule 6); `activate_group` takes the same trusted `ActivationGrant` as `activate`, because a group is not a way around the focus-stealing checks. A request naming a window or group that no longer exists is ignored, not an error: the shell's view arrives by event and is allowed to be one frame stale |
| Anything that changes group membership, a group's name, or which window is in one | `templar_shell_toplevel_v1.group` after `identity` on every sync (`protocols_shell::send_group`): `group_id` 0 for a loose window, otherwise the id, the name and the accent every surface draws that group with. The compositor is the sole source of truth for groups; this event is the shell's only view of them |
| `templar_shell_v2.open_taskfield` from the shell | `WindowService::enter_taskfield(filter)` — a chip whose fan is too small, a launcher result's arrow, a place's "N windows active" line, or the shelf band. The compositor never learns what a place is: for `windows` the shell sends the ids and the `key` carries only the header's text. Refused, with nothing changed, when the filter selects nothing. Every entry and every change of query or selection sends `taskfield_state` back, which is the shell's whole view of the mode; it computes its own card rectangles by running `templar_window::taskfield::layout` over the toplevels and outputs it already holds, so no card geometry crosses the wire. The shell's chrome for the mode is a `Layer::Presentation` surface, mapped only while `Presentation::Taskfield` and configured to the whole output, reserving nothing. |
| `Super+Space` | `reveal_home(RevealHome::Show)` and then `launcher_focus` to the manager, which arms the Apps bar on Home. Not a second launcher — one launcher object, one more route into it — and the reveal hides client windows without unfullscreening any, so the fullscreen window is undisturbed |
| `templar_shell_manager_v2.set_terminal` | Store the canonical app id (`ShellProtocolState::terminal_app_id`) for `Super+grave` (§4.6). Kept when the shell disconnects — the chord still toggles an existing terminal window with the shell dead; only *launching* one needs the shell back |
| `Super+grave`, the drop-down terminal | "The terminal" is the topmost client window whose **resolved** identity (`CachedIdentity`, from the cgroup — never the client's own `app_id`) is that stored id. Shelved or unfocused: unshelve and activate. Focused: `WindowCommand::Shelve` and hand the keyboard back to `previous_focus`, then to the topmost window still on screen, then to Home. Absent: `templar_shell_toplevel_v1`'s manager gets `terminal_requested`, once per press, and the shell launches the provider through the ordinary launch chain. **Never closed**: an accidental press must cost nothing |
| Switching and overview chords: `Alt+Tab`/`Alt+Shift+Tab`, `` Alt+` ``, `Super+Tab`, `Super+G`, `Super+Shift+G` | `Alt+Tab` walks `WindowService::alt_tab_order` — every open window: each record whose semantic role is `Application` and which is protocol-mapped, **parked (shelved) ones included** in their recency position, most-recently-activated first by `WindowRecord::last_activated`. No cap: with more windows than the output has rows, the HUD shows the contiguous run around the selection (`chrome::layout::switcher_visible_rows`). Fullscreen, always-on-top and visual layer are deliberately irrelevant. The list is **frozen** into a `templar_window::SwitchRing` at the first press. A step only moves the selection and redraws the HUD; the walk **commits on release** — when neither Alt nor Super is held any more, `input::on_keyboard` calls `TemplarCompState::commit_switch_walk`, which activates the selection (unshelving a parked one through `WindowService::activate`) unless it is gone or already active. A quick `Alt+Tab` tap therefore lands on the previous window, `Alt+Shift+Tab` on the least recent. Escape while the chord is held (swallowed, never forwarded), any other non-modifier key, another reserved action, a pointer press, the overview opening, a Home reveal or a key the pressure overlay takes all **cancel** the walk (`cancel_switch_walk`) and activate nothing. A window that is destroyed or unmapped mid-walk is pruned from the ring (`SwitchRing::remove`, the selection moving to the next window) and the HUD rebuilt; an emptied ring ends the walk. Each HUD row is the window's title, else the application's short name, else `(untitled)`; a parked window's row is drawn in muted ink. `Alt+Tab` itself is never scoped to a group — that made a group a place `Alt+Tab` could not leave; within-unit cycling is `` Alt+` ``'s, under a chord that reads as "this one". `` Alt+` `` is the same ring, committed the same way, over `task::within_unit` — every window of the focused window's *unit*, parked ones included, which is its group when it is grouped and its application otherwise, so the group feature costs nothing until a group exists. `Super+Tab` opens the window overview (`WindowService::enter_taskfield(TaskFilter::All)`) and closes it again; it used to cycle between groups, whose question — which group is next — the overview answers by showing every group as a section. `group_cycle_targets` is gone. `Super+G` groups the focused window with the previously focused one (`TemplarCompState::previous_focus`, tracked in `input::activate_window`) and sets it down beside it; `Super+Shift+G` takes it out. **No `Ctrl+Tab`**: it is the tab chord in every browser, terminal and editor, and a compositor-reserved chord wins (planning doc §2.1). Shift+Tab arrives as `ISO_Left_Tab`, which both switchers accept. |
| An `Alt+Tab` / `` Alt+` `` walk's HUD state changes (a step, a prune, a commit or a cancel) | `TemplarCompState::set_switcher`, the one funnel: it stores `switcher`, calls `WindowService::set_switch_walk(walking)` and, when that flips, `space_sync::sync_after_change`, then sends `switcher_state` to the shell. `WindowService::is_visible` shows a `Layer::Presentation` surface only during Taskfield **or a walk**, so the shell's icon/preview switcher is in `Space` exactly while the walk is up; the shell commits an empty input region, so it takes no pointer. The compositor's text HUD (`chrome::collect_output_chrome`) stands down only for a Presentation surface that `is_visible` — never for one merely mapped — so with the shell dead, slow to map, or hidden, the text HUD draws. A Presentation surface hidden outside Taskfield used to suppress the text HUD while never being drawn: the walk showed for one frame and then nothing |
| Any pointer or keyboard event while `Presentation::Taskfield` | The mode is modal, and the compositor owns it. **Pointer**: a window owns its *card* and nothing else — `input::press_candidates` substitutes `WindowService::presentation_rect` with no frame, no band, no control tags and no resize zone, because none of them was drawn, so the renderer and the hit-test read the same rectangle and a card is clicked exactly where it is seen. `surface_under` refuses every client surface and offers only the shell's `Layer::Presentation` one, inside its own input region: a picture of a window takes no pointer events. **Keyboard**: `recovery_action` runs first, so `Super+Tab` still closes the mode, `Ctrl+Alt+Escape` still escapes and the bare `Super` tap still goes Home — a modal mode that swallowed the recovery chords would be a trap — and every remaining key is `keybindings::taskfield_key`'s: arrows step in reading order (`taskfield::step`), Escape clears the query and then leaves, Delete *asks* the selected window's client to close, and any printable character goes into the query, which dims nothing and **moves nothing** (the layout is frozen for the length of the mode, so a card never moves out from under the pointer). A key it does not name is swallowed, and so is its release. The primary button's *release* is what decides, over the same `input::CLICK_SLOP` the name tag's pin uses (`input::taskfield_release`, pure): a release within the slop is a click, and a release beyond it is a drag. Dragging a card out of its section and dropping it on another section's card groups the two through `window_groups::group_windows` — the section's *first* card is the anchor, so the group already there keeps its identity — and then leaves. A drag that starts on the scrim, ends on the scrim, or ends inside its own section changes nothing and leaves the mode up: a gesture thought better of must not also close what was being looked at. There is no grab: the mode already owns the pointer, so a press point is the whole of the gesture's state and there is nothing to unwind if the mode ends underneath it. **Exactly three acts do anything**: `Enter` on the selection, a left click on a card, and that drop. Leaving any other way — Escape, the chord again, a click on nothing, the shell dying — restores the previous scene and activates nothing at all, because looking at your windows is not choosing one |
| Fixed recovery key combinations (`docs/architecture/accessibility.md`): show every window, escape fullscreen/modal, cycle the fallback switcher, go home | Call `WindowService::handle_command(WindowCommand::ShowAllWindows \| EscapeFullscreenOrModal \| RevealHome, CommandContext{trusted_shell: true, ..})`, `WindowService::enter_taskfield` or `WindowService::alt_tab_order` directly from the compositor's own keybinding handler — **not** through the wire protocol, and not gated on a shell connection existing at all; this is what makes them work with the shell dead. After a reveal starts or ends, `input::refocus_after_reveal` moves the keyboard to the Home Surface or back to the topmost visible window |
| `xdg_toplevel.set_maximized` / `unset_maximized` | `xdg_shell::apply_maximized` → `WindowService::handle_command(SetMaximized{..})`; the returned geometry (one output's `usable()`, or the repaired floating rect) goes out as the pending size + one configure. `on_outputs_changed` re-derives a maximized window from its output rather than repairing it |
| Any input event at all — keyboard, pointer, touch — before it is dispatched anywhere | `TemplarCompState::note_activity` (`src/idle.rs`) records the authoritative seat activity and wakes a blanked screen. The Power1 actor reads authenticated compositor activity/inhibition, applies stored dim/lock/screen-off deadlines, and requests typed native control. Dimming restores only a value still owned by the actor. Lock acknowledgment and automatic blank/sleep authorization require the exact protected generation to be presented across every active output; inventory/backend uncertainty is not coverage. Idle inhibition requires exposed fullscreen video: a live Wayland inhibitor plus `wp_content_type_v1.Video`, or a ScreenSaver playing-video request attributed to its bus-authenticated PID with one exposed application window. Pause/stop inhibitor release, leaving fullscreen, shelf, Home, unmap, complete occlusion and disconnect release inhibition. Generic app/audio/presentation inhibitors are insufficient; ScreenSaver has no window handle, so ambiguous multi-window activity is ignored. The private shell manager no longer relays idle configuration. Physical DPMS, protected presentation and suspend/resume acceptance remain distinct from private actor fixtures. |
| Anything that changed toplevel list, shelf contents or stacking order | `templar_comp_integration::snapshot(&service)` then `ShellStateBroadcaster::publish` — never encode/send `templar_shell_v2` events synchronously from the same call site that touched `WindowService`. Both channel hops are latest-value slots: a burst replaces an obsolete complete snapshot instead of building an unbounded protocol backlog. The snapshot exposes semantic application records only and carries `mru_rank`; the taskbar never infers application eligibility or row order from `Space`. |
| `templar_shell_toplevel_v1.set_minimize_target` (`since=12`) | `protocols_shell::set_minimize_target` stores the window's card — `(Home surface's WindowId, surface-local Rect)` — in `TemplarCompState::motion` (`src/motion.rs`). Refused silently unless the surface is this connection's own `Layer::HomeSurface`; a null surface or an empty rectangle clears it; destroying either window drops it (`on_window_destroyed`, and every resync prunes cards of windows `WindowService` no longer has). A drawing hint only: never geometry, layout, stacking, focus or input, and `WindowService` never sees it |
| A client window's `shelved` changes — whatever caused it (`Shelve`/`Unshelve`/`ShelveGroup`/`UnshelveGroup`, `xdg_toplevel.set_minimized`, the band's park button, `Ctrl+Alt+P`, the terminal chord, activating or showing a parked window) | `space_sync::start_shelf_motions`, inside `sync_after_change` — the one funnel all of them end in — compares each window's `(shelved, is_visible)` with the previous resync. Visible→shelved with a card on a mapped Home that is on screen afterwards: a 200 ms **park** from its client area to the card resolved against Home's current position. Shelved→visible with a card whose Home was on screen before: an **unpark** from the card back. A newer motion replaces an older one from its current interpolated rect. No card, session locked, Taskfield, or Home not on screen: no motion — the window appears or vanishes exactly as before. `scene::collect` draws a parking window at its stacking position while shelved (surface tree only, scaled into `rect_at(now)`, faded 1.0→0.3, no chrome, no input — `Space` and the hit test know it only as shelved), and an unparking one at `rect_at(now)` with its chrome held until it lands. Both backends already render every refresh (udev: page flips, or `schedule_retry` on a damage-free frame; winit: `request_redraw` per redraw), so a motion needs no timer of its own; each render prunes finished motions first |

`Layer::SecureLock` and `Layer::Recovery` surfaces (the lock screen, the pressure overlay, the recovery
tab) are created by the compositor core calling `WindowService::set_layer(id, layer, ClientKind::CompositorCore)`
directly — no protocol request exists for them, by design (`interfaces/wayland/templar-recovery-v1.xml`).

The standard foreign-parent globals are public on the ordinary socket, independently of the
private shell/recovery globals. Tokens authorize only a transient presentation relationship;
credentials still come from each child's own Wayland connection. The pinned Smithay foreign
delegate retains one child per export and does not revoke exported-surface loss, so this adapter
uses Smithay's `Dispatch2` and xdg-shell state with the unchanged standard v2 wire protocol and
explicit all-import/all-child lifetimes. There are no duplicate globals or compatibility aliases.
Parent relationships never issue xdg-activation tokens or synthetic input. Activation still
requires its real producer's recent-input grant; foreign parenting alone does not focus a dialog.


## A screen-filling window's own chrome

A window filling its output — maximized or fullscreen (`WindowRecord::covers_screen`) — wears no
frame: the client owns every pixel of it, bottom edge included. That leaves it with no pointer
route to its own controls at all: the island carries no copy of them, and there is no strip beside
the window to reach for.

Both of its bottom corners belong to it. Resting the pointer in either one shows that window's name
tag and control tags *over* it, on the strip its own geometry already puts along the bottom edge of
the screen: the tag at the left, close/maximize/minimize at the right. The strip stays while the
pointer is on it or on either corner, and goes when the pointer leaves — the same asymmetry every
other reveal on this desktop has.

Three regions divide the bottom edge of an output, and never two of them claim one point (the bottom edge itself belongs to Peek or to the gesture gate, never both):

| Region | With a screen-filling window | Without one |
|---|---|---|
| top-left square | reveal Home | reveal Home |
| bottom-right square | that window's chrome | nothing |
| the rest of the bottom edge, its left corner included — gesture gate **disabled** | peek: raise the shell's overlay, send `bar_peeked`, and draw the corner affordance — and, resting there a second time, open the window overview | nothing |
| the gesture gate, centred on the bottom edge — gate **enabled** (Peek, its promotion and its corner mark are then off everywhere) | entering it from above and going sideways opens a rail: the output is peeked for the rail's lifetime, so the shell's overlay rises over a fullscreen window, and `rail` names the side | the same — a rail opens whatever is on screen |

Home's square is at the *top* left, and is Home's whatever is on the screen: the bar carries no
Home chip, so it is the only pointer route there, and a window filling the screen is when it is
needed most. It is at the top because the bottom-left corner is where the island starts, and that
is where Peek's affordance has to be — a mark that says "the bar is here" at the other end of the
screen from the bar would be a worse instruction than none. Home is not diminished by the move: its
primary route is the bare `Super` tap.

The one state-dependent part is that while the chrome is already showing, neither the bottom-right
square nor the bottom edge asks for anything — the strip is under the pointer and the strip is what
is being reached for. Without that, resting on the close tag long enough to aim at it would raise
the island underneath the thing being aimed at (the strip is `TAG_HEIGHT` tall and the peek band is
its bottom ten pixels). The top-left square needs no such guard: no window's chrome reaches it.

Two things follow from the strip being drawn *over* the client rather than beside it, and both are
new with maximized windows joining this rule. The gap between the two tags belongs to that window
(`PressCandidate::strip_over_client`) rather than falling through to whatever is behind, or every
click along the bottom row of the front window would raise the one underneath. And a press the
compositor takes is not also the client's: `input::on_pointer_button` swallows the press *and its
release* for every affordance (`TemplarCompState::swallowing_press`), because the tags that act and
return — close, park, unmaximize, pin — install no grab, and the surface with pointer focus under
that strip is the window's own client.

`WindowService::covering_chrome` is the whole of the state, filtered on read so a window that stops
covering the screen stops showing chrome by that fact alone. `chrome::collect_window_chrome` draws
it, and `press_candidates` makes the same strip hit-testable, so a click on the close tag reaches
the window rather than the client underneath. On this strip the middle control leaves whichever
state the window is in — fullscreen first, then maximized — and the name tag pins rather than
drags: dragging a window whose geometry *is* the output would move it off the output, which is
never repaired.

## Native Input and Displays ownership

`org.templaros.Input1` and `org.templaros.Displays1` are sessiond-owned preference brokers.
Their bounded typed control channel authenticates the native compositor with a kernel socket-bound
peer pidfd, UID and exact user-service cgroup; a numeric PID or a nested winit process is not a substitute.
The native event-loop thread alone reads or changes live libinput/KMS objects. Every write uses the
current owner epoch/revision and returns fresh native readback, including on rejection.

Input identities use real udev path/serial plus device capabilities, not `eventN`. Ambiguous identities
are unavailable and hardware without a stable identity is explicitly session-only. Defaults come from
the device's libinput API, including genuine NONE/custom acceleration and NONE click/scroll modes;
unsupported properties carry reasons instead of invented values. Sparse overrides preserve explicit
false/zero values. Reset really applies and reads back defaults, then removes the durable override and
consumes its migration tombstone. The old global tap choice is snapshotted/deleted in one SQLite
transaction and applies only to the first nonempty eligible stable touchpad roster observed while
active/unlocked. Keyboards, unsupported/inactive observations and later arrivals never widen that set.
The Settings editor retains a selected device's numeric drafts until its corresponding actual value
or selected identity changes.

The display inventory spans every seat GPU. Failed activation, connector scan, renderer, queue,
submitted-frame and clear operations remain visible separately from successful outputs; no fake
surface or claim of having darkened a failed GPU is manufactured. Physical KMS ACTIVE/routing/exact
timing readback is distinct from the last-owned layout and desired idle flag. Unknown initial heads
without mapped `Output`s block protected coverage. A render failure clears only on a real owned
submitted frame; recovery changes protected coverage generation before accepting another frame.
`Activity.blanked` means every connected physical head was read back disabled, not “blank was requested.”

Connector presence combines connected status, nonempty kernel EDID bounded to 32768 bytes,
or observed enabled physical routing. Secondary rescans reconcile owned surfaces against
that combined observation even when raw status does not change; presence never invents modes.
Logical primary is projected from connected, enabled heads with live mapped outputs. A valid
explicit primary wins, otherwise stable display identity determines failover. Inventory,
Home placement and preview capture share that projection, while each scanout owner's exact
configuration remains unchanged for frame admission.

A profile contains the complete connected connector/EDID identity set, disabled heads included, exact
advertised timings, positions, fractional scales, rotations, one primary, explicit mirror sources and
an explicit headless policy. Native changes run real KMS TEST_ONLY and readback. Interactive previews
start their 15-second confirmation clock only after every target's exact queued token is presented;
operational lid/confirmed-profile restores have their own five-second completion/readback barrier and
remain legal while locked. A stale buffer, token or protected generation is never a completion.
Failure/expiry/topology changes restore against the current topology, preserving genuinely new heads.
A failed rollback stays owned and retries effects only after native inventory recovery.

Signal timings are never rounded or rewritten to implement calibration. Real usable hardware
underscan is preferred; otherwise final composition clips/relocates asymmetric signal-pixel margins
and disables direct scanout. Work areas, maximized/fullscreen client sizes and absolute-pointer inverse
mapping use the visible logical geometry. Duplicate heads sample one explicit source viewport/cursor,
not independent overlapping scenes. Identification/calibration draws on the real applied output;
native calibration uses an ARGB layer with transparent center, four visible corners and a center mark
so Settings remains usable. Owned chrome/lock/identification consume the shared pure appearance palette;
appearance changes invalidate cache keys, retain buffers/fonts and never recreate placement/cards.
Governor enrichment aggregates actual nonzero peer PIDs by OR of every sibling's activation and
visibility; anonymous semantic-instance fallback is separate and never invents a workload PID.

## Shared Displays page and standalone X11

`crates/templar-display-ui` owns the PixelKit display page used both by native Settings and the
independent `templar-displays` application (Debian package `templaros-displays`). The page provides
drag/keyboard arrangement, Extend/Duplicate/Internal/External presets, exact modes, primary/rotation/
scale, symmetric/asymmetric/custom-aspect margins and actual-output identification. It displays
backend capabilities and the rollback owner's deadline; the GUI does not own a rollback timer.

The standalone app runs against a real Lubuntu/X11 RandR server without sessiond or Alpha services.
`--print` is a read-only native snapshot. Effects use actual CRTC routing, advertised timings,
RandR 16.16 transforms, driver border properties, root framebuffer and primary-output readback.
A private child of the same binary owns the original raw server configuration before any effect and
restores on EOF/GUI death, expiration or configuration/hotplug changes. Failed rollback ownership
survives lost readback and stops repeated effects until a real recovery. Confirming requires the
exact live token/revision and successful server readback; durable profiles require proven stable
kernel GPU/connector/EDID identities and bounded user-owned atomic files. Shape identification is
input-transparent and calibration is also visually transparent centrally.

`RandRReadback` attests only X-server configuration. It is **not** native KMS TEST_ONLY, protected-lock
coverage, page-flip presentation or compositor-only asymmetric black-margin proof. Xvfb cannot
establish those hardware properties. Integrated builds/unit/private-bus/Xvfb checks and owner hardware
acceptance are separate: real multi-GPU failure/isolation, connector churn, lock-generation present,
DPMS readback, driver TEST_ONLY/underscan and calibrated pointer/mirror/fullscreen behavior require
the owner hardware lab, not an asserted manual success.
