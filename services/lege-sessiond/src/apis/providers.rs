//! `org.legeos.Providers1`.

use crate::apis::failed;
use lege_session_core::providers::{self, ProviderRegistry};
use lege_state::Store;
use lege_types::{AppId, ProviderCapability, ProviderRole};
use parking_lot::RwLock;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct ProvidersApi {
    registry: Arc<RwLock<ProviderRegistry>>,
    store: Store,
}

impl ProvidersApi {
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>, store: Store) -> Self {
        Self { registry, store }
    }
}

fn role_by_name(name: &str) -> fdo::Result<ProviderRole> {
    ProviderRole::ALL
        .iter()
        .copied()
        .find(|role| role.as_str() == name)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown provider role {name:?}")))
}

fn parse_app_id(app_id: &str) -> fdo::Result<AppId> {
    app_id
        .parse()
        .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))
}

#[zbus::interface(name = "org.legeos.Providers1")]
impl ProvidersApi {
    /// `(role, resolution-json)`
    async fn report(&self) -> Vec<(String, String)> {
        self.registry
            .read()
            .report()
            .into_iter()
            .map(|(role, resolution)| {
                (
                    role.to_string(),
                    serde_json::to_string(&resolution).unwrap_or_default(),
                )
            })
            .collect()
    }

    async fn select(
        &self,
        role: &str,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let role_enum = role_by_name(role)?;
        let app_id = parse_app_id(app_id)?;
        self.store
            .transaction(|tx| providers::select_within(tx, role_enum, &app_id))
            .map_err(failed)?;
        self.registry.write().select(role_enum, app_id);
        let _ = emitter.changed(role.to_owned()).await;
        Ok(())
    }

    async fn declare(
        &self,
        app_id: &str,
        role: &str,
        priority: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let role_enum = role_by_name(role)?;
        let app_id = parse_app_id(app_id)?;
        let capability = ProviderCapability {
            role: role_enum,
            priority,
        };
        self.store
            .transaction(|tx| providers::declare_within(tx, &app_id, &capability))
            .map_err(failed)?;
        self.registry.write().declare(app_id, capability);
        let _ = emitter.changed(role.to_owned()).await;
        Ok(())
    }

    async fn undeclare(
        &self,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let affected_roles: Vec<ProviderRole> = {
            self.registry
                .read()
                .declarations
                .iter()
                .filter(|declaration| declaration.app_id == app_id)
                .map(|declaration| declaration.capability.role)
                .collect()
        };
        self.store
            .transaction(|tx| providers::undeclare_within(tx, &app_id))
            .map_err(failed)?;
        self.registry.write().undeclare_app(&app_id);
        for role in affected_roles {
            let _ = emitter.changed(role.to_string()).await;
        }
        Ok(())
    }

    /// A role's resolution may have changed; re-call `Report` for the current state.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>, role: String) -> zbus::Result<()>;
}
