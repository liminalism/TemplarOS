//! Boot deployments and rollback. The `BootManager` trait abstracts the two backends decided in
//! ADR-0004: `systemd-boot` (boot-assessment counters + `systemd-bless-boot`) and `grub`
//! (env-block counter maintained by hostd). Root is btrfs subvolumes; a deployment is a read-only
//! snapshot plus a hostd-owned copy of kernel + initrd + entry on the ESP.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    /// btrfs subvolume path (e.g. `@deployments/2026-08-17T10-00`).
    pub subvolume: String,
    /// ESP directory holding this deployment's kernel/initrd/entry.
    pub esp_dir: String,
    pub kernel: String,
    pub nvidia_modules: Option<String>,
    pub known_good: bool,
}

/// What every backend must provide.
pub trait BootManager: Send + Sync {
    /// Register `candidate` as the next boot with `tries` attempts before automatic fallback.
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> anyhow::Result<()>;
    /// Mark the currently booted deployment good (called by boot-health via hostd).
    fn mark_good(&self) -> anyhow::Result<()>;
    /// Make `previous` the default again and drop the candidate's tries.
    fn rollback(&self, previous: &Deployment) -> anyhow::Result<()>;
    /// Human-readable backend name for `lege doctor`.
    fn name(&self) -> &'static str;
}

/// systemd-boot backend: entries `legeos-<id>+<tries>.conf`, `systemd-bless-boot`.
#[derive(Debug, Default)]
pub struct SystemdBoot;

impl BootManager for SystemdBoot {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> anyhow::Result<()> {
        tracing::info!(id = %candidate.id, tries, "would write boot-assessment entry (not implemented in M0)");
        Ok(())
    }
    fn mark_good(&self) -> anyhow::Result<()> {
        tracing::info!("would run systemd-bless-boot good (not implemented in M0)");
        Ok(())
    }
    fn rollback(&self, previous: &Deployment) -> anyhow::Result<()> {
        tracing::info!(id = %previous.id, "would restore previous default entry (not implemented in M0)");
        Ok(())
    }
    fn name(&self) -> &'static str {
        "systemd-boot"
    }
}

/// Select the backend for this installation (ADR-0004). systemd-boot when its loader is present.
pub fn detect_backend(root: &std::path::Path) -> Box<dyn BootManager> {
    if root.join("boot/efi/loader/loader.conf").exists()
        || root.join("efi/loader/loader.conf").exists()
    {
        Box::new(SystemdBoot)
    } else {
        Box::new(Grub)
    }
}

/// `org.legeos.Host.Deployments1`
pub struct DeploymentsApi {
    pub backend: Box<dyn BootManager>,
    pub known: Vec<Deployment>,
}

impl std::fmt::Debug for DeploymentsApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeploymentsApi")
            .field("backend", &self.backend.name())
            .finish_non_exhaustive()
    }
}

#[zbus::interface(name = "org.legeos.Host.Deployments1")]
impl DeploymentsApi {
    /// Boot-manager backend in use (`systemd-boot` or `grub`).
    async fn backend(&self) -> String {
        self.backend.name().to_owned()
    }

    /// Known deployments as JSON.
    async fn list(&self) -> String {
        serde_json::to_string(&self.known).unwrap_or_default()
    }

    /// Stage `id` for a trial boot with `tries` attempts (M0: logs only).
    async fn stage(&self, id: &str, tries: u8) -> zbus::fdo::Result<()> {
        let candidate =
            self.known.iter().find(|d| d.id == id).ok_or_else(|| {
                zbus::fdo::Error::FileNotFound(format!("unknown deployment {id}"))
            })?;
        self.backend
            .stage_candidate(candidate, tries)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Mark the running deployment good (called through boot-health).
    async fn mark_good(&self) -> zbus::fdo::Result<()> {
        self.backend
            .mark_good()
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    /// Roll back to `id`.
    async fn rollback(&self, id: &str) -> zbus::fdo::Result<()> {
        let previous =
            self.known.iter().find(|d| d.id == id).ok_or_else(|| {
                zbus::fdo::Error::FileNotFound(format!("unknown deployment {id}"))
            })?;
        self.backend
            .rollback(previous)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

/// GRUB backend: `GRUB_DEFAULT=saved`, `grub-reboot` for the trial, `grub-set-default` on success.
#[derive(Debug, Default)]
pub struct Grub;

impl BootManager for Grub {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> anyhow::Result<()> {
        tracing::info!(id = %candidate.id, tries, "would grub-reboot into candidate (not implemented in M0)");
        Ok(())
    }
    fn mark_good(&self) -> anyhow::Result<()> {
        tracing::info!("would grub-set-default (not implemented in M0)");
        Ok(())
    }
    fn rollback(&self, previous: &Deployment) -> anyhow::Result<()> {
        tracing::info!(id = %previous.id, "would grub-set-default previous (not implemented in M0)");
        Ok(())
    }
    fn name(&self) -> &'static str {
        "grub"
    }
}
