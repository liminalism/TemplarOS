//! `ext-foreign-toplevel-list-v1`: the generic, title/app-id-only toplevel listing COMPOSITOR.md
//! lists as a required protocol for third-party listeners (screen readers, task switchers that
//! aren't `lege-shell`). `lege_shell_v1` (`src/protocols_shell.rs`) is the richer, shell-specific
//! channel; this one exists for everyone else and carries nothing beyond what the standard
//! protocol itself defines.

use crate::state::{Backend, LegeCompState};
use lege_window::WindowId;
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct ForeignToplevelState {
    handles: HashMap<WindowId, ForeignToplevelHandle>,
}

impl<B: Backend + 'static> ForeignToplevelListHandler for LegeCompState<B> {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

/// Bring every handle's title/app-id up to date with `WindowService`, creating one for each
/// newly-mapped window and closing+dropping one for each window no longer present. Called from
/// `src/shell_sink.rs`'s calloop channel callback, alongside `protocols_shell::sync_toplevels`.
pub fn sync<B: Backend + 'static>(state: &mut LegeCompState<B>) {
    let live: Vec<(WindowId, String, String)> = state
        .window_service
        .shell_toplevels()
        .into_iter()
        .map(|toplevel| {
            (
                toplevel.window_id,
                toplevel.title,
                toplevel
                    .canonical_app_id
                    .map(|id| id.to_string())
                    .unwrap_or_default(),
            )
        })
        .collect();
    let live_ids: std::collections::HashSet<WindowId> = live.iter().map(|(id, _, _)| *id).collect();

    for (window_id, title, app_id) in live {
        let handle = state
            .foreign_toplevels
            .handles
            .entry(window_id)
            .or_insert_with(|| {
                state
                    .foreign_toplevel_list_state
                    .new_toplevel::<LegeCompState<B>>(title.clone(), app_id.clone())
            });
        handle.send_title(&title);
        handle.send_app_id(&app_id);
        handle.send_done();
    }

    state.foreign_toplevels.handles.retain(|window_id, handle| {
        if live_ids.contains(window_id) {
            true
        } else {
            handle.send_closed();
            false
        }
    });
}
