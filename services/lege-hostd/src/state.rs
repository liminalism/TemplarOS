//! `HostdState`: the state every `org.legeos.Host.*` interface shares. Keeping it in one place is
//! what lets `MarkGood`/`Rollback` on `Deployments1` also settle the matching `Updates1`
//! transaction — kernel and NVIDIA succeed or roll back *as a pair*, and that pairing is exactly
//! the coupling between a boot deployment and the transaction that produced it.

use crate::{
    apt::Apt,
    clock::Clock,
    cmd::CommandRunner,
    deploy::{self, BootManager, Deployment},
    drivers::{self, DriverPlan, SupportViolation, VersionLock},
    hardware,
    journal::Journal,
    polkit::PolkitAuthority,
    session_report::CallerIdentity,
    snapshot::Snapshotter,
    storage::{self, TrimReport, VolumeUsage},
    updates::{ChangeSet, Engine, Phase, Transaction},
};
use anyhow::{Context, Result};
use lege_hwdetect::GpuMode;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct HostdState {
    pub engine: Engine,
    pub boot: Arc<dyn BootManager>,
    pub snapshotter: Arc<dyn Snapshotter>,
    pub commands: Arc<dyn CommandRunner>,
    pub polkit: Arc<dyn PolkitAuthority>,
    pub caller_identity: Arc<dyn CallerIdentity>,
    pub deployments: Mutex<Vec<Deployment>>,
    pub deployments_path: PathBuf,
    /// Sysfs root for hardware detection (`/sys` on a live system).
    pub sys_root: PathBuf,
    /// Root for dpkg/apt-sources inspection (`/` on a live system).
    pub apt_root: PathBuf,
    /// Where declarative config (`gpu-mode`, `version-lock.toml`) lives (`/etc/legeos/hardware`).
    pub etc_dir: PathBuf,
    /// hostd's own `RuntimeDirectory` (`/run/legeos` in production) — the only place the
    /// `Session1` report markers `lege-boot-health` reads are ever written.
    pub run_dir: PathBuf,
}

/// Everything [`HostdState::new`] needs to wire up — real backends in `main`, fakes in tests.
pub struct HostdConfig {
    pub apt: Arc<dyn Apt>,
    pub snapshotter: Arc<dyn Snapshotter>,
    pub boot: Arc<dyn BootManager>,
    pub commands: Arc<dyn CommandRunner>,
    pub clock: Arc<dyn Clock>,
    pub polkit: Arc<dyn PolkitAuthority>,
    pub caller_identity: Arc<dyn CallerIdentity>,
    pub journal_dir: PathBuf,
    pub source_root: PathBuf,
    pub deployments_dir: PathBuf,
    pub esp_root: PathBuf,
    pub deployments_path: PathBuf,
    pub sys_root: PathBuf,
    pub apt_root: PathBuf,
    pub etc_dir: PathBuf,
    pub run_dir: PathBuf,
}

impl HostdState {
    pub fn new(config: HostdConfig) -> Result<Self> {
        let engine = Engine::new(
            Journal::new(&config.journal_dir)?,
            config.apt,
            config.snapshotter.clone(),
            config.boot.clone(),
            config.clock,
            config.source_root,
            config.deployments_dir,
            config.esp_root,
        )?;
        let deployments = deploy::load_deployments(&config.deployments_path)?;
        Ok(Self {
            engine,
            boot: config.boot,
            snapshotter: config.snapshotter,
            commands: config.commands,
            polkit: config.polkit,
            caller_identity: config.caller_identity,
            deployments: Mutex::new(deployments),
            deployments_path: config.deployments_path,
            sys_root: config.sys_root,
            apt_root: config.apt_root,
            etc_dir: config.etc_dir,
            run_dir: config.run_dir,
        })
    }

    // -- Session report ---------------------------------------------------------------------

    /// Persist "the compositor's Wayland socket is accepting connections" — called only after
    /// [`crate::session_report::require_session_caller`] has verified the caller is
    /// `lege-sessiond.service`.
    pub fn report_compositor_active(&self) -> Result<()> {
        write_marker(&self.run_dir.join("compositor-active"))
    }

    /// Persist "the session's own startup sequence finished and a client frame was observed" —
    /// same caller verification as above.
    pub fn report_session_healthy(&self) -> Result<()> {
        write_marker(&self.run_dir.join("session-healthy"))
    }

    fn version_lock_path(&self) -> PathBuf {
        self.etc_dir.join("version-lock.toml")
    }

    // -- Hardware -----------------------------------------------------------------------------

    pub fn graphics_profile(&self) -> Result<lege_hwdetect::GraphicsProfile> {
        hardware::probe(&self.sys_root)
    }

    pub fn set_gpu_mode(&self, mode: GpuMode) -> Result<()> {
        let profile = self.graphics_profile()?;
        hardware::set_gpu_mode(mode, &profile, &self.etc_dir)
    }

    pub fn gpu_mode(&self) -> Result<Option<GpuMode>> {
        hardware::read_gpu_mode(&self.etc_dir)
    }

    // -- Drivers --------------------------------------------------------------------------------

    fn driver_violations(&self) -> Result<Vec<SupportViolation>> {
        let mut violations = drivers::detect_violations(&self.apt_root);
        let lock = drivers::read_version_lock(&self.version_lock_path())?;
        let packages = lege_hwdetect::dpkg::scan_status(&self.apt_root.join("var/lib/dpkg/status"));
        violations.extend(drivers::version_lock_violations(&lock, &packages));
        Ok(violations)
    }

    pub fn driver_plan(&self) -> Result<DriverPlan> {
        let profile = self.graphics_profile()?;
        let mut plan = drivers::plan_for(&profile);
        plan.violations = self.driver_violations()?;
        Ok(plan)
    }

    /// Accept the currently-installed NVIDIA LRM/userspace versions as the new locked baseline —
    /// the declarative, hostd-owned alternative to hand-editing a pin file.
    pub fn repair_driver_lock(&self) -> Result<VersionLock> {
        let packages = lege_hwdetect::dpkg::scan_status(&self.apt_root.join("var/lib/dpkg/status"));
        let nvidia_lrm = lege_hwdetect::dpkg::find_installed(&packages, "linux-modules-nvidia-")
            .first()
            .map(|p| p.version.clone());
        let nvidia_userspace = lege_hwdetect::dpkg::find_installed(&packages, "nvidia-driver-")
            .first()
            .map(|p| p.version.clone());
        let lock = VersionLock {
            kernel: None,
            nvidia_lrm,
            nvidia_userspace,
        };
        drivers::write_version_lock(&self.version_lock_path(), &lock)?;
        Ok(lock)
    }

    // -- Storage --------------------------------------------------------------------------------

    // Kept as a method (rather than an associated function) for symmetry with `storage_enforce`
    // and the rest of this uniform `&self`-taking read/write surface.
    #[allow(clippy::unused_self)]
    pub fn storage_usage(&self, mount_point: &Path) -> Result<VolumeUsage> {
        storage::query(mount_point)
    }

    pub fn storage_enforce(&self, mount_point: &Path) -> Result<Vec<TrimReport>> {
        let usage = storage::query(mount_point)?;
        let old_deployments = self.trimmable_deployments();
        storage::enforce(
            mount_point,
            self.commands.clone(),
            self.snapshotter.clone(),
            old_deployments,
            usage,
        )
    }

    /// `(subvolume path, 0)` for every deployment that is not known-good — the size estimate is
    /// left at 0 (no du walk implemented yet) rather than fabricated; see the crate report.
    fn trimmable_deployments(&self) -> Vec<(String, u64)> {
        self.deployments
            .lock()
            .expect("poisoned")
            .iter()
            .filter(|d| !d.known_good)
            .map(|d| (d.subvolume.clone(), 0))
            .collect()
    }

    // -- Updates --------------------------------------------------------------------------------

    pub fn current_transaction(&self) -> Option<Transaction> {
        self.engine.current()
    }

    pub fn transaction_history(&self) -> Result<Vec<Transaction>> {
        self.engine.history()
    }

    pub fn transaction(&self, id: &str) -> Result<Option<Transaction>> {
        self.engine.transaction(id)
    }

    /// Records the transaction and returns immediately — a real `apt-get install` for a kernel
    /// and NVIDIA modules can run far longer than a D-Bus method call should block for (dbus's
    /// and zbus's own default reply timeouts are on the order of seconds, not minutes). The
    /// caller gets the id back at once and polls `Current`/`History` for progress, exactly as it
    /// would after hostd restarts mid-transaction and resumes on its own.
    pub fn plan_update(&self, changes: ChangeSet) -> Result<String> {
        self.engine.plan(changes)
    }

    /// Drive the current transaction forward. Failures are recorded in the transaction itself
    /// (`Phase::Failed`) rather than propagated to the caller — there usually isn't one waiting,
    /// since this runs either at startup (resuming the journal) or detached from the `Plan` RPC.
    pub fn drive_pipeline(&self) {
        if self.engine.current().is_some()
            && let Err(error) = self.engine.run_to_awaiting_reboot()
        {
            tracing::warn!(%error, "transaction pipeline stopped");
        }
    }

    /// Resume whatever the journal remembers — called once at startup, synchronously: hostd
    /// should not advertise itself as ready while it does not yet know the true phase of a
    /// transaction that was in flight when it last stopped.
    pub fn resume(&self) {
        self.drive_pipeline();
    }

    // -- Deployments ----------------------------------------------------------------------------

    pub fn backend_name(&self) -> &'static str {
        self.boot.name()
    }

    pub fn list_deployments(&self) -> Vec<Deployment> {
        self.deployments.lock().expect("poisoned").clone()
    }

    fn save_deployments(&self, deployments: &[Deployment]) -> Result<()> {
        deploy::save_deployments(&self.deployments_path, deployments)
    }

    pub fn stage_deployment(&self, id: &str, tries: u8) -> Result<()> {
        let deployments = self.deployments.lock().expect("poisoned");
        let candidate = deployments
            .iter()
            .find(|d| d.id == id)
            .context("unknown deployment")?;
        self.boot.stage_candidate(candidate, tries)?;
        self.engine.record_trial_boot().ok();
        Ok(())
    }

    /// Bless the running deployment. If it is the candidate of a transaction still in
    /// [`Phase::Trial`], the transaction is promoted and the candidate joins the registry as
    /// known-good — the pairing ADR-0003 requires.
    pub fn mark_good(&self) -> Result<()> {
        self.boot.mark_good()?;
        if let Some(txn) = self.engine.current()
            && txn.phase == Phase::Trial
        {
            self.engine.promote()?;
            if let Some(deployment) = self.engine.candidate_deployment() {
                let mut deployments = self.deployments.lock().expect("poisoned");
                for existing in deployments.iter_mut() {
                    existing.known_good = false;
                }
                match deployments.iter_mut().find(|d| d.id == deployment.id) {
                    Some(existing) => existing.known_good = true,
                    None => deployments.push(Deployment {
                        known_good: true,
                        ..deployment
                    }),
                }
                self.save_deployments(&deployments)?;
            }
        }
        Ok(())
    }

    /// Roll back to `id`. If the running deployment was a transaction's candidate still in
    /// [`Phase::Trial`], the transaction is marked rolled back to match.
    pub fn rollback(&self, id: &str) -> Result<()> {
        let previous = {
            let deployments = self.deployments.lock().expect("poisoned");
            deployments
                .iter()
                .find(|d| d.id == id)
                .cloned()
                .context("unknown deployment")?
        };
        self.boot.rollback(&previous)?;
        if let Some(txn) = self.engine.current()
            && txn.phase == Phase::Trial
        {
            self.engine
                .roll_back(format!("rolled back to known-good deployment {id}"))?;
        }
        Ok(())
    }
}

/// Atomic write-temp-then-rename of a one-line timestamp, matching the journal's own pattern —
/// `lege-boot-health` only cares whether the file is readable, but a timestamp costs nothing and
/// helps a human reading `cat /run/legeos/session-healthy` after the fact.
fn write_marker(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{now}\n"))
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        apt::FakeApt, clock::FixedClock, cmd::FakeCommandRunner, deploy::SystemdBoot,
        polkit::AllowAll, snapshot::FakeSnapshotter,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn config(dir: &Path) -> HostdConfig {
        let commands = Arc::new(FakeCommandRunner::default());
        HostdConfig {
            apt: Arc::new(FakeApt::default()),
            snapshotter: Arc::new(FakeSnapshotter::default()),
            boot: Arc::new(SystemdBoot {
                commands: commands.clone(),
                esp_root: dir.join("esp"),
            }),
            commands,
            clock: Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_786_960_800))),
            polkit: Arc::new(AllowAll),
            caller_identity: Arc::new(crate::session_report::FixedCallerIdentity::allow()),
            journal_dir: dir.join("journal"),
            source_root: dir.join("root"),
            deployments_dir: dir.join("deployments"),
            esp_root: dir.join("esp"),
            deployments_path: dir.join("deployments.json"),
            sys_root: dir.join("sys"),
            apt_root: dir.join("root"),
            etc_dir: dir.join("etc/legeos/hardware"),
            run_dir: dir.join("run/legeos"),
        }
    }

    fn changes() -> ChangeSet {
        ChangeSet {
            kernel: Some("linux-image-7.2.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            nvidia_userspace: Some("nvidia-driver-580-open".into()),
            firmware: vec![],
            other_packages: vec![],
        }
    }

    fn with_bootable_root(dir: &Path) {
        std::fs::create_dir_all(dir.join("root/boot")).unwrap();
        std::fs::write(dir.join("root/boot/vmlinuz-x"), b"k").unwrap();
        std::fs::write(dir.join("root/boot/initrd.img-x"), b"i").unwrap();
    }

    #[test]
    fn plan_mark_good_promotes_transaction_and_registers_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();

        let id = state.plan_update(changes()).unwrap();
        assert_eq!(
            state.current_transaction().unwrap().phase,
            Phase::Downloading
        );
        state.drive_pipeline();
        assert_eq!(
            state.current_transaction().unwrap().phase,
            Phase::AwaitingReboot
        );

        state.stage_deployment_for_test(&id);
        state.mark_good().unwrap();

        let txn = state.current_transaction().unwrap();
        assert_eq!(txn.phase, Phase::Promoted);
        let deployments = state.list_deployments();
        assert_eq!(deployments.len(), 1);
        assert!(deployments[0].known_good);
    }

    #[test]
    fn rollback_rolls_back_the_matching_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();
        let id = state.plan_update(changes()).unwrap();
        state.drive_pipeline();
        state.stage_deployment_for_test(&id);

        // The candidate must exist in the registry to be rolled back to; register it as this
        // deployment's own previous state would have been.
        {
            let mut deployments = state.deployments.lock().unwrap();
            deployments.push(state.engine.candidate_deployment().unwrap());
        }
        state.rollback(&id).unwrap();
        assert_eq!(
            state.current_transaction().unwrap().phase,
            Phase::RolledBack
        );
    }

    #[test]
    fn plan_update_alone_does_not_drive_the_pipeline() {
        // The whole point of splitting `plan_update` from `drive_pipeline`: a D-Bus `Plan` call
        // must return the id fast, not block for the length of a real apt install.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();
        state.plan_update(changes()).unwrap();
        // Still Downloading, not Staging or further: drive_pipeline (and therefore apt.download)
        // has not run yet.
        assert_eq!(
            state.current_transaction().unwrap().phase,
            Phase::Downloading
        );
    }

    #[test]
    fn session_reports_write_markers_under_run_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();

        assert!(!dir.join("run/legeos/compositor-active").exists());
        state.report_compositor_active().unwrap();
        assert!(dir.join("run/legeos/compositor-active").exists());

        assert!(!dir.join("run/legeos/session-healthy").exists());
        state.report_session_healthy().unwrap();
        assert!(dir.join("run/legeos/session-healthy").exists());
    }

    // Test-only helpers.
    impl HostdState {
        fn stage_deployment_for_test(&self, _id: &str) {
            self.engine.record_trial_boot().unwrap();
        }
    }
}
