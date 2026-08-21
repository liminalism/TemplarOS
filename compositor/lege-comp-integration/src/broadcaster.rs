//! Shell state broadcaster: turns `WindowService` state into sanitized snapshots for the shell,
//! computed off the render thread.
//!
//! [`snapshot`] itself is a cheap, synchronous, render-thread-safe read (it only clones what
//! `WindowService` already exposes — no D-Bus, no I/O). What must stay off the render thread is
//! turning that snapshot into `lege_shell_v1` protocol events: encoding and the actual Wayland
//! write can stall behind a slow or wedged shell client. [`ShellStateBroadcaster::publish`]
//! hands the snapshot to a channel and returns immediately; a background task drains the channel
//! and calls a [`ShellStateSink`] the fork implements over the real protocol object.

use lege_window::{IdentityCache, ShellToplevel, WindowId, WindowService};
use tokio::sync::mpsc;

/// Everything the shell needs to redraw the dock, switcher and Window Shelf after one state
/// change. Cloned from `WindowService` — no PIDs, no raw claims (`ShellToplevel` is already
/// sanitized).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellStateSnapshot {
    pub toplevels: Vec<ShellToplevel>,
    /// The subset of `toplevels` that are shelved, most-recently-shelved first — the Window
    /// Shelf's rendering order.
    pub shelf: Vec<ShellToplevel>,
    /// Every window, top-most first, across all layers.
    pub stacking_order: Vec<WindowId>,
}

/// Build one snapshot from the current `WindowService` state. Called from the render thread
/// after any change worth telling the shell about (map/unmap, activation, shelve/unshelve,
/// layer change) — it is a handful of clones of already-computed data, not a scan.
pub fn snapshot<I: IdentityCache>(service: &WindowService<I>) -> ShellStateSnapshot {
    ShellStateSnapshot {
        toplevels: service.shell_toplevels(),
        shelf: service.shelf(),
        stacking_order: service.stacking_order(),
    }
}

/// What the fork's `lege_shell_v1` implementation does with one broadcast: turn it into
/// protocol events on the shell's connection (or drop it if no shell is bound — `set_layer_surface`
/// is how the shell claims `Layer::ShellOverlay` / `Layer::Notification` / `Layer::HomeSurface`
/// surfaces in the first place, so the sink knows whether one is currently listening). Implemented
/// in the fork; this crate only decides *when* and *what* to send.
pub trait ShellStateSink: Send + 'static {
    fn send(&mut self, snapshot: ShellStateSnapshot);
}

/// Publishes [`ShellStateSnapshot`]s from the render thread to a [`ShellStateSink`] running on a
/// background task, so a slow shell client can never stall the compositor.
#[derive(Clone, Debug)]
pub struct ShellStateBroadcaster {
    tx: mpsc::UnboundedSender<ShellStateSnapshot>,
}

impl ShellStateBroadcaster {
    /// Spawn the sink task on `handle`. Snapshots are delivered in publish order; a sink that
    /// falls behind only delays the shell's view, never the compositor (the channel is
    /// unbounded because a snapshot is small and infrequent — bounded by window-state changes,
    /// not by frames).
    pub fn spawn<S: ShellStateSink>(handle: &tokio::runtime::Handle, mut sink: S) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<ShellStateSnapshot>();
        handle.spawn(async move {
            while let Some(snapshot) = rx.recv().await {
                sink.send(snapshot);
            }
        });
        Self { tx }
    }

    /// Called from the render thread; never blocks.
    pub fn publish(&self, snapshot: ShellStateSnapshot) {
        let _ = self.tx.send(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_window::{CachedIdentity, ClientKind, Layer, Output, Rect, WindowClaims};
    use std::sync::{Arc, Mutex};

    struct NullCache;
    impl IdentityCache for NullCache {
        fn resolve_cached(&self, _pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
            None
        }
        fn request_resolution(&self, _window_id: WindowId, _pid: u32, _claims: WindowClaims) {}
    }

    #[test]
    fn snapshot_reflects_layer_and_shelf_state() {
        let outputs = vec![Output {
            name: "eDP-1".into(),
            logical: Rect::new(0, 0, 1920, 1080),
            scale_milli: 1000,
        }];
        let mut service = WindowService::new(NullCache, outputs);
        let normal = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "a".into(),
            Rect::new(0, 0, 100, 100),
        );
        let shelved = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "b".into(),
            Rect::new(0, 0, 100, 100),
        );
        service.shelve(shelved);

        let snap = snapshot(&service);
        assert_eq!(snap.toplevels.len(), 2);
        assert_eq!(snap.shelf.len(), 1);
        assert_eq!(snap.shelf[0].window_id, shelved);
        assert!(snap.stacking_order.contains(&normal));
        assert_eq!(
            snap.toplevels
                .iter()
                .find(|t| t.window_id == normal)
                .unwrap()
                .layer,
            Layer::Normal
        );
    }

    struct RecordingSink {
        received: Arc<Mutex<Vec<ShellStateSnapshot>>>,
    }
    impl ShellStateSink for RecordingSink {
        fn send(&mut self, snapshot: ShellStateSnapshot) {
            self.received.lock().unwrap().push(snapshot);
        }
    }

    #[tokio::test]
    async fn publish_reaches_the_sink_off_the_caller() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let handle = tokio::runtime::Handle::current();
        let broadcaster = ShellStateBroadcaster::spawn(
            &handle,
            RecordingSink {
                received: Arc::clone(&received),
            },
        );

        let snap = ShellStateSnapshot {
            toplevels: Vec::new(),
            shelf: Vec::new(),
            stacking_order: Vec::new(),
        };
        broadcaster.publish(snap.clone());

        // publish() itself never awaits (it's an unbounded send); yield so the background task
        // gets a turn before we assert.
        tokio::task::yield_now().await;
        assert_eq!(received.lock().unwrap().as_slice(), &[snap]);
    }
}
