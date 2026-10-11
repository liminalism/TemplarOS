//! Mirrors `templar_window::WindowService` state into Smithay's `Space` (rendering/hit-testing only)
//! and pushes it out to everything downstream: the governor, the sanitized shell snapshot, and our
//! own `templar_shell_v2`/`ext_foreign_toplevel_list_v1` protocol objects. `WindowService` is always
//! the input to this module, never the output — nothing here feeds back into it.
//!
//! [`sync_after_change`] is meant to run after any `WindowService` call that can change geometry,
//! stacking, title, decoration or shelf/fullscreen/modal state — it is a handful of already-cheap
//! lookups (`stacking_order`, `windows()`), not a scan of anything Smithay-side, so calling it
//! liberally rather than hand-rolling per-call-site diffing is the intended usage (mirrors
//! `GovernorNotifier`'s own "safe to call every frame" contract).

use crate::state::{Backend, TemplarCompState};
use smithay::desktop::Window;
use smithay::input::pointer::MotionEvent;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;
use templar_window::{Point, Rect, WindowId};

/// Where every visible window sits, top first — the scene as the pointer hit-test sees it.
///
/// Taken once before a resync and once after, so [`pointer_scene_changed`] can answer whether the
/// thing under a motionless pointer moved out from under it.
fn pointer_scene<B: Backend + 'static>(state: &TemplarCompState<B>) -> Vec<(WindowId, Rect)> {
    state
        .window_service
        .stacking_order()
        .into_iter()
        .filter(|window_id| state.window_service.is_visible(*window_id))
        .map(|window_id| (window_id, client_area(state, window_id)))
        .collect()
}

/// Whether the scene at `pointer` changed in a way the pointer has to be told about: the topmost
/// window containing the pointer changed identity or rectangle, including a stacking swap of two
/// overlapping windows whose rectangles did not move.
///
/// The guard, and the reason it is a guard: a resync runs after anything at all that touches
/// `WindowService`, most of which moves nothing the pointer is over. Re-delivering motion
/// unconditionally would send a `wl_pointer.motion` per resync — a wake-up per event on a desktop
/// that is meant to be silent when nothing is happening (`scripts/idle-wakeups.sh`).
///
/// Pure: two lists and a point, no seat, no `Space`, no display. Lists are top-first.
fn pointer_scene_changed(
    before: &[(WindowId, Rect)],
    after: &[(WindowId, Rect)],
    pointer: Point,
) -> bool {
    let contains = |rect: &Rect| {
        pointer.x >= rect.x
            && pointer.x < rect.right()
            && pointer.y >= rect.y
            && pointer.y < rect.bottom()
    };
    let under = |list: &[(WindowId, Rect)]| {
        list.iter()
            .find(|(_, rect)| contains(rect))
            .map(|(window_id, rect)| (*window_id, *rect))
    };
    match (under(before), under(after)) {
        (None, None) => false,
        (Some((before_id, before_rect)), Some((after_id, after_rect))) => {
            before_id != after_id || before_rect != after_rect
        }
        _ => true,
    }
}

/// Re-deliver pointer motion at the same place, because the surface under the pointer moved.
///
/// A pointer that has not moved gets no motion event, so a surface that grows *upward* under it —
/// the island opening its row, the machine cluster opening its popover — leaves the client
/// believing the pointer is where it was in the old, shorter surface. Every surface-local
/// coordinate it computes from then on is wrong by the height it gained, which is a chip that
/// highlights a row above the pointer, and (when the growth is driven by that very hover) an
/// oscillation: grow, lose the chip, shrink, find it again.
///
/// This is the compositor's job, not the client's: the compositor is the only party that knows
/// the surface moved, and `wl_pointer` has no other way to say so. Re-running the motion handler
/// at the unchanged global location is exactly what a zero-pixel move would have done — focus is
/// recomputed, an enter/leave pair is sent if the pointer crossed into another surface, and the
/// surface-local coordinate is derived from the new placement.
///
/// Never called while Smithay is dispatching to the pointer: it runs from the event loop's idle
/// list ([`sync_after_change`] queues it), because every `PointerHandle` call locks the pointer's
/// `std` mutex — the very mutex a grab's `button`/`motion` runs under — and a second lock on the
/// same thread never returns. A move/resize remains untouched: its grab owns geometry.
/// Native file DnD is the narrow exception, because revealing Home must deliver Enter to
/// its new destination without releasing the source grab. Not `note_activity` either —
/// screen must not wake for this.
fn refresh_pointer_focus<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    before: &[(WindowId, Rect)],
) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    if state.session_lock.locked || (pointer.is_grabbed() && !state.native_file_drag_active) {
        return;
    }
    let location = pointer.current_location();
    let at = Point {
        x: location.x.floor() as i32,
        y: location.y.floor() as i32,
    };
    if !pointer_scene_changed(before, &pointer_scene(state), at) {
        return;
    }
    let under = state.surface_under(location);
    let time = crate::window_bridge::now_monotonic_ns(&state.clock) / 1_000_000;
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time: u32::try_from(time % u64::from(u32::MAX)).unwrap_or(0),
        },
    );
    pointer.frame(state);
}

/// The Smithay `Window` for `window_id`: the index first (`WindowIndex::element_of`, which knows
/// hidden windows too), then `Space` for anything mapped before the index learned about it.
pub fn element_for_window<B: Backend + 'static>(
    state: &TemplarCompState<B>,
    window_id: WindowId,
) -> Option<Window> {
    if let Some(window) = state.windows.element_of(window_id) {
        return Some(window.clone());
    }
    let surface = state.windows.surface_for(window_id)?;
    state
        .space
        .elements()
        .find(|w| w.wl_surface().as_deref() == Some(surface))
        .cloned()
}

/// Where `window_id`'s surface goes and how big it is: `WindowService::client_area` — the
/// geometry, whole, for client windows and shell surfaces alike. Every `map_element` and every
/// size configure in the compositor goes through this, so the client rectangle is derived in
/// exactly one place and a pure tab relocation configures nothing at all. A window the service
/// does not know is mapped at its geometry as given.
pub fn client_area<B: Backend + 'static>(state: &TemplarCompState<B>, window_id: WindowId) -> Rect {
    state
        .window_service
        .client_area(window_id)
        .or_else(|| state.window_service.window(window_id).map(|w| w.geometry))
        .unwrap_or(Rect::new(0, 0, 1, 1))
}

/// After a drag-motion event: move the dragged window's `Space` element to `geometry` (applied
/// as-is, matching COMPOSITOR.md's drag row — no repair, no stacking change, no broadcast, since
/// dragging alone changes neither the toplevel list, the shelf nor the stacking order).
pub fn apply_dragged_geometry<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    window_id: WindowId,
    _geometry: Rect,
) {
    let Some(window) = element_for_window(state, window_id) else {
        return;
    };
    let client = client_area(state, window_id);
    state.space.map_element(window, (client.x, client.y), false);
}

/// After a resize-motion event: give `window_id` the size `WindowService` just computed.
///
/// Unlike a move, this cannot go through `Space` alone — `Space` mirrors *where* a window is and
/// never how big it is, so the size has to be configured onto the client itself, on whichever of
/// the two shells it speaks. `still_resizing` sets or clears `xdg_toplevel.state.resizing`, which
/// is how a Wayland client knows whether to keep throttling its own relayout or settle; X11 has
/// no equivalent, so there it only decides nothing.
pub fn apply_resized_geometry<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    window_id: WindowId,
    _geometry: Rect,
    still_resizing: bool,
) {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
    use smithay::utils::{Point, Rectangle, Size};

    let Some(window) = element_for_window(state, window_id) else {
        return;
    };
    let geometry = client_area(state, window_id);
    let size = (geometry.width.max(1), geometry.height.max(1));
    // Cloned out of the element before `map_element` takes ownership of it below; the two shells
    // are mutually exclusive (`Window::toplevel`/`Window::x11_surface`), so this is one match
    // spelled as two lookups because `WindowSurface` itself is not `Clone`.
    if let Some(toplevel) = window.toplevel().cloned() {
        toplevel.with_pending_state(|pending| {
            if still_resizing {
                pending.states.set(XdgState::Resizing);
            } else {
                pending.states.unset(XdgState::Resizing);
            }
            pending.size = Some(size.into());
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_configure();
        }
    } else if let Some(surface) = window.x11_surface().cloned() {
        // An X11 client is told position and size in one request; there is no separate "pending
        // state" to accumulate, and a refusal (the window is already gone) is not something this
        // path can do anything about.
        let _ = surface.configure(Some(Rectangle::new(
            Point::from((geometry.x, geometry.y)),
            Size::from(size),
        )));
    }
    state
        .space
        .map_element(window, (geometry.x, geometry.y), false);
}

/// `on_toplevel_destroyed` plus the identity/governor cleanup COMPOSITOR.md's destroy row
/// specifies, then a full resync.
pub fn on_window_destroyed<B: Backend + 'static>(
    state: &mut TemplarCompState<B>,
    window_id: WindowId,
) {
    let record = state.window_service.window(window_id).cloned();
    state.window_service.on_toplevel_destroyed(window_id);
    if state
        .name_tab_menu
        .open
        .is_some_and(|menu| menu.window_id == window_id)
    {
        state.name_tab_menu.dismiss();
    }
    state.motion.forget_window(window_id);
    // Demand-gating: a destroyed window keeps no demand, no pacing and no queued frames —
    // its capture in flight retires through the protocol's failure lifecycle, unrendered.
    state.screencopy.forget_window(window_id);
    if state.previous_focus == Some(window_id) {
        // `Super+G` must never name a window that is gone; the next activation refills this.
        state.previous_focus = None;
    }
    // A switcher walk must not draw, or commit to, a window that no longer exists.
    state.prune_switch_walk(window_id);

    if let Some(record) = record {
        let pid_still_live = state
            .window_service
            .windows()
            .any(|window| window.peer_pid == record.peer_pid);
        if !pid_still_live {
            state.identity_cache.forget(record.peer_pid);
            if record.peer_pid != 0 {
                state.governor.forget_process(record.peer_pid);
            }
        }
        if record.peer_pid == 0
            && let Some(instance_id) = record.identity.instance_id
        {
            let instance_still_live = state
                .window_service
                .windows()
                .any(|window| window.identity.instance_id == Some(instance_id));
            if !instance_still_live {
                state.governor.forget_instance(instance_id);
            }
        }
    }

    sync_after_change(state);
}

/// Say once, when it changes, whether the Home surface is on screen and why — the fact the
/// black-screen-at-login fault turned on, and the one thing a session log had nothing to say
/// about. `Home surface is not mapped` with no reveal and no covering window is the bug;
/// `not mapped (covered)` while a maximized window is up is the design.
fn log_home_visibility<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let home = state
        .window_service
        .stacking_order()
        .into_iter()
        .find(|id| state.window_service.layer_of(*id) == Some(templar_window::Layer::HomeSurface));
    let mapped = home.map(|id| state.window_service.is_visible(id));
    if mapped == state.home_mapped {
        return;
    }
    state.home_mapped = mapped;
    match mapped {
        Some(true) => tracing::info!(
            revealed = state.window_service.home_revealed(),
            "Home surface mapped"
        ),
        Some(false) => tracing::info!(
            revealed = state.window_service.home_revealed(),
            "Home surface unmapped: a fullscreen or maximized window covers its output"
        ),
        None => tracing::info!("no Home surface: the shell has not placed one"),
    }
}

/// Re-mirror every live window's geometry, visibility and stacking order into `Space`, re-enrich
/// the governor, and broadcast a fresh sanitized snapshot to the shell and our own toplevel
/// protocol objects. Idempotent and cheap enough to call after any `WindowService`-mutating
/// handler.
///
/// Visibility is `WindowService::is_visible`'s verdict and nothing else: a window it says is
/// hidden (shelved, or a client window behind a "go home" reveal) is *unmapped* from `Space`, so
/// it neither renders nor receives pointer input — `Space` is the render/hit-test mirror, and a
/// window that is in it is on screen. The governor is told `visible = mapped && !shelved`, but
/// deliberately ignores a Home reveal: a momentary view must not suspend every application.
pub fn sync_after_change<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if state.name_tab_menu.open.is_some_and(|menu| {
        !state.window_service.is_visible(menu.window_id)
            || !matches!(
                state.window_service.presentation(),
                templar_window::Presentation::Normal
            )
            || !state
                .window_service
                .outputs()
                .iter()
                .any(|output| output.logical.intersection_area(menu.panel) > 0)
    }) {
        state.name_tab_menu.dismiss();
    }
    // Reconcile existence before order. A toplevel may be temporarily unmapped while retaining
    // its semantic record, and a broken/missing layer assignment must never leave an old Space
    // element visible even though stacking_order cannot name it.
    let records: Vec<(WindowId, bool)> = state
        .window_service
        .windows()
        .map(|record| (record.id, record.mapped))
        .collect();
    for (window_id, mapped) in records {
        let layer = state.window_service.layer_of(window_id);
        let element = element_for_window(state, window_id);
        if mapped && layer.is_none() {
            tracing::error!(
                ?window_id,
                role = ?state.window_service.window(window_id).map(|record| record.role),
                "mapped toplevel has no layer; removing it from the render mirror"
            );
        }
        if mapped && element.is_none() {
            tracing::error!(
                ?window_id,
                role = ?state.window_service.window(window_id).map(|record| record.role),
                "mapped toplevel has no Smithay element"
            );
        }
        if (!state.window_service.is_visible(window_id) || layer.is_none())
            && let Some(element) = element
        {
            state.space.unmap_elem(&element);
        }
    }

    // Bottom-to-top so each `raise_element` call leaves the most-recently-processed (i.e.
    // topmost, per `stacking_order`'s top-first contract) window on top of `Space`'s own order.
    for window_id in state.window_service.stacking_order().into_iter().rev() {
        let Some(record) = state.window_service.window(window_id) else {
            continue;
        };
        let _ = record;
        let client = client_area(state, window_id);
        let Some(window) = element_for_window(state, window_id) else {
            continue;
        };
        if state.window_service.is_visible(window_id) {
            state
                .space
                .map_element(window.clone(), (client.x, client.y), false);
            state.space.raise_element(&window, false);
        } else {
            state.space.unmap_elem(&window);
        }
    }

    prune_capture_demand(state);

    log_home_visibility(state);
    start_shelf_motions(state);

    for (pid, (foreground, visible)) in aggregated_process_states(state) {
        state.governor.set_process_state(pid, foreground, visible);
    }
    for (instance_id, (foreground, visible)) in aggregated_instance_states(state) {
        state
            .governor
            .set_instance_state(instance_id, foreground, visible);
    }

    crate::tab_metadata::publish(state);

    // `publish` hands the snapshot to the broadcaster's background task and returns immediately;
    // the actual `templar_shell_v2` encoding happens back on this thread, but later — decoupled from
    // this call site — when `src/shell_sink.rs`'s calloop channel wakes up
    // (`src/shell_sink.rs`'s module doc explains why encoding can't happen on the background
    // task itself). This is what COMPOSITOR.md means by "never encode/send `templar_shell_v2`
    // events synchronously from the same call site that touched `WindowService`".
    let snapshot =
        templar_comp_integration::snapshot(&state.window_service, &state.tab_metadata.counts);
    state.shell_broadcaster.publish(snapshot);

    // Last, and only if something under the pointer actually moved: the pointer's own view of the
    // scene is part of the scene, and this is the one funnel every change comes through. Queued,
    // not run: this function is called from inside pointer grabs (a name-tag click pins the window
    // from `WindowGestureGrab::button`; a band drop groups from the same place), where Smithay
    // holds the pointer's mutex and touching the pointer here deadlocked the compositor for good.
    if !state.pointer_refresh_pending {
        state.pointer_refresh_pending = true;
        state.loop_handle.insert_idle(|state| {
            state.pointer_refresh_pending = false;
            let before = std::mem::take(&mut state.last_pointer_scene);
            let after = pointer_scene(state);
            refresh_pointer_focus(state, &before);
            state.last_pointer_scene = after;
        });
    }
}

/// Demand-gating hook: capture demand and queued frames never outlive the window they
/// name. A resync runs after every `WindowService` change, so demand for a window the
/// service no longer knows (close, identity loss) is dropped here and its queued frames
/// retire unrendered — the pre-render check would catch them anyway, but failing them now
/// frees the queue without waiting for the next render pass. Live windows are untouched,
/// including parked ones: a parked window's card still shows its last submitted content.
fn prune_capture_demand<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    if state.screencopy.pending.is_empty() && state.screencopy.demand.is_none() {
        return;
    }
    let live: std::collections::HashSet<WindowId> = state
        .window_service
        .windows()
        .map(|record| record.id)
        .collect();
    state.screencopy.retain_demand(|id| live.contains(&id));
    state.screencopy.prune_pending(|id| live.contains(&id));
}

/// Start a park or unpark motion for every client window whose shelf state changed since the
/// last resync (`src/motion.rs`).
///
/// This is the one funnel every shelve and unshelve passes through — `WindowCommand::Shelve`/
/// `Unshelve`, `xdg_toplevel.set_minimized`, the band's park button, `Ctrl+Alt+P`, the terminal
/// chord, a group's park/unpark, activating a parked window — because each of them ends in
/// [`sync_after_change`]. Observing the result here rather than instrumenting every command is
/// what keeps a future shelving path from silently skipping the motion.
///
/// No motion — the window simply appears or disappears, exactly as before cards were known — when
/// the window has no card, the card's Home surface is not mapped (or was not on screen to shrink
/// into / grow out of), the session is locked, or Taskfield is presenting.
fn start_shelf_motions<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let now = crate::window_bridge::now_monotonic_ns(&state.clock);
    let seen: std::collections::HashMap<WindowId, crate::motion::Seen> = state
        .window_service
        .windows()
        .map(|record| {
            (
                record.id,
                crate::motion::Seen {
                    shelved: record.shelved,
                    visible: state.window_service.is_visible(record.id),
                },
            )
        })
        .collect();
    let live: std::collections::HashSet<WindowId> = seen.keys().copied().collect();
    state.motion.retain_windows(&live);
    let before = state.motion.replace_seen(seen.clone());

    let quiet = state.session_lock.locked
        || matches!(
            state.window_service.presentation(),
            templar_window::Presentation::Taskfield(_)
        );
    for (window_id, after) in seen {
        let Some(kind) = before
            .get(&window_id)
            .and_then(|before| crate::motion::transition(*before, after))
        else {
            continue;
        };
        let card = state
            .window_service
            .window(window_id)
            .filter(|record| record.role == templar_window::WindowRole::Application && !quiet)
            .and_then(|_| state.motion.target(window_id))
            .filter(|target| {
                let home_mapped = state
                    .window_service
                    .window(target.surface_window)
                    .is_some_and(|home| home.mapped);
                // The card has to be on screen at the end a motion touches it: after a park,
                // before an unpark.
                let home_on_screen = match kind {
                    crate::motion::MotionKind::Park => {
                        state.window_service.is_visible(target.surface_window)
                    }
                    crate::motion::MotionKind::Unpark => before
                        .get(&target.surface_window)
                        .is_some_and(|home| home.visible),
                };
                home_mapped && home_on_screen
            })
            .map(|target| {
                let home = client_area(state, target.surface_window);
                Rect::new(
                    home.x + target.rect.x,
                    home.y + target.rect.y,
                    target.rect.width,
                    target.rect.height,
                )
            });
        let Some(card) = card else {
            // A shelf change with nowhere to draw it from or to: whatever was in flight for this
            // window would now be drawing the wrong thing.
            state.motion.cancel(window_id);
            continue;
        };
        let own = client_area(state, window_id);
        let (from, to) = match kind {
            crate::motion::MotionKind::Park => (own, card),
            crate::motion::MotionKind::Unpark => (card, own),
        };
        state.motion.start(window_id, kind, from, to, now);
        let effect_own = state.windows.element_of(window_id).map_or(own, |window| {
            let geometry = window.geometry();
            let tree = window.bbox_with_popups();
            Rect::new(
                own.x + tree.loc.x - geometry.loc.x,
                own.y + tree.loc.y - geometry.loc.y,
                tree.size.w,
                tree.size.h,
            )
        });
        if let Some(output) = state
            .window_service
            .outputs()
            .iter()
            .find(|output| own.intersection(output.logical).is_some())
        {
            let obstacles: Vec<_> = state
                .window_service
                .windows()
                .filter(|record| {
                    record.id != window_id
                        && record.role == templar_window::WindowRole::Application
                        && state.window_service.is_visible(record.id)
                })
                .map(|record| client_area(state, record.id))
                .collect();
            state.motion.start_essence(
                window_id,
                kind,
                effect_own,
                card,
                output.logical,
                &obstacles,
                now,
            );
        }
    }
}

/// Real peer credentials, not app_id claims, drive general application/workload resource state.
fn aggregated_process_states<B: Backend + 'static>(
    state: &TemplarCompState<B>,
) -> std::collections::HashMap<u32, (bool, bool)> {
    aggregate_process_states(state.window_service.windows().map(|record| {
        (
            record.peer_pid,
            record.activated,
            record.mapped && !record.shelved,
        )
    }))
}
fn aggregate_process_states(
    windows: impl Iterator<Item = (u32, bool, bool)>,
) -> std::collections::HashMap<u32, (bool, bool)> {
    let mut by_pid = std::collections::HashMap::new();
    for (pid, foreground, visible) in windows.filter(|(pid, _, _)| *pid != 0) {
        let entry = by_pid.entry(pid).or_insert((false, false));
        entry.0 |= foreground;
        entry.1 |= visible;
    }
    by_pid
}

/// One governor record per instance: OR of `activated` and of `mapped && !shelved` across every
/// sibling window. Publishing per `WindowRecord` let HashMap iteration pick a shelved sibling
/// to overwrite the active one.
fn aggregated_instance_states<B: Backend + 'static>(
    state: &TemplarCompState<B>,
) -> std::collections::HashMap<templar_types::InstanceId, (bool, bool)> {
    aggregate_instance_states(
        state
            .window_service
            .windows()
            .filter(|record| record.peer_pid == 0)
            .filter_map(|record| {
                record.identity.instance_id.map(|instance_id| {
                    (
                        instance_id,
                        record.activated,
                        record.mapped && !record.shelved,
                    )
                })
            }),
    )
}

fn aggregate_instance_states(
    windows: impl Iterator<Item = (templar_types::InstanceId, bool, bool)>,
) -> std::collections::HashMap<templar_types::InstanceId, (bool, bool)> {
    let mut by_instance = std::collections::HashMap::new();
    for (instance_id, foreground, visible) in windows {
        let entry = by_instance.entry(instance_id).or_insert((false, false));
        entry.0 |= foreground;
        entry.1 |= visible;
    }
    by_instance
}

#[cfg(test)]
mod tests {
    use super::{aggregate_instance_states, aggregate_process_states, pointer_scene_changed};
    use templar_window::{Point, Rect, WindowId};

    fn at(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    /// The guard that decides whether a resync owes the pointer a motion event. It has to be
    /// *both* precise and cheap: too eager and every resync becomes a wake-up on an idle desktop,
    /// too shy and the machine cluster's popover leaves the client hit-testing the pointer
    /// against the surface it used to be.
    #[test]
    fn only_a_window_moving_under_the_pointer_owes_it_a_motion_event() {
        let island = (WindowId(1), Rect::new(600, 1032, 300, 48));
        let cluster_closed = (WindowId(2), Rect::new(732, 852, 548, 48));
        // The popover grew it upward: the bottom edge stayed, the top moved up 330.
        let cluster_open = (WindowId(2), Rect::new(732, 522, 548, 378));
        let on_a_chip = at(1202, 876);

        // The case this exists for: the pointer is resting on a chip and the surface under it
        // grows. Its surface-local position changed without the pointer moving.
        assert!(pointer_scene_changed(
            &[island, cluster_closed],
            &[island, cluster_open],
            on_a_chip
        ));
        // And back again when the popover closes.
        assert!(pointer_scene_changed(
            &[island, cluster_open],
            &[island, cluster_closed],
            on_a_chip
        ));

        // Something moved, but not under the pointer: this is most resyncs, and it must cost
        // nothing.
        let island_wide = (WindowId(1), Rect::new(400, 1032, 700, 48));
        assert!(!pointer_scene_changed(
            &[island, cluster_closed],
            &[island_wide, cluster_closed],
            on_a_chip
        ));
        // Nothing moved at all: the overwhelmingly common resync.
        assert!(!pointer_scene_changed(
            &[island, cluster_closed],
            &[island, cluster_closed],
            on_a_chip
        ));
        // An empty scene, and a scene that became empty somewhere else.
        assert!(!pointer_scene_changed(&[], &[], on_a_chip));
        assert!(!pointer_scene_changed(&[island], &[], on_a_chip));

        // A window vanishing from under the pointer — shelved, closed, hidden by a reveal — is
        // the pointer's business: it is now over whatever was behind.
        assert!(pointer_scene_changed(&[cluster_closed], &[], on_a_chip));
        // And one appearing under it.
        assert!(pointer_scene_changed(&[], &[cluster_closed], on_a_chip));

        // A window that moved *onto* the pointer counts even though it was nowhere near before.
        let elsewhere = (WindowId(3), Rect::new(0, 0, 100, 100));
        let onto = (WindowId(3), Rect::new(1100, 800, 400, 200));
        assert!(pointer_scene_changed(&[elsewhere], &[onto], on_a_chip));

        // Overlapping windows swapping stacking: neither rectangle moved, but the window under
        // the pointer did. Lists are top-first.
        let lower = (WindowId(1), Rect::new(0, 0, 400, 400));
        let upper = (WindowId(2), Rect::new(100, 100, 400, 400));
        let overlap = at(200, 200);
        assert!(pointer_scene_changed(
            &[upper, lower],
            &[lower, upper],
            overlap
        ));
        assert!(!pointer_scene_changed(
            &[upper, lower],
            &[upper, lower],
            overlap
        ));

        // The rectangle is half-open, exactly as the hit-test is: a pointer on the bottom or
        // right edge is already outside, so a window resizing to end there owes nothing.
        let box_ = (WindowId(4), Rect::new(0, 0, 100, 100));
        let taller = (WindowId(4), Rect::new(0, 0, 100, 140));
        assert!(!pointer_scene_changed(&[box_], &[taller], at(100, 50)));
        assert!(pointer_scene_changed(&[box_], &[taller], at(99, 50)));
    }

    #[test]
    fn instance_state_aggregates_siblings_in_either_order() {
        let instance = templar_types::InstanceId::new();
        let active_visible = (instance, true, true);
        let inactive_shelved = (instance, false, false);
        for order in [
            [active_visible, inactive_shelved],
            [inactive_shelved, active_visible],
        ] {
            let aggregated = aggregate_instance_states(order.into_iter());
            assert_eq!(aggregated.len(), 1);
            assert_eq!(aggregated[&instance], (true, true));
        }
    }
    #[test]
    fn workload_peer_pid_state_ors_all_windows_and_excludes_anonymous_fallback() {
        for rows in [
            [
                (234, true, false),
                (234, false, true),
                (567, false, false),
                (0, true, true),
            ],
            [
                (0, true, true),
                (567, false, false),
                (234, false, true),
                (234, true, false),
            ],
        ] {
            let states = aggregate_process_states(rows.into_iter());
            assert_eq!(states.len(), 2);
            assert_eq!(states[&234], (true, true));
            assert_eq!(states[&567], (false, false));
        }
    }
}
