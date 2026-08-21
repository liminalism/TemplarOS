//! `org.legeos.History1`, including the cross-domain undo dispatcher.
//!
//! A `change_log` entry's `domain` field says which of associations/settings/providers wrote
//! it. Reversing it is inherently cross-domain — this is the one place in the daemon that needs
//! to know about all three, exactly the kind of composition ADR-0001 puts in the thin daemon
//! rather than in any one domain crate.

use crate::apis::failed;
use anyhow::{Context, bail};
use lege_associationd::engine::AssociationEngine;
use lege_session_core::{
    change_log,
    history::{
        Event, EventAction, EventId, EventKind, HistoryStore, SqliteHistory, append_within,
        find_within, mark_not_reversible_within,
    },
    providers::{self, ProviderRegistry},
    settings::{self, SettingsValues},
};
use lege_state::Store;
use lege_types::ProviderRole;
use parking_lot::RwLock;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct HistoryApi {
    history: Arc<SqliteHistory>,
    store: Store,
    engine: Arc<RwLock<AssociationEngine>>,
    settings_values: Arc<RwLock<SettingsValues>>,
    providers: Arc<RwLock<ProviderRegistry>>,
}

impl HistoryApi {
    pub fn new(
        history: Arc<SqliteHistory>,
        store: Store,
        engine: Arc<RwLock<AssociationEngine>>,
        settings_values: Arc<RwLock<SettingsValues>>,
        providers: Arc<RwLock<ProviderRegistry>>,
    ) -> Self {
        Self {
            history,
            store,
            engine,
            settings_values,
            providers,
        }
    }
}

fn to_json_vec(events: Vec<Event>) -> fdo::Result<Vec<String>> {
    events
        .into_iter()
        .map(|event| {
            serde_json::to_string(&event).map_err(|error| fdo::Error::Failed(error.to_string()))
        })
        .collect()
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

#[zbus::interface(name = "org.legeos.History1")]
impl HistoryApi {
    async fn recent(&self, limit: u32) -> fdo::Result<Vec<String>> {
        to_json_vec(self.history.recent(limit as usize).map_err(failed)?)
    }

    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<String>> {
        to_json_vec(self.history.search(query, limit as usize).map_err(failed)?)
    }

    #[allow(clippy::too_many_arguments)]
    async fn append(
        &self,
        kind: &str,
        app_id: &str,
        instance_id: &str,
        message: &str,
        cause: &str,
        actions_json: &str,
        reversible: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let kind = EventKind::parse(kind)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown event kind {kind:?}")))?;
        let app_id = nonempty(app_id)
            .map(|value| value.parse())
            .transpose()
            .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        let instance_id =
            nonempty(instance_id).and_then(|value| lege_types::InstanceId::parse(&value));
        let actions: Vec<EventAction> = if actions_json.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(actions_json).map_err(|error| {
                fdo::Error::InvalidArgs(format!("invalid actions_json: {error}"))
            })?
        };

        let event = Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind,
            app_id,
            instance_id,
            message: message.to_owned(),
            cause: nonempty(cause),
            actions,
            reversible,
        };
        let id = event.id;
        self.history.append(event).map_err(failed)?;
        let _ = emitter.changed().await;
        Ok(id.to_string())
    }

    async fn undo(
        &self,
        event_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let event_id: EventId = event_id
            .parse()
            .map_err(|_| fdo::Error::InvalidArgs("event_id is not a UUID".into()))?;

        let reverted_domain = self
            .store
            .transaction(|tx| -> anyhow::Result<Option<String>> {
                let Some(event) = find_within(tx, event_id)? else {
                    return Ok(None);
                };
                if !event.reversible {
                    return Ok(None);
                }
                let Some(change_id) = event
                    .actions
                    .iter()
                    .find_map(|action| change_log::parse_undo_command(&action.command))
                else {
                    return Ok(None);
                };
                let Some(entry) = change_log::find_within(tx, change_id)? else {
                    return Ok(None);
                };
                if entry.undone {
                    return Ok(None);
                }

                match entry.domain.as_str() {
                    domain if domain.starts_with("associations.") => {
                        lege_associationd::store::revert_within(tx, &entry)?;
                    }
                    "settings" => {
                        let mut values = self.settings_values.write();
                        settings::revert_within(
                            tx,
                            &mut values,
                            &entry.key,
                            entry.old_value.as_deref(),
                        )?;
                    }
                    "providers.selection" => {
                        let role = ProviderRole::ALL
                            .iter()
                            .copied()
                            .find(|role| role.as_str() == entry.key)
                            .context("unknown provider role in change_log entry")?;
                        let mut registry = self.providers.write();
                        providers::revert_selection_within(
                            tx,
                            &mut registry,
                            role,
                            entry.old_value.as_deref(),
                        )?;
                    }
                    other => bail!("no undo applier registered for change_log domain {other:?}"),
                }

                change_log::mark_undone_within(tx, change_id)?;
                mark_not_reversible_within(tx, event_id)?;
                append_within(
                    tx,
                    &Event {
                        id: EventId::new(),
                        at_ms: lege_state::now_ms(),
                        kind: event.kind,
                        app_id: event.app_id.clone(),
                        instance_id: event.instance_id,
                        message: format!("Undone: {}", event.message),
                        cause: Some("undo".into()),
                        actions: Vec::new(),
                        reversible: false,
                    },
                )?;
                Ok(Some(entry.domain))
            })
            .map_err(failed)?;

        let Some(domain) = reverted_domain else {
            return Ok(false);
        };
        if domain.starts_with("associations.") {
            match lege_associationd::store::load_db(&self.store) {
                Ok(db) => self.engine.write().replace_database(db),
                Err(error) => {
                    tracing::warn!(%error, "undo committed but reloading the association database failed");
                }
            }
        }
        let _ = emitter.changed().await;
        Ok(true)
    }

    /// A new event was appended, or one was undone; re-call `Recent`/`Search` for current state.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
