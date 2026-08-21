//! The wire transport that delivers a [`CheckpointRequest`] to a live application instance.
//!
//! `docs/protocols/checkpoint.md` defines the protocol; `org.legeos.Checkpoint1`
//! (`interfaces/dbus/org.legeos.Checkpoint1.xml`, `lege_types::dbus::checkpoint`) is the wire
//! format an application instance serves for itself, at a bus name derived from its own
//! `InstanceId` — nothing to register, no directory to go stale. [`DbusCheckpointTransport`] is
//! the real implementation; [`NullCheckpointTransport`] is kept for `--dry-run` and tests, and
//! doubles as the documented legacy fallback: an application that cannot be reached is a legacy
//! application, governed with less confidence, and the governor may still terminate it after the
//! deadline.
//!
//! A bus name is a claim, exactly like a Wayland `app_id`, so [`DbusCheckpointTransport`] never
//! trusts one without checking it: it resolves the claimed name's PID
//! (`org.freedesktop.DBus.GetConnectionUnixProcessID`) and confirms that PID's cgroup actually
//! names the target instance's unit — the same property `AppIdentity1` checks for a window,
//! using the same self-describing unit name (`unit::parse_cgroup_path`) that lets it work even
//! against a cgroup sessiond never registered. An application that merely claims another
//! instance's bus name cannot answer "checkpoint complete" on its behalf.
//!
//! Peer resolution and the actual method call are behind two small traits
//! ([`PeerResolver`], [`CheckpointCaller`]) so verification and decoding are unit-tested without
//! a live bus — mirrors `lege-hostd`'s `CallerIdentity` (`services/lege-hostd/src/session_report.rs`).

use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_types::{
    AppId, InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
    dbus::checkpoint as wire,
    unit,
};
use std::{sync::Arc, time::Duration};

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

/// Resolves a claimed `org.legeos.Instance.i*` bus name to the `(AppId, InstanceId)` its cgroup
/// actually names, or `None` if nobody currently owns that name. A trait so verification is
/// testable without a real bus or `/proc`.
#[async_trait]
trait PeerResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(&self, bus_name: &str) -> Result<Option<(AppId, InstanceId)>>;
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
}

#[async_trait]
impl PeerResolver for DbusPeerResolver {
    async fn resolve(&self, bus_name: &str) -> Result<Option<(AppId, InstanceId)>> {
        let dbus = zbus::fdo::DBusProxy::new(&self.connection)
            .await
            .context("connecting to org.freedesktop.DBus")?;
        let name = zbus::names::BusName::try_from(bus_name)
            .with_context(|| format!("{bus_name} is not a valid bus name"))?;
        // No owner: not an error, just nobody to verify.
        let Ok(pid) = dbus.get_connection_unix_process_id(name).await else {
            return Ok(None);
        };
        let cgroup = lege_appd::identity::unified_cgroup_of(pid)
            .with_context(|| format!("reading the cgroup of pid {pid} (peer of {bus_name})"))?;
        Ok(unit::parse_cgroup_path(&cgroup))
    }
}

#[derive(Debug, Clone)]
struct DbusCaller {
    connection: zbus::Connection,
}

#[async_trait]
impl CheckpointCaller for DbusCaller {
    async fn call(&self, bus_name: &str, request_json: &str) -> Result<String> {
        let proxy = lege_client::proxies::CheckpointProxy::builder(&self.connection)
            .destination(bus_name)
            .with_context(|| format!("{bus_name} is not a valid destination"))?
            .build()
            .await
            .with_context(|| format!("connecting to {bus_name}"))?;
        proxy
            .request(request_json)
            .await
            .context("org.legeos.Checkpoint1.Request")
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
        CheckpointRequest::PrepareForPressure { .. } | CheckpointRequest::RestoreInstance { .. } => {
            Duration::from_secs(5)
        }
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
        Err(error) => refused(instance, format!("invalid response from the instance: {error}")),
    }
}

/// The real transport: verify, then call, both bounded by [`call_budget`].
#[derive(Debug)]
pub struct DbusCheckpointTransport {
    resolver: Arc<dyn PeerResolver>,
    caller: Arc<dyn CheckpointCaller>,
}

impl DbusCheckpointTransport {
    pub fn new(connection: zbus::Connection) -> Self {
        Self {
            resolver: Arc::new(DbusPeerResolver {
                connection: connection.clone(),
            }),
            caller: Arc::new(DbusCaller { connection }),
        }
    }

    #[cfg(test)]
    fn with_fakes(resolver: Arc<dyn PeerResolver>, caller: Arc<dyn CheckpointCaller>) -> Self {
        Self { resolver, caller }
    }

    async fn attempt(&self, instance: InstanceId, request: &CheckpointRequest) -> CheckpointResponse {
        let bus_name = wire::instance_bus_name(instance);

        let resolved = match self.resolver.resolve(&bus_name).await {
            Ok(resolved) => resolved,
            Err(error) => return refused(instance, format!("could not resolve {bus_name}: {error}")),
        };
        let Some((_app_id, resolved_instance)) = resolved else {
            return refused(instance, format!("no application instance currently owns {bus_name}"));
        };
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
            Err(error) => return refused(instance, format!("failed to encode the checkpoint request: {error}")),
        };
        match self.caller.call(&bus_name, &request_json).await {
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
            Err(_) => refused(instance, format!("no answer from the instance within {budget:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

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
    struct FixedResolver(Option<(AppId, InstanceId)>);
    #[async_trait]
    impl PeerResolver for FixedResolver {
        async fn resolve(&self, _bus_name: &str) -> Result<Option<(AppId, InstanceId)>> {
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
        async fn call(&self, _bus_name: &str, _request_json: &str) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    fn app_id() -> AppId {
        "org.example.Impostor".parse().unwrap()
    }

    #[tokio::test]
    async fn a_peer_whose_cgroup_names_a_different_instance_is_refused_without_being_called() {
        let target = InstanceId::new();
        let impostor_instance = InstanceId::new();
        let transport = DbusCheckpointTransport::with_fakes(
            Arc::new(FixedResolver(Some((app_id(), impostor_instance)))),
            Arc::new(PanicsIfCalled),
        );
        let response = transport
            .send(target, CheckpointRequest::PrepareForPressure { instance: target })
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
        let transport =
            DbusCheckpointTransport::with_fakes(Arc::new(FixedResolver(None)), Arc::new(PanicsIfCalled));
        let response = transport
            .send(target, CheckpointRequest::PrepareForPressure { instance: target })
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
            Arc::new(FixedResolver(Some((app_id(), target)))),
            Arc::new(FixedCaller(reply)),
        );
        let response = transport
            .send(target, CheckpointRequest::PrepareForPressure { instance: target })
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
            Arc::new(FixedResolver(Some((app_id(), target)))),
            Arc::new(NeverAnswers),
        );
        let request = CheckpointRequest::CheckpointNow {
            instance: target,
            deadline: StdDuration::from_millis(50),
        };
        let response = tokio::time::timeout(StdDuration::from_secs(2), transport.send(target, request))
            .await
            .expect("send() must itself return well within the outer test timeout");
        match response {
            CheckpointResponse::Refused { reason, .. } => assert!(reason.contains("no answer"), "{reason}"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

}
