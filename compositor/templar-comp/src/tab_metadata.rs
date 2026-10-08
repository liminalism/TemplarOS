//! Authenticated sessiond count feed. No focus, title, or X11 PID-property binding.
use crate::state::{Backend, TemplarCompState};
use smithay::reexports::wayland_server::Resource;
use std::collections::HashMap;
use templar_types::compositor_control::{ControlError, ControlReply, ControlResponse, TabCountUpdate, TabWindow, TabWindows};
use templar_window::{TabCount, WindowId};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Debug)]
pub struct TabMetadata {
    owner: Uuid,
    windows: Vec<TabWindow>,
    subscriber: Option<watch::Sender<Option<ControlReply>>>,
    pub counts: HashMap<WindowId, TabCount>,
    peers: HashMap<WindowId, (u32, Uuid)>,
}
impl Default for TabMetadata {
    fn default() -> Self {
        Self { owner: Uuid::new_v4(), windows: Vec::new(), subscriber: None, counts: HashMap::new(), peers: HashMap::new() }
    }
}
pub fn observation<B: Backend + 'static>(state: &TemplarCompState<B>) -> TabWindows {
    TabWindows { owner: state.tab_metadata.owner, windows: state.tab_metadata.windows.clone() }
}
fn sole(windows: &[TabWindow], id: u64, pid: u32) -> bool {
    pid != 0 && windows.iter().filter(|window| window.peer_pid == pid).count() == 1
        && windows.iter().any(|window| window.window_id == id && window.peer_pid == pid)
}
/// Called from the existing window lifecycle funnel, including park/remap/destroy.
pub fn publish<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let mut windows: Vec<_> = state.window_service.windows().filter_map(|record| {
        let surface = state.windows.surface_for(record.id)?;
        state.windows.element_for(&surface.id())?.toplevel()?;
        let credentials = surface.client()?.get_credentials(&state.display_handle).ok()?;
        let peer_pid = u32::try_from(credentials.pid).ok().filter(|pid| *pid != 0)?;
        let binding = state.tab_metadata.windows.iter().find(|old| old.window_id == record.id.0 && old.peer_pid == peer_pid)
            .map_or_else(Uuid::new_v4, |old| old.binding);
        Some(TabWindow { window_id: record.id.0, peer_pid, binding })
    }).collect();
    windows.sort_unstable_by_key(|window| window.window_id);
    if windows == state.tab_metadata.windows { return; }
    // A transient second same-process window destroys the sole-window proof. Carry
    // this fence in the snapshot even if the watch channel coalesces 1 -> 2 -> 1.
    let changed_peers: Vec<_> = windows.iter().filter(|window| {
        state.tab_metadata.windows.iter().filter(|old| old.peer_pid == window.peer_pid).count()
            != windows.iter().filter(|new| new.peer_pid == window.peer_pid).count()
    }).map(|window| window.peer_pid).collect();
    for window in &mut windows {
        if changed_peers.contains(&window.peer_pid) { window.binding = Uuid::new_v4(); }
    }
    state.tab_metadata.windows = windows;
    let metadata = &mut state.tab_metadata;
    metadata.peers.retain(|id, (pid, binding)| sole(&metadata.windows, id.0, *pid)
        && metadata.windows.iter().any(|window| window.window_id == id.0 && window.binding == *binding));
    metadata.counts.retain(|id, _| metadata.peers.contains_key(id));
    if let Some(sender) = &metadata.subscriber {
        sender.send_replace(Some(ControlReply { result: Ok(ControlResponse::TabWindows(TabWindows { owner: metadata.owner, windows: metadata.windows.clone() })) }));
    }
}
fn redraw<B: Backend + 'static>(state: &mut TemplarCompState<B>) {
    let snapshot = templar_comp_integration::snapshot(&state.window_service, &state.tab_metadata.counts);
    state.shell_broadcaster.publish(snapshot);
    B::request_redraw(state);
}
pub fn subscribe<B: Backend + 'static>(state: &mut TemplarCompState<B>, sender: watch::Sender<Option<ControlReply>>) {
    state.tab_metadata = TabMetadata::default();
    state.tab_metadata.subscriber = Some(sender);
    publish(state);
    redraw(state);
}
pub fn disconnect<B: Backend + 'static>(state: &mut TemplarCompState<B>, owner: Uuid) {
    if state.tab_metadata.owner != owner { return; }
    state.tab_metadata.subscriber = None;
    state.tab_metadata.counts.clear();
    state.tab_metadata.peers.clear();
    redraw(state);
}
pub fn set_counts<B: Backend + 'static>(state: &mut TemplarCompState<B>, owner: Uuid, updates: Vec<TabCountUpdate>) -> Result<(), ControlError> {
    publish(state);
    let metadata = &mut state.tab_metadata;
    if owner != metadata.owner || metadata.subscriber.as_ref().is_none_or(|sender| sender.is_closed()) {
        return Err(ControlError { code: "stale_owner".into(), message: "The tab metadata owner connection ended".into() });
    }
    let mut counts = HashMap::new();
    let mut peers = HashMap::new();
    for update in updates {
        if !sole(&metadata.windows, update.window_id, update.peer_pid)
            || !metadata.windows.iter().any(|window| window.window_id == update.window_id && window.binding == update.binding) { continue; }
        if let Some(count) = TabCount::verified(update.count, update.generation, update.provenance) {
            let id = WindowId(update.window_id);
            counts.insert(id, count);
            peers.insert(id, (update.peer_pid, update.binding));
        }
    }
    if counts != metadata.counts {
        metadata.counts = counts;
        metadata.peers = peers;
        redraw(state);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sole_native_mapping_rejects_foreign_and_ambiguous_windows() {
        let mut windows = vec![TabWindow { window_id: 4, peer_pid: 20, binding: Uuid::new_v4() }];
        assert!(sole(&windows, 4, 20));
        assert!(!sole(&windows, 4, 21));
        assert!(!sole(&windows, 5, 20));
        windows.push(TabWindow { window_id: 5, peer_pid: 20, binding: Uuid::new_v4() });
        assert!(!sole(&windows, 4, 20));
        assert!(!sole(&[], 4, 20));
    }
}
