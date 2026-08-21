//! D-Bus interfaces `org.legeos.AppIdentity1` and `org.legeos.Launcher1`.
//!
//! The compositor keeps a local cache and never blocks its render loop on these calls: on a
//! cache miss it maps a window provisionally as unverified, requests resolution asynchronously
//! and updates the window record when the answer arrives.

use crate::identity::Registry;
use crate::launcher::{self, LaunchBackend};
use lege_types::{AppCatalog, AppId, WindowClaims};
use std::sync::Arc;
use zbus::fdo;

/// `org.legeos.AppIdentity1`
#[derive(Debug)]
pub struct IdentityApi {
    registry: Arc<Registry>,
}

impl IdentityApi {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

#[zbus::interface(name = "org.legeos.AppIdentity1")]
impl IdentityApi {
    /// Returns `(canonical_app_id, instance_id, confidence, client_claim_matches_manifest)`.
    async fn resolve_window(
        &self,
        pid: u32,
        wayland_app_id: &str,
        startup_wm_class: &str,
        sandbox_app_id: &str,
    ) -> fdo::Result<(String, String, u8, bool)> {
        let claims = WindowClaims {
            wayland_app_id: nonempty(wayland_app_id),
            startup_wm_class: nonempty(startup_wm_class),
            sandbox_app_id: nonempty(sandbox_app_id),
        };

        let resolution = self
            .registry
            .resolve_window(pid, &claims)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        Ok((
            resolution
                .app_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            resolution
                .instance_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            resolution.confidence as u8,
            resolution.claim_matches_manifest,
        ))
    }

    /// `(instance_id, app_id, systemd_unit, control_group)` for every live instance.
    /// A signal-driven API replaces this once the prototype works.
    async fn list_instances(&self) -> Vec<(String, String, String, String)> {
        self.registry
            .list_instances()
            .into_iter()
            .map(|instance| {
                (
                    instance.id.to_string(),
                    instance.app_id.to_string(),
                    instance.systemd_unit,
                    instance.control_group.display().to_string(),
                )
            })
            .collect()
    }

    /// Canonical IDs of every installed manifest.
    async fn list_apps(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .registry
            .manifests()
            .into_iter()
            .map(|manifest| manifest.id.to_string())
            .collect();
        ids.sort();
        ids
    }
}

/// `org.legeos.Launcher1`
#[derive(Debug)]
pub struct LauncherApi {
    registry: Arc<Registry>,
    backend: LaunchBackend,
    connection: zbus::Connection,
}

impl LauncherApi {
    pub fn new(
        registry: Arc<Registry>,
        backend: LaunchBackend,
        connection: zbus::Connection,
    ) -> Self {
        Self {
            registry,
            backend,
            connection,
        }
    }
}

#[zbus::interface(name = "org.legeos.Launcher1")]
impl LauncherApi {
    /// Launch `app_id` with `arguments`; returns the new instance ID.
    async fn launch(
        &self,
        app_id: &str,
        arguments: Vec<String>,
        activation_token: &str,
    ) -> fdo::Result<String> {
        let app_id: AppId = app_id
            .parse()
            .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        let manifest = self
            .registry
            .manifest(&app_id)
            .ok_or_else(|| fdo::Error::FileNotFound(format!("no manifest for {app_id}")))?;

        let record = launcher::launch(
            Arc::clone(&self.registry),
            manifest,
            &arguments,
            nonempty(activation_token).as_deref(),
            self.backend,
            Some(&self.connection),
        )
        .await
        .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        Ok(record.id.to_string())
    }
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}
