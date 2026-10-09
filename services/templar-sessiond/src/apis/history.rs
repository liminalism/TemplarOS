//! `org.templaros.History1`, including the cross-domain undo dispatcher.
//!
//! A `change_log` entry's `domain` field says which of associations/settings/providers wrote
//! it. Reversing it is inherently cross-domain — this is the one place in the daemon that needs
//! to know about all three, exactly the kind of composition ADR-0001 puts in the thin daemon
//! rather than in any one domain crate.

use crate::apis::failed;
use anyhow::{Context, bail};
use parking_lot::RwLock;
use std::{path::PathBuf, sync::Arc};
use templar_associationd::engine::AssociationEngine;
use templar_session_core::{
    change_log,
    history::{
        Event, EventAction, EventId, EventKind, HistoryStore, SqliteHistory, append_within,
        find_within, mark_not_reversible_within,
    },
    providers::{self, ProviderRegistry},
    settings::{self, SettingsValues},
};
use templar_state::Store;
use templar_types::{ProviderRole, dbus::session as names};
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct HistoryApi {
    history: Arc<SqliteHistory>,
    store: Store,
    engine: Arc<RwLock<AssociationEngine>>,
    settings_values: Arc<RwLock<SettingsValues>>,
    providers: Arc<RwLock<ProviderRegistry>>,
    mimeapps_path: PathBuf,
    appearance: Arc<super::appearance::AppearanceEffects>,
    wallpaper: super::wallpaper::WallpaperHost,
}

impl HistoryApi {
    pub fn new(
        history: Arc<SqliteHistory>,
        store: Store,
        engine: Arc<RwLock<AssociationEngine>>,
        settings_values: Arc<RwLock<SettingsValues>>,
        providers: Arc<RwLock<ProviderRegistry>>,
        mimeapps_path: PathBuf,
        appearance: Arc<super::appearance::AppearanceEffects>,
        wallpaper: super::wallpaper::WallpaperHost,
    ) -> Self {
        Self {
            history,
            store,
            engine,
            settings_values,
            providers,
            mimeapps_path,
            appearance,
            wallpaper,
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

#[zbus::interface(name = "org.templaros.History1")]
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
            .map_err(|error: templar_types::AppIdError| {
                fdo::Error::InvalidArgs(error.to_string())
            })?;
        let instance_id =
            nonempty(instance_id).and_then(|value| templar_types::InstanceId::parse(&value));
        let actions: Vec<EventAction> = if actions_json.is_empty() {
            Vec::new()
        } else {
            serde_json::from_str(actions_json).map_err(|error| {
                fdo::Error::InvalidArgs(format!("invalid actions_json: {error}"))
            })?
        };

        let event = Event {
            id: EventId::new(),
            at_ms: templar_state::now_ms(),
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

        let guard = self.appearance.lock().await;
        let reverted_domain = (|| -> anyhow::Result<Option<(String, String)>> {
            let mut connection = self.store.lock();
            let mut values = self.settings_values.write();
            let transaction = connection.transaction()?;
            let tx = &transaction;
            let mut pending_setting = None;
            let mut pending_provider = None;
            let mut pending_association = None;
            let domain = (|| -> anyhow::Result<Option<(String, String)>> {
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
                        let engine = self.engine.write();
                        templar_associationd::store::revert_within(tx, &entry)?;
                        let restored = templar_associationd::store::load_db_within(tx)?;
                        pending_association = Some((engine, restored));
                    }
                    "settings" => {
                        let restored = settings::revert_within(
                            tx, &entry.key, entry.old_value.as_deref(),
                        )?;
                        pending_setting = Some((entry.key.clone(), restored));
                    }
                    "providers.selection" => {
                        let role = ProviderRole::ALL
                            .iter()
                            .copied()
                            .find(|role| role.as_str() == entry.key)
                            .context("unknown provider role in change_log entry")?;
                        let registry = self.providers.write();
                        let restored = providers::revert_selection_within(
                            tx, role, entry.old_value.as_deref(),
                        )?;
                        pending_provider = Some((registry, role, restored));
                    }
                    "background.approval" => {
                        // No in-memory cache to reload afterward: `Background1` always reads
                        // approvals straight from the store.
                        templar_session_core::autostart::revert_approval_within(
                            tx,
                            &entry.key,
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
                        at_ms: templar_state::now_ms(),
                        kind: event.kind,
                        app_id: event.app_id.clone(),
                        instance_id: event.instance_id,
                        message: format!("Undone: {}", event.message),
                        cause: Some("undo".into()),
                        actions: Vec::new(),
                        reversible: false,
                    },
                )?;
                Ok(Some((entry.domain, entry.key)))
            })()?;
            transaction.commit()?;
            if let Some((key, restored)) = pending_setting {
                match restored {
                    Some(value) => values.set(&key, value),
                    None => { values.reset(&key); }
                }
            }
            if let Some((mut registry, role, restored)) = pending_provider {
                match restored {
                    Some(app_id) => registry.select(role, app_id),
                    None => { registry.deselect(role); }
                }
            }
            if let Some((mut engine, restored)) = pending_association {
                engine.replace_database(restored);
                if let Err(error) = engine.export_mimeapps(&self.mimeapps_path) {
                    tracing::warn!(%error, path = %self.mimeapps_path.display(), "undo committed but regenerating mimeapps.list failed");
                }
            }
            Ok(domain)
        })().map_err(failed)?;

        let Some((domain, mut key)) = reverted_domain else {
            return Ok(false);
        };
        let effects = if domain == "settings" && super::appearance::AppearanceEffects::affects(&key)
        {
            self.appearance.reconcile(guard).await
        } else {
            drop(guard);
            Ok(())
        };
        if domain == "settings" && super::wallpaper::WallpaperHost::affects(&key) {
            self.wallpaper.reconcile().await.map_err(failed)?;
        }
        if let Some(scope) = domain.strip_prefix("associations.") {
            if scope == "project-mime" {
                key = key.replacen('\u{1f}', ":", 1);
            }
            if let Err(error) = emitter
                .connection()
                .emit_signal(
                    None::<&str>,
                    names::ASSOCIATIONS_PATH,
                    "org.templaros.Associations1",
                    "DefaultsChanged",
                    &(scope, key.as_str()),
                )
                .await
            {
                tracing::warn!(%error, "undo committed but association change notification failed");
            }
        }
        if domain == "settings" || domain == "providers.selection" {
            let (path, interface) = if domain == "settings" {
                (names::SETTINGS_PATH, "org.templaros.Settings1")
            } else {
                (names::PROVIDERS_PATH, "org.templaros.Providers1")
            };
            if let Err(error) = emitter
                .connection()
                .emit_signal(None::<&str>, path, interface, "Changed", &(key,))
                .await
            {
                tracing::warn!(%error, "undo committed but domain change notification failed");
            }
        }
        let _ = emitter.changed().await;
        effects.map_err(|error| {
            failed(format!(
                "Undo committed; supported toolkit configuration incomplete: {error:#}"
            ))
        })?;
        Ok(true)
    }

    /// A new event was appended, or one was undone; re-call `Recent`/`Search` for current state.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// A handle background work (autostart, crash ingestion, the checkpoint relay) uses to tell
/// already-connected clients that history changed — the same `History1.Changed` a D-Bus-invoked
/// mutation already emits via `#[zbus(signal_emitter)]`, so the shell's history view does not
/// silently miss a crash, an autostart decision or a pressure action: precisely the planning
/// document's own examples of things a user must be able to see and undo. Background work still
/// owns *how* it persists an event; this only adds the signal.
#[derive(Clone)]
pub struct HistoryChangeNotifier(zbus::object_server::InterfaceRef<HistoryApi>);

impl HistoryChangeNotifier {
    pub fn new(interface: zbus::object_server::InterfaceRef<HistoryApi>) -> Self {
        Self(interface)
    }

    /// Emit `History1.Changed`. Best-effort: the event this follows is already durably
    /// persisted, so a signal failure is logged, never propagated as if the append itself failed.
    pub async fn notify(&self) {
        if let Err(error) = self.0.changed().await {
            tracing::debug!(%error, "failed to emit History1.Changed");
        }
    }
}
