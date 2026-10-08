//! Count-only metadata. Sessiond obtains window authority from the authenticated
//! compositor control socket, never from an adapter's PID/challenge/confirmation.
//! The reachable route is an in-process producer owning exactly one native Wayland
//! window. External Kitty/browser adapters and multi-window producers remain unknown
//! until they supply a non-focus-stealing native surface binding.
use crate::apis::{device_prompt, failed};
use futures_util::StreamExt;
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc, time::Duration};
use templar_client::compositor_control::CompositorControl;
use templar_types::compositor_control::{ControlRequest, TabCountUpdate, TabWindows};
use tokio::sync::watch;
use zbus::{fdo, object_server::SignalEmitter};

pub const PATH: &str = "/org/templaros/Session/TabCounts";

#[derive(Debug)]
struct Adapter {
    sender: String,
    pid: u32,
    generation: u64,
    bindings: HashMap<String, (u64, uuid::Uuid)>,
}
#[derive(Debug, Default)]
struct TabRegistry {
    adapters: HashMap<String, Adapter>,
    inventory: Option<TabWindows>,
    counts: HashMap<u64, TabCountUpdate>,
    generation: u64,
}
impl TabRegistry {
    fn sender(&self, provenance: &str, sender: &str) -> fdo::Result<&Adapter> {
        let adapter = self.adapters.get(provenance).ok_or_else(|| fdo::Error::AccessDenied("Adapter is not registered".into()))?;
        if adapter.sender != sender { return Err(fdo::Error::AccessDenied("Caller does not own this adapter".into())); }
        Ok(adapter)
    }
    fn remove_counts(&mut self, provenance: &str) -> Vec<u64> {
        let removed = self.counts.iter().filter_map(|(id, count)| (count.provenance == provenance).then_some(*id)).collect();
        self.counts.retain(|_, count| count.provenance != provenance);
        removed
    }
    fn register(&mut self, provenance: &str, sender: &str, pid: u32) -> fdo::Result<(u64, Vec<u64>)> {
        if pid == 0 || provenance.is_empty() || provenance.len() > 64
            || !provenance.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
            return Err(fdo::Error::InvalidArgs("A nonzero credential PID and short adapter identifier are required".into()));
        }
        if self.adapters.contains_key(provenance) { self.sender(provenance, sender)?; }
        self.generation = self.generation.checked_add(1).filter(|generation| *generation <= u64::from(u32::MAX))
            .ok_or_else(|| fdo::Error::LimitsExceeded("Tab metadata generation exhausted".into()))?;
        let removed = self.remove_counts(provenance);
        self.adapters.insert(provenance.into(), Adapter { sender: sender.into(), pid, generation: self.generation, bindings: HashMap::new() });
        Ok((self.generation, removed))
    }
    fn sole(inventory: Option<&TabWindows>, window_id: u64, pid: u32) -> bool {
        inventory.is_some_and(|inventory| {
            inventory.windows.iter().filter(|window| window.peer_pid == pid).count() == 1
                && inventory.windows.iter().any(|window| window.window_id == window_id && window.peer_pid == pid)
        })
    }
    fn bind(&mut self, provenance: &str, sender: &str, internal: &str, window_id: u64) -> fdo::Result<Vec<u64>> {
        let adapter = self.sender(provenance, sender)?;
        if internal.is_empty() || internal.len() > 128 { return Err(fdo::Error::InvalidArgs("Internal window identifier must be 1..128 bytes".into())); }
        if !Self::sole(self.inventory.as_ref(), window_id, adapter.pid) {
            return Err(fdo::Error::AccessDenied("Binding requires the caller's sole credential-owned native Wayland window".into()));
        }
        if self.adapters.iter().any(|(name, adapter)| name != provenance && adapter.bindings.values().any(|(id, _)| *id == window_id)) {
            return Err(fdo::Error::AccessDenied("Window already belongs to another adapter".into()));
        }
        let binding = self.inventory.as_ref().and_then(|inventory| inventory.windows.iter().find(|window| window.window_id == window_id))
            .expect("verified native window").binding;
        // A sole-window producer has one internal identity. Rebinding retires its old count.
        let removed = self.remove_counts(provenance);
        let adapter = self.adapters.get_mut(provenance).expect("checked adapter");
        adapter.bindings.clear();
        adapter.bindings.insert(internal.into(), (window_id, binding));
        Ok(removed)
    }
    fn report(&mut self, provenance: &str, sender: &str, internal: &str, count: u32) -> fdo::Result<u64> {
        let adapter = self.sender(provenance, sender)?;
        if count == 0 || count > i32::MAX as u32 { return Err(fdo::Error::InvalidArgs("Tab count must fit a positive wire integer".into())); }
        let (window_id, binding) = *adapter.bindings.get(internal).ok_or_else(|| fdo::Error::AccessDenied("No verified window binding".into()))?;
        if !Self::sole(self.inventory.as_ref(), window_id, adapter.pid) { return Err(fdo::Error::AccessDenied("Window binding is no longer current".into())); }
        self.counts.insert(window_id, TabCountUpdate { window_id, peer_pid: adapter.pid, binding, count, generation: adapter.generation, provenance: provenance.into() });
        Ok(window_id)
    }
    fn inventory(&mut self, next: Option<TabWindows>) -> Vec<u64> {
        let same_owner = self.inventory.as_ref().zip(next.as_ref()).is_some_and(|(old, new)| old.owner == new.owner);
        self.inventory = next;
        for adapter in self.adapters.values_mut() {
            adapter.bindings.retain(|_, (id, binding)| same_owner && Self::sole(self.inventory.as_ref(), *id, adapter.pid)
                && self.inventory.as_ref().is_some_and(|inventory| inventory.windows.iter().any(|window| window.window_id == *id && window.binding == *binding)));
        }
        let removed = self.counts.keys().filter(|id| !self.adapters.values().any(|adapter| adapter.bindings.values().any(|(bound, _)| bound == *id))).copied().collect::<Vec<_>>();
        for id in &removed { self.counts.remove(id); }
        removed
    }
    fn disconnect(&mut self, sender: &str) -> Vec<u64> {
        let names = self.adapters.iter().filter_map(|(name, adapter)| (adapter.sender == sender).then_some(name.clone())).collect::<Vec<_>>();
        let mut removed = Vec::new();
        for name in names { self.adapters.remove(&name); removed.extend(self.remove_counts(&name)); }
        removed
    }
}

#[derive(Clone)]
pub struct TabCountsApi {
    connection: zbus::Connection,
    registry: Arc<Mutex<TabRegistry>>,
    updates: watch::Sender<u64>,
}
impl TabCountsApi {
    pub fn new(connection: zbus::Connection) -> Self {
        let (updates, receiver) = watch::channel(0);
        let api = Self { connection, registry: Arc::new(Mutex::new(TabRegistry::default())), updates };
        tokio::spawn(api.clone().feed(receiver));
        api
    }
    async fn publish(&self, windows: Vec<u64>) {
        self.updates.send_modify(|serial| *serial = serial.wrapping_add(1));
        self.emit_changes(windows).await;
    }
    async fn emit_changes(&self, windows: Vec<u64>) {
        if let Ok(emitter) = SignalEmitter::new(&self.connection, PATH) {
            for window in windows {
                if let Err(error) = Self::changed(&emitter, window).await { tracing::debug!(%error, "publishing tab count invalidation"); }
            }
        }
    }
    async fn feed(self, mut updates: watch::Receiver<u64>) {
        loop {
            let result: anyhow::Result<()> = async {
                let control = CompositorControl::from_runtime()?;
                let mut stream = control.subscribe_tab_windows().await?;
                loop {
                    tokio::select! {
                        next = stream.next() => {
                            let removed = self.registry.lock().inventory(Some(next?));
                            self.emit_changes(removed).await;
                        },
                        changed = updates.changed() => { changed?; },
                    }
                    let snapshot = {
                        let registry = self.registry.lock();
                        registry.inventory.as_ref().map(|inventory| (inventory.owner, registry.counts.values().cloned().collect()))
                    };
                    if let Some((owner, counts)) = snapshot {
                        control.request(ControlRequest::SetTabCounts { owner, counts }).await?;
                    }
                }
            }.await;
            let removed = self.registry.lock().inventory(None);
            self.emit_changes(removed).await;
            if let Err(error) = result { tracing::debug!(%error, "native tab metadata feed unavailable; counts unknown"); }
            // Reconnect only when authority is unavailable; healthy feeds are event-driven.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

#[zbus::interface(name = "org.templaros.TabCounts1")]
impl TabCountsApi {
    async fn register_adapter(&self, provenance: &str, #[zbus(header)] header: zbus::message::Header<'_>) -> fdo::Result<u64> {
        let sender = device_prompt::sender(&header).map_err(failed)?;
        let bus = zbus::fdo::DBusProxy::new(&self.connection).await.map_err(failed)?;
        // Subscribe before looking up credentials: losing the unique name cannot be missed.
        let mut owners = bus.receive_name_owner_changed().await.map_err(failed)?;
        let pid = bus.get_connection_unix_process_id(sender.as_str().try_into().map_err(failed)?).await.map_err(failed)?;
        let (generation, removed) = self.registry.lock().register(provenance, &sender, pid)?;
        let api = self.clone();
        tokio::spawn(async move {
            while let Some(change) = owners.next().await {
                let Ok(args) = change.args() else { break; };
                if args.name().as_str() == sender && args.new_owner().as_ref().is_none() { break; }
            }
            let removed = api.registry.lock().disconnect(&sender);
            api.publish(removed).await;
        });
        self.publish(removed).await;
        Ok(generation)
    }
    /// No public compositor confirmation or self-asserted peer PID exists.
    async fn bind_window(&self, provenance: &str, internal: &str, window_id: u64, #[zbus(header)] header: zbus::message::Header<'_>) -> fdo::Result<()> {
        let sender = device_prompt::sender(&header).map_err(failed)?;
        let removed = self.registry.lock().bind(provenance, &sender, internal, window_id)?;
        self.publish(removed).await;
        Ok(())
    }
    async fn report_tabs(&self, provenance: &str, internal: &str, count: u32, #[zbus(header)] header: zbus::message::Header<'_>) -> fdo::Result<()> {
        let sender = device_prompt::sender(&header).map_err(failed)?;
        let window = self.registry.lock().report(provenance, &sender, internal, count)?;
        self.publish(vec![window]).await;
        Ok(())
    }
    async fn detach_window(&self, provenance: &str, internal: &str, #[zbus(header)] header: zbus::message::Header<'_>) -> fdo::Result<()> {
        let sender = device_prompt::sender(&header).map_err(failed)?;
        let removed = {
            let mut registry = self.registry.lock();
            registry.sender(provenance, &sender)?;
            let window = registry.adapters.get_mut(provenance).expect("checked adapter").bindings.remove(internal);
            window.map(|(id, _)| id).filter(|window| registry.counts.remove(window).is_some()).into_iter().collect()
        };
        self.publish(removed).await;
        Ok(())
    }
    async fn get_count(&self, window_id: u64) -> (bool, u32, u64, String) {
        self.registry.lock().counts.get(&window_id).map_or((false, 0, 0, String::new()), |count| (true, count.count, count.generation, count.provenance.clone()))
    }
    #[zbus(signal)]
    async fn changed(emitter: &SignalEmitter<'_>, window_id: u64) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::compositor_control::TabWindow;
    fn fixture() -> TabRegistry {
        let mut registry = TabRegistry::default();
        registry.inventory(Some(TabWindows { owner: uuid::Uuid::new_v4(), windows: vec![TabWindow { window_id: 7, peer_pid: 42, binding: uuid::Uuid::new_v4() }] }));
        registry.register("native", ":1.7", 42).unwrap();
        registry
    }
    #[test]
    fn credentials_not_claimed_pid_or_public_confirmation_authorize_binding() {
        let mut registry = fixture();
        assert!(registry.bind("native", ":1.8", "a", 7).is_err());
        assert!(registry.bind("native", ":1.7", "a", 8).is_err());
        registry.register("attacker", ":1.8", 99).unwrap();
        assert!(registry.bind("attacker", ":1.8", "a", 7).is_err());
        assert!(registry.register("native", ":1.8", 42).is_err());
        registry.bind("native", ":1.7", "a", 7).unwrap();
        assert!(registry.report("native", ":1.8", "a", 3).is_err());
        assert!(registry.report("native", ":1.7", "a", 0).is_err());
        registry.report("native", ":1.7", "a", 3).unwrap();
        assert_eq!(registry.counts[&7].count, 3);
    }
    #[test]
    fn second_window_close_and_owner_loss_invalidate_without_focus() {
        for transition in 0..5 {
            let mut registry = fixture();
            registry.bind("native", ":1.7", "a", 7).unwrap();
            registry.report("native", ":1.7", "a", 3).unwrap();
            let mut next = registry.inventory.clone().unwrap();
            match transition {
                0 => next.windows.push(TabWindow { window_id: 8, peer_pid: 42, binding: uuid::Uuid::new_v4() }),
                1 => next.windows.clear(),
                2 => next.owner = uuid::Uuid::new_v4(),
                4 => next.windows[0].binding = uuid::Uuid::new_v4(),
                _ => {},
            }
            assert_eq!(registry.inventory((transition != 3).then_some(next)), vec![7]);
            assert!(registry.counts.is_empty());
            assert!(registry.report("native", ":1.7", "a", 3).is_err());
        }
    }
    #[test]
    fn adapter_restart_disconnect_and_rebind_drop_counts() {
        let mut registry = fixture();
        registry.bind("native", ":1.7", "a", 7).unwrap();
        registry.report("native", ":1.7", "a", 3).unwrap();
        assert_eq!(registry.bind("native", ":1.7", "b", 7).unwrap(), vec![7]);
        assert!(registry.report("native", ":1.7", "a", 3).is_err());
        registry.report("native", ":1.7", "b", 4).unwrap();
        let (generation, removed) = registry.register("native", ":1.7", 42).unwrap();
        assert_eq!(removed, vec![7]);
        assert_eq!(generation, 2);
        assert!(registry.report("native", ":1.7", "b", 4).is_err());
        registry.bind("native", ":1.7", "b", 7).unwrap();
        registry.report("native", ":1.7", "b", 4).unwrap();
        assert_eq!(registry.disconnect(":1.7"), vec![7]);
        assert!(registry.adapters.is_empty());
        assert!(registry.counts.is_empty());
    }

    /// Real D-Bus sender credentials and owner-loss signals, on an owned private
    /// bus. Inventory is a registry fixture, not evidence of a Kitty integration.
    #[tokio::test]
    async fn private_bus_rejects_confirmation_and_publishes_disconnect() -> anyhow::Result<()> {
        use tokio::io::AsyncBufReadExt;
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut child = tokio::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(std::process::Stdio::piped()).kill_on_drop(true).spawn()?;
            let mut output = tokio::io::BufReader::new(child.stdout.take().expect("piped bus address"));
            let mut address = String::new();
            output.read_line(&mut address).await?;
            let address = address.trim();
            let server = zbus::connection::Builder::address(address)?.build().await?;
            server.request_name("org.test.TabCounts").await?;
            let (updates, _receiver) = watch::channel(0);
            let api = TabCountsApi { connection: server.clone(), registry: Arc::new(Mutex::new(TabRegistry::default())), updates };
            let mut inventory = TabWindows { owner: uuid::Uuid::new_v4(), windows: vec![
                TabWindow { window_id: 7, peer_pid: std::process::id().saturating_add(1), binding: uuid::Uuid::new_v4() }
            ] };
            api.registry.lock().inventory(Some(inventory.clone()));
            server.object_server().at(PATH, api.clone()).await?;
            let client = zbus::connection::Builder::address(address)?.build().await?;
            let monitor = zbus::connection::Builder::address(address)?.build().await?;
            let proxy = zbus::Proxy::new(&client, "org.test.TabCounts", PATH, "org.templaros.TabCounts1").await?;
            let observer = zbus::Proxy::new(&monitor, "org.test.TabCounts", PATH, "org.templaros.TabCounts1").await?;
            let mut changes = observer.receive_signal("Changed").await?;
            let forged: zbus::Result<bool> = proxy.call("ConfirmBinding", &(0u64, 7u64, i64::from(std::process::id()), true)).await;
            assert!(matches!(&forged, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod"));
            let _: u64 = proxy.call("RegisterAdapter", &("native",)).await?;
            let denied: zbus::Result<()> = proxy.call("BindWindow", &("native", "a", 7u64)).await;
            assert!(matches!(&denied, Err(zbus::Error::MethodError(name, _, _)) if name.as_str() == "org.freedesktop.DBus.Error.AccessDenied"));
            inventory.windows[0].peer_pid = std::process::id();
            api.registry.lock().inventory(Some(inventory));
            let _: () = proxy.call("BindWindow", &("native", "a", 7u64)).await?;
            let _: () = proxy.call("ReportTabs", &("native", "a", 3u32)).await?;
            let changed = changes.next().await.expect("report signal");
            assert_eq!(changed.body().deserialize::<(u64,)>()?, (7,));
            let count: (bool, u32, u64, String) = observer.call("GetCount", &(7u64,)).await?;
            assert!(count.0);
            assert_eq!(count.1, 3);
            drop(proxy);
            client.close().await?;
            let changed = changes.next().await.expect("disconnect signal");
            assert_eq!(changed.body().deserialize::<(u64,)>()?, (7,));
            let count: (bool, u32, u64, String) = observer.call("GetCount", &(7u64,)).await?;
            assert!(!count.0);
            child.kill().await?;
            child.wait().await?;
            Ok::<_, anyhow::Error>(())
        }).await?
    }
}
