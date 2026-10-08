//! The [`ShellStateSink`] the compositor gives `ShellStateBroadcaster::spawn`, and the plumbing
//! that gets a snapshot back onto the render thread to actually become `templar_shell_v2` events.
//!
//! `ShellStateSink::send` runs on `templar-comp-integration`'s background tokio task, off the render
//! thread — but `wayland_server::Resource`s (our `TemplarShellToplevelV1` objects) are bound to the
//! thread that owns the `Display`, so nothing here may touch one directly. [`RenderThreadSink`]
//! only forwards the already-computed snapshot across a `calloop::channel` (the one primitive in
//! this crate meant for exactly this: waking the render thread's own event loop from another
//! thread); the real encoding (`protocols_shell::sync_toplevels`/`sync_foreign_toplevels`) runs in
//! that channel's calloop callback, i.e. back on the render thread, but strictly later than —
//! decoupled from — whatever `WindowService` call produced the snapshot
//! (`src/space_sync.rs::sync_after_change`'s doc; COMPOSITOR.md's broadcast row).

use crate::state::{Backend, TemplarCompState};
use smithay::reexports::calloop::{
    LoopHandle,
    channel::{self, Event},
};
use std::sync::{Arc, Mutex};
use templar_comp_integration::{ShellStateSink, ShellStateSnapshot};

pub struct RenderThreadSink {
    latest: Arc<Mutex<Option<ShellStateSnapshot>>>,
    wake: channel::Sender<()>,
}

impl ShellStateSink for RenderThreadSink {
    fn send(&mut self, snapshot: ShellStateSnapshot) {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let needs_wake = latest.is_none();
        *latest = Some(snapshot);
        drop(latest);
        if needs_wake {
            // A closed receiver only happens once the render thread itself is gone, at which
            // point there is nothing left to tell anyway.
            let _ = self.wake.send(());
        }
    }
}

/// Build the sink and register its receiving half on `loop_handle`. Called once during
/// `TemplarCompState::new`, before `ShellStateBroadcaster::spawn` is given the returned sink.
pub fn install<B: Backend + 'static>(
    loop_handle: &LoopHandle<'static, TemplarCompState<B>>,
) -> RenderThreadSink {
    let latest = Arc::new(Mutex::new(None::<ShellStateSnapshot>));
    let callback_latest = Arc::clone(&latest);
    let (wake, rx) = channel::channel::<()>();
    loop_handle
        .insert_source(rx, move |event, (), state: &mut TemplarCompState<B>| {
            if let Event::Msg(()) = event
                && let Some(snapshot) = callback_latest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
            {
                // Foreign handles first: `sync_toplevels` relays each window's
                // `ext_foreign_toplevel_handle_v1.identifier`, which only exists once its handle does.
                crate::foreign_toplevel::sync(state);
                crate::protocols_shell::sync_toplevels(state, &snapshot);
            }
        })
        .expect("inserting the shell-state channel source");
    RenderThreadSink { latest, wake }
}
