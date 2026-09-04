//! The D-Bus surface: six interfaces at one object path, all sharing one [`HostdState`]. Every
//! administrative mutating method checks polkit *in the daemon*, before doing anything, via
//! [`crate::polkit::require_authorized`] — "a system-bus daemon that does not check is the whole
//! attack surface" (ADR-0003). Read-only methods (profiles, plans, usage reports) are open to any
//! bus peer, same as `systemctl status` needs no privilege while `systemctl restart` does.
//! `Session1` is neither: it is gated by caller identity instead of polkit — see
//! `crate::session_report` for why.

use crate::{
    polkit::require_authorized, session_report::require_session_caller, state::HostdState,
};
use std::{path::Path, sync::Arc};
use zbus::message::Header;

fn to_json<T: serde::Serialize>(value: &T) -> zbus::fdo::Result<String> {
    serde_json::to_string(value).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
}

// Takes `anyhow::Error` by value so it can be passed directly as `.map_err(failed)` at every call
// site below, rather than a closure per call.
#[allow(clippy::needless_pass_by_value)]
fn failed(error: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}

#[derive(Debug)]
pub struct UpdatesApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Updates1")]
impl UpdatesApi {
    /// The current transaction as JSON, or an empty string.
    async fn current(&self) -> String {
        self.0
            .current_transaction()
            .and_then(|t| serde_json::to_string(&t).ok())
            .unwrap_or_default()
    }

    /// Plan a transaction for `changes` (JSON `ChangeSet`) and return its id immediately — the
    /// download/stage/verify pipeline runs in the background (a real apt install can run far
    /// longer than a D-Bus call should block for); poll `Current`/`History` for progress. Fails
    /// if one is already in flight and not terminal.
    async fn plan(
        &self,
        changes_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        require_authorized(&*self.0.polkit, &header, "update").await?;
        let changes = serde_json::from_str(changes_json)
            .map_err(|e| zbus::fdo::Error::InvalidArgs(e.to_string()))?;
        let id = self.0.plan_update(changes).map_err(failed)?;
        let state = self.0.clone();
        tokio::task::spawn_blocking(move || state.drive_pipeline());
        Ok(id)
    }

    /// Every transaction the journal remembers, as JSON (`Vec<Transaction>`) — the durable
    /// "why did that happen" record for updates.
    async fn history(&self) -> zbus::fdo::Result<String> {
        to_json(&self.0.transaction_history().map_err(failed)?)
    }

    /// Look up one past (or current) transaction by id, as JSON, or an empty string if unknown.
    async fn transaction(&self, id: &str) -> zbus::fdo::Result<String> {
        match self.0.transaction(id).map_err(failed)? {
            Some(txn) => to_json(&txn),
            None => Ok(String::new()),
        }
    }
}

#[derive(Debug)]
pub struct DeploymentsApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Deployments1")]
impl DeploymentsApi {
    /// Boot-manager backend in use (`systemd-boot` or `grub`).
    async fn backend(&self) -> String {
        self.0.backend_name().to_owned()
    }

    /// Known deployments as JSON.
    async fn list(&self) -> String {
        serde_json::to_string(&self.0.list_deployments()).unwrap_or_default()
    }

    /// Stage `id` for a trial boot with `tries` attempts.
    async fn stage(
        &self,
        id: &str,
        tries: u8,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        require_authorized(&*self.0.polkit, &header, "deploy").await?;
        self.0.stage_deployment(id, tries).map_err(failed)
    }

    /// Mark the running deployment good (called through boot-health).
    async fn mark_good(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        require_authorized(&*self.0.polkit, &header, "deploy").await?;
        self.0.mark_good().map_err(failed)
    }

    /// Roll back to `id`.
    async fn rollback(
        &self,
        id: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        require_authorized(&*self.0.polkit, &header, "deploy").await?;
        self.0.rollback(id).map_err(failed)
    }
}

#[derive(Debug)]
pub struct DriversApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Drivers1")]
impl DriversApi {
    /// The driver plan for the detected hardware, as JSON (`DriverPlan`: metapackage,
    /// `default_mode`, `violations`).
    async fn plan(&self) -> zbus::fdo::Result<String> {
        let plan = self.0.driver_plan().map_err(failed)?;
        to_json(&plan)
    }

    /// Support violations alone, as JSON (`Vec<SupportViolation>`) — what `lege doctor` renders.
    async fn violations(&self) -> zbus::fdo::Result<String> {
        let plan = self.0.driver_plan().map_err(failed)?;
        to_json(&plan.violations)
    }

    /// Accept the currently-installed NVIDIA versions as the new locked baseline. Returns the
    /// written `VersionLock` as JSON.
    async fn repair(&self, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<String> {
        require_authorized(&*self.0.polkit, &header, "driver").await?;
        let lock = self.0.repair_driver_lock().map_err(failed)?;
        to_json(&lock)
    }
}

#[derive(Debug)]
pub struct StorageApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Storage1")]
impl StorageApi {
    /// The trim order as kebab-case names.
    async fn trim_order(&self) -> Vec<String> {
        crate::storage::TRIM_ORDER
            .iter()
            .map(|target| {
                serde_json::to_string(target)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_owned()
            })
            .collect()
    }

    /// Usage and reserve for `mount_point`, as JSON (`VolumeUsage`).
    async fn usage(&self, mount_point: &str) -> zbus::fdo::Result<String> {
        let usage = self
            .0
            .storage_usage(Path::new(mount_point))
            .map_err(failed)?;
        to_json(&usage)
    }

    /// Run the trim order against `mount_point` until it is healthy or the order is exhausted.
    /// Returns what ran, as JSON (`Vec<TrimReport>`).
    async fn enforce(
        &self,
        mount_point: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        require_authorized(&*self.0.polkit, &header, "storage").await?;
        let reports = self
            .0
            .storage_enforce(Path::new(mount_point))
            .map_err(failed)?;
        to_json(&reports)
    }
}

#[derive(Debug)]
pub struct HardwareApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Hardware1")]
impl HardwareApi {
    /// The graphics profile as JSON.
    async fn graphics_profile(&self) -> zbus::fdo::Result<String> {
        let profile = self.0.graphics_profile().map_err(failed)?;
        to_json(&profile)
    }

    /// The declared GPU mode (`hybrid`, `integrated`, `discrete`), or an empty string if unset.
    async fn gpu_mode(&self) -> zbus::fdo::Result<String> {
        Ok(self
            .0
            .gpu_mode()
            .map_err(failed)?
            .map(|mode| match mode {
                lege_hwdetect::GpuMode::Hybrid => "hybrid",
                lege_hwdetect::GpuMode::Integrated => "integrated",
                lege_hwdetect::GpuMode::Discrete => "discrete",
            })
            .unwrap_or_default()
            .to_owned())
    }

    /// Set the GPU mode (`hybrid`, `integrated` or `discrete`) — the one declarative setting
    /// (planning doc 01 §4). Rejected if the detected hardware does not support it.
    async fn set_gpu_mode(
        &self,
        mode: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        require_authorized(&*self.0.polkit, &header, "hardware").await?;
        let mode = match mode {
            "hybrid" => lege_hwdetect::GpuMode::Hybrid,
            "integrated" => lege_hwdetect::GpuMode::Integrated,
            "discrete" => lege_hwdetect::GpuMode::Discrete,
            other => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "unknown gpu mode {other:?}"
                )));
            }
        };
        self.0.set_gpu_mode(mode).map_err(failed)
    }
}

#[derive(Debug)]
pub struct SessionApi(pub Arc<HostdState>);

#[zbus::interface(name = "org.legeos.Host.Session1")]
impl SessionApi {
    /// The compositor's Wayland socket is accepting connections. Not an administrative action —
    /// see `crate::session_report` for why this checks the caller's identity instead of polkit.
    async fn report_compositor_active(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        require_session_caller(&*self.0.caller_identity, &header).await?;
        self.0.report_compositor_active().map_err(failed)
    }

    /// The session's own startup sequence finished and a client frame was observed.
    async fn report_session_healthy(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        require_session_caller(&*self.0.caller_identity, &header).await?;
        self.0.report_session_healthy().map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn served_interface_names_match_lege_types() {
        use lege_types::dbus::host::{
            DEPLOYMENTS_IFACE, DRIVERS_IFACE, HARDWARE_IFACE, STORAGE_IFACE, UPDATES_IFACE,
        };
        let names = [
            UPDATES_IFACE,
            DEPLOYMENTS_IFACE,
            DRIVERS_IFACE,
            STORAGE_IFACE,
            HARDWARE_IFACE,
        ];
        assert_eq!(
            names,
            [
                "org.legeos.Host.Updates1",
                "org.legeos.Host.Deployments1",
                "org.legeos.Host.Drivers1",
                "org.legeos.Host.Storage1",
                "org.legeos.Host.Hardware1",
            ]
        );
    }
}

/// Read-only `@home` checkpoints and the btrfs metadata delta between two of them
/// (`crate::index_checkpoint`). Unadopted: nothing in the running system calls it, and it is
/// served so the idea can be measured against a real filesystem rather than argued about.
///
/// It carries its own `IndexCheckpoints` alongside the shared state because the store is a
/// spike's configuration, not part of the daemon's settled shape; authorization still goes
/// through the one `HostdState` every other interface uses.
#[derive(Debug)]
pub struct IndexCheckpointApi {
    pub state: Arc<HostdState>,
    pub checkpoints: Arc<crate::index_checkpoint::IndexCheckpoints>,
}

#[zbus::interface(name = "org.legeos.Host.IndexCheckpoint1")]
impl IndexCheckpointApi {
    /// Take a read-only checkpoint. `request_json` is a `CheckpointRequest`; an empty string is
    /// the default request.
    async fn create(
        &self,
        request_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        require_authorized(&*self.state.polkit, &header, "index-checkpoint").await?;
        let request = parse_request(request_json)?;
        to_json(&self.checkpoints.create(&request).map_err(failed)?)
    }

    /// Every retained checkpoint, as JSON. An unauthenticated read: it names snapshot ids and
    /// their subvolume, which `btrfs subvolume list` would tell any local user anyway.
    async fn list(&self) -> zbus::fdo::Result<String> {
        to_json(&self.checkpoints.list().map_err(failed)?)
    }

    /// `btrfs send --no-data -p <from> <to>` into a root-owned file; returns where it landed.
    async fn delta(
        &self,
        request_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<String> {
        require_authorized(&*self.state.polkit, &header, "index-checkpoint").await?;
        let request = parse_request(request_json)?;
        let checkpoints = self.checkpoints.clone();
        // `btrfs send` walks two metadata trees and can take a while on a large subvolume; a D-Bus
        // method must not hold the executor while it does.
        let stream = tokio::task::spawn_blocking(move || checkpoints.delta(&request))
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            .map_err(failed)?;
        to_json(&stream)
    }

    /// Drop a checkpoint, once the delta taken against it has been applied.
    async fn discard(&self, id: &str, #[zbus(header)] header: Header<'_>) -> zbus::fdo::Result<()> {
        require_authorized(&*self.state.polkit, &header, "index-checkpoint").await?;
        self.checkpoints.discard(id).map_err(failed)
    }
}

/// An empty argument is the default request, so a caller that wants the defaults does not have to
/// know the JSON shape.
fn parse_request<T: Default + serde::de::DeserializeOwned>(text: &str) -> zbus::fdo::Result<T> {
    if text.trim().is_empty() {
        return Ok(T::default());
    }
    serde_json::from_str(text).map_err(|error| zbus::fdo::Error::InvalidArgs(error.to_string()))
}
