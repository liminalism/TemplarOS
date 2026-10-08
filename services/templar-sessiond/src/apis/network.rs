//! NetworkManager remains the connection and authorization authority. Native credentials are
//! handled only while a caller-owned UI lease is active; the existing provider stays installed.
use crate::apis::failed;
use super::{device_prompt::{self, PromptHub, PromptReply}, secret_store::SecretStore};
use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex, Weak}, time::Duration};
use templar_client::devices::{AccessPoint, NetworkDevice, NetworkProfile, NetworkSnapshot, PromptKind, ssid_label};
use templar_types::settings::Capability;
use zbus::{message::Header, zvariant::{OwnedFd, OwnedObjectPath, OwnedValue, Value}};

pub const PATH: &str = "/org/templaros/Session/Network";
const IFACE: &str = "org.templaros.Network1";
const NM: &str = "org.freedesktop.NetworkManager";
const ROOT: &str = "/org/freedesktop/NetworkManager";
const AGENT: &str = "/org/freedesktop/NetworkManager/SecretAgent";
const DEADLINE: Duration = Duration::from_secs(12);
type Properties = HashMap<String, OwnedValue>;
type Settings = HashMap<String, Properties>;
/// NM requires a{sa{sv}}. Borrow variants while encoding rather than allocating an
/// unzeroized OwnedValue copy of the credential.
struct NetworkSecrets(zeroize::Zeroizing<String>);
struct SecuritySecrets<'a>(&'a str);
impl serde::Serialize for SecuritySecrets<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut fields = serializer.serialize_map(Some(2))?;
        fields.serialize_entry("name", &Value::from("802-11-wireless-security"))?;
        fields.serialize_entry("psk", &Value::from(self.0))?;
        fields.end()
    }
}
impl serde::Serialize for NetworkSecrets {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut settings = serializer.serialize_map(Some(1))?;
        settings.serialize_entry("802-11-wireless-security", &SecuritySecrets(self.0.as_str()))?;
        settings.end()
    }
}
impl zbus::zvariant::Type for NetworkSecrets {
    const SIGNATURE: &'static zbus::zvariant::Signature = <Settings as zbus::zvariant::Type>::SIGNATURE;
}
fn text<'a>(properties: &'a Properties, key: &str) -> Result<&'a str> {
    <&str>::try_from(properties.get(key).with_context(|| format!("NetworkManager omitted {key}"))?).map_err(Into::into)
}
fn number(properties: &Properties, key: &str) -> Result<u32> { u32::try_from(properties.get(key).with_context(|| format!("NetworkManager omitted {key}"))?).map_err(Into::into) }
fn boolean(properties: &Properties, key: &str) -> Result<bool> { bool::try_from(properties.get(key).with_context(|| format!("NetworkManager omitted {key}"))?).map_err(Into::into) }
fn owned(value: &str) -> Result<OwnedValue> { Ok(Value::from(value).try_to_owned()?) }
async fn proxy<'a>(connection: &'a zbus::Connection, path: &'a str, interface: &'a str) -> Result<zbus::Proxy<'a>> {
    Ok(zbus::Proxy::new(connection, NM, path, interface).await?)
}
async fn properties(connection: &zbus::Connection, path: &str, interface: &str) -> Result<Properties> {
    Ok(proxy(connection, path, "org.freedesktop.DBus.Properties").await?.call("GetAll", &(interface,)).await?)
}
fn personal_secret_flags(connection: &Settings) -> Result<u32> {
    let security = connection.get("802-11-wireless-security").context("personal security setting missing")?;
    ensure!(matches!(text(security, "key-mgmt")?, "wpa-psk" | "sae"), "advanced security belongs to the existing provider");
    let flags = security.get("psk-flags").map(u32::try_from).transpose()?.unwrap_or(0);
    ensure!(flags & 3 != 0, "system-owned credentials belong to the existing provider");
    Ok(flags)
}
struct NativeProfile { label: String, session_only: bool, lease: device_prompt::Lease, created: bool, cancelled: bool }
struct PendingActivation { cancelled: bool, task: tokio::task::AbortHandle }
struct Runtime {
    profiles: HashMap<String, NativeProfile>,
    active: HashSet<String>,
    pending: HashMap<uuid::Uuid, PendingActivation>,
    registered: bool,
    operation_error: Option<String>,
    cleanup_error: Option<String>,
    authority: Option<zbus::names::OwnedUniqueName>,
}
struct Core {
    system: Option<zbus::Connection>,
    system_error: Option<String>,
    session: zbus::Connection,
    prompts: Arc<PromptHub>,
    store: SecretStore,
    runtime: Mutex<Runtime>,
    mutation: tokio::sync::Mutex<()>,
    observer_tasks: Mutex<Vec<tokio::task::AbortHandle>>,
    caller_task: Mutex<Option<(device_prompt::Lease, tokio::task::AbortHandle)>>,
}
impl Drop for Core {
    fn drop(&mut self) {
        for task in self.observer_tasks.get_mut().expect("network observers poisoned").drain(..) { task.abort(); }
        if let Some((_, task)) = self.caller_task.get_mut().expect("network caller observer poisoned").take() { task.abort(); }
        for (_, pending) in self.runtime.get_mut().expect("network runtime poisoned").pending.drain() { pending.task.abort(); }
    }
}
impl Core {
    fn end_caller(&self, lease: Option<&device_prompt::Lease>) {
        let mut task = self.caller_task.lock().expect("network caller observer poisoned");
        if lease.is_none() || task.as_ref().is_some_and(|(current, _)| Some(current) == lease) {
            if let Some((_, task)) = task.take() { task.abort(); }
        }
    }
    // Called under mutation: a fresh owner query prevents queued old signals from
    // destroying a lease already registered with the replacement authority.
    fn reconcile_authority(&self, authority: Option<zbus::names::OwnedUniqueName>) {
        let mut runtime = self.runtime.lock().expect("network runtime poisoned");
        if runtime.authority == authority { return; }
        runtime.authority = authority;
        runtime.registered = false;
        for (_, pending) in runtime.pending.drain() { pending.task.abort(); }
        runtime.active.clear(); runtime.profiles.clear(); runtime.cleanup_error = None;
        drop(runtime);
        self.end_caller(None);
        self.prompts.producer_restart();
    }
    fn system(&self) -> Result<&zbus::Connection> { self.system.as_ref().ok_or_else(|| anyhow::anyhow!("system D-Bus unavailable: {}", self.system_error.as_deref().unwrap_or("connection missing"))) }
    async fn effect_proxy<'a>(&'a self, path: &'a str, interface: &'a str) -> Result<zbus::Proxy<'a>> {
        let authority = self.runtime.lock().expect("network runtime poisoned").authority.clone().context("native network authority lease is unavailable")?;
        Ok(zbus::Proxy::new(self.system()?, authority, path, interface).await?)
    }
    // Retain the issued RPC across a UI deadline. Its reply is the only authority
    // for an existing-profile activation path; never infer ownership from its UUID.
    async fn issued_activation<F>(self: &Arc<Self>, lease: device_prompt::Lease, created: Option<String>, future: F) -> Result<OwnedObjectPath>
    where F: std::future::Future<Output = Result<OwnedObjectPath>> + Send + 'static {
        ensure!(self.runtime.lock().expect("network runtime poisoned").pending.is_empty(), "an earlier native activation reply is still unresolved");
        let token = uuid::Uuid::new_v4();
        let weak = Arc::downgrade(self);
        let (reply, received) = tokio::sync::oneshot::channel();
        let (start, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            if started.await.is_err() { return; }
            let result = tokio::time::timeout(Duration::from_secs(90), future).await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("native activation reply deadline expired; outcome remains unresolved")));
            let Some(core) = weak.upgrade() else { return; };
            let cancelled = {
                let mut runtime = core.runtime.lock().expect("network runtime poisoned");
                let Some(pending) = runtime.pending.get(&token) else { return; };
                let cancelled = pending.cancelled || !core.prompts.owns_lease(&lease);
                match &result {
                    Ok(active) => {
                        runtime.active.insert(active.to_string());
                        if let Some(uuid) = &created {
                            if let Some(profile) = runtime.profiles.get_mut(uuid) { profile.created = false; }
                        }
                        runtime.pending.remove(&token);
                    }
                    Err(error) if error.downcast_ref::<zbus::Error>().is_some_and(|error| device_prompt::definitive_refusal(error, runtime.authority.as_ref())) => {
                        runtime.pending.remove(&token);
                        if let Some(uuid) = &created { runtime.profiles.remove(uuid); }
                    }
                    Err(_) => { runtime.cleanup_error = Some("native activation outcome is unresolved; cancellation cannot yet be confirmed".into()); }
                }
                cancelled
            };
            let _ = reply.send(result);
            core.prompts.changed();
            if cancelled {
                let _mutation = core.mutation.lock().await;
                // A successor can finish this old cleanup before the late task gets
                // the serialization lock; never touch its newly acquired lease.
                if core.prompts.owner().is_some() && !core.prompts.owns_lease(&lease) { return; }
                if let Err(error) = tokio::time::timeout(DEADLINE, core.cancel()).await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("late native activation cleanup timed out"))) {
                    core.runtime.lock().expect("network runtime poisoned").cleanup_error = Some(error.to_string());
                }
                core.prompts.changed();
            }
        });
        self.runtime.lock().expect("network runtime poisoned").pending.insert(token, PendingActivation { cancelled: false, task: task.abort_handle() });
        let _ = start.send(());
        received.await.context("native activation reply owner stopped")?
    }
    async fn inventory(&self, owner: &str) -> Result<NetworkSnapshot> {
        let connection = self.system()?;
        let before = self.prompts.revision();
        let manager = properties(connection, ROOT, NM).await?;
        let paths: Vec<OwnedObjectPath> = proxy(connection, ROOT, NM).await?.call("GetDevices", &()).await?;
        ensure!(paths.len() <= 128, "NetworkManager returned too many devices");
        let mut devices = Vec::with_capacity(paths.len());
        let mut access_points = Vec::new();
        for path in paths {
            let mut fields = properties(connection, path.as_str(), "org.freedesktop.NetworkManager.Device").await?;
            let reason: (u32, u32) = zbus::zvariant::Structure::try_from(fields.remove("StateReason").context("device reason missing")?)?.try_into()?;
            let kind = number(&fields, "DeviceType")?;
            let active: OwnedObjectPath = fields.remove("ActiveConnection").context("active connection missing")?.try_into()?;
            devices.push(NetworkDevice { path: path.to_string(), interface: text(&fields, "Interface")?.into(), kind,
                state: number(&fields, "State")?, reason: reason.1, firmware_missing: boolean(&fields, "FirmwareMissing")?,
                managed: boolean(&fields, "Managed")?, active_connection: active.to_string() });
            if kind != 2 { continue; }
            let points: Vec<OwnedObjectPath> = proxy(connection, path.as_str(), "org.freedesktop.NetworkManager.Device.Wireless").await?.call("GetAllAccessPoints", &()).await?;
            ensure!(access_points.len() + points.len() <= 2048, "scan exceeds the native inventory bound");
            for point in points {
                let mut fields = properties(connection, point.as_str(), "org.freedesktop.NetworkManager.AccessPoint").await?;
                let ssid: Vec<u8> = fields.remove("Ssid").context("AP SSID missing")?.try_into()?;
                let wpa = number(&fields, "WpaFlags")?;
                let rsn = number(&fields, "RsnFlags")?;
                let privacy = number(&fields, "Flags")? & 1 != 0;
                access_points.push(AccessPoint { path: point.to_string(), device: path.to_string(), label: ssid_label(&ssid), ssid,
                    strength: u8::try_from(fields.get("Strength").context("AP signal missing")?)?, privacy, wpa_flags: wpa, rsn_flags: rsn,
                    personal_supported: !privacy || (wpa | rsn) & (0x100 | 0x400) != 0 });
            }
        }
        let settings = proxy(connection, "/org/freedesktop/NetworkManager/Settings", "org.freedesktop.NetworkManager.Settings").await?;
        let paths: Vec<OwnedObjectPath> = settings.call("ListConnections", &()).await?;
        ensure!(paths.len() <= 2048, "saved profiles exceed the native inventory bound");
        let mut profiles = Vec::with_capacity(paths.len());
        for path in paths {
            let fields: Settings = proxy(connection, path.as_str(), "org.freedesktop.NetworkManager.Settings.Connection").await?.call("GetSettings", &()).await?;
            let fields = fields.get("connection").context("profile connection setting missing")?;
            profiles.push(NetworkProfile { path: path.to_string(), uuid: text(fields, "uuid")?.into(), label: text(fields, "id")?.into(),
                kind: text(fields, "type")?.into(), autoconnect: fields.get("autoconnect").map(bool::try_from).transpose()?.unwrap_or(true) });
        }
        devices.sort_by(|left, right| left.path.cmp(&right.path));
        access_points.sort_by(|left, right| left.device.cmp(&right.device).then(left.ssid.cmp(&right.ssid)).then(left.path.cmp(&right.path)));
        profiles.sort_by(|left, right| left.path.cmp(&right.path));
        ensure!(before == self.prompts.revision(), "network state changed during readback; refresh again");
        let capability = self.runtime.lock().expect("network runtime poisoned").operation_error.as_ref()
            .map(|reason| Capability::Unavailable { reason: reason.clone() }).unwrap_or(Capability::Available { requires_restart: false });
        Ok(NetworkSnapshot { revision: before, capability, ui_owned: self.prompts.owns(owner),
            state: number(&manager, "State")?, connectivity: number(&manager, "Connectivity")?, wireless_enabled: boolean(&manager, "WirelessEnabled")?,
            wireless_hardware_enabled: boolean(&manager, "WirelessHardwareEnabled")?, devices, access_points, profiles, prompts: self.prompts.metadata(owner),
            operation_error: self.runtime.lock().expect("network runtime poisoned").cleanup_error.clone() })
    }
    async fn cancel(&self) -> Result<()> {
        self.prompts.cancel_all();
        {
            let mut runtime = self.runtime.lock().expect("network runtime poisoned");
            for pending in runtime.pending.values_mut() { pending.cancelled = true; }
            for profile in runtime.profiles.values_mut() { profile.cancelled = true; }
        }
        let (known, created): (HashSet<String>, HashSet<String>) = {
            let runtime = self.runtime.lock().expect("network runtime poisoned");
            (runtime.active.clone(), runtime.profiles.iter().filter(|(_, profile)| profile.created).map(|(uuid, _)| uuid.clone()).collect())
        };
        if known.is_empty() && created.is_empty() {
            ensure!(self.runtime.lock().expect("network runtime poisoned").pending.is_empty(), "native activation reply remains unresolved; cancellation not confirmed");
            return Ok(());
        }
        let manager = self.effect_proxy(ROOT, NM).await?;
        let paths: Vec<OwnedObjectPath> = manager.get_property("ActiveConnections").await?;
        ensure!(paths.len() <= 2048, "active connections exceed the native cancellation bound");
        let present: HashSet<_> = paths.iter().map(|path| path.to_string()).collect();
        self.runtime.lock().expect("network runtime poisoned").active.retain(|path| present.contains(path));
        let mut errors = Vec::new();
        for path in paths {
            let result = async {
                let active = self.effect_proxy(path.as_str(), "org.freedesktop.NetworkManager.Connection.Active").await?;
                let owned = known.contains(path.as_str()) || (!created.is_empty() && created.contains(&active.get_property::<String>("Uuid").await?));
                if !owned { return Ok::<_, anyhow::Error>(()); }
                self.runtime.lock().expect("network runtime poisoned").active.insert(path.to_string());
                let state: u32 = active.get_property("State").await?;
                if state == 1 { manager.call::<_, _, ()>("DeactivateConnection", &(&path,)).await?; }
                self.runtime.lock().expect("network runtime poisoned").active.remove(path.as_str());
                Ok(())
            }.await;
            if let Err(error) = result { errors.push(error.to_string()); }
        }
        ensure!(errors.is_empty(), "some native activations could not be canceled: {}", errors.join("; "));
        ensure!(self.runtime.lock().expect("network runtime poisoned").pending.is_empty(), "native activation reply remains unresolved; cancellation not confirmed");
        Ok(())
    }
    async fn unregister(&self) -> Result<()> {
        if !self.runtime.lock().expect("network runtime poisoned").registered { return Ok(()); }
        let result = self.effect_proxy("/org/freedesktop/NetworkManager/AgentManager", "org.freedesktop.NetworkManager.AgentManager").await?
            .call::<_, _, ()>("Unregister", &()).await;
        // This documented AgentManager result is positive readback that no agent remains.
        if let Err(error) = result
            && !matches!(&error, zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.NetworkManager.AgentManager.NotRegistered") { return Err(error.into()); }
        self.runtime.lock().expect("network runtime poisoned").registered = false;
        Ok(())
    }
    async fn release(&self, owner: &str) -> Result<()> {
        let lease = self.prompts.release(owner)?;
        let _mutation = self.mutation.lock().await;
        // A queued successor can finish the old cleanup before this waiter gets the lock.
        // Never cancel/unregister that successor's newly acquired lease.
        if self.prompts.owner().is_some() {
            if let Some(lease) = &lease { self.end_caller(Some(lease)); }
            return Ok(());
        }
        let cancel = tokio::time::timeout(Duration::from_secs(5), self.cancel()).await;
        let unregister = tokio::time::timeout(Duration::from_secs(5), self.unregister()).await;
        if matches!(&cancel, Ok(Ok(()))) { self.runtime.lock().expect("network runtime poisoned").profiles.clear(); }
        let mut errors = Vec::new();
        match cancel { Ok(Err(error)) => errors.push(error.to_string()), Err(_) => errors.push("native activation cancellation timed out; pending ownership retained".into()), _ => {} }
        match unregister { Ok(Err(error)) => errors.push(error.to_string()), Err(_) => errors.push("native agent unregister timed out; registration ownership retained".into()), _ => {} }
        self.runtime.lock().expect("network runtime poisoned").cleanup_error = (!errors.is_empty()).then(|| errors.join("; "));
        self.end_caller(lease.as_ref());
        ensure!(errors.is_empty(), "native network cleanup incomplete: {}", errors.join("; "));
        Ok(())
    }
    async fn acquire(self: &Arc<Self>, owner: &str) -> Result<()> {
        let _mutation = self.mutation.lock().await;
        let authority = device_prompt::authority_owner(self.system()?, NM).await?;
        ensure!(authority.is_some(), "NetworkManager is unavailable");
        self.reconcile_authority(authority);
        if self.prompts.owns(owner) { return Ok(()); }
        ensure!(self.prompts.owner().is_none(), "native network controls belong to another UI");
        let pending = { let runtime = self.runtime.lock().expect("network runtime poisoned"); !runtime.active.is_empty() || !runtime.profiles.is_empty() || !runtime.pending.is_empty() };
        if pending {
            self.cancel().await?;
            self.runtime.lock().expect("network runtime poisoned").profiles.clear();
        }
        self.unregister().await?;
        self.runtime.lock().expect("network runtime poisoned").cleanup_error = None;
        self.end_caller(None);
        self.prompts.acquire(owner)?;
        let lease = self.prompts.lease(owner)?;
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
                if core.prompts.owns_lease(&watched_lease) {
                    let _ = tokio::time::timeout(DEADLINE, core.release(&caller)).await;
                }
            }
        });
        *self.caller_task.lock().expect("network caller observer poisoned") = Some((lease.clone(), caller_task.abort_handle()));
        let result = async {
            let manager = self.effect_proxy("/org/freedesktop/NetworkManager/AgentManager", "org.freedesktop.NetworkManager.AgentManager").await?;
            // Track an issued registration before awaiting its reply: timeout is not refusal.
            self.runtime.lock().expect("network runtime poisoned").registered = true;
            manager.call::<_, _, ()>("RegisterWithCapabilities", &("templaros-native-settings", 0u32)).await?;
            ensure!(self.prompts.owns_lease(&lease), "native network UI closed during registration");
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = result { let _ = self.prompts.release(owner); self.end_caller(Some(&lease)); return Err(error); }
        Ok(())
    }
    async fn activate(self: &Arc<Self>, owner: &str, profile: &str, device: &str) -> Result<OwnedObjectPath> {
        ensure!(self.runtime.lock().expect("network runtime poisoned").pending.is_empty(), "an earlier native activation reply is still unresolved");
        let snapshot = self.inventory(owner).await?;
        ensure!(snapshot.devices.iter().any(|item| item.path == device), "device disappeared");
        let profile = snapshot.profiles.iter().find(|item| item.path == profile).context("saved profile disappeared")?;
        ensure!(matches!(profile.kind.as_str(), "802-11-wireless" | "802-3-ethernet"), "use the existing network provider for enterprise/VPN/advanced connections");
        if profile.kind == "802-11-wireless" {
            let settings: Settings = self.effect_proxy(&profile.path, "org.freedesktop.NetworkManager.Settings.Connection").await?.call("GetSettings", &()).await?;
            if let Some(security) = settings.get("802-11-wireless-security") {
                ensure!(matches!(text(security, "key-mgmt")?, "wpa-psk" | "sae" | "owe"), "enterprise and legacy security belong to the existing network provider");
            }
            let flags = settings.get("802-11-wireless-security").and_then(|setting| setting.get("psk-flags"))
                .map(u32::try_from).transpose()?.unwrap_or(0);
            // Do not solicit an existing system-owned PSK and let NM persist it in a keyfile.
            if flags & 3 != 0 {
                self.runtime.lock().expect("network runtime poisoned").profiles.insert(profile.uuid.clone(), NativeProfile { label: profile.label.clone(), session_only: flags & 2 != 0, lease: self.prompts.lease(owner)?, created: false, cancelled: false });
            }
        }
        let connection = self.system()?.clone();
        let authority = self.runtime.lock().expect("network runtime poisoned").authority.clone().context("activation authority unavailable")?;
        let profile = OwnedObjectPath::try_from(profile.path.as_str())?;
        let device = OwnedObjectPath::try_from(device)?;
        self.issued_activation(self.prompts.lease(owner)?, None, async move {
            Ok(zbus::Proxy::new(&connection, authority, ROOT, NM).await?.call("ActivateConnection", &(
                profile, device, OwnedObjectPath::try_from("/")?)).await?)
        }).await
    }
    async fn connect(self: &Arc<Self>, owner: &str, device: &str, point: &str, ssid: &[u8], security: &str, hidden: bool, session_only: bool) -> Result<OwnedObjectPath> {
        ensure!(self.runtime.lock().expect("network runtime poisoned").pending.is_empty(), "an earlier native activation reply is still unresolved");
        ensure!(!ssid.is_empty() && ssid.len() <= 32, "SSID must contain 1..32 bytes");
        ensure!(matches!(security, "open" | "wpa-psk" | "sae"), "unsupported security; use the existing network provider");
        let snapshot = self.inventory(owner).await?;
        let selected = snapshot.devices.iter().find(|item| item.path == device && item.kind == 2).context("Wi-Fi device disappeared")?;
        ensure!(selected.managed && !selected.firmware_missing && snapshot.wireless_enabled && snapshot.wireless_hardware_enabled,
            "Wi-Fi unavailable: check hardware/software radio, firmware and NetworkManager management");
        if !hidden {
            let access = snapshot.access_points.iter().find(|item| item.path == point && item.device == device && item.ssid == ssid).context("AP/SSID identity changed; scan again")?;
            ensure!(access.personal_supported, "use the existing provider for enterprise/legacy security");
            ensure!(match security { "open" => !access.privacy, "sae" => access.rsn_flags & 0x400 != 0,
                _ => (access.rsn_flags | access.wpa_flags) & 0x100 != 0 }, "security selection does not match this access point");
        } else { ensure!(point == "/", "hidden connections have no selected access point"); }
        if security != "open" && !session_only { self.store.available().await?; }
        let uuid = uuid::Uuid::new_v4().to_string();
        let label = ssid_label(ssid);
        let mut settings: Settings = HashMap::new();
        settings.insert("connection".into(), HashMap::from([("id".into(), owned(&label)?), ("uuid".into(), owned(&uuid)?),
            ("type".into(), owned("802-11-wireless")?), ("autoconnect".into(), OwnedValue::from(false))]));
        settings.insert("802-11-wireless".into(), HashMap::from([("ssid".into(), Value::from(ssid.to_vec()).try_into()?),
            ("mode".into(), owned("infrastructure")?), ("hidden".into(), OwnedValue::from(hidden))]));
        settings.insert("ipv4".into(), HashMap::from([("method".into(), owned("auto")?)]));
        settings.insert("ipv6".into(), HashMap::from([("method".into(), owned("auto")?)]));
        if security != "open" {
            settings.insert("802-11-wireless-security".into(), HashMap::from([("key-mgmt".into(), owned(security)?),
                ("psk-flags".into(), OwnedValue::from(if session_only { 2u32 } else { 1u32 }))]));
        }
        let lease = self.prompts.lease(owner)?;
        let connection = self.system()?.clone();
        let authority = self.runtime.lock().expect("network runtime poisoned").authority.clone().context("activation authority unavailable")?;
        let device = OwnedObjectPath::try_from(device)?;
        let point = OwnedObjectPath::try_from(point)?;
        self.runtime.lock().expect("network runtime poisoned").profiles.insert(uuid.clone(), NativeProfile { label, session_only, lease: lease.clone(), created: true, cancelled: false });
        self.issued_activation(lease, Some(uuid), async move {
            let (_, active): (OwnedObjectPath, OwnedObjectPath) = zbus::Proxy::new(&connection, authority, ROOT, NM).await?.call("AddAndActivateConnection", &(settings, device, point)).await?;
            Ok(active)
        }).await
    }
}
#[derive(Clone)]
pub struct NetworkApi(Arc<Core>);
impl NetworkApi {
    pub async fn new(session: zbus::Connection) -> Self {
        match zbus::Connection::system().await {
            Ok(system) => Self::with_connections(session, Some(system), None).await,
            Err(error) => Self::with_connections(session, None, Some(error.to_string())).await,
        }
    }
    pub async fn with_connections(session: zbus::Connection, system: Option<zbus::Connection>, system_error: Option<String>) -> Self {
        let core = Arc::new(Core { system, system_error, session: session.clone(), prompts: PromptHub::new(session.clone(), PATH, IFACE),
            store: SecretStore::new(session), runtime: Mutex::new(Runtime { profiles: HashMap::new(), active: HashSet::new(), pending: HashMap::new(), registered: false, operation_error: None, cleanup_error: None, authority: None }),
            mutation: tokio::sync::Mutex::new(()), observer_tasks: Mutex::new(Vec::new()), caller_task: Mutex::new(None) });
        if let Some(system) = &core.system {
            if let Err(error) = system.object_server().at(AGENT, NetworkAgent(Arc::downgrade(&core))).await {
                core.runtime.lock().expect("network runtime poisoned").operation_error = Some(format!("native secret agent export failed: {error}"));
            }
            let authority_connection = system.clone();
            let authority_core = Arc::downgrade(&core);
            let (authority_ready, authority_registered) = tokio::sync::oneshot::channel();
            let authority_task = tokio::spawn(async move {
                let result = async {
                    let bus = zbus::fdo::DBusProxy::new(&authority_connection).await?;
                    let mut changes = bus.receive_name_owner_changed().await?;
                    let _ = authority_ready.send(());
                    while let Some(change) = changes.next().await {
                        let args = change.args()?;
                        if args.name().as_str() != NM { continue; }
                        let Some(core) = authority_core.upgrade() else { break };
                        let _mutation = core.mutation.lock().await;
                        let authority = device_prompt::authority_owner(core.system()?, NM).await?;
                        core.reconcile_authority(authority);
                    }
                    Err::<(), anyhow::Error>(anyhow::anyhow!("NetworkManager ownership observation stream closed"))
                }.await;
                if let Err(error) = result
                    && let Some(core) = authority_core.upgrade() {
                    core.prompts.producer_restart();
                    core.runtime.lock().expect("network runtime poisoned").operation_error =
                        Some(format!("NetworkManager ownership observation failed: {error}"));
                }
            });
            let connection = system.clone();
            let weak = Arc::downgrade(&core);
            let (events_ready, events_registered) = tokio::sync::oneshot::channel();
            let event_task = tokio::spawn(async move {
                let result = async {
                    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(NM)?.build();
                    let mut stream = zbus::MessageStream::for_match_rule(rule, &connection, Some(64)).await?;
                    let _ = events_ready.send(());
                    while let Some(message) = stream.next().await {
                        message?;
                        let Some(core) = weak.upgrade() else { break };
                        core.prompts.changed();
                    }
                    Err::<(), anyhow::Error>(anyhow::anyhow!("NetworkManager event observation stream closed"))
                }.await;
                if let Some(core) = weak.upgrade() {
                    core.prompts.producer_restart();
                    if let Err(error) = result { core.runtime.lock().expect("network runtime poisoned").operation_error = Some(format!("network event observation failed: {error}")); }
                }
            });
            core.observer_tasks.lock().expect("network observers poisoned").extend([authority_task.abort_handle(), event_task.abort_handle()]);
            let (authority, events) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(10), authority_registered),
                tokio::time::timeout(Duration::from_secs(10), events_registered));
            if !matches!(authority, Ok(Ok(()))) || !matches!(events, Ok(Ok(()))) {
                for task in core.observer_tasks.lock().expect("network observers poisoned").iter() { task.abort(); }
                core.runtime.lock().expect("network runtime poisoned").operation_error.get_or_insert_with(|| "NetworkManager event observation could not be installed before readback".into());
            }
        }
        Self(core)
    }
}
#[zbus::interface(name = "org.templaros.Network1")]
impl NetworkApi {
    async fn inventory(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<String> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let result = tokio::time::timeout(DEADLINE, self.0.inventory(&owner)).await;
        let snapshot = match result {
            Ok(Ok(snapshot)) => snapshot,
            result => NetworkSnapshot { revision: self.0.prompts.revision(), capability: Capability::Unavailable { reason: match result {
                Ok(Err(error)) => format!("NetworkManager unavailable/readback failed: {error:#}"), _ => "NetworkManager readback timed out".into() } },
                ui_owned: self.0.prompts.owns(&owner), state: 0, connectivity: 0, wireless_enabled: false, wireless_hardware_enabled: false,
                devices: Vec::new(), access_points: Vec::new(), profiles: Vec::new(), prompts: self.0.prompts.metadata(&owner),
                operation_error: self.0.runtime.lock().expect("network runtime poisoned").cleanup_error.clone() },
        };
        serde_json::to_string(&snapshot).map_err(failed)
    }
    async fn acquire_ui(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        if let Some(error) = &self.0.runtime.lock().expect("network runtime poisoned").operation_error { return Err(failed(error)); }
        match tokio::time::timeout(DEADLINE, self.0.acquire(&owner)).await {
            Ok(result) => result.map_err(failed),
            Err(_) => {
                if self.0.prompts.owns(&owner) {
                    match tokio::time::timeout(DEADLINE, self.0.release(&owner)).await {
                        Ok(Err(error)) => return Err(failed(format!("native network acquisition timed out; cleanup incomplete: {error}"))),
                        Err(_) => return Err(failed("native network acquisition and cleanup timed out; UI lease revoked")),
                        _ => {}
                    }
                }
                Err(failed("native network agent acquisition timed out; UI lease revoked"))
            }
        }
    }
    async fn release_ui(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.release(&owner)).await.map_err(failed)?.map_err(failed)
    }
    async fn set_wireless(&self, enabled: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async { self.0.effect_proxy(ROOT, NM).await?.set_property("WirelessEnabled", enabled).await?; Ok::<_, anyhow::Error>(()) };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn scan(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            ensure!(snapshot.devices.iter().any(|item| item.path == device && item.kind == 2), "Wi-Fi device disappeared");
            self.0.effect_proxy(device, "org.freedesktop.NetworkManager.Device.Wireless").await?.call::<_, _, ()>("RequestScan", &(HashMap::<String, OwnedValue>::new(),)).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn connect(&self, device: &str, access_point: &str, ssid: Vec<u8>, security: &str, hidden: bool, session_only: bool,
        #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<OwnedObjectPath> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.connect(&owner, device, access_point, &ssid, security, hidden, session_only)).await.map_err(failed)?.map_err(failed)
    }
    async fn activate(&self, profile: &str, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<OwnedObjectPath> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.activate(&owner, profile, device)).await.map_err(failed)?.map_err(failed)
    }
    async fn disconnect(&self, device: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            ensure!(snapshot.devices.iter().any(|item| item.path == device), "device disappeared");
            self.0.effect_proxy(device, "org.freedesktop.NetworkManager.Device").await?.call::<_, _, ()>("Disconnect", &()).await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn cancel(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        tokio::time::timeout(DEADLINE, self.0.cancel()).await.map_err(failed)?.map_err(failed)
    }
    async fn forget(&self, profile: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            let selected = snapshot.profiles.iter().find(|item| item.path == profile).context("saved profile disappeared")?;
            self.0.effect_proxy(profile, "org.freedesktop.NetworkManager.Settings.Connection").await?.call::<_, _, ()>("Delete", &()).await?;
            self.0.runtime.lock().expect("network runtime poisoned").profiles.remove(&selected.uuid);
            self.0.store.delete(&selected.uuid).await.context("profile deleted, but keyring credential deletion failed; review the keyring")?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(DEADLINE, result).await.map_err(failed)?.map_err(failed)
    }
    async fn set_autoconnect(&self, profile: &str, enabled: bool, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        let owner = device_prompt::sender(&header).map_err(failed)?;
        let _mutation = self.0.mutation.lock().await;
        self.0.prompts.require(&owner).map_err(failed)?;
        let result = async {
            let snapshot = self.0.inventory(&owner).await?;
            ensure!(snapshot.profiles.iter().any(|item| item.path == profile), "saved profile disappeared");
            let connection = self.0.effect_proxy(profile, "org.freedesktop.NetworkManager.Settings.Connection").await?;
            let mut settings: Settings = connection.call("GetSettings", &()).await?;
            settings.get_mut("connection").context("connection setting missing")?.insert("autoconnect".into(), OwnedValue::from(enabled));
            connection.call::<_, _, ()>("Update", &(settings,)).await?;
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
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>, revision_json: &str) -> zbus::Result<()>;
}

#[derive(zbus::DBusError, Debug)]
#[zbus(prefix = "org.freedesktop.NetworkManager.SecretAgent")]
enum AgentError { UserCanceled(String), NoSecrets(String), Failed(String), #[zbus(error)] ZBus(zbus::Error) }
struct NetworkAgent(Weak<Core>);
impl NetworkAgent {
    async fn core(&self, header: &Header<'_>) -> std::result::Result<Arc<Core>, AgentError> {
        let core = self.0.upgrade().ok_or_else(|| AgentError::NoSecrets("native UI unavailable".into()))?;
        device_prompt::agent_sender(core.system().map_err(|_| AgentError::Failed("system bus unavailable".into()))?, NM, header).await
            .map_err(|_| AgentError::Failed("request is not from NetworkManager".into()))?;
        Ok(core)
    }
}
#[zbus::interface(name = "org.freedesktop.NetworkManager.SecretAgent")]
impl NetworkAgent {
    async fn get_secrets(&self, connection: Settings, connection_path: OwnedObjectPath, setting_name: &str, _hints: Vec<String>, flags: u32,
        #[zbus(header)] header: Header<'_>) -> std::result::Result<NetworkSecrets, AgentError> {
        let core = self.core(&header).await?;
        if setting_name != "802-11-wireless-security" { return Err(AgentError::NoSecrets("advanced security belongs to the existing provider".into())); }
        let identity = connection.get("connection").ok_or_else(|| AgentError::NoSecrets("connection identity missing".into()))?;
        let uuid = text(identity, "uuid").map_err(|_| AgentError::NoSecrets("connection UUID missing".into()))?;
        let credential_flags = personal_secret_flags(&connection).map_err(|error| AgentError::NoSecrets(error.to_string()))?;
        let (label, session_only, lease) = {
            let runtime = core.runtime.lock().expect("network runtime poisoned");
            let native = runtime.profiles.get(uuid).ok_or_else(|| AgentError::NoSecrets("connection is not owned by this native UI operation".into()))?;
            if runtime.authority.as_ref().map(|owner| owner.as_str()) != header.sender().map(|sender| sender.as_str()) {
                return Err(AgentError::UserCanceled("network authority changed; reacquire the native UI lease".into()));
            }
            if native.cancelled { return Err(AgentError::UserCanceled("native activation was canceled".into())); }
            (native.label.clone(), native.session_only, native.lease.clone())
        };
        if session_only != (credential_flags & 2 != 0) { return Err(AgentError::NoSecrets("credential storage policy changed; use the existing provider".into())); }
        if !core.prompts.owns_lease(&lease) { return Err(AgentError::UserCanceled("native UI lease expired".into())); }
        let subject = format!("{}:{setting_name}", connection_path.as_str());
        let mut secret = if flags & 2 == 0 && !session_only {
            core.store.get(uuid).await.map_err(|error| AgentError::Failed(error.to_string()))?
        } else { None };
        let prompted = secret.is_none();
        if secret.is_none() {
            if flags & 1 == 0 { return Err(AgentError::NoSecrets("interactive credential request is not permitted".into())); }
            secret = match core.prompts.ask(&lease, &subject, &label, PromptKind::Password, None, 0).await {
                Ok(PromptReply::Secret(secret)) => Some(secret),
                _ => return Err(AgentError::UserCanceled("user canceled the network credential request".into())),
            };
        }
        let secret = secret.expect("resolved above");
        if !core.prompts.owns_lease(&lease) { return Err(AgentError::UserCanceled("native UI lease expired".into())); }
        if prompted && !session_only { core.store.put(uuid, &label, &secret).await.map_err(|error| AgentError::Failed(error.to_string()))?; }
        if !core.prompts.owns_lease(&lease) { return Err(AgentError::UserCanceled("native UI lease expired".into())); }
        Ok(NetworkSecrets(secret))
    }
    async fn cancel_get_secrets(&self, connection_path: OwnedObjectPath, setting_name: &str, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let core = self.core(&header).await?;
        core.prompts.cancel_subject(&format!("{}:{setting_name}", connection_path.as_str()));
        Ok(())
    }
    async fn save_secrets(&self, connection: Settings, _connection_path: OwnedObjectPath, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let core = self.core(&header).await?;
        let identity = connection.get("connection").ok_or_else(|| AgentError::NoSecrets("connection identity missing".into()))?;
        let uuid = text(identity, "uuid").map_err(|_| AgentError::NoSecrets("connection UUID missing".into()))?;
        let credential_flags = personal_secret_flags(&connection).map_err(|error| AgentError::NoSecrets(error.to_string()))?;
        let (label, session_only, lease) = {
            let runtime = core.runtime.lock().expect("network runtime poisoned");
            let profile = runtime.profiles.get(uuid).ok_or_else(|| AgentError::NoSecrets("connection is not owned by this native UI".into()))?;
            if runtime.authority.as_ref().map(|owner| owner.as_str()) != header.sender().map(|sender| sender.as_str()) {
                return Err(AgentError::UserCanceled("network authority changed; reacquire the native UI lease".into()));
            }
            if profile.cancelled { return Err(AgentError::UserCanceled("native activation was canceled".into())); }
            (profile.label.clone(), profile.session_only, profile.lease.clone())
        };
        if !core.prompts.owns_lease(&lease) { return Err(AgentError::UserCanceled("native UI lease expired".into())); }
        if session_only != (credential_flags & 2 != 0) { return Err(AgentError::NoSecrets("credential storage policy changed".into())); }
        if session_only { return Ok(()); }
        let secret = connection.get("802-11-wireless-security").and_then(|setting| setting.get("psk"))
            .ok_or_else(|| AgentError::NoSecrets("WPA secret missing".into()))?;
        let secret = <&str>::try_from(secret).map_err(|_| AgentError::Failed("WPA secret type invalid".into()))?;
        core.store.put(uuid, &label, secret).await.map_err(|error| AgentError::Failed(error.to_string()))
    }
    async fn delete_secrets(&self, connection: Settings, _connection_path: OwnedObjectPath, #[zbus(header)] header: Header<'_>) -> std::result::Result<(), AgentError> {
        let core = self.core(&header).await?;
        let identity = connection.get("connection").ok_or_else(|| AgentError::NoSecrets("connection identity missing".into()))?;
        let uuid = text(identity, "uuid").map_err(|_| AgentError::NoSecrets("connection UUID missing".into()))?;
        core.store.delete(uuid).await.map_err(|error| AgentError::Failed(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct ActivationBoundary;
    #[zbus::interface(name = "org.templaros.Test.ActivationBoundary")]
    impl ActivationBoundary {
        fn activate(&self, uncertain: bool) -> zbus::fdo::Result<OwnedObjectPath> {
            if uncertain { Err(zbus::fdo::Error::NoReply("issued activation outcome remains unknown".into())) }
            else { Err(zbus::fdo::Error::AccessDenied("activation was refused".into())) }
        }
    }
    #[tokio::test]
    async fn issued_activation_deadline_error_retains_ownership_but_native_refusal_clears_it() {
        use std::process::Stdio;
        use tokio::{io::AsyncBufReadExt, process::Command};
        let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().unwrap();
        let mut address = String::new();
        tokio::time::timeout(Duration::from_secs(5), tokio::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address)).await.unwrap().unwrap();
        let producer = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        producer.object_server().at("/activation", ActivationBoundary).await.unwrap();
        let caller = zbus::connection::Builder::address(address.trim()).unwrap().build().await.unwrap();
        let authority: zbus::names::OwnedUniqueName = producer.unique_name().unwrap().to_owned().into();
        let owner = caller.unique_name().unwrap().as_str();
        for uncertain in [true, false] {
            let api = NetworkApi::with_connections(caller.clone(), None, None).await;
            api.0.runtime.lock().expect("network runtime poisoned").authority = Some(authority.clone());
            api.0.prompts.acquire(owner).unwrap();
            let lease = api.0.prompts.lease(owner).unwrap();
            let connection = caller.clone();
            let authority = authority.clone();
            let result = api.0.issued_activation(lease, None, async move {
                Ok(zbus::Proxy::new(&connection, authority, "/activation", "org.templaros.Test.ActivationBoundary")
                    .await?.call::<_, _, OwnedObjectPath>("Activate", &(uncertain,)).await?)
            }).await;
            assert!(result.is_err());
            assert_eq!(!api.0.runtime.lock().expect("network runtime poisoned").pending.is_empty(), uncertain);
            assert_eq!(api.0.cancel().await.is_err(), uncertain);
            assert_eq!(api.0.release(owner).await.is_err(), uncertain);
            assert!(!api.0.prompts.owns(owner), "page leave must revoke answers despite unresolved cleanup");
            let runtime = api.0.runtime.lock().expect("network runtime poisoned");
            assert_eq!(!runtime.pending.is_empty(), uncertain);
            assert_eq!(runtime.cleanup_error.is_some(), uncertain);
        }
        bus.kill().await.unwrap();
        bus.wait().await.unwrap();
    }
    #[test]
    fn typed_secret_reply_matches_the_actual_nm_dictionary_signature() {
        use zbus::zvariant::{Type, serialized::Context};
        let reply = NetworkSecrets(zeroize::Zeroizing::new("fixture-password".into()));
        assert_eq!(NetworkSecrets::SIGNATURE, Settings::SIGNATURE);
        let bytes = zbus::zvariant::to_bytes(Context::new_dbus(zbus::zvariant::LE, 0), &reply).unwrap();
        let (decoded, _): (Settings, _) = bytes.deserialize().unwrap();
        assert_eq!(text(&decoded["802-11-wireless-security"], "name").unwrap(), "802-11-wireless-security");
        assert_eq!(text(&decoded["802-11-wireless-security"], "psk").unwrap(), "fixture-password");
    }
    #[test]
    fn cancellation_is_nm_terminal_user_canceled_not_a_second_provider_error() {
        use zbus::DBusError;
        assert_eq!(AgentError::UserCanceled("fixture rejection".into()).name().as_str(),
            "org.freedesktop.NetworkManager.SecretAgent.UserCanceled");
        assert_eq!(AgentError::NoSecrets("not our profile".into()).name().as_str(),
            "org.freedesktop.NetworkManager.SecretAgent.NoSecrets");
    }
    #[test]
    fn native_credentials_never_take_over_system_owned_or_enterprise_profiles() {
        let connection = |key: &str, flags| HashMap::from([("802-11-wireless-security".into(),
            HashMap::from([("key-mgmt".into(), owned(key).unwrap()), ("psk-flags".into(), OwnedValue::from(flags))]))]);
        assert!(personal_secret_flags(&connection("wpa-psk", 0u32)).is_err());
        assert!(personal_secret_flags(&connection("wpa-eap", 1u32)).is_err());
        assert_eq!(personal_secret_flags(&connection("sae", 1u32)).unwrap(), 1);
        assert_eq!(personal_secret_flags(&connection("wpa-psk", 2u32)).unwrap(), 2);
    }
}
