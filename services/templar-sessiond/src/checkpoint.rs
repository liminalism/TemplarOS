//! The wire transport that delivers a [`CheckpointRequest`] to a live application instance.
//!
//! `docs/protocols/checkpoint.md` defines the protocol; `org.templaros.Checkpoint1`
//! (`interfaces/dbus/org.templaros.Checkpoint1.xml`, `templar_types::dbus::checkpoint`) is the wire
//! format an application instance serves for itself, at a bus name derived from its own
//! `InstanceId` — nothing to register, no directory to go stale. [`DbusCheckpointTransport`] is
//! the real implementation; [`NullCheckpointTransport`] is kept for `--dry-run` and tests, and
//! doubles as the documented legacy fallback: an application that cannot be reached is a legacy
//! application, governed with less confidence, and the governor may still terminate it after the
//! deadline.
//!
//! A bus name is a claim, exactly like a Wayland `app_id`, so [`DbusCheckpointTransport`] never
//! trusts one without checking it: it resolves the claimed name's PID
//! (`org.freedesktop.DBus.GetConnectionUnixProcessID`) and verifies it through the same trusted
//! launch/current-user-manager registry as `AppIdentity1`. A matching cgroup name cannot
//! authorize an unregistered peer. The request is addressed to the verified unique bus owner,
//! so a later handoff of the claimed well-known name cannot redirect it to another process.
//!
//! Peer resolution and the actual method call are behind two small traits
//! ([`PeerResolver`], [`CheckpointCaller`]) so verification and decoding are unit-tested without
//! a live bus — mirrors `templar-hostd`'s `CallerIdentity` (`services/templar-hostd/src/session_report.rs`).

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use templar_types::{
    InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
    dbus::checkpoint as wire,
};

/// Deliver a [`CheckpointRequest`] to `instance` and wait for its answer.
#[async_trait]
pub trait CheckpointTransport: Send + Sync + std::fmt::Debug {
    async fn send(&self, instance: InstanceId, request: CheckpointRequest) -> CheckpointResponse;
}

/// Treats every instance as a legacy application: no transport exists to deliver the request,
/// so every checkpoint attempt is refused rather than silently pretending to succeed.
#[derive(Debug, Default)]
pub struct NullCheckpointTransport;

#[async_trait]
impl CheckpointTransport for NullCheckpointTransport {
    async fn send(&self, instance: InstanceId, _request: CheckpointRequest) -> CheckpointResponse {
        CheckpointResponse::Refused {
            instance,
            reason: "no checkpoint transport is wired up for this instance (legacy application)"
                .into(),
        }
    }
}

/// A verified instance plus the unique owner to which the actual request must be addressed.
#[derive(Clone, Debug)]
struct ResolvedPeer {
    instance: InstanceId,
    owner: zbus::names::OwnedUniqueName,
}

/// Resolve the claimed name to a registry-verified peer; no owner or no verified identity is
/// `None`. The trait permits deterministic verification tests without a live bus or `/proc`.
#[async_trait]
trait PeerResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(&self, bus_name: &str) -> Result<Option<ResolvedPeer>>;
}

/// Makes the actual `Checkpoint1.Request` call. Separated from [`PeerResolver`] so a
/// verification-failure test never needs to construct a connection at all — it is never reached.
#[async_trait]
trait CheckpointCaller: Send + Sync + std::fmt::Debug {
    async fn call(&self, bus_name: &str, request_json: &str) -> Result<String>;
}

#[derive(Debug, Clone)]
struct DbusPeerResolver {
    connection: zbus::Connection,
    registry: Arc<templar_appd::Registry>,
}

#[async_trait]
impl PeerResolver for DbusPeerResolver {
    async fn resolve(&self, bus_name: &str) -> Result<Option<ResolvedPeer>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection)
            .await
            .context("connecting to org.freedesktop.DBus")?;
        let name = zbus::names::BusName::try_from(bus_name)
            .with_context(|| format!("{bus_name} is not a valid bus name"))?;
        let Ok(owner) = dbus.get_name_owner(name).await else {
            return Ok(None);
        };
        let pid = dbus.get_connection_unix_process_id(owner.clone().into()).await?;
        let identity = self.registry.resolve_window(pid, &templar_types::WindowClaims::default())
            .with_context(|| format!("verifying pid {pid} (peer of {bus_name})"))?;
        Ok(verified_peer(owner, identity))
    }
}

fn verified_peer(owner: zbus::names::OwnedUniqueName, identity: templar_appd::identity::IdentityResolution) -> Option<ResolvedPeer> {
    if !identity.confidence.is_verified() || identity.app_id.is_none() { return None; }
    Some(ResolvedPeer { instance: identity.instance_id?, owner })
}

#[derive(Debug, Clone)]
struct DbusCaller {
    connection: zbus::Connection,
}

#[async_trait]
impl CheckpointCaller for DbusCaller {
    async fn call(&self, bus_name: &str, request_json: &str) -> Result<String> {
        let proxy = templar_client::proxies::CheckpointProxy::builder(&self.connection)
            .destination(bus_name)
            .with_context(|| format!("{bus_name} is not a valid destination"))?
            .build()
            .await
            .with_context(|| format!("connecting to {bus_name}"))?;
        proxy
            .request(request_json)
            .await
            .context("org.templaros.Checkpoint1.Request")
    }
}

/// How long a single round trip may take before the relay gives up and answers `Refused`
/// itself. `CheckpointNow` already carries a deadline the governor chose — that is the budget,
/// not a hint. The other two requests carry none, so they get a fixed, generous bound: a
/// checkpoint request happens under memory pressure, when the target may be barely responsive,
/// and this call must never hang the relay task waiting for it.
fn call_budget(request: &CheckpointRequest) -> Duration {
    match request {
        CheckpointRequest::CheckpointNow { deadline, .. } => *deadline,
        CheckpointRequest::PrepareForPressure { .. }
        | CheckpointRequest::RestoreInstance { .. } => Duration::from_secs(5),
    }
}

fn refused(instance: InstanceId, reason: impl Into<String>) -> CheckpointResponse {
    CheckpointResponse::Refused {
        instance,
        reason: reason.into(),
    }
}

/// Parse a `Checkpoint1.Request` reply into a [`CheckpointResponse`]. Pure, so the decode path
/// (including a malformed reply) is testable with a literal string.
fn decode_response(instance: InstanceId, response_json: &str) -> CheckpointResponse {
    match serde_json::from_str::<CheckpointResponse>(response_json) {
        Ok(response) => response,
        Err(error) => refused(
            instance,
            format!("invalid response from the instance: {error}"),
        ),
    }
}

/// The real transport: verify, then call, both bounded by [`call_budget`].
#[derive(Debug)]
pub struct DbusCheckpointTransport {
    resolver: Arc<dyn PeerResolver>,
    caller: Arc<dyn CheckpointCaller>,
}

impl DbusCheckpointTransport {
    pub fn new(connection: zbus::Connection, registry: Arc<templar_appd::Registry>) -> Self {
        Self {
            resolver: Arc::new(DbusPeerResolver {
                connection: connection.clone(),
                registry,
            }),
            caller: Arc::new(DbusCaller { connection }),
        }
    }

    #[cfg(test)]
    fn with_fakes(resolver: Arc<dyn PeerResolver>, caller: Arc<dyn CheckpointCaller>) -> Self {
        Self { resolver, caller }
    }

    async fn attempt(
        &self,
        instance: InstanceId,
        request: &CheckpointRequest,
    ) -> CheckpointResponse {
        let bus_name = wire::instance_bus_name(instance);

        let resolved = match self.resolver.resolve(&bus_name).await {
            Ok(resolved) => resolved,
            Err(error) => {
                return refused(instance, format!("could not resolve {bus_name}: {error}"));
            }
        };
        let Some(peer) = resolved else {
            return refused(
                instance,
                format!("no application instance currently owns {bus_name}"),
            );
        };
        let resolved_instance = peer.instance;
        if resolved_instance != instance {
            return refused(
                instance,
                format!(
                    "{bus_name} is currently owned by a peer whose cgroup names a different \
                     instance ({resolved_instance}) — refusing to trust its answer"
                ),
            );
        }

        let request_json = match serde_json::to_string(request) {
            Ok(json) => json,
            Err(error) => {
                return refused(
                    instance,
                    format!("failed to encode the checkpoint request: {error}"),
                );
            }
        };
        match self.caller.call(peer.owner.as_str(), &request_json).await {
            Ok(response_json) => decode_response(instance, &response_json),
            Err(error) => refused(instance, format!("{bus_name} did not answer: {error}")),
        }
    }
}

#[async_trait]
impl CheckpointTransport for DbusCheckpointTransport {
    async fn send(&self, instance: InstanceId, request: CheckpointRequest) -> CheckpointResponse {
        let budget = call_budget(&request);
        match tokio::time::timeout(budget, self.attempt(instance, &request)).await {
            Ok(response) => response,
            Err(_) => refused(
                instance,
                format!("no answer from the instance within {budget:?}"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    #[test]
    fn a_self_describing_unregistered_or_claim_only_peer_never_authorizes_checkpoint() {
        use templar_appd::identity::{InstanceRecord, Registry};
        use templar_types::{AppAliases, AppManifest, IdentityConfidence, ResourceClass, unit};
        let registry = Registry::default();
        let app = "org.example.Checkpoint".parse().unwrap();
        registry.insert_manifest(AppManifest {
            id: app, display_name: "Checkpoint".into(), primary_desktop_file_id: "checkpoint.desktop".into(),
            executable: "/usr/bin/true".into(), aliases: AppAliases::default(), mime_types: Default::default(),
            resource_class: ResourceClass::Normal, supports_checkpoint: true, icon: None,
            categories: Default::default(), keywords: Default::default(), no_display: false,
            provenance: None, provides: Vec::new(),
        });
        let app = "org.example.Checkpoint".parse().unwrap();
        let instance = InstanceId::new();
        let unit_name = unit::unit_name(&app, instance);
        let group = std::path::PathBuf::from(format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{unit_name}"));
        let owner = || zbus::names::OwnedUniqueName::try_from(":1.23").unwrap();
        let claims = templar_types::WindowClaims::default();
        assert!(verified_peer(owner(), registry.resolve_cgroup(group.clone(), &claims)).is_none());
        registry.register_instance(InstanceRecord {
            id: instance, app_id: app, systemd_unit: unit_name, control_group: group.clone(),
        });
        let identity = registry.resolve_cgroup(group.clone(), &claims);
        assert_eq!(verified_peer(owner(), identity).unwrap().instance, instance);
        let mut claim_only = registry.resolve_cgroup(group, &claims);
        claim_only.confidence = IdentityConfidence::ClaimOnly;
        assert!(verified_peer(owner(), claim_only).is_none());
    }

    #[tokio::test]
    async fn null_transport_always_refuses() {
        let instance = InstanceId::new();
        let response = NullCheckpointTransport
            .send(
                instance,
                CheckpointRequest::CheckpointNow {
                    instance,
                    deadline: StdDuration::from_secs(1),
                },
            )
            .await;
        assert!(matches!(response, CheckpointResponse::Refused { .. }));
    }

    #[test]
    fn call_budget_uses_the_requests_own_deadline_for_checkpoint_now() {
        let instance = InstanceId::new();
        assert_eq!(
            call_budget(&CheckpointRequest::CheckpointNow {
                instance,
                deadline: StdDuration::from_millis(250),
            }),
            StdDuration::from_millis(250)
        );
        assert_eq!(
            call_budget(&CheckpointRequest::PrepareForPressure { instance }),
            StdDuration::from_secs(5)
        );
    }

    #[test]
    fn decode_path_accepts_a_valid_reply_and_refuses_a_malformed_one() {
        let instance = InstanceId::new();
        let valid = serde_json::to_string(&CheckpointResponse::CheckpointComplete {
            instance,
            token: "state-v1".into(),
            has_unsaved_work: true,
        })
        .unwrap();
        assert_eq!(
            decode_response(instance, &valid),
            CheckpointResponse::CheckpointComplete {
                instance,
                token: "state-v1".into(),
                has_unsaved_work: true,
            }
        );

        let malformed = decode_response(instance, "not json");
        assert!(matches!(malformed, CheckpointResponse::Refused { .. }));
    }

    #[derive(Debug)]
    struct FixedResolver(Option<ResolvedPeer>);
    #[async_trait]
    impl PeerResolver for FixedResolver {
        async fn resolve(&self, _bus_name: &str) -> Result<Option<ResolvedPeer>> {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    struct PanicsIfCalled;
    #[async_trait]
    impl CheckpointCaller for PanicsIfCalled {
        async fn call(&self, _bus_name: &str, _request_json: &str) -> Result<String> {
            panic!("must not be called: verification should have refused first")
        }
    }

    #[derive(Debug)]
    struct FixedCaller(String);
    #[async_trait]
    impl CheckpointCaller for FixedCaller {
        async fn call(&self, bus_name: &str, _request_json: &str) -> Result<String> {
            assert_eq!(bus_name, ":1.23", "request must pin the verified unique owner");
            Ok(self.0.clone())
        }
    }

    fn peer(instance: InstanceId) -> ResolvedPeer {
        ResolvedPeer { instance, owner: ":1.23".try_into().unwrap() }
    }

    #[tokio::test]
    async fn a_peer_whose_cgroup_names_a_different_instance_is_refused_without_being_called() {
        let target = InstanceId::new();
        let impostor_instance = InstanceId::new();
        let transport = DbusCheckpointTransport::with_fakes(
            Arc::new(FixedResolver(Some(peer(impostor_instance)))),
            Arc::new(PanicsIfCalled),
        );
        let response = transport
            .send(
                target,
                CheckpointRequest::PrepareForPressure { instance: target },
            )
            .await;
        match response {
            CheckpointResponse::Refused { instance, reason } => {
                assert_eq!(instance, target);
                assert!(reason.contains("different instance"), "{reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unowned_bus_name_is_refused_without_being_called() {
        let target = InstanceId::new();
        let transport = DbusCheckpointTransport::with_fakes(
            Arc::new(FixedResolver(None)),
            Arc::new(PanicsIfCalled),
        );
        let response = transport
            .send(
                target,
                CheckpointRequest::PrepareForPressure { instance: target },
            )
            .await;
        assert!(matches!(response, CheckpointResponse::Refused { .. }));
    }

    #[tokio::test]
    async fn a_verified_peer_is_actually_called_and_its_answer_decoded() {
        let target = InstanceId::new();
        let reply = serde_json::to_string(&CheckpointResponse::Prepared {
            instance: target,
            released_bytes: 4096,
        })
        .unwrap();
        let transport = DbusCheckpointTransport::with_fakes(
            Arc::new(FixedResolver(Some(peer(target)))),
            Arc::new(FixedCaller(reply)),
        );
        let response = transport
            .send(
                target,
                CheckpointRequest::PrepareForPressure { instance: target },
            )
            .await;
        assert_eq!(
            response,
            CheckpointResponse::Prepared {
                instance: target,
                released_bytes: 4096
            }
        );
    }

    #[derive(Debug)]
    struct NeverAnswers;
    #[async_trait]
    impl CheckpointCaller for NeverAnswers {
        async fn call(&self, _bus_name: &str, _request_json: &str) -> Result<String> {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_call_past_its_deadline_is_refused_rather_than_hanging() {
        let target = InstanceId::new();
        let transport = DbusCheckpointTransport::with_fakes(
            Arc::new(FixedResolver(Some(peer(target)))),
            Arc::new(NeverAnswers),
        );
        let request = CheckpointRequest::CheckpointNow {
            instance: target,
            deadline: StdDuration::from_millis(50),
        };
        let response =
            tokio::time::timeout(StdDuration::from_secs(2), transport.send(target, request))
                .await
                .expect("send() must itself return well within the outer test timeout");
        match response {
            CheckpointResponse::Refused { reason, .. } => {
                assert!(reason.contains("no answer"), "{reason}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }
}
