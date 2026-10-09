//! Input1 owns durable preferences; only the compositor's libinput thread changes devices.
use crate::apis::failed;
use anyhow::{Context, Result, bail};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use templar_client::compositor_control::CompositorControl;
use templar_session_core::settings::SettingValue;
use templar_state::Store;
use templar_types::{
    compositor_control::{ControlRequest, ControlResponse, MAX_CONTROL_FRAME},
    input::{self, InputConfiguration, InputId, InputObservation},
    settings::{Capability, Revision},
};
use zbus::fdo;

pub const PATH: &str = "/org/templaros/Session/Input";
const KEY: &str = "_internal.input.overrides";
const LEGACY_TAP_KEY: &str = "input.touchpad.tap-to-click";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u32,
    overrides: BTreeMap<InputId, InputConfiguration>,
    legacy_tap: Option<bool>,
    /// None means no eligible native inventory has been observed yet. Some is a finite cutover
    /// set, not an indefinitely inherited global preference for newly attached hardware.
    migration_pending: Option<BTreeSet<InputId>>,
    consumed: BTreeSet<InputId>,
}
impl Preferences {
    fn consume(&mut self, id: &InputId) {
        self.consumed.insert(id.clone());
        if let Some(pending) = &mut self.migration_pending {
            pending.remove(id);
            if pending.is_empty() {
                self.legacy_tap = None;
            }
        }
    }
    fn capture_eligible(&mut self, observation: &InputObservation) -> bool {
        if self.legacy_tap.is_none()
            || self.migration_pending.is_some()
            || !observation.session_active
            || observation.locked
        {
            return false;
        }
        let eligible: BTreeSet<_> = observation
            .devices
            .iter()
            .filter(|device| {
                device.touchpad
                    && matches!(device.capabilities.tap, Capability::Available { .. })
                    && matches!(device.configuration, Capability::Available { .. })
                    && matches!(device.persistence, Capability::Available { .. })
            })
            .map(|device| device.id.clone())
            .collect();
        if eligible.is_empty() {
            return false;
        }
        let pending: BTreeSet<_> = eligible.difference(&self.consumed).cloned().collect();
        if pending.is_empty() {
            self.legacy_tap = None;
        }
        self.migration_pending = Some(pending);
        true
    }
    fn restore_configuration(&self, id: &InputId) -> InputConfiguration {
        let mut configuration = self.overrides.get(id).cloned().unwrap_or_default();
        if self
            .migration_pending
            .as_ref()
            .is_some_and(|pending| pending.contains(id))
            && configuration.tap.is_none()
        {
            configuration.tap = self.legacy_tap;
        }
        configuration
    }
}

#[derive(Clone, Debug)]
pub struct InputApi {
    control: CompositorControl,
    store: Store,
    writes: Arc<tokio::sync::Mutex<()>>,
}
impl InputApi {
    pub fn new(store: Store) -> Result<Self> {
        let api = Self {
            control: CompositorControl::from_runtime()?,
            store,
            writes: Arc::new(tokio::sync::Mutex::new(())),
        };
        migrate_legacy(&api.store)?;
        Ok(api)
    }
    fn read_preferences(&self) -> Result<Option<Preferences>> {
        let json: Option<String> = self
            .store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key=?1",
                [KEY],
                |row| row.get(0),
            )
            .optional()?;
        let preferences: Option<Preferences> =
            json.map(|json| serde_json::from_str(&json)).transpose()?;
        if preferences
            .as_ref()
            .is_some_and(|preferences| preferences.version != 1)
        {
            bail!("unsupported durable input preference version");
        }
        Ok(preferences)
    }
    fn preferences(&self) -> Result<Preferences> {
        self.read_preferences()?
            .context("durable input preference snapshot disappeared")
    }
    fn save_preferences(&self, preferences: &Preferences) -> Result<()> {
        let json = serde_json::to_string(preferences)?;
        self.store.transaction(|tx| {
            tx.execute("INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,updated_ms=excluded.updated_ms", rusqlite::params![KEY, json, templar_state::now_ms()])?;
            Ok(())
        })
    }
    async fn request(&self, request: ControlRequest) -> Result<InputObservation> {
        match self.control.request(request).await? {
            ControlResponse::Inputs(observation) => Ok(observation),
            _ => bail!("the compositor returned a different observation for an input operation"),
        }
    }
    async fn actual(&self) -> Result<InputObservation> {
        self.request(ControlRequest::Inputs).await
    }

    async fn restore(&self, candidates: &BTreeSet<InputId>) -> Result<()> {
        let _write = self.writes.lock().await;
        let mut observation = self.actual().await?;
        if !observation.session_active || observation.locked {
            return Ok(());
        }
        let mut preferences = self.preferences()?;
        if preferences.capture_eligible(&observation) {
            self.save_preferences(&preferences)?;
        }
        for id in candidates {
            let Some(device) = observation.devices.iter().find(|device| &device.id == id) else {
                continue;
            };
            if !matches!(device.configuration, Capability::Available { .. })
                || !matches!(device.persistence, Capability::Available { .. })
            {
                continue;
            }
            let migrating = preferences
                .migration_pending
                .as_ref()
                .is_some_and(|pending| pending.contains(id));
            let configuration = preferences.restore_configuration(id);
            if configuration == InputConfiguration::default() {
                continue;
            }
            input::validate_configuration(device, &configuration)?;
            if !input::configuration_matches(&configuration, &device.effective) {
                observation = self
                    .request(ControlRequest::ConfigureInput {
                        expected: observation.revision,
                        id: id.clone(),
                        configuration: configuration.clone(),
                    })
                    .await?;
                let actual = observation
                    .devices
                    .iter()
                    .find(|device| &device.id == id)
                    .context("configured device disappeared from native readback")?;
                if !input::configuration_matches(&configuration, &actual.effective) {
                    bail!("native input preference readback differs from the requested properties");
                }
            }
            if migrating {
                input::merge_configuration(
                    preferences.overrides.entry(id.clone()).or_default(),
                    &configuration,
                );
                preferences.consume(id);
                self.save_preferences(&preferences).context(
                    "native tap choice is effective, but durable migration commit failed",
                )?;
            }
        }
        Ok(())
    }
}

/// Snapshot and retire the old typed setting in one SQLite transaction. A failed snapshot
/// leaves the legacy value untouched; an existing consumed migration can never be resurrected.
fn migrate_legacy(store: &Store) -> Result<()> {
    store.transaction(|tx| {
        let current: Option<String> = tx
            .query_row(
                "SELECT value_json FROM settings_values WHERE key=?1",
                [KEY],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(json) = current {
            let preferences: Preferences = serde_json::from_str(&json)?;
            if preferences.version != 1 {
                bail!("unsupported durable input preference version");
            }
        } else {
            let legacy: Option<String> = tx
                .query_row(
                    "SELECT value_json FROM settings_values WHERE key=?1",
                    [LEGACY_TAP_KEY],
                    |row| row.get(0),
                )
                .optional()?;
            let tap = match legacy {
                None => true,
                Some(json) => match serde_json::from_str::<SettingValue>(&json)? {
                    SettingValue::Bool(value) => value,
                    _ => bail!("legacy touchpad tap preference is not a boolean"),
                },
            };
            let preferences = Preferences {
                version: 1,
                overrides: BTreeMap::new(),
                legacy_tap: Some(tap),
                migration_pending: None,
                consumed: BTreeSet::new(),
            };
            tx.execute(
                "INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,?3)",
                rusqlite::params![
                    KEY,
                    serde_json::to_string(&preferences)?,
                    templar_state::now_ms()
                ],
            )?;
        }
        tx.execute("DELETE FROM settings_values WHERE key=?1", [LEGACY_TAP_KEY])?;
        Ok(())
    })
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> fdo::Result<T> {
    if json.len() > MAX_CONTROL_FRAME {
        return Err(fdo::Error::InvalidArgs(
            "input argument exceeds the bounded compositor frame".into(),
        ));
    }
    serde_json::from_str(json).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
}
#[zbus::interface(name = "org.templaros.Input1")]
impl InputApi {
    async fn observe(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.actual().await.map_err(failed)?).map_err(failed)
    }
    async fn configure(
        &self,
        expected: &str,
        id: &str,
        configuration: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let expected: Revision = decode(expected)?;
        let configuration: InputConfiguration = decode(configuration)?;
        if id.len() > MAX_CONTROL_FRAME {
            return Err(fdo::Error::InvalidArgs(
                "input identity is too large".into(),
            ));
        }
        let id = InputId(id.into());
        let _write = self.writes.lock().await;
        let mut preferences = self.preferences().map_err(failed)?;
        let observation = self
            .request(ControlRequest::ConfigureInput {
                expected,
                id: id.clone(),
                configuration: configuration.clone(),
            })
            .await
            .map_err(failed)?;
        let device = observation
            .devices
            .iter()
            .find(|device| device.id == id)
            .ok_or_else(|| failed("configured device disappeared from native readback"))?;
        if !input::configuration_matches(&configuration, &device.effective) {
            return Err(failed("native input configuration readback does not match"));
        }
        if matches!(device.persistence, Capability::Available { .. }) {
            input::merge_configuration(
                preferences.overrides.entry(id.clone()).or_default(),
                &configuration,
            );
            if device.touchpad && configuration.tap.is_some() {
                preferences.consume(&id);
            }
            self.save_preferences(&preferences).map_err(|error| {
                failed(format!(
                    "native input changed, but durable preference commit failed: {error}"
                ))
            })?;
        }
        let _ = Self::changed(&emitter).await;
        serde_json::to_string(&observation).map_err(failed)
    }
    async fn reset(
        &self,
        expected: &str,
        id: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let expected: Revision = decode(expected)?;
        if id.len() > MAX_CONTROL_FRAME {
            return Err(fdo::Error::InvalidArgs(
                "input identity is too large".into(),
            ));
        }
        let id = InputId(id.into());
        let _write = self.writes.lock().await;
        let mut preferences = self.preferences().map_err(failed)?;
        let observation = self
            .request(ControlRequest::ResetInput {
                expected,
                id: id.clone(),
            })
            .await
            .map_err(failed)?;
        let device = observation
            .devices
            .iter()
            .find(|device| device.id == id)
            .ok_or_else(|| failed("reset device disappeared from native readback"))?;
        if !input::configuration_matches(&device.defaults, &device.effective) {
            return Err(failed("libinput defaults were not restored exactly"));
        }
        preferences.overrides.remove(&id);
        if matches!(device.persistence, Capability::Available { .. }) {
            preferences.consume(&id);
            self.save_preferences(&preferences).map_err(|error| {
                failed(format!(
                    "native defaults restored, but durable override removal failed: {error}"
                ))
            })?;
        }
        let _ = Self::changed(&emitter).await;
        serde_json::to_string(&observation).map_err(failed)
    }
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

pub fn spawn_watch(connection: zbus::Connection) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = watch_once(&connection).await {
                tracing::warn!(%error, "native input observation disconnected");
                if let Ok(interface) = connection
                    .object_server()
                    .interface::<_, InputApi>(PATH)
                    .await
                {
                    let _ = InputApi::changed(interface.signal_emitter()).await;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
async fn watch_once(connection: &zbus::Connection) -> Result<()> {
    let interface = connection
        .object_server()
        .interface::<_, InputApi>(PATH)
        .await?;
    let api = interface.get().await.clone();
    let mut stream = api.control.subscribe_activity().await?;
    let mut previous = None;
    let mut known = BTreeSet::new();
    while let Some(activity) = stream.next().await {
        let activity = activity?;
        let state = (
            activity.input_revision,
            activity.session_active,
            activity.locked,
        );
        if previous == Some(state) {
            continue;
        }
        let observation = api.actual().await?;
        let current: BTreeSet<_> = observation
            .devices
            .iter()
            .map(|device| device.id.clone())
            .collect();
        let recover = previous.is_none_or(|(revision, active, locked): (Revision, bool, bool)| {
            revision.owner_epoch != activity.input_revision.owner_epoch
                || !active && activity.session_active
                || locked && !activity.locked
        });
        let candidates = if recover {
            current.clone()
        } else {
            current.difference(&known).cloned().collect()
        };
        // Update the source epoch before applying preferences: our own setter revision is not a
        // hardware recovery and must not create an unbounded retry loop after a real failure.
        known = current;
        previous = Some(state);
        if !candidates.is_empty()
            && let Err(error) = api.restore(&candidates).await
        {
            tracing::warn!(%error, "native input preference restoration failed");
        }
        let _ = InputApi::changed(interface.signal_emitter()).await;
    }
    bail!("native compositor activity stream ended")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reset_consumes_pending_migration_without_inheriting_into_later_devices() {
        let id = InputId("stable-touchpad".into());
        let mut preferences = Preferences {
            version: 1,
            overrides: BTreeMap::new(),
            legacy_tap: Some(true),
            migration_pending: Some(BTreeSet::from([id.clone()])),
            consumed: BTreeSet::new(),
        };
        preferences.consume(&id);
        assert!(preferences.consumed.contains(&id));
        assert_eq!(preferences.legacy_tap, None);
        assert!(preferences.migration_pending.unwrap().is_empty());
    }
    #[test]
    fn explicit_tap_override_wins_over_legacy_choice() {
        let id = InputId("stable-touchpad".into());
        let preferences = Preferences {
            version: 1,
            overrides: BTreeMap::from([(
                id.clone(),
                InputConfiguration {
                    tap: Some(false),
                    ..Default::default()
                },
            )]),
            legacy_tap: Some(true),
            migration_pending: Some(BTreeSet::from([id.clone()])),
            consumed: BTreeSet::new(),
        };
        assert_eq!(preferences.restore_configuration(&id).tap, Some(false));
    }
    #[test]
    fn legacy_snapshot_and_retirement_are_one_real_sqlite_transaction() {
        let store = Store::in_memory().unwrap();
        store.lock().execute("INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,0) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json", rusqlite::params![LEGACY_TAP_KEY, serde_json::to_string(&SettingValue::Bool(false)).unwrap()]).unwrap();
        migrate_legacy(&store).unwrap();
        let json: String = store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key=?1",
                [KEY],
                |row| row.get(0),
            )
            .unwrap();
        let preferences: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(preferences.legacy_tap, Some(false));
        assert_eq!(
            store
                .lock()
                .query_row(
                    "SELECT count(*) FROM settings_values WHERE key=?1",
                    [LEGACY_TAP_KEY],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        migrate_legacy(&store).unwrap();
        assert_eq!(
            store
                .lock()
                .query_row(
                    "SELECT value_json FROM settings_values WHERE key=?1",
                    [KEY],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            json
        );
    }
    #[test]
    fn malformed_snapshot_never_deletes_the_old_setting() {
        let store = Store::in_memory().unwrap();
        store.lock().execute("INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,0) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json", rusqlite::params![LEGACY_TAP_KEY, serde_json::to_string(&SettingValue::Bool(true)).unwrap()]).unwrap();
        store
            .lock()
            .execute(
                "INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,0)",
                rusqlite::params![KEY, "{\"version\":2}"],
            )
            .unwrap();
        assert!(migrate_legacy(&store).is_err());
        assert_eq!(
            store
                .lock()
                .query_row(
                    "SELECT count(*) FROM settings_values WHERE key=?1",
                    [LEGACY_TAP_KEY],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }
    fn observed_device(id: &str, touchpad: bool, tap_supported: bool) -> input::InputDevice {
        let unavailable = Capability::Unsupported {
            reason: "Not advertised in this pure inventory fixture".into(),
        };
        let available = Capability::Available {
            requires_restart: false,
        };
        input::InputDevice {
            id: InputId(id.into()),
            name: id.into(),
            kernel_name: id.into(),
            touchpad,
            persistence: available.clone(),
            default_restore: available.clone(),
            configuration: available.clone(),
            capabilities: input::InputCapabilities {
                speed: unavailable.clone(),
                acceleration_profiles: Vec::new(),
                natural_scroll: unavailable.clone(),
                left_handed: unavailable.clone(),
                tap: if tap_supported {
                    available
                } else {
                    unavailable.clone()
                },
                tap_drag: unavailable.clone(),
                tap_drag_lock: unavailable.clone(),
                click_methods: Vec::new(),
                scroll_methods: Vec::new(),
                scroll_button: unavailable.clone(),
                disable_while_typing: unavailable,
            },
            defaults: InputConfiguration::default(),
            effective: InputConfiguration::default(),
        }
    }
    #[test]
    fn only_first_nonempty_available_touchpad_roster_freezes_cutover() {
        let mut preferences = Preferences {
            version: 1,
            overrides: BTreeMap::new(),
            legacy_tap: Some(true),
            migration_pending: None,
            consumed: BTreeSet::new(),
        };
        let mut observation = InputObservation {
            revision: Revision::new_owner(),
            devices: vec![
                observed_device("keyboard", false, false),
                observed_device("unsupported-pad", true, false),
            ],
            session_active: true,
            locked: false,
        };
        assert!(!preferences.capture_eligible(&observation));
        assert!(preferences.migration_pending.is_none());
        let reset = InputId("reset-pad".into());
        preferences.consume(&reset);
        observation.devices.extend([
            observed_device("first-pad", true, true),
            observed_device("reset-pad", true, true),
        ]);
        observation.locked = true;
        assert!(!preferences.capture_eligible(&observation));
        observation.locked = false;
        assert!(preferences.capture_eligible(&observation));
        assert_eq!(
            preferences.migration_pending.as_ref().unwrap(),
            &BTreeSet::from([InputId("first-pad".into())])
        );
        observation
            .devices
            .push(observed_device("later-pad", true, true));
        assert!(!preferences.capture_eligible(&observation));
        assert_eq!(
            preferences
                .restore_configuration(&InputId("later-pad".into()))
                .tap,
            None
        );
        assert_eq!(preferences.restore_configuration(&reset).tap, None);
    }
}
