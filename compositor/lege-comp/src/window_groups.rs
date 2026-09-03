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
use lege_window::{CommandContext, CommandEffect, WindowCommand, WindowId};

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
