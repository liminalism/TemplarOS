//! `org.templaros.Settings1`.

use crate::apis::failed;
use parking_lot::RwLock;
use std::sync::Arc;
use templar_session_core::{
    change_log,
    history::{Event, EventAction, EventId, EventKind, append_within},
    settings::{SettingValue, SettingsSchema, SettingsValues},
};
use templar_state::Store;
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct SettingsApi {
    schema: Arc<RwLock<SettingsSchema>>,
    values: Arc<RwLock<SettingsValues>>,
    store: Store,
    revision_owner: templar_types::settings::Revision,
    appearance: Arc<super::appearance::AppearanceEffects>,
    wallpaper: super::wallpaper::WallpaperHost,
}

impl SettingsApi {
    pub fn new(
        schema: Arc<RwLock<SettingsSchema>>,
        values: Arc<RwLock<SettingsValues>>,
        store: Store,
        appearance: Arc<super::appearance::AppearanceEffects>,
        wallpaper: super::wallpaper::WallpaperHost,
    ) -> Self {
        Self {
            schema,
            values,
            store,
            revision_owner: templar_types::settings::Revision::new_owner(),
            appearance,
            wallpaper,
        }
    }
}

#[zbus::interface(name = "org.templaros.Settings1")]
impl SettingsApi {
    /// `(key, description, default-json, privileged)`
    async fn schema(&self) -> Vec<(String, String, String, bool)> {
        self.schema
            .read()
            .specs
            .values()
            .map(|spec| {
                (
                    spec.key.clone(),
                    spec.description.clone(),
                    serde_json::to_string(&spec.default).unwrap_or_default(),
                    spec.privileged,
                )
            })
            .collect()
    }

    /// Complete [`SettingSpec`] documents. `Schema` remains for compact/older clients; graphical
    /// editors use this method so choice sets and numeric bounds come from policy rather than
    /// being duplicated in the UI.
    async fn schema_details(&self) -> Vec<String> {
        self.schema
            .read()
            .specs
            .values()
            .filter_map(|spec| serde_json::to_string(spec).ok())
            .collect()
    }

    async fn get(&self, key: &str) -> fdo::Result<String> {
        let schema = self.schema.read();
        let values = self.values.read();
        let value = values
            .effective(&schema, key)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown setting {key:?}")))?;
        serde_json::to_string(&value).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn set(
        &self,
        key: &str,
        value_json: &str,
        origin: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let value: SettingValue = serde_json::from_str(value_json)
            .map_err(|error| fdo::Error::InvalidArgs(format!("invalid value JSON: {error}")))?;
        let guard = self.appearance.lock().await;
        (|| -> anyhow::Result<()> {
                // Store -> schema -> values is also Inspect's lock order. Keep publication
                // excluded until the entire audit/history transaction has committed.
                let mut connection = self.store.lock();
                let schema = self.schema.read();
                let mut values = self.values.write();
                let transaction = connection.transaction()?;
                let tx = &transaction;
                let (old_json, new_json, _audit_id) = templar_session_core::settings::apply_within(
                    tx,
                    &schema,
                    &values,
                    key,
                    &value,
                    origin,
                )?;

                let event_id = EventId::new();
                let change_id = change_log::record_within(
                    tx,
                    "settings",
                    key,
                    old_json.as_deref(),
                    Some(&new_json),
                    event_id,
                )?;
                append_within(
                    tx,
                    &Event {
                        id: event_id,
                        at_ms: templar_state::now_ms(),
                        kind: EventKind::SettingChange,
                        app_id: None,
                        instance_id: None,
                        message: format!("The setting {key} was changed."),
                        cause: Some(origin.to_owned()),
                        actions: vec![EventAction {
                            label: "Undo".into(),
                            command: change_log::undo_command(change_id),
                        }],
                        reversible: true,
                    },
                )?;
                transaction.commit()?;
                values.set(key, value);
                Ok(())
            })().map_err(failed)?;
        let effects = if super::appearance::AppearanceEffects::affects(key) {
            self.appearance.reconcile(guard).await
        } else {
            drop(guard);
            Ok(())
        };
        if super::wallpaper::WallpaperHost::affects(key) { self.wallpaper.reconcile().await.map_err(failed)?; }
        let _ = emitter.changed(key.to_owned()).await;
        effects.map_err(|error| failed(format!("Setting committed; supported toolkit configuration incomplete: {error:#}")))
    }

    /// Every known key at its effective value, as `(key, value_json)` pairs.
    async fn list(&self) -> Vec<(String, String)> {
        let schema = self.schema.read();
        let values = self.values.read();
        values
            .list(&schema)
            .into_iter()
            .map(|(key, value)| (key, serde_json::to_string(&value).unwrap_or_default()))
            .collect()
    }

    /// Audit revision, capability and stored effective values under one write exclusion.
    async fn inspect(&self) -> fdo::Result<String> {
        let connection = self.store.lock();
        let revision: i64 = connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM settings_changes", [], |row| row.get(0)
        ).map_err(failed)?;
        let revision = u64::try_from(revision).map_err(failed)?;
        let schema = self.schema.read();
        let values = self.values.read();
        let snapshot = templar_types::settings::Observation {
            revision: self.revision_owner.at_generation(revision),
            value: values.list(&schema).into_iter().map(|(key, value)| {
                let requires_restart = schema.specs.get(&key).is_some_and(|spec| spec.privileged);
                templar_types::settings::SettingObservation {
                    key,
                    value,
                    capability: templar_types::settings::Capability::Available { requires_restart },
                }
            }).collect::<Vec<_>>(),
        };
        serde_json::to_string(&snapshot).map_err(failed)
    }

    /// Recent `settings_changes` audit rows as JSON.
    async fn changes(&self, limit: u32) -> fdo::Result<Vec<String>> {
        let connection = self.store.lock();
        let mut statement = connection
            .prepare("SELECT at_ms, key, old_json, new_json, origin FROM settings_changes ORDER BY id DESC LIMIT ?1")
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let rows = statement
            .query_map([i64::from(limit)], |row| {
                Ok(serde_json::json!({
                    "at_ms": row.get::<_, i64>(0)?,
                    "key": row.get::<_, String>(1)?,
                    "old_json": row.get::<_, Option<String>>(2)?,
                    "new_json": row.get::<_, String>(3)?,
                    "origin": row.get::<_, String>(4)?,
                })
                .to_string())
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn reset(
        &self,
        key: &str,
        origin: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let guard = self.appearance.lock().await;
        let changed = (|| -> anyhow::Result<Option<String>> {
                let mut connection = self.store.lock();
                let mut values = self.values.write();
                let transaction = connection.transaction()?;
                let changed = templar_session_core::settings::reset_within(
                    &transaction, &values, key, origin,
                )?;
                transaction.commit()?;
                if changed.is_some() { values.reset(key); }
                Ok(changed)
            })().map_err(failed)?;
        let effects = if changed.is_some() && super::appearance::AppearanceEffects::affects(key) {
            self.appearance.reconcile(guard).await
        } else {
            drop(guard);
            Ok(())
        };
        if changed.is_some() && super::wallpaper::WallpaperHost::affects(key) { self.wallpaper.reconcile().await.map_err(failed)?; }
        if changed.is_some() {
            let _ = emitter.changed(key.to_owned()).await;
        }
        effects.map_err(|error| failed(format!("Reset committed; supported toolkit configuration incomplete: {error:#}")))
    }

    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, key: String) -> zbus::Result<()>;
}
