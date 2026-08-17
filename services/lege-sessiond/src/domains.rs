//! D-Bus faces of the session-core domains (providers, history, settings). Projects, power,
//! location, notifications and first-run follow the same pattern as they gain code.

use lege_session_core::{
    history::{HistoryStore, MemoryHistory},
    providers::ProviderRegistry,
    settings::{SettingsSchema, baseline_schema},
};
use lege_types::ProviderRole;
use parking_lot::RwLock;
use std::sync::Arc;
use zbus::fdo;

/// Everything sessiond hosts besides appd/associationd. In-memory for now; persisted through
/// `lege-state` as each domain lands.
#[derive(Debug)]
pub struct Domains {
    pub providers: Arc<RwLock<ProviderRegistry>>,
    pub history: Arc<MemoryHistory>,
    pub settings: Arc<RwLock<SettingsSchema>>,
}

impl Domains {
    pub fn new() -> Self {
        Self {
            providers: Arc::new(RwLock::new(ProviderRegistry::default())),
            history: Arc::new(MemoryHistory::default()),
            settings: Arc::new(RwLock::new(baseline_schema())),
        }
    }
}

/// `org.legeos.Providers1`
#[derive(Debug)]
pub struct ProvidersApi(Arc<RwLock<ProviderRegistry>>);

impl ProvidersApi {
    pub fn new(registry: Arc<RwLock<ProviderRegistry>>) -> Self {
        Self(registry)
    }
}

#[zbus::interface(name = "org.legeos.Providers1")]
impl ProvidersApi {
    /// `(role, resolution-json)` for every exclusive role.
    async fn report(&self) -> Vec<(String, String)> {
        self.0
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

    /// Select `app_id` as the provider for `role`.
    async fn select(&self, role: &str, app_id: &str) -> fdo::Result<()> {
        let role = ProviderRole::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.as_str() == role)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown role {role}")))?;
        let app_id = app_id
            .parse()
            .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        self.0.write().select(role, app_id);
        Ok(())
    }
}

/// `org.legeos.History1`
#[derive(Debug)]
pub struct HistoryApi(Arc<MemoryHistory>);

impl HistoryApi {
    pub fn new(history: Arc<MemoryHistory>) -> Self {
        Self(history)
    }
}

#[zbus::interface(name = "org.legeos.History1")]
impl HistoryApi {
    /// Most recent events as JSON strings, newest first.
    async fn recent(&self, limit: u32) -> fdo::Result<Vec<String>> {
        self.0
            .recent(limit as usize)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?
            .into_iter()
            .map(|event| {
                serde_json::to_string(&event).map_err(|error| fdo::Error::Failed(error.to_string()))
            })
            .collect()
    }

    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<String>> {
        self.0
            .search(query, limit as usize)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?
            .into_iter()
            .map(|event| {
                serde_json::to_string(&event).map_err(|error| fdo::Error::Failed(error.to_string()))
            })
            .collect()
    }
}

/// `org.legeos.Settings1`
#[derive(Debug)]
pub struct SettingsApi(Arc<RwLock<SettingsSchema>>);

impl SettingsApi {
    pub fn new(schema: Arc<RwLock<SettingsSchema>>) -> Self {
        Self(schema)
    }
}

#[zbus::interface(name = "org.legeos.Settings1")]
impl SettingsApi {
    /// `(key, description, default-json, privileged)` for every known setting.
    async fn schema(&self) -> Vec<(String, String, String, bool)> {
        self.0
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
}
