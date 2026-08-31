//! `org.legeos.Power1`: get, set and reset the stored [`PowerProfile`] for a power source.
//!
//! **That is the whole of it.** This module's documentation used to claim "idle/lid/brightness
//! policy over logind/upower", which described the intended shape of the feature rather than the
//! code underneath it: nothing here talks to logind or upower, nothing watches for idleness, and
//! setting `suspend_after` has never suspended anything. The three methods read and write a JSON
//! document in the state store, and a reader who trusted the old summary would have gone looking
//! for an idle watcher that does not exist.
//!
//! What is stored is a real and useful thing — the profile a future idle watcher would enforce,
//! and the one a settings UI edits — so the interface is not vapour. It is simply a *record* of
//! policy and not an enforcer of it, and the difference matters to anyone deciding whether this
//! machine will lock itself.
//!
//! The unimplemented half is deliberate remaining scope, not an oversight: an idle watcher needs
//! an idle source (the compositor's, since logind's `IdleHint` is not driven by a Wayland session
//! on its own), a way to dim and blank through `Backlight1`, and a lock surface to drive — and
//! `Session.Lifecycle1.Lock` is itself still a `NotSupported` stub for want of a Wayland
//! lock-manager client. Building the watcher before those exist would produce a timer that fires
//! into nothing. Battery state, one of the things the old summary promised, does now exist and is
//! `org.legeos.Battery1`; brightness is `org.legeos.Backlight1`. Neither belongs here: those are
//! hardware readings and this is stored policy.
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

    /// Store a profile as this source's override.
    ///
    /// Stores it, and only stores it. No timer is armed by this call and no idle watcher reads
    /// the result: an enforcer is deliberate remaining scope (see the module documentation for
    /// what it needs first). Anything relying on `suspend_after` to suspend the machine is
    /// relying on something that does not exist yet.
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
