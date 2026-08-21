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
        // Not modelled yet: an icon path/theme name, urgency/category hints, and an explicit
        // expiry are all legitimate future additions once the shell renders notifications.
        let (_, _, _) = (app_icon, hints, expire_timeout);
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
            cause: Some("urgency:normal".into()),
            actions: action_pairs
                .into_iter()
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
        self.store
            .transaction(|tx| -> anyhow::Result<u32> {
                if replaces_id != 0 {
                    tx.execute(
                        "INSERT INTO notifications_active (id, event_id, app_name, app_id, created_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT (id) DO UPDATE SET
                            event_id = excluded.event_id, app_name = excluded.app_name,
                            app_id = excluded.app_id, created_ms = excluded.created_ms",
                        params![replaces_id, event_id.to_string(), app_name, app_id_str, lege_state::now_ms()],
                    )?;
                    Ok(replaces_id)
                } else {
                    tx.execute(
                        "INSERT INTO notifications_active (event_id, app_name, app_id, created_ms)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![event_id.to_string(), app_name, app_id_str, lege_state::now_ms()],
                    )?;
                    Ok(u32::try_from(tx.last_insert_rowid()).unwrap_or(u32::MAX))
                }
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))
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
