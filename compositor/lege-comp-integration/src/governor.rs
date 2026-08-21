//! Governor enrichment: tells `lege-resourced` which instance is on screen
//! (`org.legeos.Resources1.SetInstanceState`) and when one has no windows left
//! (`ForgetInstance`). The contract is `interfaces/dbus/org.legeos.Resources1.xml`; the proxy is
//! `lege_client::proxies::ResourcesProxy` — both owned by the session-daemon/governor track, read
//! here, never edited.
//!
//! `lege-resourced` never *needs* this to contain a runaway application (unit names are
//! self-describing, so cgroup → instance works from `/sys/fs/cgroup` alone, ADR-0002); this is
//! enrichment only, so it is safe to coalesce, delay, drop or fail silently — never safe to block
//! the render thread for.

use lege_client::proxies::ResourcesProxy;
use lege_types::InstanceId;
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc;

/// How long to batch focus/visibility churn before flushing to the governor. Window activation
/// during a fast alt-tab sequence can generate several state changes a frame; only the final
/// state within one window matters to the governor.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InstanceState {
    foreground: bool,
    visible: bool,
}

#[derive(Clone, Copy, Debug)]
enum Event {
    State(InstanceId, InstanceState),
    Forget(InstanceId),
}

/// Sends focus/visibility enrichment and end-of-life notices to `lege-resourced`. Cheap to
/// clone; every clone shares the same background task and coalescing buffer.
#[derive(Clone, Debug)]
pub struct GovernorNotifier {
    tx: mpsc::UnboundedSender<Event>,
}

impl GovernorNotifier {
    /// Spawn the background task on `handle`. Never blocks; if `org.legeos.Resources` is not on
    /// the bus (governor not started yet, or restarting), every call up to that point is
    /// dropped silently and picked back up automatically once a connection succeeds.
    pub fn spawn(handle: &tokio::runtime::Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(run(rx));
        Self { tx }
    }

    /// Focus or visibility changed for `instance_id`. Safe to call every frame; only the state
    /// at the end of the coalescing window is actually sent.
    pub fn set_instance_state(&self, instance_id: InstanceId, foreground: bool, visible: bool) {
        let _ = self.tx.send(Event::State(
            instance_id,
            InstanceState {
                foreground,
                visible,
            },
        ));
    }

    /// `instance_id`'s last window just closed.
    pub fn forget_instance(&self, instance_id: InstanceId) {
        let _ = self.tx.send(Event::Forget(instance_id));
    }
}

async fn run(mut events: mpsc::UnboundedReceiver<Event>) {
    // `ResourcesProxy` owns everything it needs internally (zbus connections are Arc-backed), so
    // no separate `Connection` binding needs to be kept alive alongside it.
    let mut proxy: Option<ResourcesProxy<'_>> = None;

    loop {
        // Wait for the first event of a new batch, then drain whatever else arrives within the
        // coalescing window before touching the bus at all.
        let Some(first) = events.recv().await else {
            return;
        };

        let mut pending_state: HashMap<InstanceId, InstanceState> = HashMap::new();
        let mut pending_forget: Vec<InstanceId> = Vec::new();
        apply(first, &mut pending_state, &mut pending_forget);

        let deadline = tokio::time::Instant::now() + COALESCE_WINDOW;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                next = events.recv() => match next {
                    Some(event) => apply(event, &mut pending_state, &mut pending_forget),
                    None => break,
                },
            }
        }

        if proxy.is_none() {
            match zbus::Connection::session().await {
                Ok(new_connection) => match ResourcesProxy::new(&new_connection).await {
                    Ok(new_proxy) => proxy = Some(new_proxy),
                    Err(error) => {
                        tracing::debug!(%error, "Resources1 proxy unavailable; governor enrichment dropped for this batch");
                    }
                },
                Err(error) => {
                    tracing::debug!(%error, "no session bus; governor enrichment dropped for this batch");
                }
            }
        }

        let Some(active) = &proxy else {
            continue;
        };

        let mut connection_lost = false;
        for (instance_id, state) in pending_state {
            let instance_id = instance_id.to_string();
            if let Err(error) = active
                .set_instance_state(&instance_id, state.foreground, state.visible)
                .await
            {
                tracing::debug!(%error, %instance_id, "SetInstanceState failed");
                connection_lost = true;
            }
        }
        for instance_id in pending_forget {
            let instance_id = instance_id.to_string();
            if let Err(error) = active.forget_instance(&instance_id).await {
                tracing::debug!(%error, %instance_id, "ForgetInstance failed");
                connection_lost = true;
            }
        }

        // As in the identity cache: treat a call failure as "maybe the governor restarted" and
        // reconnect from scratch next batch, rather than staying wedged on a dead proxy forever.
        if connection_lost {
            proxy = None;
        }
    }
}

fn apply(
    event: Event,
    pending_state: &mut HashMap<InstanceId, InstanceState>,
    pending_forget: &mut Vec<InstanceId>,
) {
    match event {
        Event::State(instance_id, state) => {
            pending_state.insert(instance_id, state);
        }
        Event::Forget(instance_id) => {
            pending_state.remove(&instance_id);
            pending_forget.push(instance_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgetting_an_instance_cancels_its_pending_state() {
        let mut state = HashMap::new();
        let mut forget = Vec::new();
        let instance = InstanceId::new();

        apply(
            Event::State(
                instance,
                InstanceState {
                    foreground: true,
                    visible: true,
                },
            ),
            &mut state,
            &mut forget,
        );
        assert_eq!(state.len(), 1);

        apply(Event::Forget(instance), &mut state, &mut forget);
        assert!(state.is_empty());
        assert_eq!(forget, vec![instance]);
    }

    #[test]
    fn later_state_for_the_same_instance_overwrites_the_earlier_one() {
        let mut state = HashMap::new();
        let mut forget = Vec::new();
        let instance = InstanceId::new();

        apply(
            Event::State(
                instance,
                InstanceState {
                    foreground: false,
                    visible: true,
                },
            ),
            &mut state,
            &mut forget,
        );
        apply(
            Event::State(
                instance,
                InstanceState {
                    foreground: true,
                    visible: true,
                },
            ),
            &mut state,
            &mut forget,
        );
        assert_eq!(state.len(), 1);
        assert!(state[&instance].foreground);
    }

    #[tokio::test]
    async fn calls_with_no_session_bus_do_not_panic_or_block() {
        let handle = tokio::runtime::Handle::current();
        let notifier = GovernorNotifier::spawn(&handle);
        notifier.set_instance_state(InstanceId::new(), true, true);
        notifier.forget_instance(InstanceId::new());
        // Give the background task a chance to run and hit the (expected) connection failure;
        // the assertion is simply that this test completes instead of hanging or panicking.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
