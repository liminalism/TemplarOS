//! The wire transport that delivers a [`CheckpointRequest`] to a live application instance.
//!
//! `docs/protocols/checkpoint.md` defines the *protocol* (`lege_types::checkpoint`,
//! `lege_client::sdk::CheckpointHandler`) but deliberately not its transport — no
//! `org.legeos.*` interface for "call into this running instance" exists anywhere in this
//! codebase yet, and adding one is out of this track's file ownership (it would need a new
//! `interfaces/dbus/*.xml` beyond the nine this track owns, and very likely a
//! `lege_types::dbus` addition, which is off-limits here). This module is the seam sessiond
//! needs on both sides that *do* belong to this track — the checkpoint relay
//! ([`crate::background::checkpoint_relay`]) and `Projects1` open/close
//! ([`crate::apis::projects`]) — so that wiring up a real transport later touches one function,
//! not every call site.
//!
//! [`NullCheckpointTransport`] is not a stub to be embarrassed about: it implements exactly the
//! documented fallback for an application with no transport — "legacy applications are governed
//! with less confidence (no unsaved-work signal, no restore)" — by always answering `Refused`.

use async_trait::async_trait;
use lege_types::{
    InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_transport_always_refuses() {
        let instance = InstanceId::new();
        let response = NullCheckpointTransport
            .send(
                instance,
                CheckpointRequest::CheckpointNow {
                    instance,
                    deadline: std::time::Duration::from_secs(1),
                },
            )
            .await;
        assert!(matches!(response, CheckpointResponse::Refused { .. }));
    }
}
