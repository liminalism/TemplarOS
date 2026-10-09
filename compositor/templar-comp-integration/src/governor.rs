//! Focus/visibility enrichment never blocks rendering. Kernel PID membership resolves native
//! applications, explicit workloads and observed Flatpak scopes without granting an AppId.
//! Multiple window-owning processes in one resource are combined with OR, not last-writer wins.

use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use templar_client::proxies::ResourcesProxy;
use templar_types::{InstanceId, ResourceId};
use tokio::sync::mpsc;

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
    Process(u32, InstanceState),
    ProcessGone(u32),
}

#[derive(Clone, Debug)]
pub struct GovernorNotifier {
    tx: mpsc::UnboundedSender<Event>,
}

impl GovernorNotifier {
    pub fn spawn(handle: &tokio::runtime::Handle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        handle.spawn(run(rx));
        Self { tx }
    }

    /// Fallback for a genuinely resolved native instance whose peer PID is unavailable.
    pub fn set_instance_state(&self, instance_id: InstanceId, foreground: bool, visible: bool) {
        let _ = self.tx.send(Event::State(
            instance_id,
            InstanceState {
                foreground,
                visible,
            },
        ));
    }

    pub fn forget_instance(&self, instance_id: InstanceId) {
        let _ = self.tx.send(Event::Forget(instance_id));
    }

    /// The caller must aggregate all windows belonging to this actual peer PID first.
    pub fn set_process_state(&self, pid: u32, foreground: bool, visible: bool) {
        let _ = self.tx.send(Event::Process(
            pid,
            InstanceState {
                foreground,
                visible,
            },
        ));
    }

    /// Remove the PID when its last window disappears. This does not forget a headless workload.
    pub fn forget_process(&self, pid: u32) {
        let _ = self.tx.send(Event::ProcessGone(pid));
    }
}

async fn run(mut events: mpsc::UnboundedReceiver<Event>) {
    let mut instances = HashMap::new();
    let mut processes = HashMap::new();
    let mut forgotten = Vec::new();
    let mut combined = HashMap::new();
    let mut published: HashMap<ResourceId, InstanceState> = HashMap::new();
    let mut departed = Vec::new();

    loop {
        let Some(first) = events.recv().await else {
            return;
        };
        apply(first, &mut instances, &mut processes, &mut forgotten);
        let setup = async {
            let connection = zbus::Connection::session().await?;
            let proxy = ResourcesProxy::new(&connection).await?;
            let bus = zbus::fdo::DBusProxy::new(&connection).await?;
            // Subscribe before resolving PIDs. First-tick membership and owner acquisition can
            // otherwise be missed while the compositor is idle.
            let owners = bus.receive_name_owner_changed().await?;
            let targets = proxy.receive_targets_changed().await?;
            Ok::<_, zbus::Error>((proxy, owners, targets, connection))
        }
        .await;
        let (mut active, mut owners, mut targets, connection) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                tracing::debug!(%error, "governor enrichment unavailable; retaining window state");
                continue;
            }
        };
        published.clear();

        'connected: loop {
            let deadline = tokio::time::Instant::now() + COALESCE_WINDOW;
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => break,
                    next = events.recv() => match next {
                        Some(event) => apply(event, &mut instances, &mut processes, &mut forgotten),
                        None => return,
                    },
                }
            }

            combined.clear();
            combined.extend(instances.iter().map(|(id, state)| (*id, *state)));
            let mut resolution_failed = false;
            for (&pid, state) in &processes {
                match active.resolve_pid(pid).await {
                    Ok(key) => {
                        if let Some(id) = ResourceId::parse(&key) {
                            combined
                                .entry(id)
                                .and_modify(|value| {
                                    value.foreground |= state.foreground;
                                    value.visible |= state.visible;
                                })
                                .or_insert(*state);
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, pid, "kernel resource resolution unavailable");
                        resolution_failed = true;
                    }
                }
            }
            // A failed read is not evidence that a previously visible resource became headless.
            if resolution_failed {
                for (id, state) in &published {
                    combined.entry(*id).or_insert(*state);
                }
            }
            departed.clear();
            departed.extend(
                published
                    .keys()
                    .filter(|id| !combined.contains_key(id))
                    .copied(),
            );
            let mut uuid_buffer = [0_u8; 36];
            for id in &departed {
                let key = id.uuid().hyphenated().encode_lower(&mut uuid_buffer);
                match active.set_instance_state(key, false, false).await {
                    Ok(()) => {
                        published.remove(id);
                    }
                    Err(error) => tracing::debug!(%error, %id, "clearing window enrichment failed"),
                }
            }
            for (id, state) in &combined {
                if published.get(id) == Some(state) {
                    continue;
                }
                let key = id.uuid().hyphenated().encode_lower(&mut uuid_buffer);
                match active
                    .set_instance_state(key, state.foreground, state.visible)
                    .await
                {
                    Ok(()) => {
                        published.insert(*id, *state);
                    }
                    Err(error) => {
                        tracing::debug!(%error, %id, "publishing window enrichment failed")
                    }
                }
            }
            for id in forgotten.drain(..) {
                if combined.contains_key(&id) {
                    continue;
                }
                let key = id.uuid().hyphenated().encode_lower(&mut uuid_buffer);
                if let Err(error) = active.forget_instance(key).await {
                    tracing::debug!(%error, %id, "forgetting native instance failed");
                }
            }

            'wait: loop {
                tokio::select! {
                    next = events.recv() => {
                        let Some(event) = next else { return; };
                        apply(event, &mut instances, &mut processes, &mut forgotten);
                        break 'wait;
                    }
                    changed = owners.next() => {
                        let Some(changed) = changed else { break 'connected; };
                        if let Ok(args) = changed.args() {
                            if args.name().as_str() == templar_types::dbus::resources::BUS_NAME {
                                if args.new_owner().as_ref().is_some() {
                                    // Signal streams are owner-bound. Register against the new
                                    // producer before taking its initial PID-resolution snapshot.
                                    match ResourcesProxy::new(&connection).await {
                                        Ok(proxy) => match proxy.receive_targets_changed().await {
                                            Ok(stream) => { active = proxy; targets = stream; }
                                            Err(error) => tracing::debug!(%error, "new governor membership subscription unavailable"),
                                        },
                                        Err(error) => tracing::debug!(%error, "new governor proxy unavailable"),
                                    }
                                }
                                published.clear();
                                break 'wait;
                            }
                        }
                    }
                    changed = targets.next() => {
                        if changed.is_none() { break 'connected; }
                        break 'wait;
                    }
                }
            }
        }
    }
}

fn apply(
    event: Event,
    instances: &mut HashMap<ResourceId, InstanceState>,
    processes: &mut HashMap<u32, InstanceState>,
    forgotten: &mut Vec<ResourceId>,
) {
    match event {
        Event::State(id, state) => {
            let id = ResourceId::from(id);
            instances.insert(id, state);
            forgotten.retain(|forgotten| *forgotten != id);
        }
        Event::Forget(id) => {
            let id = ResourceId::from(id);
            instances.remove(&id);
            forgotten.push(id);
        }
        Event::Process(pid, state) => {
            processes.insert(pid, state);
        }
        Event::ProcessGone(pid) => {
            processes.remove(&pid);
        }
    }
}
