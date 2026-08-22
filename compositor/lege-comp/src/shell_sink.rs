//! The [`ShellStateSink`] the compositor gives `ShellStateBroadcaster::spawn`, and the plumbing
//! that gets a snapshot back onto the render thread to actually become `lege_shell_v1` events.
//!
//! `ShellStateSink::send` runs on `lege-comp-integration`'s background tokio task, off the render
//! thread — but `wayland_server::Resource`s (our `LegeShellToplevelV1` objects) are bound to the
//! thread that owns the `Display`, so nothing here may touch one directly. [`RenderThreadSink`]
//! only forwards the already-computed snapshot across a `calloop::channel` (the one primitive in
//! this crate meant for exactly this: waking the render thread's own event loop from another
//! thread); the real encoding (`protocols_shell::sync_toplevels`/`sync_foreign_toplevels`) runs in
//! that channel's calloop callback, i.e. back on the render thread, but strictly later than —
//! decoupled from — whatever `WindowService` call produced the snapshot
//! (`src/space_sync.rs::sync_after_change`'s doc; COMPOSITOR.md's broadcast row).

use crate::state::{Backend, LegeCompState};
use lege_comp_integration::{ShellStateSink, ShellStateSnapshot};
use smithay::reexports::calloop::{
    LoopHandle,
    channel::{self, Event},
};

pub struct RenderThreadSink {
    tx: channel::Sender<ShellStateSnapshot>,
}

impl ShellStateSink for RenderThreadSink {
    fn send(&mut self, snapshot: ShellStateSnapshot) {
        // A full channel/closed receiver only happens once the render thread itself is gone, at
        // which point there is nothing left to tell anyway.
        let _ = self.tx.send(snapshot);
    }
}

/// Build the sink and register its receiving half on `loop_handle`. Called once during
/// `LegeCompState::new`, before `ShellStateBroadcaster::spawn` is given the returned sink.
pub fn install<B: Backend + 'static>(
    loop_handle: &LoopHandle<'static, LegeCompState<B>>,
) -> RenderThreadSink {
    let (tx, rx) = channel::channel::<ShellStateSnapshot>();
    loop_handle
        .insert_source(rx, |event, (), state: &mut LegeCompState<B>| {
            if let Event::Msg(snapshot) = event {
                // Foreign handles first: `sync_toplevels` relays each window's
                // `ext_foreign_toplevel_handle_v1.identifier`, which only exists once its handle does.
                crate::foreign_toplevel::sync(state);
                crate::protocols_shell::sync_toplevels(state, &snapshot);
            }
        })
        .expect("inserting the shell-state channel source");
    RenderThreadSink { tx }
}
