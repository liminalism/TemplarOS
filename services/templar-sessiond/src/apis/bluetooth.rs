//! BlueZ device effects and an application-specific pairing agent. Never RequestDefaultAgent.
use crate::apis::failed;
use super::device_prompt::{self, PromptHub, PromptReply};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex, Weak}, time::Duration};
use templar_client::devices::{BluetoothAdapter, BluetoothDevice, BluetoothSnapshot, PromptKind, RadioBlock};
use templar_types::settings::Capability;
use zeroize::Zeroizing;
use zbus::{message::Header, zvariant::{OwnedFd, OwnedObjectPath, OwnedValue}};

pub const PATH: &str = "/org/templaros/Session/Bluetooth";
const IFACE: &str = "org.templaros.Bluetooth1";
const BLUEZ: &str = "org.bluez";
const AGENT: &str = "/org/templaros/Session/Bluetooth/Agent";
const DEADLINE: Duration = Duration::from_secs(12);
const PAIR_DEADLINE: Duration = Duration::from_secs(180);
type Properties = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Properties>>;
fn text<'a>(properties: &'a Properties, key: &str) -> Result<&'a str> { <&str>::try_from(properties.get(key).with_context(|| format!("BlueZ omitted {key}"))?).map_err(Into::into) }
fn boolean(properties: &Properties, key: &str) -> Result<bool> { bool::try_from(properties.get(key).with_context(|| format!("BlueZ omitted {key}"))?).map_err(Into::into) }
async fn proxy<'a>(connection: &'a zbus::Connection, path: &'a str, interface: &'a str) -> Result<zbus::Proxy<'a>> {
    Ok(zbus::Proxy::new(connection, BLUEZ, path, interface).await?)
}
async fn cancel_device(connection: &zbus::Connection, authority: &str, path: &str) -> Result<()> {
    let result = zbus::Proxy::new(connection, authority, path, "org.bluez.Device1").await?.call::<_, _, ()>("CancelPairing", &()).await;
    // BlueZ explicitly reports DoesNotExist when no pairing operation remains.
    if let Err(error) = result
        && !matches!(&error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.DoesNotExist") { return Err(error.into()); }
    Ok(())
}
fn radio_blocks() -> Result<Vec<RadioBlock>> {
    let entries = match std::fs::read_dir("/sys/class/rfkill") {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading Bluetooth rfkill inventory"),
    };
    let mut blocks = Vec::new();
    for entry in entries.take(128) {
        let path = entry?.path();
        if std::fs::read_to_string(path.join("type"))?.trim() != "bluetooth" { continue; }
        let software: u8 = std::fs::read_to_string(path.join("soft"))?.trim().parse()?;
        let hardware: u8 = std::fs::read_to_string(path.join("hard"))?.trim().parse()?;
        ensure!(software <= 1 && hardware <= 1, "invalid Bluetooth rfkill state");
        blocks.push(RadioBlock { name: std::fs::read_to_string(path.join("name"))?.trim().into(), software: software == 1, hardware: hardware == 1 });
    }
    Ok(blocks)
}
struct Pairing { device: String, token: uuid::Uuid, task: tokio::task::AbortHandle }
impl Drop for Pairing { fn drop(&mut self) { self.task.abort(); } }
struct Runtime {
    discovery: HashSet<String>,
    pairing: Option<Pairing>,
    registered: bool,
    agent_lease: Option<device_prompt::Lease>,
    agent_error: Option<String>,
    operation_error: Option<String>,
    authority: Option<zbus::names::OwnedUniqueName>,
}
struct Core {
    system: Option<zbus::Connection>,
    system_error: Option<String>,
    session: zbus::Connection,
    prompts: Arc<PromptHub>,
    runtime: Mutex<Runtime>,
    mutation: tokio::sync::Mutex<()>,
    observer_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    caller_task: Mutex<Option<(device_prompt::Lease, tokio::task::AbortHandle)>>,
}
impl Drop for Core {
    fn drop(&mut self) {
        for task in self.observer_tasks.get_mut().expect("Bluetooth observers poisoned").drain(..) { task.abort(); }
        if let Some((_, task)) = self.caller_task.get_mut().expect("Bluetooth caller observer poisoned").take() { task.abort(); }
    }
}
impl Core {
    fn end_caller(&self, lease: Option<&device_prompt::Lease>) {
        let mut task = self.caller_task.lock().expect("Bluetooth caller observer poisoned");
        if lease.is_none() || task.as_ref().is_some_and(|(current, _)| Some(current) == lease) {
            if let Some((_, task)) = task.take() { task.abort(); }
        }
    }
    fn reconcile_authority(&self, authority: Option<zbus::names::OwnedUniqueName>) {
        let mut runtime = self.runtime.lock().expect("Bluetooth runtime poisoned");
        if runtime.authority == authority { return; }
        runtime.authority = authority;
        runtime.registered = false; runtime.agent_lease = None;
        runtime.pairing = None; runtime.discovery.clear(); runtime.operation_error = None;
        drop(runtime);
        self.end_caller(None);
        self.prompts.producer_restart();
    }
    fn system(&self) -> Result<&zbus::Connection> { self.system.as_ref().ok_or_else(|| anyhow::anyhow!("system D-Bus unavailable: {}", self.system_error.as_deref().unwrap_or("connection missing"))) }
    async fn effect_proxy<'a>(&'a self, path: &'a str, interface: &'a str) -> Result<zbus::Proxy<'a>> {
        let authority = self.runtime.lock().expect("Bluetooth runtime poisoned").authority.clone().context("native Bluetooth authority lease is unavailable")?;
        Ok(zbus::Proxy::new(self.system()?, authority, path, interface).await?)
    }
    async fn inventory(&self, owner: &str) -> Result<BluetoothSnapshot> {
        let before = self.prompts.revision();
        let objects: Objects = proxy(self.system()?, "/", "org.freedesktop.DBus.ObjectManager").await?.call("GetManagedObjects", &()).await?;
        ensure!(objects.len() <= 4096, "BlueZ inventory exceeds the native bound");
        let mut adapters = Vec::new();
        let mut devices = Vec::new();
        for (path, interfaces) in objects {
            if let Some(properties) = interfaces.get("org.bluez.Adapter1") {
                adapters.push(BluetoothAdapter { path: path.to_string(), address: text(properties, "Address")?.into(), label: text(properties, "Alias")?.into(),
                    powered: boolean(properties, "Powered")?, discovering: boolean(properties, "Discovering")?,
                    discovery_owned: self.prompts.owns(owner) && self.runtime.lock().expect("Bluetooth runtime poisoned").discovery.contains(path.as_str()),
                    pairable: boolean(properties, "Pairable")? });
            }
            if let Some(properties) = interfaces.get("org.bluez.Device1") {
                let adapter = <&zbus::zvariant::ObjectPath<'_>>::try_from(properties.get("Adapter").context("Bluetooth device adapter missing")?)?;
                devices.push(BluetoothDevice { path: path.to_string(), adapter: adapter.to_string(), address: text(properties, "Address")?.into(),
                    label: properties.get("Alias").map(<&str>::try_from).transpose()?.unwrap_or(text(properties, "Address")?).into(),
                    paired: boolean(properties, "Paired")?, connected: boolean(properties, "Connected")?, trusted: boolean(properties, "Trusted")?, blocked: boolean(properties, "Blocked")? });
            }
        }
        adapters.sort_by(|left, right| left.path.cmp(&right.path));
        devices.sort_by(|left, right| left.path.cmp(&right.path));
        let radio_blocks = radio_blocks()?;
        ensure!(before == self.prompts.revision(), "Bluetooth changed during readback; refresh again");
        let runtime = self.runtime.lock().expect("Bluetooth runtime poisoned");
        let capability = if let Some(reason) = &runtime.agent_error { Capability::Unavailable { reason: reason.clone() } }
            else if adapters.is_empty() { Capability::Unavailable { reason: "No Bluetooth adapter is exposed by BlueZ; check the device, firmware and rfkill".into() } }
            else { Capability::Available { requires_restart: false } };
        Ok(BluetoothSnapshot { revision: before, capability, ui_owned: self.prompts.owns(owner), adapters, devices, radio_blocks,
            prompts: self.prompts.metadata(owner), operation_error: runtime.operation_error.clone(), pairing: runtime.pairing.as_ref().map(|pairing| pairing.device.clone()) })
    }
    async fn cancel_pairing(&self) -> Result<()> {
        self.prompts.cancel_all();
        let pending = self.runtime.lock().expect("Bluetooth runtime poisoned").pairing.as_ref().map(|pairing| (pairing.device.clone(), pairing.token));
        if let Some((path, token)) = pending {
            let authority = self.runtime.lock().expect("Bluetooth runtime poisoned").authority.clone().context("pairing authority lease unavailable")?;
            let result = cancel_device(self.system()?, authority.as_str(), &path).await;
            let mut runtime = self.runtime.lock().expect("Bluetooth runtime poisoned");
            if runtime.pairing.as_ref().is_some_and(|pairing| pairing.token == token) {
                if result.is_ok() { runtime.pairing = None; runtime.operation_error = None; }
                else { runtime.operation_error = Some("Pairing cancellation was not confirmed; pending native ownership retained".into()); }
            }
            result.context("BlueZ pairing cancellation failed")?;
        }
        self.prompts.changed();
        Ok(())
    }
    async fn stop_discovery(&self) -> Result<()> {
        let discovery: Vec<_> = self.runtime.lock().expect("Bluetooth runtime poisoned").discovery.iter().cloned().collect();
        let mut errors = Vec::new();
        for adapter in discovery {
            let result = async { self.effect_proxy(&adapter, "org.bluez.Adapter1").await?.call::<_, _, ()>("StopDiscovery", &()).await?; Ok::<_, anyhow::Error>(()) }.await;
            match result {
                Ok(()) => { self.runtime.lock().expect("Bluetooth runtime poisoned").discovery.remove(&adapter); },
                Err(error) => errors.push(error.to_string()),
            }
        }
        ensure!(errors.is_empty(), "native discovery cleanup incomplete: {}", errors.join("; "));
        Ok(())
    }
    async fn unregister(&self) -> Result<()> {
        if !self.runtime.lock().expect("Bluetooth runtime poisoned").registered { return Ok(()); }
        let result = self.effect_proxy("/org/bluez", "org.bluez.AgentManager1").await?
            .call::<_, _, ()>("UnregisterAgent", &(OwnedObjectPath::try_from(AGENT)?,)).await;
        if let Err(error) = result
            && !matches!(&error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.bluez.Error.DoesNotExist") { return Err(error.into()); }
        let mut runtime = self.runtime.lock().expect("Bluetooth runtime poisoned");
        runtime.registered = false;
        runtime.agent_lease = None;
        Ok(())
    }
    async fn release(&self, owner: &str) -> Result<()> {
        // Revoke answers before waiting for an in-flight mutation or any upstream cleanup.
        let lease = self.prompts.release(owner)?;
        let _mutation = self.mutation.lock().await;
        if self.prompts.owner().is_some() {
            if let Some(lease) = &lease { self.end_caller(Some(lease)); }
            return Ok(());
        }
        let (pairing, discovery, agent) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(4), self.cancel_pairing()),
            tokio::time::timeout(Duration::from_secs(4), self.stop_discovery()),
            tokio::time::timeout(Duration::from_secs(4), self.unregister()));
        let mut errors = Vec::new();
        for (operation, result) in [("pairing", pairing), ("discovery", discovery), ("agent", agent)] {
            match result { Ok(Err(error)) => errors.push(format!("{operation}: {error}")),
                Err(_) => errors.push(format!("{operation} cleanup timed out; ownership retained")), _ => {} }
        }
        self.runtime.lock().expect("Bluetooth runtime poisoned").operation_error = (!errors.is_empty()).then(|| errors.join("; "));
        self.end_caller(lease.as_ref());
        ensure!(errors.is_empty(), "Bluetooth cleanup incomplete: {}", errors.join("; "));
        Ok(())
    }
    async fn acquire(self: &Arc<Self>, owner: &str) -> Result<()> {
        let _mutation = self.mutation.lock().await;
        let authority = device_prompt::authority_owner(self.system()?, BLUEZ).await?;
        ensure!(authority.is_some(), "BlueZ is unavailable");
        self.reconcile_authority(authority);
        if self.prompts.owns(owner) { return Ok(()); }
        ensure!(self.prompts.owner().is_none(), "native Bluetooth controls belong to another UI");
        let pairing_pending = self.runtime.lock().expect("Bluetooth runtime poisoned").pairing.is_some();
        if pairing_pending { self.cancel_pairing().await?; }
        self.stop_discovery().await?;
        self.unregister().await?;
        if let Some(error) = &self.runtime.lock().expect("Bluetooth runtime poisoned").agent_error { anyhow::bail!("{error}"); }
        self.runtime.lock().expect("Bluetooth runtime poisoned").operation_error = None;
        self.end_caller(None);
        self.prompts.acquire(owner)?;
        let lease = self.prompts.lease(owner)?;
        self.runtime.lock().expect("Bluetooth runtime poisoned").agent_lease = Some(lease.clone());
        let weak = Arc::downgrade(self);
        let session = self.session.clone();
        let caller = owner.to_owned();
        let watched_lease = lease.clone();
        let caller_task = tokio::spawn(async move {
            let _ = async {
                let bus = zbus::fdo::DBusProxy::new(&session).await?;
                let mut changes = bus.receive_name_owner_changed().await?;
                if !bus.name_has_owner(caller.as_str().try_into()?).await? { return Ok::<_, anyhow::Error>(()); }
                while let Some(change) = changes.next().await {
                    let args = change.args()?;
                    if args.name().as_str() == caller && args.new_owner().as_ref().is_none() { break; }
                    if weak.strong_count() == 0 { return Ok(()); }
                }
                Ok(())
            }.await;
            if let Some(core) = weak.upgrade() {
                if core.prompts.owns_lease(&watched_lease) { let _ = tokio::time::timeout(DEADLINE, core.release(&caller)).await; }
            }
        });
        *self.caller_task.lock().expect("Bluetooth caller observer poisoned") = Some((lease.clone(), caller_task.abort_handle()));
        let result = async {
            // The same connection exports Agent1 and initiates Pair below. BlueZ selects
            // this application's agent, not the desktop's default agent.
            let manager = self.effect_proxy("/org/bluez", "org.bluez.AgentManager1").await?;
            self.runtime.lock().expect("Bluetooth runtime poisoned").registered = true;
            manager.call::<_, _, ()>("RegisterAgent", &(OwnedObjectPath::try_from(AGENT)?, "KeyboardDisplay")).await?;
            ensure!(self.prompts.owns_lease(&lease), "native Bluetooth UI closed during registration");
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = result { let _ = self.prompts.release(owner); self.end_caller(Some(&lease)); return Err(error); }
        Ok(())
    }
    async fn pair(self: &Arc<Self>, owner: &str, device: &str) -> Result<()> {
        self.prompts.require(owner)?;
        let snapshot = self.inventory(owner).await?;
        let selected = snapshot.devices.iter().find(|item| item.path == device).context("Bluetooth device disappeared")?;
        ensure!(!selected.blocked, "Bluetooth device is blocked; review it with the existing provider");
        ensure!(snapshot.adapters.iter().any(|adapter| adapter.path == selected.adapter && adapter.powered), "Bluetooth adapter is powered off or blocked");
        ensure!(self.runtime.lock().expect("Bluetooth runtime poisoned").pairing.is_none(), "another native pairing is still pending; cancel it first");
        let connection = self.system()?.clone();
        let authority = self.runtime.lock().expect("Bluetooth runtime poisoned").authority.clone().context("pairing authority lease unavailable")?;
        let path = device.to_owned();
        let weak = Arc::downgrade(self);
        let token = uuid::Uuid::new_v4();
        let (start, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if started.await.is_err() { return; }
            let result = tokio::time::timeout(PAIR_DEADLINE, async {
                zbus::Proxy::new(&connection, authority.as_str(), path.as_str(), "org.bluez.Device1").await?.call::<_, _, ()>("Pair", &()).await?;
                Ok::<_, anyhow::Error>(())
            }).await;
            let cancellation = if result.is_err() {
                Some(tokio::time::timeout(DEADLINE, cancel_device(&connection, authority.as_str(), &path)).await)
            } else { None };
            if let Some(core) = weak.upgrade() {
                let mut runtime = core.runtime.lock().expect("Bluetooth runtime poisoned");
                if runtime.pairing.as_ref().is_some_and(|pairing| pairing.token == token) {
                    core.prompts.cancel_subject(&path);
                    let cancellation_confirmed = cancellation.as_ref().is_none_or(|result| matches!(result, Ok(Ok(()))));
                    if cancellation_confirmed { runtime.pairing = None; }
                    runtime.operation_error = match result {
                        Ok(Ok(())) => None, Ok(Err(error)) => Some(format!("Pairing failed: {error}")),
                        Err(_) if cancellation_confirmed => Some("Pairing deadline expired; BlueZ cancellation confirmed".into()),
                        Err(_) => Some("Pairing deadline expired; cancellation was not confirmed, so pending ownership is retained".into()),
                    };
                }
                drop(runtime);
                core.prompts.changed();
            }
        });
        self.runtime.lock().expect("Bluetooth runtime poisoned").pairing = Some(Pairing { device: device.into(), token, task: task.abort_handle() });
        let _ = start.send(());
        self.prompts.changed();
        Ok(())
    }
    async fn device(&self, owner: &str, path: &str) -> Result<BluetoothDevice> {
        self.inventory(owner).await?.devices.into_iter().find(|device| device.path == path).context("Bluetooth device disappeared")
    }
}
#[derive(Clone)]
pub struct BluetoothApi(Arc<Core>);
impl BluetoothApi {
    pub async fn new(session: zbus::Connection) -> Self {
        match zbus::Connection::system().await {
            Ok(system) => Self::with_connections(session, Some(system), None).await,
            Err(error) => Self::with_connections(session, None, Some(error.to_string())).await,
        }
    }
    pub async fn with_connections(session: zbus::Connection, system: Option<zbus::Connection>, system_error: Option<String>) -> Self {
        let core = Arc::new(Core { system, system_error, session: session.clone(), prompts: PromptHub::new(session, PATH, IFACE),
            runtime: Mutex::new(Runtime { discovery: HashSet::new(), pairing: None, registered: false, agent_lease: None, agent_error: None, operation_error: None, authority: None }),
            mutation: tokio::sync::Mutex::new(()), observer_tasks: Mutex::new(Vec::new()), caller_task: Mutex::new(None) });
        if let Some(connection) = &core.system {
            if let Err(error) = connection.object_server().at(AGENT, BluetoothAgent(Arc::downgrade(&core))).await {
                core.runtime.lock().expect("Bluetooth runtime poisoned").agent_error = Some(format!("Bluetooth pairing agent export failed: {error}"));
            }
            let authority_connection = connection.clone();
            let authority_core = Arc::downgrade(&core);
            let (authority_ready, authority_registered) = tokio::sync::oneshot::channel();
            let authority_task = tokio::spawn(async move {
                let result = async {
                    let bus = zbus::fdo::DBusProxy::new(&authority_connection).await?;
                    let mut changes = bus.receive_name_owner_changed().await?;
                    let _ = authority_ready.send(());
                    while let Some(change) = changes.next().await {
                        let args = change.args()?;
                        if args.name().as_str() != BLUEZ { continue; }
                        let Some(core) = authority_core.upgrade() else { break };
                        let _mutation = core.mutation.lock().await;
                        let authority = device_prompt::authority_owner(core.system()?, BLUEZ).await?;
                        core.reconcile_authority(authority);
                    }
                    Err::<(), anyhow::Error>(anyhow::anyhow!("BlueZ ownership observation stream closed"))
                }.await;
                if let Err(error) = result
                    && let Some(core) = authority_core.upgrade() {
                    core.prompts.producer_restart();
                    core.runtime.lock().expect("Bluetooth runtime poisoned").agent_error =
                        Some(format!("BlueZ ownership observation failed: {error}"));
                }
            });
            let connection = connection.clone();
            let weak = Arc::downgrade(&core);
            let (events_ready, events_registered) = tokio::sync::oneshot::channel();
            let event_task = tokio::spawn(async move {
                let result = async {
                    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(BLUEZ)?.build();
                    let mut stream = zbus::MessageStream::for_match_rule(rule, &connection, Some(64)).await?;
                    let _ = events_ready.send(());
                    while let Some(message) = stream.next().await {
                        message?;
                        let Some(core) = weak.upgrade() else { break };
                        core.prompts.changed();
                    }
                    Err::<(), anyhow::Error>(anyhow::anyhow!("BlueZ event observation stream closed"))
                }.await;
                if let Some(core) = weak.upgrade() {
                    core.prompts.producer_restart();
                    if let Err(error) = result { core.runtime.lock().expect("Bluetooth runtime poisoned").agent_error = Some(format!("Bluetooth observation failed: {error}")); }
                }
            });
            core.observer_tasks.lock().expect("Bluetooth observers poisoned").extend([authority_task.abort_handle(), event_task.abort_handle()]);
            let (authority, events) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(10), authority_registered),
                tokio::time::timeout(Duration::from_secs(10), events_registered));
            if !matches!(authority, Ok(Ok(()))) || !matches!(events, Ok(Ok(()))) {
                for task in core.observer_tasks.lock().expect("Bluetooth observers poisoned").iter() { task.abort(); }
                core.runtime.lock().expect("Bluetooth runtime poisoned").agent_error.get_or_insert_with(|| "BlueZ event observation could not be installed before readback".into());
            }
        }
        Self(core)
    }
}
#[zbus::interface(name = "org.templaros.Bluetooth1")]
impl BluetoothApi {
    async fn inventory(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<String> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let snapshot = match tokio::time::timeout(DEADLINE, self.0.inventory(&owner)).await {
            Ok(Ok(snapshot)) => snapshot,
            result => {
                let (operation_error, pairing) = {
                    let runtime = self.0.runtime.lock().expect("Bluetooth runtime poisoned");
                    (runtime.operation_error.clone(), runtime.pairing.as_ref().map(|pairing| pairing.device.clone()))
                };
                BluetoothSnapshot { revision: self.0.prompts.revision(), capability: Capability::Unavailable { reason: match result {
                    Ok(Err(error)) => format!("BlueZ unavailable/readback failed: {error:#}"), _ => "BlueZ readback timed out".into() } }, ui_owned: self.0.prompts.owns(&owner),
                    adapters: Vec::new(), devices: Vec::new(), radio_blocks: Vec::new(), prompts: self.0.prompts.metadata(&owner), operation_error, pairing }
            },
        };
        serde_json::to_string(&snapshot).map_err(failed)
    }
    async fn acquire_ui(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        match tokio::time::timeout(DEADLINE, self.0.acquire(&owner)).await {
            Ok(result) => result.map_err(failed),
            Err(_) => {
                if self.0.prompts.owns(&owner) {
                    match tokio::time::timeout(DEADLINE, self.0.release(&owner)).await {
                        Ok(Err(error)) => return Err(failed(format!("native Bluetooth acquisition timed out; cleanup incomplete: {error}"))),
                        Err(_) => return Err(failed("native Bluetooth acquisition and cleanup timed out; UI lease revoked")),
                        _ => {}
                    }
                }
                Err(failed("native Bluetooth agent acquisition timed out; UI lease revoked"))
            }
        }
    }
    async fn release_ui(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.release(&owner)).await.map_err(failed)?.map_err(failed)
    }
    async fn set_powered(&self, adapter: &str, powered: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            ensure!(snapshot.adapters.iter().any(|item| item.path == adapter), "Bluetooth adapter disappeared");
            ensure!(!powered || !snapshot.radio_blocks.iter().any(|block| block.hardware), "hardware rfkill blocks Bluetooth; use the hardware switch");
            self.0.effect_proxy(adapter, "org.bluez.Adapter1").await?.set_property("Powered", powered).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn discover(&self, adapter: &str, enabled: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            ensure!(snapshot.adapters.iter().any(|item| item.path == adapter && item.powered), "Bluetooth adapter is absent, powered off or rfkill-blocked");
            let adapter_proxy = self.0.effect_proxy(adapter, "org.bluez.Adapter1").await?;
            if enabled {
                if self.0.runtime.lock().expect("Bluetooth runtime poisoned").discovery.contains(adapter) { return Ok(()); }
                // Retain issued ownership if the reply times out; cleanup must still attempt StopDiscovery.
                self.0.runtime.lock().expect("Bluetooth runtime poisoned").discovery.insert(adapter.into());
                if let Err(error) = adapter_proxy.call::<_, _, ()>("StartDiscovery", &()).await {
                    let mut runtime = self.0.runtime.lock().expect("Bluetooth runtime poisoned");
                    if device_prompt::definitive_refusal(&error, runtime.authority.as_ref()) {
                        runtime.discovery.remove(adapter);
                    } else {
                        runtime.operation_error = Some("Discovery start outcome is unresolved; pending native ownership retained until StopDiscovery is confirmed".into());
                    }
                    return Err(error.into());
                }
            } else if self.0.runtime.lock().expect("Bluetooth runtime poisoned").discovery.contains(adapter) {
                adapter_proxy.call::<_, _, ()>("StopDiscovery", &()).await?;
                self.0.runtime.lock().expect("Bluetooth runtime poisoned").discovery.remove(adapter);
            } else { anyhow::bail!("this UI has no discovery lease on that adapter"); }
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn pair(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        tokio::time::timeout(DEADLINE, self.0.pair(&owner, device)).await.map_err(failed)?.map_err(failed)
    }
    async fn cancel(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.cancel_pairing()).await.map_err(failed)?.map_err(failed)
    }
    async fn connect(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        self.device_operation(&header, device, "Connect").await
    }
    async fn disconnect(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        self.device_operation(&header, device, "Disconnect").await
    }
    async fn forget(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let selected = self.0.device(&owner, device).await?;
            if self.0.runtime.lock().expect("Bluetooth runtime poisoned").pairing.as_ref().is_some_and(|pairing| pairing.device == device) {
                self.0.cancel_pairing().await?;
            }
            self.0.effect_proxy(&selected.adapter, "org.bluez.Adapter1").await?.call::<_, _, ()>("RemoveDevice", &(OwnedObjectPath::try_from(device)?,)).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn set_trusted(&self, device: &str, trusted: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            self.0.device(&owner, device).await?;
            self.0.effect_proxy(device, "org.bluez.Device1").await?.set_property("Trusted", trusted).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    fn answer(&self, token: &str, approved: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        self.0.prompts.answer(&device_prompt::sender(&header).map_err(failed)?, token, approved).map_err(failed)
    }
    fn answer_secret(&self, token: &str, secret: OwnedFd, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        self.0.prompts.answer_secret(&device_prompt::sender(&header).map_err(failed)?, token, secret).map_err(failed)
    }
    fn display_secret(&self, token: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<OwnedFd> {
        self.0.prompts.display_secret(&device_prompt::sender(&header).map_err(failed)?, token).map_err(failed)
    }
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>, revision_json: &str) -> zbus::Result<()>;
}
impl BluetoothApi {
    async fn device_operation(&self, header: &Header<'_>, device: &str, operation: &str) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            self.0.device(&owner, device).await?;
            self.0.effect_proxy(device, "org.bluez.Device1").await?.call::<_, _, ()>(operation, &()).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
}
#[derive(zbus::DBusError, Debug)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError { Rejected(String), Canceled(String), #[zbus(error)] ZBus(zbus::Error) }
struct BluetoothAgent(Weak<Core>);
impl BluetoothAgent {
    async fn core(&self, header: &Header<'_>) -> std::result::Result<Arc<Core>, AgentError> {
        let core = self.0.upgrade().ok_or_else(|| AgentError::Canceled("native UI unavailable".into()))?;
        device_prompt::agent_sender(core.system().map_err(|_| AgentError::Canceled("system bus unavailable".into()))?, BLUEZ, header).await
            .map_err(|_| AgentError::Rejected("agent caller is not current BlueZ owner".into()))?;
        if core.runtime.lock().expect("Bluetooth runtime poisoned").authority.as_ref().map(|owner| owner.as_str())
            != header.sender().map(|sender| sender.as_str()) { return Err(AgentError::Canceled("BlueZ authority changed; reacquire the native UI lease".into())); }
        Ok(core)
    }
    async fn prompt(&self, header: &Header<'_>, device: &OwnedObjectPath, kind: PromptKind,
        code: Option<Zeroizing<String>>, label: &str) -> std::result::Result<PromptReply, AgentError> {
        let core = self.core(header).await?;
        let lease = core.runtime.lock().expect("Bluetooth runtime poisoned").agent_lease.clone()
            .ok_or_else(|| AgentError::Canceled("native agent lease is closed".into()))?;
        let reply = core.prompts.ask(&lease, device.as_str(), label, kind, code, 0).await
            .map_err(|_| AgentError::Canceled("native prompt unavailable".into()))?;
        if !core.prompts.owns_lease(&lease) { return Err(AgentError::Canceled("native UI lease expired".into())); }
        Ok(reply)
    }
    fn approved(reply: PromptReply) -> std::result::Result<(), AgentError> {
        match reply { PromptReply::Approved => Ok(()), _ => Err(AgentError::Rejected("user rejected the pairing request".into())) }
    }
}
#[zbus::interface(name = "org.bluez.Agent1")]
impl BluetoothAgent {
    async fn release(&self, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let core = self.core(&header).await?;
        { let mut runtime = core.runtime.lock().expect("Bluetooth runtime poisoned"); runtime.registered = false; runtime.agent_lease = None; }
        core.prompts.producer_restart();
        Ok(())
    }
    async fn request_pin_code(&self, device: OwnedObjectPath, #[zbus(header)] header: Header<'_>) -> std::result::Result<device_prompt::SecretText, AgentError> {
        match self.prompt(&header, &device, PromptKind::Pin, None, "Enter the Bluetooth PIN").await? {
            PromptReply::Secret(secret) => Ok(device_prompt::SecretText(secret)),
            _ => Err(AgentError::Rejected("user rejected the PIN request".into())),
        }
    }
    async fn request_passkey(&self, device: OwnedObjectPath, #[zbus(header)] header: Header<'_>) -> std::result::Result<u32, AgentError> {
        match self.prompt(&header, &device, PromptKind::Passkey, None, "Enter the six-digit Bluetooth passkey").await? {
            PromptReply::Secret(secret) => secret.parse().map_err(|_| AgentError::Rejected("invalid passkey".into())),
            _ => Err(AgentError::Rejected("user rejected the passkey request".into())),
        }
    }
    async fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        Self::approved(self.prompt(&header, &device, PromptKind::Confirmation, Some(Zeroizing::new(format!("{passkey:06}"))), "Do both devices show the same Bluetooth code?").await?)
    }
    async fn request_authorization(&self, device: OwnedObjectPath, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        Self::approved(self.prompt(&header, &device, PromptKind::Authorization, None, "Authorize this Bluetooth pairing request?").await?)
    }
    async fn authorize_service(&self, device: OwnedObjectPath, uuid: &str, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        Self::approved(self.prompt(&header, &device, PromptKind::Authorization, None, &format!("Authorize Bluetooth service {uuid}?")).await?)
    }
    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let pincode = Zeroizing::new(pincode);
        let core = self.core(&header).await?;
        let lease = core.runtime.lock().expect("Bluetooth runtime poisoned").agent_lease.clone()
            .ok_or_else(|| AgentError::Canceled("native agent lease is closed".into()))?;
        core.prompts.show(&lease, device.as_str(), "Enter this Bluetooth PIN on the other device", PromptKind::DisplayPin, pincode, 0)
            .map_err(|_| AgentError::Canceled("native UI unavailable".into()))
    }
    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, entered: u16, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let core = self.core(&header).await?;
        let lease = core.runtime.lock().expect("Bluetooth runtime poisoned").agent_lease.clone()
            .ok_or_else(|| AgentError::Canceled("native agent lease is closed".into()))?;
        core.prompts.show(&lease, device.as_str(), "Enter this Bluetooth passkey on the other device", PromptKind::DisplayPasskey,
            Zeroizing::new(format!("{passkey:06}")), entered).map_err(|_| AgentError::Canceled("native UI unavailable".into()))
    }
    async fn cancel(&self, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        self.core(&header).await?.prompts.cancel_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, sync::atomic::{AtomicBool, Ordering}};
    use tokio::{io::AsyncBufReadExt, process::Command};

    #[derive(Default)]
    struct CleanupState { registered: AtomicBool, unconfirmed: AtomicBool }
    struct CleanupManager(Arc<CleanupState>);
    #[zbus::interface(name = "org.bluez.AgentManager1")]
    impl CleanupManager {
        fn register_agent(&self, _path: OwnedObjectPath, _capability: &str) {
            self.0.registered.store(true, Ordering::SeqCst);
        }
        fn unregister_agent(&self, _path: OwnedObjectPath) -> zbus::fdo::Result<()> {
            if self.0.unconfirmed.load(Ordering::SeqCst) {
                return Err(zbus::fdo::Error::NoReply("cleanup outcome is unknown".into()));
            }
            self.0.registered.store(false, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn unconfirmed_unregister_blocks_successor_until_cleanup_then_clears_readback_error() {
        let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
        let mut address = String::new();
        tokio::time::timeout(Duration::from_secs(5), tokio::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address)).await.unwrap().unwrap();
        let producer = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        let state = Arc::new(CleanupState::default());
        producer.object_server().at("/org/bluez", CleanupManager(state.clone())).await.unwrap();
        producer.request_name(BLUEZ).await.unwrap();
        let service = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        let caller = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        let successor = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        let api = BluetoothApi::with_connections(service.clone(), Some(service), None).await;
        let owner = caller.unique_name().unwrap().as_str();
        let next = successor.unique_name().unwrap().as_str();
        api.0.acquire(owner).await.unwrap();
        state.unconfirmed.store(true, Ordering::SeqCst);
        assert!(api.0.release(owner).await.is_err());
        assert!(!api.0.prompts.owns(owner));
        assert!(api.0.runtime.lock().expect("Bluetooth runtime poisoned").registered);
        assert!(state.registered.load(Ordering::SeqCst));
        assert!(api.0.acquire(next).await.is_err());
        assert!(api.0.runtime.lock().expect("Bluetooth runtime poisoned").operation_error.is_some());
        state.unconfirmed.store(false, Ordering::SeqCst);
        api.0.acquire(next).await.unwrap();
        assert!(api.0.prompts.owns(next));
        assert!(api.0.runtime.lock().expect("Bluetooth runtime poisoned").operation_error.is_none(),
            "confirmed cleanup must not leave an obsolete failure on the successor page");
        api.0.release(next).await.unwrap();
        assert!(!state.registered.load(Ordering::SeqCst));
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }
}
