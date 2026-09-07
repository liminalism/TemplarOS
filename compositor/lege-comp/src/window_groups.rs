//! The compositor's side of window groups: the handful of operations every route into
//! `lege_window`'s group model shares, in one place so the chord, the protocol request and the
//! band drop cannot drift apart.
//!
//! The model itself is `crates/lege-window` (`WindowService::group`, `place_beside`,
//! `shelve_group`, …) — nothing here decides anything about membership or geometry. What lives
//! here is the *compositor's* half of each operation: unmaximizing a window before it is set down
//! beside another, configuring the client that moved, and re-mirroring `Space` afterwards. That
//! is also why these are free functions over `LegeCompState` rather than methods on
//! `WindowService`: they need a display, and `lege-window` must never.

use crate::state::{Backend, LegeCompState};
use crate::window_bridge::now_monotonic_ns;
use lege_window::{CommandContext, CommandEffect, Point, WindowCommand, WindowId};

/// One declared action (`lege_types::action`) that a pointer gesture *in the compositor* performs
/// — as opposed to a shell menu row or region, which is what `xtask check-actions` normally
/// counts. The manifest's rule is that a chord is an accelerator and never the only route; some
/// of those routes are gestures on the workspace, which the shell cannot see and which keep
/// working when it is dead, and this is how they say so.
///
/// Nothing calls this: it is a declaration the check reads out of the source, and a test below
/// keeps what it reads true.
#[allow(dead_code)]
pub struct PointerGesture(pub &'static str);

/// Grouping's pointer route: dropping one window's title band on another's groups the two and
/// sets the dropped window down beside it (`src/input.rs`'s `WindowGestureGrab::button`,
/// [`group_windows`]). It is the *primary* route — the plan's rule 6 — with `Super+G` as its
/// accelerator, which is exactly the shape the action manifest asks for.
#[allow(dead_code)]
pub const POINTER_GESTURES: &[PointerGesture] = &[PointerGesture("window.group")];

fn trusted_ctx<B: Backend + 'static>(state: &LegeCompState<B>) -> CommandContext {
    CommandContext {
        trusted_shell: true,
        now_monotonic_ns: now_monotonic_ns(&state.clock),
    }
}

/// Put `joining` in `anchor`'s group and set it down beside it (`WindowCommand::Group`, planning
/// doc §2 rule 6). Returns whether anything was grouped.
///
/// A maximized window is unmaximized first, through the ordinary route, so the client is told:
/// otherwise it would keep `xdg_toplevel.state.maximized` while sitting in half the screen, and
/// the next output change would re-derive its geometry from the output and quietly undo the
/// placement.
pub fn group_windows<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    anchor: WindowId,
    joining: WindowId,
) -> bool {
    if state
        .window_service
        .window(joining)
        .is_some_and(|window| window.maximized)
    {
        crate::xdg_shell::apply_maximized(state, joining, false);
    }
    let ctx = trusted_ctx(state);
    let effect = state.window_service.handle_command(
        WindowCommand::Group {
            a: anchor,
            b: joining,
        },
        ctx,
    );
    match effect {
        Ok(CommandEffect::Grouped {
            group, geometry, ..
        }) => {
            if let Some(geometry) = geometry {
                // Position only — `place_beside` never changes a window's size — but it goes out
                // as a configure all the same, because an X11 client is told where it is in the
                // same request that tells it how big it is.
                crate::space_sync::apply_resized_geometry(state, joining, geometry, false);
            }
            tracing::debug!(?group, ?anchor, ?joining, "windows grouped");
            crate::space_sync::sync_after_change(state);
            true
        }
        Ok(_) => false,
        Err(error) => {
            tracing::debug!(%error, ?anchor, ?joining, "refused to group");
            false
        }
    }
}

/// Take `window_id` out of its group; the group dissolves if that leaves fewer than two members.
pub fn ungroup_window<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    window_id: WindowId,
) -> bool {
    let ctx = trusted_ctx(state);
    let grouped = state
        .window_service
        .handle_command(WindowCommand::Ungroup { window_id }, ctx)
        .is_ok();
    if grouped {
        crate::space_sync::sync_after_change(state);
    }
    grouped
}

/// Park every member of this window's group under one shelf rank, and hand the keyboard on the
/// way `park_focused` does — a group that parks takes the focused window with it.
pub fn shelve_group<B: Backend + 'static>(state: &mut LegeCompState<B>, window_id: WindowId) {
    let ctx = trusted_ctx(state);
    if state
        .window_service
        .handle_command(WindowCommand::ShelveGroup { window_id }, ctx)
        .is_err()
    {
        return;
    }
    crate::space_sync::sync_after_change(state);
    let any_left =
        state.window_service.stacking_order().into_iter().any(|id| {
            state.window_service.is_visible(id) && state.window_service.is_client_window(id)
        });
    crate::input::refocus_after_reveal(state, !any_left);
}

/// Bring every shelved member of this window's group back, each one re-validated against the
/// current topology by `WindowService::unshelve`.
pub fn unshelve_group<B: Backend + 'static>(state: &mut LegeCompState<B>, window_id: WindowId) {
    let ctx = trusted_ctx(state);
    let Ok(CommandEffect::GroupUnshelved { windows }) = state
        .window_service
        .handle_command(WindowCommand::UnshelveGroup { window_id }, ctx)
    else {
        return;
    };
    for (id, geometry) in windows {
        crate::space_sync::apply_resized_geometry(state, id, geometry, false);
    }
    crate::space_sync::sync_after_change(state);
}

/// The window whose name tag is under `pointer`, ignoring `dragged` — the drop target of a
/// name-tag drag (planning doc §2 rule 6: "only the band is a drop target, so ordinary
/// overlapping placement is untouched"; the name tag is what the band became).
///
/// Walks the stacking order top-first exactly as the press hit test does, so the tag a person
/// can see under the pointer is the one that answers and never one buried beneath it. The dragged
/// window is skipped because it is on top and the pointer is inside *its* tag by definition. A
/// window without a frame (maximized under the bar) has no tag to drop on.
pub fn tag_drop_target<B: Backend + 'static>(
    state: &LegeCompState<B>,
    dragged: WindowId,
    pointer: Point,
) -> Option<WindowId> {
    state
        .window_service
        .stacking_order()
        .into_iter()
        .filter(|id| {
            *id != dragged
                && state.window_service.is_visible(*id)
                && state.window_service.is_client_window(*id)
                && state.window_service.has_frame(*id)
        })
        .find(|id| {
            state
                .window_service
                .recovery_affordance(*id)
                .is_some_and(|affordance| {
                    contains(crate::chrome::layout::name_tag(affordance.strip), pointer)
                })
        })
}

/// The followers of a band drag: every other member of `anchor`'s group, with its offset from the
/// anchor's origin. Empty for an ungrouped window and for a `Shift`-held drag, which moves the
/// one window (planning doc §2.1).
///
/// Offsets rather than a delta per motion event: the followers then track the anchor exactly,
/// with no accumulated rounding, and a repair at the end of the gesture — never during it —
/// keeps them where the drag put them (COMPOSITOR.md's drag rows).
pub fn drag_followers<B: Backend + 'static>(
    state: &LegeCompState<B>,
    anchor: WindowId,
) -> Vec<(WindowId, Point)> {
    let Some(group) = state.window_service.group_of(anchor) else {
        return Vec::new();
    };
    let Some(origin) = state
        .window_service
        .window(anchor)
        .map(|window| window.geometry)
    else {
        return Vec::new();
    };
    state
        .window_service
        .group_members(group)
        .into_iter()
        .filter(|id| *id != anchor)
        .filter_map(|id| {
            let geometry = state.window_service.window(id)?.geometry;
            Some((
                id,
                Point {
                    x: geometry.x.saturating_sub(origin.x),
                    y: geometry.y.saturating_sub(origin.y),
                },
            ))
        })
        .collect()
}

fn contains(rect: lege_window::Rect, point: Point) -> bool {
    point.x >= rect.x && point.x < rect.right() && point.y >= rect.y && point.y < rect.bottom()
}

/// Name this window's group. The shell learns the new name from the next `group` event, like
/// every other fact about a group.
pub fn set_group_name<B: Backend + 'static>(
    state: &mut LegeCompState<B>,
    window_id: WindowId,
    name: String,
) {
    let ctx = trusted_ctx(state);
    if state
        .window_service
        .handle_command(WindowCommand::SetGroupName { window_id, name }, ctx)
        .is_ok()
    {
        crate::space_sync::sync_after_change(state);
    }
}

#[cfg(test)]
mod tests {
    use super::POINTER_GESTURES;

    /// A gesture route naming an action nobody declared would satisfy `xtask check-actions` while
    /// pointing at nothing. The check reads this table out of the source; this test is what keeps
    /// what it reads true.
    #[test]
    fn every_declared_pointer_gesture_names_a_real_action() {
        for gesture in POINTER_GESTURES {
            assert!(
                lege_types::action::action(gesture.0).is_some(),
                "{} is not a declared action",
                gesture.0
            );
        }
    }
}
