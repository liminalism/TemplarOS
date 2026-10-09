//! Shell state broadcaster: turns `WindowService` state into sanitized snapshots for the shell,
//! computed off the render thread.
//!
//! [`snapshot`] itself is a cheap, synchronous, render-thread-safe read (it only clones what
//! `WindowService` already exposes — no D-Bus, no I/O). What must stay off the render thread is
//! turning that snapshot into `templar_shell_v2` protocol events: encoding and the actual Wayland
//! write can stall behind a slow or wedged shell client. [`ShellStateBroadcaster::publish`]
//! hands the snapshot to a channel and returns immediately; a background task drains the channel
//! and calls a [`ShellStateSink`] the fork implements over the real protocol object.

use std::collections::HashMap;
use templar_window::{IdentityCache, ShellToplevel, TabCount, WindowId, WindowService};
use tokio::sync::watch;

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
    /// Whether a "go home" reveal is in effect (`WindowService::home_revealed`).
    pub home_revealed: bool,
}

/// Build one snapshot from the current `WindowService` state. Called from the render thread
/// after any change worth telling the shell about (map/unmap, activation, shelve/unshelve,
/// layer change) — it is a handful of clones of already-computed data, not a scan.
pub fn snapshot<I: IdentityCache>(
    service: &WindowService<I>,
    tab_counts: &HashMap<WindowId, TabCount>,
) -> ShellStateSnapshot {
    let mut snapshot = ShellStateSnapshot {
        toplevels: service.shell_toplevels(),
        shelf: service.shelf(),
        stacking_order: service.stacking_order(),
        home_revealed: service.home_revealed(),
    };
    // Counts participate in snapshot equality, so a count-only update wakes the
    // broadcaster just like a title change. Keep parked and ordinary projections
    // on the same committed metadata, including unknown after invalidation.
    for window in snapshot
        .toplevels
        .iter_mut()
        .chain(snapshot.shelf.iter_mut())
    {
        window.tab_count = tab_counts.get(&window.window_id).cloned();
    }
    snapshot
}

/// What the fork's `templar_shell_v2` implementation does with one broadcast: turn it into
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
    tx: watch::Sender<Option<ShellStateSnapshot>>,
}

impl ShellStateBroadcaster {
    /// Spawn the sink task on `handle`. The pending slot keeps only the newest snapshot: every
    /// snapshot describes the complete shell-visible state, so replaying intermediate states
    /// after a burst would add latency without adding information.
    pub fn spawn<S: ShellStateSink>(handle: &tokio::runtime::Handle, mut sink: S) -> Self {
        let (tx, mut rx) = watch::channel::<Option<ShellStateSnapshot>>(None);
        handle.spawn(async move {
            while rx.changed().await.is_ok() {
                if let Some(snapshot) = rx.borrow_and_update().clone() {
                    sink.send(snapshot);
                }
            }
        });
        Self { tx }
    }

    /// Called from the render thread; never blocks.
    pub fn publish(&self, snapshot: ShellStateSnapshot) {
        self.tx.send_if_modified(|pending| {
            if pending.as_ref() == Some(&snapshot) {
                false
            } else {
                *pending = Some(snapshot);
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use templar_window::{CachedIdentity, ClientKind, Layer, Output, Rect, WindowClaims};

    struct NullCache;
    impl IdentityCache for NullCache {
        fn resolve_cached(&self, _pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
            None
        }
        fn request_resolution(&self, _window_id: WindowId, _pid: u32, _claims: WindowClaims) {}
    }

    #[test]
    fn snapshot_reflects_layer_and_shelf_state() {
        let outputs = vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)];
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

        let snap = snapshot(&service, &HashMap::new());
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
            home_revealed: false,
        };
        broadcaster.publish(snap.clone());

        // publish() itself never awaits; yield so the background task gets a turn before we
        // assert.
        tokio::task::yield_now().await;
        assert_eq!(received.lock().unwrap().as_slice(), &[snap]);
    }

    #[tokio::test]
    async fn count_only_updates_and_invalidation_reach_live_and_shelf_consumers() {
        let mut service = WindowService::new(
            NullCache,
            vec![Output::new("test", Rect::new(0, 0, 1920, 1080), 1000)],
        );
        let window = service.on_toplevel_mapped(
            42,
            ClientKind::Application,
            WindowClaims::default(),
            "unchanging title".into(),
            Rect::new(0, 0, 100, 100),
        );
        service.shelve(window);
        struct CountSink(tokio::sync::mpsc::UnboundedSender<ShellStateSnapshot>);
        impl ShellStateSink for CountSink {
            fn send(&mut self, snapshot: ShellStateSnapshot) {
                self.0.send(snapshot).unwrap();
            }
        }
        let (sender, mut received) = tokio::sync::mpsc::unbounded_channel();
        let broadcaster =
            ShellStateBroadcaster::spawn(&tokio::runtime::Handle::current(), CountSink(sender));
        let mut counts = HashMap::new();
        // No geometry, title, stacking, activation or shelf state changes here.
        for next in [None, Some(3), Some(4), None] {
            match next {
                Some(count) => {
                    counts.insert(
                        window,
                        TabCount::verified(count, 1, "native".into()).unwrap(),
                    );
                }
                None => {
                    counts.remove(&window);
                }
            }
            broadcaster.publish(snapshot(&service, &counts));
            let snapshot = tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
                .await
                .expect("count-only change must wake the broadcaster")
                .expect("snapshot consumer is connected");
            assert_eq!(
                snapshot.toplevels[0]
                    .tab_count
                    .as_ref()
                    .map(|count| count.count),
                next
            );
            assert_eq!(
                snapshot.shelf[0]
                    .tab_count
                    .as_ref()
                    .map(|count| count.count),
                next
            );
        }
    }

    #[tokio::test]
    async fn a_burst_keeps_only_the_latest_complete_snapshot() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let broadcaster = ShellStateBroadcaster::spawn(
            &tokio::runtime::Handle::current(),
            RecordingSink {
                received: Arc::clone(&received),
            },
        );

        for rank in 1..=3 {
            broadcaster.publish(ShellStateSnapshot {
                toplevels: Vec::new(),
                shelf: Vec::new(),
                stacking_order: vec![WindowId(rank)],
                home_revealed: false,
            });
        }

        // The background task cannot run on this current-thread runtime until this first await,
        // so all three writes contend for the same pending slot deterministically.
        tokio::task::yield_now().await;
        assert_eq!(received.lock().unwrap().len(), 1);
        assert_eq!(
            received.lock().unwrap()[0].stacking_order,
            vec![WindowId(3)]
        );
    }
}
