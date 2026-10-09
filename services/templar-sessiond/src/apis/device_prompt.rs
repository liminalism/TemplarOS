//! Caller-bound, bounded device prompts. Metadata and secret transport are deliberately separate.
use anyhow::{Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use templar_client::devices::{DevicePrompt, PromptKind};
use templar_types::settings::Revision;
use tokio::sync::oneshot;
use zbus::{message::Header, zvariant::OwnedFd};
use zeroize::Zeroizing;

pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_PROMPTS: usize = 16;
pub enum PromptReply {
    Approved,
    Secret(Zeroizing<String>),
    Rejected,
}
#[derive(Clone, Eq, PartialEq)]
pub struct Lease {
    owner: String,
    id: uuid::Uuid,
}
/// Required upstream agent string transport, with no ordinary owned-string copy.
pub struct SecretText(pub Zeroizing<String>);
impl serde::Serialize for SecretText {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}
impl zbus::zvariant::Type for SecretText {
    const SIGNATURE: &'static zbus::zvariant::Signature =
        <String as zbus::zvariant::Type>::SIGNATURE;
}
struct Pending {
    metadata: DevicePrompt,
    answer: Option<oneshot::Sender<PromptReply>>,
    display: Option<Zeroizing<String>>,
}
struct State {
    owner: Option<String>,
    lease_id: Option<uuid::Uuid>,
    revision: Revision,
    pending: HashMap<String, Pending>,
}
pub struct PromptHub {
    state: Mutex<State>,
    session: zbus::Connection,
    path: &'static str,
    interface: &'static str,
}
impl PromptHub {
    pub fn new(
        session: zbus::Connection,
        path: &'static str,
        interface: &'static str,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                owner: None,
                lease_id: None,
                revision: Revision::new_owner(),
                pending: HashMap::new(),
            }),
            session,
            path,
            interface,
        })
    }
    pub fn acquire(&self, owner: &str) -> Result<()> {
        let mut state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state
                .owner
                .as_deref()
                .is_none_or(|current| current == owner),
            "native controls are owned by another UI; close that page first"
        );
        if state.owner.is_none() {
            state.lease_id = Some(uuid::Uuid::new_v4());
        }
        state.owner = Some(owner.into());
        drop(state);
        self.changed();
        Ok(())
    }
    pub fn require(&self, owner: &str) -> Result<()> {
        ensure!(
            self.state
                .lock()
                .expect("prompt state poisoned")
                .owner
                .as_deref()
                == Some(owner),
            "acquire the native device UI lease before changing devices"
        );
        Ok(())
    }
    pub fn owns(&self, owner: &str) -> bool {
        self.state
            .lock()
            .expect("prompt state poisoned")
            .owner
            .as_deref()
            == Some(owner)
    }
    pub fn owner(&self) -> Option<String> {
        self.state
            .lock()
            .expect("prompt state poisoned")
            .owner
            .clone()
    }
    pub fn lease(&self, owner: &str) -> Result<Lease> {
        let state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state.owner.as_deref() == Some(owner),
            "native device UI lease belongs to another caller"
        );
        Ok(Lease {
            owner: owner.into(),
            id: state
                .lease_id
                .expect("owned prompt hub lacks lease identity"),
        })
    }
    pub fn owns_lease(&self, lease: &Lease) -> bool {
        let state = self.state.lock().expect("prompt state poisoned");
        state.owner.as_deref() == Some(lease.owner.as_str()) && state.lease_id == Some(lease.id)
    }
    pub fn release(&self, owner: &str) -> Result<Option<Lease>> {
        let mut state = self.state.lock().expect("prompt state poisoned");
        if state.owner.is_none() {
            return Ok(None);
        }
        ensure!(
            state.owner.as_deref() == Some(owner),
            "native device UI lease belongs to another caller"
        );
        let lease = Lease {
            owner: owner.into(),
            id: state
                .lease_id
                .expect("owned prompt hub lacks lease identity"),
        };
        state.owner = None;
        state.lease_id = None;
        state.pending.clear();
        drop(state);
        self.changed();
        Ok(Some(lease))
    }
    pub fn producer_restart(&self) {
        let mut state = self.state.lock().expect("prompt state poisoned");
        state.pending.clear();
        state.owner = None;
        state.lease_id = None;
        state.revision = Revision::new_owner();
        drop(state);
        self.changed();
    }
    pub fn revision(&self) -> Revision {
        self.state.lock().expect("prompt state poisoned").revision
    }
    pub fn metadata(&self, owner: &str) -> Vec<DevicePrompt> {
        let state = self.state.lock().expect("prompt state poisoned");
        if state.owner.as_deref() != Some(owner) {
            return Vec::new();
        }
        let mut prompts: Vec<_> = state
            .pending
            .values()
            .map(|pending| pending.metadata.clone())
            .collect();
        prompts.sort_by(|left, right| left.token.cmp(&right.token));
        prompts
    }
    pub fn changed(&self) {
        let revision = {
            let mut state = self.state.lock().expect("prompt state poisoned");
            state.revision.generation = state.revision.generation.saturating_add(1);
            state.revision
        };
        let session = self.session.clone();
        let path = self.path;
        let interface = self.interface;
        tokio::spawn(async move {
            if let Ok(revision) = serde_json::to_string(&revision) {
                let _ = session
                    .emit_signal(None::<&str>, path, interface, "Changed", &(revision,))
                    .await;
            }
        });
    }
    pub async fn ask(
        self: &Arc<Self>,
        lease: &Lease,
        subject: &str,
        label: &str,
        kind: PromptKind,
        display: Option<Zeroizing<String>>,
        entered: u16,
    ) -> Result<PromptReply> {
        let token = uuid::Uuid::new_v4().to_string();
        let (answer, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().expect("prompt state poisoned");
            ensure!(
                state.owner.as_deref() == Some(lease.owner.as_str())
                    && state.lease_id == Some(lease.id),
                "native device UI lease expired; use the existing provider"
            );
            ensure!(
                state.pending.len() < MAX_PROMPTS,
                "too many pending device requests"
            );
            let deadline = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
                + PROMPT_TIMEOUT.as_millis();
            state.pending.insert(
                token.clone(),
                Pending {
                    metadata: DevicePrompt {
                        token: token.clone(),
                        subject: subject.into(),
                        label: label.into(),
                        kind,
                        deadline_unix_ms: u64::try_from(deadline)?,
                        entered,
                    },
                    answer: Some(answer),
                    display,
                },
            );
        }
        self.changed();
        let guard = PromptGuard {
            hub: self.clone(),
            token,
        };
        let reply = match tokio::time::timeout(PROMPT_TIMEOUT, receiver).await {
            Ok(Ok(reply)) => reply,
            _ => PromptReply::Rejected,
        };
        drop(guard);
        Ok(reply)
    }
    pub fn cancel_subject(&self, subject: &str) {
        self.state
            .lock()
            .expect("prompt state poisoned")
            .pending
            .retain(|_, pending| pending.metadata.subject != subject);
        self.changed();
    }
    pub fn cancel_all(&self) {
        self.state
            .lock()
            .expect("prompt state poisoned")
            .pending
            .clear();
        self.changed();
    }
    pub fn show(
        self: &Arc<Self>,
        lease: &Lease,
        subject: &str,
        label: &str,
        kind: PromptKind,
        display: Zeroizing<String>,
        entered: u16,
    ) -> Result<()> {
        let mut state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state.owner.as_deref() == Some(lease.owner.as_str())
                && state.lease_id == Some(lease.id),
            "native device UI lease expired"
        );
        if let Some(pending) = state
            .pending
            .values_mut()
            .find(|pending| pending.metadata.subject == subject && pending.metadata.kind == kind)
        {
            pending.display = Some(display);
            pending.metadata.entered = entered;
            drop(state);
            self.changed();
            return Ok(());
        }
        ensure!(
            state.pending.len() < MAX_PROMPTS,
            "too many pending device requests"
        );
        let token = uuid::Uuid::new_v4().to_string();
        let deadline =
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() + PROMPT_TIMEOUT.as_millis();
        state.pending.insert(
            token.clone(),
            Pending {
                metadata: DevicePrompt {
                    token: token.clone(),
                    subject: subject.into(),
                    label: label.into(),
                    kind,
                    deadline_unix_ms: u64::try_from(deadline)?,
                    entered,
                },
                answer: None,
                display: Some(display),
            },
        );
        drop(state);
        self.changed();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(PROMPT_TIMEOUT).await;
            if let Some(hub) = weak.upgrade() {
                hub.state
                    .lock()
                    .expect("prompt state poisoned")
                    .pending
                    .remove(&token);
                hub.changed();
            }
        });
        Ok(())
    }
    pub fn answer(&self, owner: &str, token: &str, approved: bool) -> Result<()> {
        let mut state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state.owner.as_deref() == Some(owner),
            "native device UI lease belongs to another caller"
        );
        let pending = state
            .pending
            .get_mut(token)
            .ok_or_else(|| anyhow::anyhow!("request expired or was already answered"))?;
        ensure!(
            !approved
                || !matches!(
                    pending.metadata.kind,
                    PromptKind::Password | PromptKind::Pin | PromptKind::Passkey
                ),
            "this prompt requires sealed secret transport"
        );
        let answer = pending
            .answer
            .take()
            .ok_or_else(|| anyhow::anyhow!("request was already answered"))?;
        answer
            .send(if approved {
                PromptReply::Approved
            } else {
                PromptReply::Rejected
            })
            .map_err(|_| anyhow::anyhow!("request expired"))?;
        Ok(())
    }
    pub fn answer_secret(&self, owner: &str, token: &str, descriptor: OwnedFd) -> Result<()> {
        let mut state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state.owner.as_deref() == Some(owner),
            "native device UI lease belongs to another caller"
        );
        let pending = state
            .pending
            .get_mut(token)
            .ok_or_else(|| anyhow::anyhow!("request expired or was already answered"))?;
        ensure!(
            matches!(
                pending.metadata.kind,
                PromptKind::Password | PromptKind::Pin | PromptKind::Passkey
            ),
            "this prompt does not accept a secret"
        );
        let secret = templar_client::secret_fd::read_utf8(descriptor, 4096)?;
        if pending.metadata.kind == PromptKind::Passkey {
            ensure!(
                secret.len() <= 6 && secret.bytes().all(|byte| byte.is_ascii_digit()),
                "Bluetooth passkey must contain at most six digits"
            );
        }
        if pending.metadata.kind == PromptKind::Pin {
            ensure!(
                secret.chars().count() <= 16,
                "Bluetooth PIN exceeds sixteen characters"
            );
        }
        let answer = pending
            .answer
            .take()
            .ok_or_else(|| anyhow::anyhow!("request was already answered"))?;
        answer
            .send(PromptReply::Secret(secret))
            .map_err(|_| anyhow::anyhow!("request expired"))?;
        Ok(())
    }
    pub fn display_secret(&self, owner: &str, token: &str) -> Result<OwnedFd> {
        let state = self.state.lock().expect("prompt state poisoned");
        ensure!(
            state.owner.as_deref() == Some(owner),
            "native device UI lease belongs to another caller"
        );
        let pending = state
            .pending
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("request expired"))?;
        let secret = pending
            .display
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("request has no display code"))?;
        Ok(templar_client::secret_fd::seal_utf8(secret, 4096)?.into())
    }
}
struct PromptGuard {
    hub: Arc<PromptHub>,
    token: String,
}
impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.hub
            .state
            .lock()
            .expect("prompt state poisoned")
            .pending
            .remove(&self.token);
        self.hub.changed();
    }
}
pub fn sender(header: &Header<'_>) -> Result<String> {
    Ok(header
        .sender()
        .ok_or_else(|| anyhow::anyhow!("device operation requires a unique bus caller"))?
        .as_str()
        .to_owned())
}
pub async fn authority_owner(
    connection: &zbus::Connection,
    service: &str,
) -> Result<Option<zbus::names::OwnedUniqueName>> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    match bus.get_name_owner(service.try_into()?).await {
        Ok(owner) => Ok(Some(owner)),
        Err(zbus::fdo::Error::NameHasNoOwner(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub async fn agent_sender(
    connection: &zbus::Connection,
    service: &str,
    header: &Header<'_>,
) -> Result<()> {
    let caller = header
        .sender()
        .ok_or_else(|| anyhow::anyhow!("agent request lacks a bus sender"))?;
    let owner = authority_owner(connection, service).await?;
    ensure!(
        owner.as_ref().map(|owner| owner.as_str()) == Some(caller.as_str()),
        "agent request is not from the current device authority"
    );
    Ok(())
}

/// Only an explicit refusal from the captured producer confirms an issued effect
/// did not succeed. Bus-generated/transport deadline errors leave its outcome unknown.
pub fn definitive_refusal(
    error: &zbus::Error,
    authority: Option<&zbus::names::OwnedUniqueName>,
) -> bool {
    let zbus::Error::MethodError(name, _, reply) = error else {
        return false;
    };
    let header = reply.header();
    let sender = header.sender().map(|sender| sender.as_str());
    sender.is_some()
        && sender == authority.map(|owner| owner.as_str())
        && !matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.NoReply"
                | "org.freedesktop.DBus.Error.Timeout"
                | "org.freedesktop.DBus.Error.TimedOut"
                | "org.freedesktop.DBus.Error.Disconnected"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, process::Stdio, task::Poll};
    use tokio::{
        io::AsyncBufReadExt,
        process::{Child, Command},
    };
    struct PrivateBus {
        child: Child,
        address: String,
    }
    impl PrivateBus {
        async fn new() -> Self {
            let mut child = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("private agent-auth test requires dbus-daemon");
            let mut address = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut address),
            )
            .await
            .expect("private D-Bus startup exceeded its deadline")
            .unwrap();
            assert!(
                !address.trim().is_empty(),
                "private D-Bus did not publish its address"
            );
            Self {
                child,
                address: address.trim().into(),
            }
        }
        async fn connect(&self) -> zbus::Connection {
            zbus::connection::Builder::address(self.address.as_str())
                .unwrap()
                .build()
                .await
                .unwrap()
        }
    }
    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.start_kill();
        }
    }
    struct GuardAgent;
    #[zbus::interface(name = "org.templaros.Test.DeviceAgent")]
    impl GuardAgent {
        async fn verify(
            &self,
            #[zbus(connection)] connection: &zbus::Connection,
            #[zbus(header)] header: Header<'_>,
        ) -> zbus::fdo::Result<()> {
            agent_sender(connection, "org.freedesktop.NetworkManager", &header)
                .await
                .map_err(|_| zbus::fdo::Error::AccessDenied("not the current authority".into()))
        }
        fn refuse(&self, uncertain: bool) -> zbus::fdo::Result<()> {
            if uncertain {
                Err(zbus::fdo::Error::NoReply(
                    "issued native outcome is unknown".into(),
                ))
            } else {
                Err(zbus::fdo::Error::AccessDenied(
                    "native operation was refused".into(),
                ))
            }
        }
    }
    #[tokio::test]
    async fn agent_authenticates_actual_private_bus_owner_not_another_client() {
        let bus = PrivateBus::new().await;
        let agent = bus.connect().await;
        agent
            .object_server()
            .at("/agent", GuardAgent)
            .await
            .unwrap();
        let authority = bus.connect().await;
        authority
            .request_name("org.freedesktop.NetworkManager")
            .await
            .unwrap();
        let intruder = bus.connect().await;
        let destination = agent.unique_name().unwrap();
        let legitimate = zbus::Proxy::new(
            &authority,
            destination,
            "/agent",
            "org.templaros.Test.DeviceAgent",
        )
        .await
        .unwrap();
        legitimate.call::<_, _, ()>("Verify", &()).await.unwrap();
        let forged = zbus::Proxy::new(
            &intruder,
            destination,
            "/agent",
            "org.templaros.Test.DeviceAgent",
        )
        .await
        .unwrap();
        assert!(forged.call::<_, _, ()>("Verify", &()).await.is_err());
        authority
            .release_name("org.freedesktop.NetworkManager")
            .await
            .unwrap();
        assert!(legitimate.call::<_, _, ()>("Verify", &()).await.is_err());
    }
    #[tokio::test]
    async fn issued_effect_requires_exact_producer_refusal_not_bus_or_deadline_error() {
        let bus = PrivateBus::new().await;
        let producer = bus.connect().await;
        producer
            .object_server()
            .at("/agent", GuardAgent)
            .await
            .unwrap();
        let caller = bus.connect().await;
        let authority: zbus::names::OwnedUniqueName =
            producer.unique_name().unwrap().to_owned().into();
        let proxy = zbus::Proxy::new(
            &caller,
            authority.as_str(),
            "/agent",
            "org.templaros.Test.DeviceAgent",
        )
        .await
        .unwrap();
        let refusal = proxy
            .call::<_, _, ()>("Refuse", &(false,))
            .await
            .unwrap_err();
        assert!(definitive_refusal(&refusal, Some(&authority)));
        let uncertain = proxy
            .call::<_, _, ()>("Refuse", &(true,))
            .await
            .unwrap_err();
        assert!(!definitive_refusal(&uncertain, Some(&authority)));
        assert!(!definitive_refusal(&refusal, None));
        let other: zbus::names::OwnedUniqueName = caller.unique_name().unwrap().to_owned().into();
        assert!(!definitive_refusal(&refusal, Some(&other)));
        let absent = zbus::Proxy::new(
            &caller,
            "org.templaros.Test.AbsentDeviceProducer",
            "/agent",
            "org.templaros.Test.DeviceAgent",
        )
        .await
        .unwrap();
        let bus_error = absent
            .call::<_, _, ()>("Refuse", &(false,))
            .await
            .unwrap_err();
        assert!(!definitive_refusal(&bus_error, Some(&authority)));
    }
    #[tokio::test]
    async fn prompt_reply_is_caller_bound_exactly_once_and_release_cancels() {
        let bus = PrivateBus::new().await;
        let hub = PromptHub::new(bus.connect().await, "/test", "org.templaros.Test.Prompts");
        hub.acquire(":1.41").unwrap();
        let lease = hub.lease(":1.41").unwrap();
        assert!(hub.acquire(":1.42").is_err());
        let request = hub.ask(
            &lease,
            "/device",
            "Pair device",
            PromptKind::Authorization,
            None,
            0,
        );
        tokio::pin!(request);
        std::future::poll_fn(|context| {
            assert!(request.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        let token = hub.metadata(":1.41")[0].token.clone();
        assert!(hub.metadata(":1.42").is_empty());
        assert!(hub.answer(":1.42", &token, true).is_err());
        hub.answer(":1.41", &token, true).unwrap();
        assert!(hub.answer(":1.41", &token, true).is_err());
        assert!(matches!(request.await.unwrap(), PromptReply::Approved));
        let request = hub.ask(&lease, "/device", "Password", PromptKind::Password, None, 0);
        tokio::pin!(request);
        std::future::poll_fn(|context| {
            assert!(request.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        hub.release(":1.41").unwrap();
        assert!(matches!(request.await.unwrap(), PromptReply::Rejected));
        assert!(hub.metadata(":1.41").is_empty());
        hub.acquire(":1.41").unwrap();
        assert!(
            !hub.owns_lease(&lease),
            "reacquiring on the same unique caller must not revive an old request"
        );
        assert!(
            hub.ask(
                &lease,
                "/device",
                "Stale request",
                PromptKind::Authorization,
                None,
                0
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn display_code_is_sealed_typed_transport_never_prompt_metadata() {
        let bus = PrivateBus::new().await;
        let hub = PromptHub::new(bus.connect().await, "/test", "org.templaros.Test.Prompts");
        hub.acquire(":1.41").unwrap();
        let lease = hub.lease(":1.41").unwrap();
        hub.show(
            &lease,
            "/device",
            "Code on other device",
            PromptKind::DisplayPasskey,
            Zeroizing::new("123456".into()),
            1,
        )
        .unwrap();
        let metadata = hub.metadata(":1.41");
        assert!(!serde_json::to_string(&metadata).unwrap().contains("123456"));
        assert!(hub.display_secret(":1.42", &metadata[0].token).is_err());
        let code = templar_client::secret_fd::read_utf8(
            hub.display_secret(":1.41", &metadata[0].token).unwrap(),
            4096,
        )
        .unwrap();
        assert_eq!(code.as_str(), "123456");
        hub.show(
            &lease,
            "/device",
            "Code on other device",
            PromptKind::DisplayPasskey,
            Zeroizing::new("123456".into()),
            3,
        )
        .unwrap();
        let updated = hub.metadata(":1.41");
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].token, metadata[0].token);
        assert_eq!(updated[0].entered, 3);
        hub.release(":1.41").unwrap();
        assert!(hub.display_secret(":1.41", &metadata[0].token).is_err());
    }
}

#[cfg(test)]
mod typed_secret_tests {
    use super::*;
    #[test]
    fn pin_reply_is_a_wire_string_without_an_owned_ordinary_string_copy() {
        use zbus::zvariant::{Type, serialized::Context};
        let reply = SecretText(Zeroizing::new("fixture-pin".into()));
        assert_eq!(SecretText::SIGNATURE, String::SIGNATURE);
        let bytes =
            zbus::zvariant::to_bytes(Context::new_dbus(zbus::zvariant::LE, 0), &reply).unwrap();
        let (decoded, _): (String, _) = bytes.deserialize().unwrap();
        assert_eq!(decoded, "fixture-pin");
    }
}
