//! Subscribes to `org.legeos.Resources1`'s `PressureAction` signal and relays `trim-caches` /
//! `request-checkpoint` actions to the target instance through
//! [`crate::checkpoint::CheckpointTransport`], records the outcome in history, and pushes the
//! reply back as `SetInstanceHints(has_unsaved_work=…)`.

use crate::checkpoint::CheckpointTransport;
use futures_util::StreamExt;
use lege_appd::Registry;
use lege_client::proxies::ResourcesProxy;
use lege_session_core::history::{Event, EventId, EventKind, HistoryStore, SqliteHistory};
use lege_types::{
    AppCatalog, InstanceId, ResourceClass,
    checkpoint::{CheckpointRequest, CheckpointResponse},
};
use std::{sync::Arc, time::Duration};

/// Run until the connection or the `Resources1` service goes away. Intended to be spawned once
/// at startup and left running for the session's lifetime.
pub async fn run(
    connection: zbus::Connection,
    registry: Arc<Registry>,
    checkpoint: Arc<dyn CheckpointTransport>,
    history: Arc<SqliteHistory>,
) {
    let proxy = match ResourcesProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::warn!(%error, "could not connect to org.legeos.Resources1; checkpoint relay disabled");
            return;
        }
    };
    let mut signals = match proxy.receive_pressure_action().await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::warn!(%error, "could not subscribe to PressureAction; checkpoint relay disabled");
            return;
        }
    };

    while let Some(signal) = signals.next().await {
        let Ok(args) = signal.args() else {
            continue;
        };
        if !*args.applied() {
            continue; // observe-only mode, or a stage below this action: nothing to relay yet.
        }
        let Some(instance_id) = InstanceId::parse(args.instance_id()) else {
            continue;
        };
        let request = match args.kind().as_str() {
            "trim-caches" => CheckpointRequest::PrepareForPressure {
                instance: instance_id,
            },
            "request-checkpoint" => CheckpointRequest::CheckpointNow {
                instance: instance_id,
                deadline: Duration::from_secs(3),
            },
            _ => continue, // set-memory-high / reclaim / kill-cgroup / restore-limits: not ours.
        };

        let response = checkpoint.send(instance_id, request).await;
        let (message, has_unsaved_work) = describe(&response);
        let app_id = registry.instance(instance_id).map(|record| record.app_id);

        if let Err(error) = history.append(Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind: EventKind::ResourceAction,
            app_id: app_id.clone(),
            instance_id: Some(instance_id),
            message,
            cause: Some(format!("pressure:{}", args.kind())),
            actions: Vec::new(),
            reversible: false,
        }) {
            tracing::warn!(%error, "failed to record checkpoint-relay history event");
        }

        let (protected, disposable, supports_checkpoint) = app_id
            .and_then(|app_id| registry.manifest(&app_id))
            .map_or((false, false, false), |manifest| {
                (
                    manifest.resource_class == ResourceClass::SessionCritical,
                    manifest.resource_class == ResourceClass::Disposable,
                    manifest.supports_checkpoint,
                )
            });
        if let Err(error) = proxy
            .set_instance_hints(
                &instance_id.to_string(),
                protected,
                disposable,
                supports_checkpoint,
                has_unsaved_work,
            )
            .await
        {
            tracing::warn!(%error, %instance_id, "failed to push checkpoint reply back to Resources1");
        }
    }
}

fn describe(response: &CheckpointResponse) -> (String, bool) {
    match response {
        CheckpointResponse::Prepared { released_bytes, .. } => (
            format!(
                "Caches were trimmed under memory pressure ({released_bytes} bytes released, best effort)."
            ),
            false,
        ),
        CheckpointResponse::CheckpointComplete {
            has_unsaved_work, ..
        } => (
            format!(
                "State was checkpointed under memory pressure ({}).",
                if *has_unsaved_work {
                    "unsaved work preserved"
                } else {
                    "no unsaved work"
                }
            ),
            *has_unsaved_work,
        ),
        CheckpointResponse::Refused { reason, .. } => (
            format!("Could not checkpoint under memory pressure: {reason}."),
            false,
        ),
    }
}
