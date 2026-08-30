//! `org.freedesktop.Notifications`, hosted only while sessiond is the selected
//! `notification-server` provider (`crate::background::notification_provider`). Every
//! notification becomes a searchable history event (`docs/architecture/history.md`) so nothing
//! is lost if the shell restarts; the shell only renders.
//!
//! Attribution to a canonical [`AppId`] is a best-effort match of the caller-supplied
//! `app_name` against an installed manifest's display name or desktop-file ID — the spec does
//! not hand this interface the caller's PID/cgroup the way `AppIdentity1.ResolveWindow` gets
//! one, so it cannot be cgroup-verified the way window identity is. A future improvement would
//! read the D-Bus message header's sender and resolve its peer credentials.

use lege_appd::Registry;
use lege_session_core::history::{
    Event, EventAction, EventId, EventKind, HistoryStore, SqliteHistory,
};
use lege_state::Store;
use lege_types::{AppCatalog, AppId};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use zbus::{fdo, object_server::SignalEmitter, zvariant::OwnedValue};

#[derive(Debug)]
pub struct NotificationsApi {
    store: Store,
    history: Arc<SqliteHistory>,
    registry: Arc<Registry>,
}

impl NotificationsApi {
    pub fn new(store: Store, history: Arc<SqliteHistory>, registry: Arc<Registry>) -> Self {
        Self {
            store,
            history,
            registry,
        }
    }

    fn attribute(&self, app_name: &str) -> Option<AppId> {
        self.registry
            .manifests()
            .into_iter()
            .find(|manifest| {
                manifest.display_name == app_name
                    || manifest
                        .primary_desktop_file_id
                        .trim_end_matches(".desktop")
                        == app_name
            })
            .map(|manifest| manifest.id.clone())
    }

    /// Insert the notification, or replace the row `replaces_id` names. Returns the live ID.
    fn store_active(&self, row: &ActiveRow<'_>) -> anyhow::Result<u32> {
        let ActiveRow {
            replaces_id,
            event_id,
            app_name,
            app_id,
            created_ms,
            summary,
            body,
            icon,
            urgency,
            category,
            actions_json,
            expires_ms,
        } = *row;
        self.store.transaction(|tx| -> anyhow::Result<u32> {
            if replaces_id == 0 {
                tx.execute(
                    "INSERT INTO notifications_active
                        (event_id, app_name, app_id, created_ms, summary, body, icon, urgency,
                         category, actions_json, expires_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        event_id,
                        app_name,
                        app_id,
                        created_ms,
                        summary,
                        body,
                        icon,
                        urgency,
                        category,
                        actions_json,
                        expires_ms
                    ],
                )?;
                return Ok(u32::try_from(tx.last_insert_rowid()).unwrap_or(u32::MAX));
            }
            tx.execute(
                "INSERT INTO notifications_active
                    (id, event_id, app_name, app_id, created_ms, summary, body, icon,
                     urgency, category, actions_json, expires_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (id) DO UPDATE SET
                    event_id = excluded.event_id, app_name = excluded.app_name,
                    app_id = excluded.app_id, created_ms = excluded.created_ms,
                    summary = excluded.summary, body = excluded.body, icon = excluded.icon,
                    urgency = excluded.urgency, category = excluded.category,
                    actions_json = excluded.actions_json, expires_ms = excluded.expires_ms",
                params![
                    replaces_id,
                    event_id,
                    app_name,
                    app_id,
                    created_ms,
                    summary,
                    body,
                    icon,
                    urgency,
                    category,
                    actions_json,
                    expires_ms
                ],
            )?;
            Ok(replaces_id)
        })
    }

    /// Remove the notification once it expires. Guarded by `event_id` so replacing an ID cannot
    /// let the outgoing notification's timer remove the one that took its place.
    fn spawn_expiry(&self, id: u32, event_id: String, expires_ms: i64) {
        let store = self.store.clone();
        tokio::spawn(async move {
            let delay = expires_ms.saturating_sub(lege_state::now_ms()).max(0) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            let _ = store.transaction(|tx| {
                tx.execute(
                    "DELETE FROM notifications_active WHERE id = ?1 AND event_id = ?2",
                    params![id, event_id],
                )?;
                Ok(())
            });
        });
    }
}

/// The columns of one `notifications_active` row, gathered so the write is a single argument.
struct ActiveRow<'a> {
    replaces_id: u32,
    event_id: &'a str,
    app_name: &'a str,
    app_id: Option<&'a str>,
    created_ms: i64,
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
    urgency: u8,
    category: &'a str,
    actions_json: &'a str,
    expires_ms: Option<i64>,
}

/// Complete presentation data for one still-active notification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveNotification {
    pub id: u32,
    pub app_name: String,
    pub app_id: Option<String>,
    pub summary: String,
    pub body: String,
    pub icon: String,
    /// freedesktop urgency: 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    pub category: String,
    pub actions: Vec<(String, String)>,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
}

/// LegeOS-side read/control API.  The freedesktop interface intentionally has no enumeration
/// method; this companion keeps the shell a renderer while the notification server owns active
/// state, expiry and dismissal.
#[derive(Debug)]
pub struct NotificationsControlApi {
    store: Store,
}

impl NotificationsControlApi {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    fn prune_expired(&self) -> anyhow::Result<()> {
        self.store.transaction(|tx| {
            tx.execute(
                "DELETE FROM notifications_active WHERE expires_ms IS NOT NULL AND expires_ms <= ?1",
                [lege_state::now_ms()],
            )?;
            Ok(())
        })
    }
}

#[zbus::interface(name = "org.legeos.Notifications1")]
impl NotificationsControlApi {
    async fn list_active(&self) -> fdo::Result<Vec<String>> {
        self.prune_expired()
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let connection = self.store.lock();
        let mut statement = connection
            .prepare(
                "SELECT id, app_name, app_id, summary, body, icon, urgency, category,
                        actions_json, created_ms, expires_ms
                   FROM notifications_active ORDER BY created_ms DESC, id DESC",
            )
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let rows = statement
            .query_map([], |row| {
                let actions_json: String = row.get(8)?;
                Ok(ActiveNotification {
                    id: row.get(0)?,
                    app_name: row.get(1)?,
                    app_id: row.get(2)?,
                    summary: row.get(3)?,
                    body: row.get(4)?,
                    icon: row.get(5)?,
                    urgency: row.get(6)?,
                    category: row.get(7)?,
                    actions: serde_json::from_str(&actions_json).unwrap_or_default(),
                    created_ms: row.get(9)?,
                    expires_ms: row.get(10)?,
                })
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        rows.map(|row| {
            let notification = row.map_err(|error| fdo::Error::Failed(error.to_string()))?;
            serde_json::to_string(&notification)
                .map_err(|error| fdo::Error::Failed(error.to_string()))
        })
        .collect()
    }

    async fn dismiss(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let removed = self
            .store
            .transaction(|tx| {
                Ok(tx.execute("DELETE FROM notifications_active WHERE id = ?1", [id])? != 0)
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        if removed {
            let _ = emitter.changed().await;
        }
        Ok(removed)
    }

    async fn clear_all(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<u32> {
        let removed = self
            .store
            .transaction(|tx| {
                let count = tx.execute("DELETE FROM notifications_active", [])?;
                Ok(u32::try_from(count).unwrap_or(u32::MAX))
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        if removed != 0 {
            let _ = emitter.changed().await;
        }
        Ok(removed)
    }

    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsApi {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> fdo::Result<u32> {
        let urgency = hints
            .get("urgency")
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(1)
            .min(2);
        let category = hints
            .get("category")
            .and_then(|value| <&str>::try_from(value).ok())
            .map(str::to_owned)
            .unwrap_or_default();
        let app_id = self.attribute(app_name);
        let action_pairs: Vec<(String, String)> = actions
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        let message = if body.is_empty() {
            format!("{app_name}: {summary}")
        } else {
            format!("{app_name}: {summary} — {body}")
        };

        let event = Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind: EventKind::Notification,
            app_id: app_id.clone(),
            instance_id: None,
            message,
            cause: Some(format!("urgency:{urgency};category:{category}")),
            actions: action_pairs
                .iter()
                .cloned()
                .map(|(key, label)| EventAction {
                    label,
                    command: format!("notify:action:{key}"),
                })
                .collect(),
            reversible: false,
        };
        let event_id = event.id;
        self.history
            .append(event)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        let app_id_str = app_id.as_ref().map(AppId::as_str);
        let created_ms = lege_state::now_ms();
        // The specification uses 0 for never, a positive value in milliseconds, and -1 for the
        // server's default. Five seconds is deliberately conservative and deterministic.
        let expires_ms = match expire_timeout {
            0 => None,
            timeout if timeout > 0 => Some(created_ms.saturating_add(i64::from(timeout))),
            _ => Some(created_ms.saturating_add(5_000)),
        };
        let actions_json = serde_json::to_string(&action_pairs)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let event_id_str = event_id.to_string();
        let id = self
            .store_active(&ActiveRow {
                replaces_id,
                event_id: &event_id_str,
                app_name,
                app_id: app_id_str,
                created_ms,
                summary,
                body,
                icon: app_icon,
                urgency,
                category: &category,
                actions_json: &actions_json,
                expires_ms,
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        if let Some(expires_ms) = expires_ms {
            self.spawn_expiry(id, event_id_str, expires_ms);
        }
        Ok(id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        self.store
            .transaction(|tx| {
                tx.execute(
                    "DELETE FROM notifications_active WHERE id = ?1",
                    params![id],
                )?;
                Ok(())
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        // Reason 3: dismissed by CloseNotification, per the spec.
        let _ = emitter.notification_closed(id, 3).await;
        Ok(())
    }

    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into(), "persistence".into()]
    }

    /// `(name, vendor, version, spec_version)`
    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "lege-sessiond".into(),
            "LegeOS".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        signal_emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        signal_emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}
