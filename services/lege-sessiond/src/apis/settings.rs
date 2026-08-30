//! `org.legeos.Settings1`.

use crate::apis::failed;
use lege_session_core::{
    change_log,
    history::{Event, EventAction, EventId, EventKind, append_within},
    settings::{SettingValue, SettingsSchema, SettingsValues},
};
use lege_state::Store;
use parking_lot::RwLock;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct SettingsApi {
    schema: Arc<RwLock<SettingsSchema>>,
    values: Arc<RwLock<SettingsValues>>,
    store: Store,
}

impl SettingsApi {
    pub fn new(
        schema: Arc<RwLock<SettingsSchema>>,
        values: Arc<RwLock<SettingsValues>>,
        store: Store,
    ) -> Self {
        Self {
            schema,
            values,
            store,
        }
    }
}

#[zbus::interface(name = "org.legeos.Settings1")]
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
        self.store
            .transaction(|tx| {
                let schema = self.schema.read();
                let mut values = self.values.write();
                let (old_json, new_json, _audit_id) = lege_session_core::settings::apply_within(
                    tx,
                    &schema,
                    &mut values,
                    key,
                    value,
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
                        at_ms: lege_state::now_ms(),
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
                )
            })
            .map_err(failed)?;
        let _ = emitter.changed(key.to_owned()).await;
        Ok(())
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
        let changed = self
            .store
            .transaction(|tx| {
                let mut values = self.values.write();
                lege_session_core::settings::reset_within(tx, &mut values, key, origin)
            })
            .map_err(failed)?;
        if changed.is_some() {
            let _ = emitter.changed(key.to_owned()).await;
        }
        Ok(())
    }

    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, key: String) -> zbus::Result<()>;
}
