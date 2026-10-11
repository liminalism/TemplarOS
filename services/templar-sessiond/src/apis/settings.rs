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
    fn commit<F>(&self, key: &str, origin: &str, prepare: F) -> anyhow::Result<SettingValue>
    where
        F: FnOnce(Option<SettingValue>) -> anyhow::Result<SettingValue>,
    {
        commit_setting(
            &self.store,
            &self.schema,
            &self.values,
            key,
            origin,
            prepare,
        )
    }
}

fn commit_setting<F>(
    store: &Store,
    schema: &RwLock<SettingsSchema>,
    values: &RwLock<SettingsValues>,
    key: &str,
    origin: &str,
    prepare: F,
) -> anyhow::Result<SettingValue>
where
    F: FnOnce(Option<SettingValue>) -> anyhow::Result<SettingValue>,
{
    // Same order as Inspect. Publication remains excluded until audit and history commit.
    let mut connection = store.lock();
    let schema = schema.read();
    let mut values = values.write();
    let value = prepare(values.effective(&schema, key))?;
    let transaction = connection.transaction()?;
    let (old_json, new_json, _) = templar_session_core::settings::apply_within(
        &transaction,
        &schema,
        &values,
        key,
        &value,
        origin,
    )?;
    let event_id = EventId::new();
    let change_id = change_log::record_within(
        &transaction,
        "settings",
        key,
        old_json.as_deref(),
        Some(&new_json),
        event_id,
    )?;
    append_within(
        &transaction,
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
    values.set(key, value.clone());
    Ok(value)
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
        self.commit(key, origin, |_| Ok(value)).map_err(failed)?;
        let effects = if super::appearance::AppearanceEffects::affects(key) {
            self.appearance.reconcile(guard).await
        } else {
            drop(guard);
            Ok(())
        };
        if super::wallpaper::WallpaperHost::affects(key) {
            self.wallpaper.reconcile().await.map_err(failed)?;
        }
        let _ = emitter.changed(key.to_owned()).await;
        effects.map_err(|error| {
            failed(format!(
                "Setting committed; supported toolkit configuration incomplete: {error:#}"
            ))
        })
    }

    /// Apply a narrow mutation against the latest owned value, preserving other clients' edits.
    async fn update_launcher_v1(
        &self,
        command_json: &str,
        origin: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        use templar_types::launcher::{LauncherMutation, PREFERENCES_KEY};
        if command_json.len() > 128 * 1024 {
            return Err(fdo::Error::InvalidArgs(
                "Launcher mutation is too large".into(),
            ));
        }
        let command: LauncherMutation = serde_json::from_str(command_json)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let _guard = self.appearance.lock().await;
        let value = self
            .commit(PREFERENCES_KEY, origin, |current| {
                let json = match current {
                    Some(SettingValue::Str(json)) => json,
                    _ => "{}".into(),
                };
                let mut preferences =
                    templar_session_core::settings::parse_launcher_preferences(&json)
                        .map_err(anyhow::Error::msg)?;
                preferences.apply(&command).map_err(anyhow::Error::msg)?;
                Ok(SettingValue::Str(serde_json::to_string(&preferences)?))
            })
            .map_err(failed)?;
        let _ = emitter.changed(PREFERENCES_KEY.into()).await;
        match value {
            SettingValue::Str(json) => Ok(json),
            _ => unreachable!("launcher value is a string"),
        }
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
        let revision: i64 = connection
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM settings_changes",
                [],
                |row| row.get(0),
            )
            .map_err(failed)?;
        let revision = u64::try_from(revision).map_err(failed)?;
        let schema = self.schema.read();
        let values = self.values.read();
        let snapshot = templar_types::settings::Observation {
            revision: self.revision_owner.at_generation(revision),
            value: values
                .list(&schema)
                .into_iter()
                .map(|(key, value)| {
                    let requires_restart =
                        schema.specs.get(&key).is_some_and(|spec| spec.privileged);
                    let capability =
                        templar_session_core::settings::setting_capability(&key, requires_restart);
                    templar_types::settings::SettingObservation {
                        key,
                        value,
                        capability,
                    }
                })
                .collect::<Vec<_>>(),
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
            let changed =
                templar_session_core::settings::reset_within(&transaction, &values, key, origin)?;
            transaction.commit()?;
            if changed.is_some() {
                values.reset(key);
            }
            Ok(changed)
        })()
        .map_err(failed)?;
        let effects = if changed.is_some() && super::appearance::AppearanceEffects::affects(key) {
            self.appearance.reconcile(guard).await
        } else {
            drop(guard);
            Ok(())
        };
        if changed.is_some() && super::wallpaper::WallpaperHost::affects(key) {
            self.wallpaper.reconcile().await.map_err(failed)?;
        }
        if changed.is_some() {
            let _ = emitter.changed(key.to_owned()).await;
        }
        effects.map_err(|error| {
            failed(format!(
                "Reset committed; supported toolkit configuration incomplete: {error:#}"
            ))
        })
    }

    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, key: String) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::launcher::{
        Favorite, FavoriteTarget, LauncherMutation, LauncherPreferences, PREFERENCES_KEY,
        app_identity,
    };

    fn mutate(
        store: &Store,
        schema: &RwLock<SettingsSchema>,
        values: &RwLock<SettingsValues>,
        command: LauncherMutation,
    ) -> anyhow::Result<SettingValue> {
        commit_setting(
            store,
            schema,
            values,
            PREFERENCES_KEY,
            "test-client",
            |current| {
                let Some(SettingValue::Str(json)) = current else {
                    anyhow::bail!("missing preferences")
                };
                let mut preferences =
                    templar_session_core::settings::parse_launcher_preferences(&json)
                        .map_err(anyhow::Error::msg)?;
                preferences.apply(&command).map_err(anyhow::Error::msg)?;
                Ok(SettingValue::Str(serde_json::to_string(&preferences)?))
            },
        )
    }

    #[test]
    fn simultaneous_launcher_clients_keep_all_edits_and_commit_matching_audit_history() {
        let store = Store::in_memory().unwrap();
        let schema = Arc::new(RwLock::new(
            templar_session_core::settings::baseline_schema(),
        ));
        let values = Arc::new(RwLock::new(SettingsValues::default()));
        let barrier = Arc::new(std::sync::Barrier::new(13));
        let threads: Vec<_> = (0..12)
            .map(|index| {
                let store = store.clone();
                let schema = schema.clone();
                let values = values.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let app = format!("org.example.App{index}");
                    let command = match index % 3 {
                        0 => LauncherMutation::SetAlias {
                            identity: app_identity(&app),
                            label: Some(format!("Name {index}")),
                        },
                        1 => LauncherMutation::SetHidden {
                            identity: app_identity(&app),
                            hidden: true,
                        },
                        _ => LauncherMutation::UpsertFavorite {
                            favorite: Favorite {
                                id: format!("favorite{index}"),
                                label: format!("Favorite {index}"),
                                target: FavoriteTarget::App { app_id: app },
                            },
                        },
                    };
                    barrier.wait();
                    mutate(&store, &schema, &values, command).unwrap();
                })
            })
            .collect();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        let Some(SettingValue::Str(json)) =
            values.read().effective(&schema.read(), PREFERENCES_KEY)
        else {
            panic!("missing preferences")
        };
        let preferences: LauncherPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(preferences.aliases.len(), 4);
        assert_eq!(preferences.visibility.hidden_apps.len(), 4);
        assert_eq!(preferences.favorites.len(), 4);
        assert_eq!(
            templar_session_core::settings::load_values(&store)
                .unwrap()
                .effective(&schema.read(), PREFERENCES_KEY),
            Some(SettingValue::Str(json.clone()))
        );
        for table in ["settings_changes", "history"] {
            let count: i64 = store
                .lock()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 12);
        }
        assert!(
            mutate(
                &store,
                &schema,
                &values,
                LauncherMutation::SetHidden {
                    identity: "path:cmVsYXRpdmU=".into(),
                    hidden: true
                }
            )
            .is_err()
        );
        assert_eq!(
            values.read().effective(&schema.read(), PREFERENCES_KEY),
            Some(SettingValue::Str(json))
        );
        let count: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM settings_changes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 12,
            "invalid mutation publishes neither state nor an audit row"
        );
    }
}
