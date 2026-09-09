//! `org.templaros.Power1`: get, set and reset the stored [`PowerProfile`] for a power source.
//!
//! **That is the whole of what happens here.** Nothing in this module talks to logind or upower
//! and nothing in it watches for idleness: the three methods read and write a JSON document in
//! the state store, and that is all they do.
//!
//! One field of that document is nonetheless enforced, elsewhere. `screen_off_after` is read by
//! `templar-shell` and relayed to `templar-comp` over `templar_shell_v1.set_idle_timeout`, and the
//! compositor turns the outputs off after that much idle time and back on at the next input
//! (`compositor/templar-comp/src/idle.rs`). It had to be the compositor: it is the only process in
//! the session that sees the seat, so it is the only one that can tell idleness from silence.
//! Writing a new value here therefore does change what the machine does — for that one field.
//!
//! What is stored is a real and useful thing — one field of it drives the screen today, and the
//! whole of it is what a settings UI edits — but the difference between "recorded" and "enforced"
//! still matters field by field, and matters most to anyone deciding whether this machine will
//! lock itself. It will not.
//!
//! The rest is deliberate remaining scope, not an oversight. `dim_after` needs a backlight ramp
//! through `Backlight1`; `lock_after` needs a lock surface to drive, and
//! `Session.Lifecycle1.Lock` is itself still a `NotSupported` stub for want of a Wayland
//! lock-manager client; `suspend_after` and the lid actions need logind. Arming those timers
//! before the things they drive exist would produce timers that fire into nothing. Battery state
//! is `org.templaros.Battery1` and brightness is `org.templaros.Backlight1`. Neither belongs here:
//! those are hardware readings and this is stored policy.
//!
//! Overrides are stored in the generic `settings_values` table under an internal key
//! (`_internal.power.profile.<source>`) rather than the typed [`SettingsSchema`] — a whole
//! [`PowerProfile`] is a structured document, not one scalar the settings UI edits directly.

use crate::apis::failed;
use rusqlite::OptionalExtension;
use templar_session_core::power::{PowerProfile, PowerSource};
use templar_state::Store;
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

#[zbus::interface(name = "org.templaros.Power1")]
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
    /// Stores it, and only stores it: no timer is armed by this call. The shell re-reads
    /// `screen_off_after` and relays it to the compositor, which is what makes that one field
    /// take effect (module documentation); every other field is still a value kept and not a
    /// thing scheduled, so anything relying on `suspend_after` to suspend the machine is relying
    /// on something that does not exist yet.
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
                    rusqlite::params![key, profile_json, templar_state::now_ms()],
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
