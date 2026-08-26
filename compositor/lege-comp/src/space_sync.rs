//! Mirrors `lege_window::WindowService` state into Smithay's `Space` (rendering/hit-testing only)
//! and pushes it out to everything downstream: the governor, the sanitized shell snapshot, and our
//! own `lege_shell_v1`/`ext_foreign_toplevel_list_v1` protocol objects. `WindowService` is always
//! the input to this module, never the output — nothing here feeds back into it.
//!
//! [`sync_after_change`] is meant to run after any `WindowService` call that can change geometry,
//! stacking, title, decoration or shelf/fullscreen/modal state — it is a handful of already-cheap
//! lookups (`stacking_order`, `windows()`), not a scan of anything Smithay-side, so calling it
//! liberally rather than hand-rolling per-call-site diffing is the intended usage (mirrors
//! `GovernorNotifier`'s own "safe to call every frame" contract).

use crate::state::{Backend, LegeCompState};
use lege_window::{Rect, WindowId};
use smithay::desktop::Window;
use smithay::wayland::seat::WaylandFocus;

/// The Smithay `Window` for `window_id`: the index first (`WindowIndex::element_of`, which knows
/// hidden windows too), then `Space` for anything mapped before the index learned about it.
pub fn element_for_window<B: Backend + 'static>(
    state: &LegeCompState<B>,
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

/// After a drag-motion event: move the dragged window's `Space` element to `geometry` (applied
/// as-is, matching COMPOSITOR.md's drag row — no repair, no stacking change, no broadcast, since
/// dragging alone changes neither the toplevel list, the shelf nor the stacking order).
pub fn apply_dragged_geometry<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    window_id: WindowId,
    geometry: Rect,
) {
    let Some(window) = element_for_window(state, window_id) else {
        return;
    };
    state
        .space
        .map_element(window, (geometry.x, geometry.y), false);
}

/// After a resize-motion event: give `window_id` the size `WindowService` just computed.
///
/// Unlike a move, this cannot go through `Space` alone — `Space` mirrors *where* a window is and
/// never how big it is, so the size has to be configured onto the client itself, on whichever of
/// the two shells it speaks. `still_resizing` sets or clears `xdg_toplevel.state.resizing`, which
/// is how a Wayland client knows whether to keep throttling its own relayout or settle; X11 has
/// no equivalent, so there it only decides nothing.
pub fn apply_resized_geometry<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    window_id: WindowId,
    geometry: Rect,
    still_resizing: bool,
) {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState;
    use smithay::utils::{Point, Rectangle, Size};

    let Some(window) = element_for_window(state, window_id) else {
        return;
    };
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
    state: &mut LegeCompState<B>,
    window_id: WindowId,
) {
    let record = state.window_service.window(window_id).cloned();
    state.window_service.on_toplevel_destroyed(window_id);

    if let Some(record) = record {
        let pid_still_live = state
            .window_service
            .windows()
            .any(|window| window.peer_pid == record.peer_pid);
        if !pid_still_live {
            state.identity_cache.forget(record.peer_pid);
        }
        if let Some(instance_id) = record.identity.instance_id {
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

/// Re-mirror every live window's geometry, visibility and stacking order into `Space`, re-enrich
/// the governor, and broadcast a fresh sanitized snapshot to the shell and our own toplevel
/// protocol objects. Idempotent and cheap enough to call after any `WindowService`-mutating
/// handler.
///
/// Visibility is `WindowService::is_visible`'s verdict and nothing else: a window it says is
/// hidden (shelved, or a client window behind a "go home" reveal) is *unmapped* from `Space`, so
/// it neither renders nor receives pointer input — `Space` is the render/hit-test mirror, and a
/// window that is in it is on screen. The governor is told `visible = !shelved`, deliberately not
/// the same predicate: a reveal is a momentary view, and must not suspend every application.
pub fn sync_after_change<B: Backend + 'static>(state: &mut LegeCompState<B>) {
    // Bottom-to-top so each `raise_element` call leaves the most-recently-processed (i.e.
    // topmost, per `stacking_order`'s top-first contract) window on top of `Space`'s own order.
    for window_id in state.window_service.stacking_order().into_iter().rev() {
        let Some(record) = state.window_service.window(window_id) else {
            continue;
        };
        let geometry = record.geometry;
        let Some(window) = element_for_window(state, window_id) else {
            continue;
        };
        if state.window_service.is_visible(window_id) {
            state
                .space
                .map_element(window.clone(), (geometry.x, geometry.y), false);
            state.space.raise_element(&window, false);
        } else {
            state.space.unmap_elem(&window);
        }
    }

    for record in state.window_service.windows() {
        if let Some(instance_id) = record.identity.instance_id {
            state
                .governor
                .set_instance_state(instance_id, record.activated, !record.shelved);
        }
    }

    // `publish` hands the snapshot to the broadcaster's background task and returns immediately;
    // the actual `lege_shell_v1` encoding happens back on this thread, but later — decoupled from
    // this call site — when `src/shell_sink.rs`'s calloop channel wakes up
    // (`src/shell_sink.rs`'s module doc explains why encoding can't happen on the background
    // task itself). This is what COMPOSITOR.md means by "never encode/send `lege_shell_v1`
    // events synchronously from the same call site that touched `WindowService`".
    let snapshot = lege_comp_integration::snapshot(&state.window_service);
    state.shell_broadcaster.publish(snapshot);
}
