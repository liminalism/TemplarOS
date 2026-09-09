//! Thin async traits over the `org.templaros.*` proxies this backend calls, so every interface's
//! decision logic is testable with a fake instead of a live session bus
//! (`crates/templar-client/src/proxies/{session,files,resources}.rs` are the real client side; this
//! module never edits them).
//!
//! Each `Live*` implementation holds an owned [`zbus::Connection`] (cheap to clone, genuinely
//! `'static`) and builds its proxy fresh per call, rather than storing a lifetime-bound proxy —
//! the same pattern `services/templar-sessiond/src/checkpoint.rs`'s `DbusCaller` uses, which is what
//! lets these clients live behind `Arc<dyn Trait>` inside a spawned background task.

use anyhow::{Context, Result};
use async_trait::async_trait;
use templar_client::proxies::{
    AppIdentityProxy, AssociationsProxy, FilesRecentsProxy, ResourcesProxy, SettingsProxy,
};

/// What `Files.Recents1` gives `FileChooser`: the shared "last folder" policy and the write-back
/// that keeps the file manager and every sandboxed application in agreement.
#[async_trait]
pub trait RecentsClient: Send + Sync + std::fmt::Debug {
    /// The `Suggestion` JSON `Files.Recents1.SuggestDirectory` returns.
    async fn suggest_directory(&self, app_id: &str, operation: &str) -> Result<String>;
    async fn record_directory(&self, app_id: &str, operation: &str, directory: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct LiveRecents(pub zbus::Connection);

#[async_trait]
impl RecentsClient for LiveRecents {
    async fn suggest_directory(&self, app_id: &str, operation: &str) -> Result<String> {
        FilesRecentsProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Files.Recents1")?
            .suggest_directory(app_id, operation)
            .await
            .context("Files.Recents1.SuggestDirectory")
    }

    async fn record_directory(&self, app_id: &str, operation: &str, directory: &str) -> Result<()> {
        FilesRecentsProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Files.Recents1")?
            .record_directory(app_id, operation, directory)
            .await
            .context("Files.Recents1.RecordDirectory")
    }
}

/// What `Associations1` gives `AppChooser`: automatic resolution before ever asking a human.
#[async_trait]
pub trait AssociationsClient: Send + Sync + std::fmt::Debug {
    /// `(canonical_app_id, detected_mime, reason)`; an empty `canonical_app_id` means "ask".
    async fn resolve_uri(&self, uri: &str) -> Result<(String, String, String)>;
    /// `(mime, app_id)` pairs — the global defaults, used to resolve a bare `content_type` with
    /// no URI to detect from.
    async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>>;
}

#[derive(Debug, Clone)]
pub struct LiveAssociations(pub zbus::Connection);

#[async_trait]
impl AssociationsClient for LiveAssociations {
    async fn resolve_uri(&self, uri: &str) -> Result<(String, String, String)> {
        AssociationsProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Associations1")?
            .resolve_uri(uri, "")
            .await
            .context("Associations1.ResolveUri")
    }

    async fn list_mime_defaults(&self) -> Result<Vec<(String, String)>> {
        AssociationsProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Associations1")?
            .list_mime_defaults()
            .await
            .context("Associations1.ListMimeDefaults")
    }
}

/// What `Settings1` gives the `Settings` portal backend.
#[async_trait]
pub trait SessionSettingsClient: Send + Sync + std::fmt::Debug {
    /// `(key, value_json)` for every known setting.
    async fn list(&self) -> Result<Vec<(String, String)>>;
}

#[derive(Debug, Clone)]
pub struct LiveSettings(pub zbus::Connection);

#[async_trait]
impl SessionSettingsClient for LiveSettings {
    async fn list(&self) -> Result<Vec<(String, String)>> {
        SettingsProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Settings1")?
            .list()
            .await
            .context("Settings1.List")
    }
}

/// What the `Background` portal backend needs to know who is running and how visible they are:
/// `AppIdentity1` for the roster, `Resources1` for foreground/visible enrichment.
#[async_trait]
pub trait AppStateClient: Send + Sync + std::fmt::Debug {
    /// `(instance_id, app_id, systemd_unit, control_group)` for every running instance.
    async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>>;
    /// Full `Report` JSON (`InstanceReport` entries carry `instance_id`, `foreground`, `visible`).
    async fn report(&self) -> Result<String>;
    /// `(canonical_app_id, display_name, desktop_file_id)` for every installed application.
    ///
    /// `AppChooser` needs this because the two sides of a choice speak different names: the
    /// portal frontend offers *desktop file ids*, while `Associations1` answers with TemplarOS
    /// canonical app ids. Without the mapping a resolved default never matches an offered
    /// choice, and every open falls through to asking the person something the system already
    /// knew.
    async fn list_applications(&self) -> Result<Vec<(String, String, String)>>;
}

#[derive(Debug, Clone)]
pub struct LiveAppState(pub zbus::Connection);

#[async_trait]
impl AppStateClient for LiveAppState {
    async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>> {
        AppIdentityProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.AppIdentity1")?
            .list_instances()
            .await
            .context("AppIdentity1.ListInstances")
    }

    async fn list_applications(&self) -> Result<Vec<(String, String, String)>> {
        Ok(AppIdentityProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.AppIdentity1")?
            .list_applications()
            .await
            .context("AppIdentity1.ListApplications")?
            .into_iter()
            .map(|(app_id, display_name, desktop_file_id, ..)| {
                (app_id, display_name, desktop_file_id)
            })
            .collect())
    }

    async fn report(&self) -> Result<String> {
        ResourcesProxy::new(&self.0)
            .await
            .context("connecting to org.templaros.Resources1")?
            .report()
            .await
            .context("Resources1.Report")
    }
}
