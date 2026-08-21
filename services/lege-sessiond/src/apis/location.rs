//! `org.legeos.Location1`, over `lege_session_core::location`.
//!
//! Timezone application goes through `org.freedesktop.timedate1` on the **system** bus
//! (polkit-gated `SetTimezone`), never by writing `/etc/localtime` directly (ADR-0003) — hence
//! the separate system-bus [`zbus::Connection`] this API holds alongside the session-scoped
//! `lege-state` store. That connection is best-effort and lazily retried: a system bus that is
//! briefly unreachable at session startup (boot ordering, a container without one, …) must not
//! take the rest of `org.legeos.Session` down with it — only `Decide`'s actual timezone
//! application degrades, logging a warning, until the system bus answers again.

use crate::apis::failed;
use lege_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    location::{TimezoneCandidate, TimezoneDecision, TimezonePolicy},
};
use lege_state::Store;
use rusqlite::OptionalExtension;
use std::sync::Arc;
use zbus::fdo;
use zbus_systemd::timedate1::TimedatedProxy;

const POLICY_KEY: &str = "_internal.location.policy";

#[derive(Debug)]
pub struct LocationApi {
    store: Store,
    /// `Some` once a system-bus connection has succeeded at least once; zbus connections are
    /// cheap to clone and stay usable across a service restart on the far end, so a success is
    /// cached rather than reconnected every call.
    system_connection: parking_lot::Mutex<Option<zbus::Connection>>,
    history: Arc<SqliteHistory>,
}

impl LocationApi {
    pub fn new(
        store: Store,
        system_connection: Option<zbus::Connection>,
        history: Arc<SqliteHistory>,
    ) -> Self {
        Self {
            store,
            system_connection: parking_lot::Mutex::new(system_connection),
            history,
        }
    }

    /// The system-bus connection, connecting (or reconnecting) lazily on demand.
    async fn system_connection(&self) -> Option<zbus::Connection> {
        if let Some(connection) = self.system_connection.lock().clone() {
            return Some(connection);
        }
        match zbus::connection::Builder::system() {
            Ok(builder) => match builder.build().await {
                Ok(connection) => {
                    *self.system_connection.lock() = Some(connection.clone());
                    Some(connection)
                }
                Err(error) => {
                    tracing::warn!(%error, "system bus still unreachable; timezone will not be applied this time");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "failed to prepare a system bus connection");
                None
            }
        }
    }

    fn load_policy(&self) -> anyhow::Result<TimezonePolicy> {
        let stored: Option<String> = self
            .store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key = ?1",
                [POLICY_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match stored {
            Some(json) => serde_json::from_str(&json)?,
            None => TimezonePolicy::new(),
        })
    }

    fn save_policy(&self, policy: &TimezonePolicy) -> anyhow::Result<()> {
        let json = serde_json::to_string(policy)?;
        self.store.transaction(|tx| {
            tx.execute(
                "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms",
                rusqlite::params![POLICY_KEY, json, lege_state::now_ms()],
            )?;
            Ok(())
        })
    }
}

#[zbus::interface(name = "org.legeos.Location1")]
impl LocationApi {
    async fn decide(&self, location_key: &str, candidates_json: &str) -> fdo::Result<String> {
        let candidates: Vec<TimezoneCandidate> =
            serde_json::from_str(candidates_json).map_err(|error| {
                fdo::Error::InvalidArgs(format!("invalid candidates_json: {error}"))
            })?;
        let policy = self.load_policy().map_err(failed)?;
        let key = (!location_key.is_empty()).then_some(location_key);
        let decision = policy.decide(key, &candidates);

        if let TimezoneDecision::Apply {
            timezone,
            confidence,
        } = &decision
            && let Some(connection) = self.system_connection().await
        {
            match TimedatedProxy::new(&connection).await {
                Ok(proxy) => match proxy.set_timezone(timezone.clone(), true).await {
                    Ok(()) => {
                        let message = format!(
                            "Timezone set to {timezone} ({:.0}% provider agreement).",
                            confidence * 100.0
                        );
                        if let Err(error) = self.history.append(Event {
                            id: EventId::new(),
                            at_ms: lege_state::now_ms(),
                            kind: EventKind::SettingChange,
                            app_id: None,
                            instance_id: None,
                            message,
                            cause: Some("location".into()),
                            actions: Vec::new(),
                            reversible: false,
                        }) {
                            tracing::warn!(%error, "failed to record timezone-change history event");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, timezone = %timezone, "org.freedesktop.timedate1 SetTimezone failed");
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "could not reach org.freedesktop.timedate1 on the system bus");
                }
            }
        }

        serde_json::to_string(&decision).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn set_manual_override(&self, location_key: &str, timezone: &str) -> fdo::Result<()> {
        if location_key.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "location_key must not be empty".into(),
            ));
        }
        let mut policy = self.load_policy().map_err(failed)?;
        policy
            .manual_overrides
            .insert(location_key.to_owned(), timezone.to_owned());
        self.save_policy(&policy).map_err(failed)
    }

    async fn clear_manual_override(&self, location_key: &str) -> fdo::Result<()> {
        let mut policy = self.load_policy().map_err(failed)?;
        policy.manual_overrides.remove(location_key);
        self.save_policy(&policy).map_err(failed)
    }

    async fn manual_overrides(&self) -> fdo::Result<Vec<(String, String)>> {
        Ok(self
            .load_policy()
            .map_err(failed)?
            .manual_overrides
            .into_iter()
            .collect())
    }
}
