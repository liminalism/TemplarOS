//! `ext-foreign-toplevel-list-v1`: the generic, title/app-id-only toplevel listing COMPOSITOR.md
//! lists as a required protocol for third-party listeners (screen readers, task switchers that
//! aren't `templar-shell`). `templar_shell_v2` (`src/protocols_shell.rs`) is the richer, shell-specific
//! channel; this one exists for everyone else and carries nothing beyond what the standard
//! protocol itself defines.

use crate::state::{Backend, TemplarCompState};
use smithay::wayland::foreign_toplevel_list::{
    ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState,
};
use std::collections::HashMap;
use templar_window::WindowId;

#[derive(Debug, Default)]
pub struct ForeignToplevelState {
    handles: HashMap<WindowId, ForeignToplevelHandle>,
    /// Each handle's `ext_foreign_toplevel_handle_v1.identifier`, which `templar_shell_v2` relays so
    /// the shell can pair a capture of a window (obtained through the standard protocols, which
    /// name windows by this identifier) with the window's `templar_shell_toplevel_v1`.
    identifiers: HashMap<WindowId, String>,
}

impl ForeignToplevelState {
    pub fn identifier_for(&self, window_id: WindowId) -> Option<&str> {
        self.identifiers.get(&window_id).map(String::as_str)
    }

    pub fn window_for_identifier(&self, identifier: &str) -> Option<WindowId> {
        self.identifiers
            .iter()
            .find(|(_, candidate)| candidate.as_str() == identifier)
            .map(|(window_id, _)| *window_id)
    }
}

impl<B: Backend + 'static> ForeignToplevelListHandler for TemplarCompState<B> {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.foreign_toplevel_list_state
    }
}

/// Bring every handle's title/app-id up to date with `WindowService`, creating one for each
/// newly-mapped window and closing+dropping one for each window no longer present. Called from
/// `src/shell_sink.rs`'s calloop channel callback, alongside `protocols_shell::sync_toplevels`.
pub fn sync<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
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
                    .new_toplevel::<TemplarCompState<B>>(title.clone(), app_id.clone())
            });
        state
            .foreign_toplevels
            .identifiers
            .entry(window_id)
            .or_insert_with(|| handle.identifier());
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
    state
        .foreign_toplevels
        .identifiers
        .retain(|window_id, _| live_ids.contains(window_id));
}
