//! `org.templaros.Background1`: background-activity permissions as a first-class service surface
//! (`@templaros.constraint.services-not-indicators`).
//!
//! Answers "what does this application want, did we say yes, and — if it is running right now —
//! why" as one call, over the declarations the autostart runner persists each scan
//! (`crate::background::autostart`, `templar_session_core::autostart::background_declarations`)
//! and the same `autostart_approvals` the planner already consults, so `Grant`/`Revoke` change
//! exactly what governs autostart — one source of truth, not a parallel one a client could
//! disagree with.

use crate::apis::failed;
use parking_lot::RwLock;
use rusqlite::params;
use std::sync::Arc;
use templar_appd::Registry;
use templar_session_core::autostart::{self, Approval};
use templar_state::Store;
use templar_types::AppId;
use zbus::{fdo, object_server::SignalEmitter};

/// Not in `templar_types::dbus::session` (outside this track's file ownership); documented
/// alongside the interface in `interfaces/dbus/org.templaros.Background1.xml`.
pub const PATH: &str = "/org/templaros/Session/Background";

#[derive(Debug)]
pub struct BackgroundApi {
    store: Store,
    registry: Arc<Registry>,
    providers: Arc<RwLock<templar_session_core::providers::ProviderRegistry>>,
}

impl BackgroundApi {
    pub fn new(
        store: Store,
        registry: Arc<Registry>,
        providers: Arc<RwLock<templar_session_core::providers::ProviderRegistry>>,
    ) -> Self {
        Self {
            store,
            registry,
            providers,
        }
    }

    /// Whether a history `AutostartDecision` event named this instance — the durable record
    /// `crate::background::autostart::run` leaves behind, and the only honest way to answer
    /// "did this one start from autostart" without inventing a second, redundant ledger.
    fn instance_has_autostart_decision(
        &self,
        instance: templar_types::InstanceId,
    ) -> rusqlite::Result<bool> {
        let count: i64 = self.store.lock().query_row(
            "SELECT COUNT(*) FROM history WHERE kind = 'autostart-decision' AND instance_id = ?1",
            params![instance.to_string()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn reason_for(
        &self,
        entry: &autostart::AutostartEntry,
        app_id: &AppId,
    ) -> fdo::Result<Option<String>> {
        let Some(instance) = self
            .registry
            .list_instances()
            .into_iter()
            .find(|record| &record.app_id == app_id)
        else {
            return Ok(None);
        };
        let started_via = if self
            .instance_has_autostart_decision(instance.id)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?
        {
            "autostart"
        } else {
            "launch"
        };
        let (role, role_selected) = match entry.role {
            Some(role) => (
                Some(role.as_str()),
                self.providers.read().provider(role).as_ref() == Some(app_id),
            ),
            None => (None, false),
        };
        let reason = serde_json::json!({
            "instance_id": instance.id.to_string(),
            "started_via": started_via,
            "role": role,
            "role_selected": role_selected,
        });
        Ok(Some(reason.to_string()))
    }
}

fn parse_app_id(app_id: &str) -> fdo::Result<AppId> {
    app_id
        .parse()
        .map_err(|error: templar_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))
}

#[zbus::interface(name = "org.templaros.Background1")]
impl BackgroundApi {
    /// `(app_id, permissions_json, approval, running, reason_json)`
    async fn list(&self) -> fdo::Result<Vec<(String, String, String, bool, String)>> {
        let declarations = autostart::load_declarations(&self.store).map_err(failed)?;
        let approvals = autostart::load_approvals(&self.store).map_err(failed)?;

        let mut entries = Vec::with_capacity(declarations.len());
        for declaration in declarations {
            let approval = autostart::approval_for(&approvals, &declaration.app_id);
            let permissions_json =
                serde_json::to_string(&declaration.requested).unwrap_or_default();
            let reason = self.reason_for(&declaration, &declaration.app_id)?;
            entries.push((
                declaration.app_id.to_string(),
                permissions_json,
                approval.as_str().to_owned(),
                reason.is_some(),
                reason.unwrap_or_default(),
            ));
        }
        Ok(entries)
    }

    async fn grant(
        &self,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        self.store
            .transaction(|tx| {
                autostart::apply_approval_within(tx, &app_id, Approval::Approved, "user")
            })
            .map_err(failed)?;
        let _ = emitter.changed(app_id.to_string()).await;
        Ok(())
    }

    async fn revoke(
        &self,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        self.store
            .transaction(|tx| {
                autostart::apply_approval_within(tx, &app_id, Approval::Denied, "user")
            })
            .map_err(failed)?;
        let _ = emitter.changed(app_id.to_string()).await;
        Ok(())
    }

    /// An application's declaration, approval or running reason may have changed; re-call `List`.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, app_id: String) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use templar_appd::identity::InstanceRecord;
    use templar_session_core::{
        history::{Event, EventId, EventKind, append_within},
        providers::ProviderRegistry,
    };
    use templar_types::{InstanceId, ProviderCapability};

    fn api(
        store: &Store,
        registry: &Arc<Registry>,
        providers: &Arc<RwLock<ProviderRegistry>>,
    ) -> BackgroundApi {
        BackgroundApi::new(store.clone(), registry.clone(), providers.clone())
    }

    #[tokio::test]
    async fn a_declared_but_undecided_app_is_listed_as_not_running() {
        let store = Store::in_memory().unwrap();
        let registry = Arc::new(Registry::default());
        let providers = Arc::new(RwLock::new(ProviderRegistry::default()));
        let sync: AppId = "org.example.Sync".parse().unwrap();
        let entry = autostart::AutostartEntry {
            app_id: sync.clone(),
            role: None,
            requested: vec![autostart::BackgroundPermission::ContinuousBackground],
        };
        store
            .transaction(|tx| autostart::save_declaration_within(tx, &entry))
            .unwrap();

        let entries = api(&store, &registry, &providers).list().await.unwrap();
        assert_eq!(entries.len(), 1);
        let (app_id, permissions_json, approval, running, reason_json) = &entries[0];
        assert_eq!(app_id, sync.as_str());
        assert!(permissions_json.contains("continuous-background"));
        assert_eq!(approval, "undecided");
        assert!(!running);
        assert_eq!(reason_json, "");
    }

    #[tokio::test]
    async fn a_running_instance_reports_why_including_role_selection() {
        let store = Store::in_memory().unwrap();
        let registry = Arc::new(Registry::default());
        let providers = Arc::new(RwLock::new(ProviderRegistry::default()));
        let tray: AppId = "org.templaros.NetworkTray".parse().unwrap();

        let entry = autostart::AutostartEntry {
            app_id: tray.clone(),
            role: Some(templar_types::ProviderRole::NetworkStatusUi),
            requested: vec![autostart::BackgroundPermission::LoginAutostart],
        };
        store
            .transaction(|tx| {
                autostart::save_declaration_within(tx, &entry)?;
                autostart::apply_approval_within(tx, &tray, Approval::Approved, "test")?;
                Ok(())
            })
            .unwrap();

        providers.write().declare(
            tray.clone(),
            ProviderCapability {
                role: templar_types::ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );
        providers
            .write()
            .select(templar_types::ProviderRole::NetworkStatusUi, tray.clone());

        let instance_id = InstanceId::new();
        registry.register_instance(InstanceRecord {
            id: instance_id,
            app_id: tray.clone(),
            systemd_unit: "app-templaros-org.templaros.NetworkTray@x.service".into(),
            control_group: PathBuf::from("/user.slice/app.slice/x"),
        });
        // The autostart runner's own record of having started this instance.
        store
            .transaction(|tx| {
                append_within(
                    tx,
                    &Event {
                        id: EventId::new(),
                        at_ms: templar_state::now_ms(),
                        kind: EventKind::AutostartDecision,
                        app_id: Some(tray.clone()),
                        instance_id: Some(instance_id),
                        message: "started at login".into(),
                        cause: None,
                        actions: Vec::new(),
                        reversible: false,
                    },
                )
            })
            .unwrap();

        let entries = api(&store, &registry, &providers).list().await.unwrap();
        assert_eq!(entries.len(), 1);
        let (app_id, _permissions_json, approval, running, reason_json) = &entries[0];
        assert_eq!(app_id, tray.as_str());
        assert_eq!(approval, "approved");
        assert!(running);
        let reason: serde_json::Value = serde_json::from_str(reason_json).unwrap();
        assert_eq!(reason["instance_id"], instance_id.to_string());
        assert_eq!(reason["started_via"], "autostart");
        assert_eq!(reason["role"], "network-status-ui");
        assert_eq!(reason["role_selected"], true);
    }

    #[tokio::test]
    async fn a_running_instance_with_no_autostart_record_reports_launch() {
        let store = Store::in_memory().unwrap();
        let registry = Arc::new(Registry::default());
        let providers = Arc::new(RwLock::new(ProviderRegistry::default()));
        let viewer: AppId = "org.templaros.Viewer".parse().unwrap();

        let entry = autostart::AutostartEntry {
            app_id: viewer.clone(),
            role: None,
            requested: vec![autostart::BackgroundPermission::Notifications],
        };
        store
            .transaction(|tx| autostart::save_declaration_within(tx, &entry))
            .unwrap();

        registry.register_instance(InstanceRecord {
            id: InstanceId::new(),
            app_id: viewer.clone(),
            systemd_unit: "app-templaros-org.templaros.Viewer@x.service".into(),
            control_group: PathBuf::from("/user.slice/app.slice/x"),
        });

        let entries = api(&store, &registry, &providers).list().await.unwrap();
        let (_, _, _, running, reason_json) = &entries[0];
        assert!(running);
        let reason: serde_json::Value = serde_json::from_str(reason_json).unwrap();
        assert_eq!(reason["started_via"], "launch");
        assert_eq!(reason["role"], serde_json::Value::Null);
        assert_eq!(reason["role_selected"], false);
    }

    #[test]
    fn parse_app_id_rejects_garbage() {
        assert!(parse_app_id("not-an-app-id").is_err());
        assert!(parse_app_id("org.templaros.Viewer").is_ok());
    }
}
