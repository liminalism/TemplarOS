//! `org.legeos.Power1`, over `lege_session_core::power`.
//!
//! Overrides are stored in the generic `settings_values` table under an internal key
//! (`_internal.power.profile.<source>`) rather than the typed [`SettingsSchema`] — a whole
//! [`PowerProfile`] is a structured document, not one scalar the settings UI edits directly.

use crate::apis::failed;
use lege_session_core::power::{PowerProfile, PowerSource};
use lege_state::Store;
use rusqlite::OptionalExtension;
use zbus::fdo;

const OVERRIDE_KEY_PREFIX: &str = "_internal.power.profile.";

#[derive(Debug)]
pub struct PowerApi {
    store: Store,
}

impl PowerApi {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

fn parse_source(source: &str) -> fdo::Result<PowerSource> {
    match source {
        "battery" => Ok(PowerSource::Battery),
        "mains" => Ok(PowerSource::Mains),
        other => Err(fdo::Error::InvalidArgs(format!(
            "unknown power source {other:?} (expected battery or mains)"
        ))),
    }
}

#[zbus::interface(name = "org.legeos.Power1")]
impl PowerApi {
    async fn get_profile(&self, source: &str) -> fdo::Result<String> {
        let source_enum = parse_source(source)?;
        let key = format!("{OVERRIDE_KEY_PREFIX}{source}");
        let stored: Option<String> = self
            .store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key = ?1",
                [&key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        match stored {
            Some(json) => Ok(json),
            None => serde_json::to_string(&PowerProfile::default_for(source_enum))
                .map_err(|error| fdo::Error::Failed(error.to_string())),
        }
    }

    async fn set_profile(&self, source: &str, profile_json: &str) -> fdo::Result<()> {
        parse_source(source)?;
        let _profile: PowerProfile = serde_json::from_str(profile_json)
            .map_err(|error| fdo::Error::InvalidArgs(format!("invalid profile JSON: {error}")))?;
        let key = format!("{OVERRIDE_KEY_PREFIX}{source}");
        self.store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms",
                    rusqlite::params![key, profile_json, lege_state::now_ms()],
                )?;
                Ok(())
            })
            .map_err(failed)
    }

    async fn reset_profile(&self, source: &str) -> fdo::Result<()> {
        parse_source(source)?;
        let key = format!("{OVERRIDE_KEY_PREFIX}{source}");
        self.store
            .transaction(|tx| {
                tx.execute("DELETE FROM settings_values WHERE key = ?1", [&key])?;
                Ok(())
            })
            .map_err(failed)
    }
}
