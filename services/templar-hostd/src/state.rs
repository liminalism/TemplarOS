//! `HostdState`: the state every `org.templaros.Host.*` interface shares. Keeping it in one place is
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
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use templar_hwdetect::GpuMode;

#[derive(Debug)]
pub struct HostdState {
    pub engine: Engine,
    pub boot: Arc<dyn BootManager>,
    pub snapshotter: Arc<dyn Snapshotter>,
    pub commands: Arc<dyn CommandRunner>,
    pub polkit: Arc<dyn PolkitAuthority>,
    pub caller_identity: Arc<dyn CallerIdentity>,
    /// Every known deployment plus the three identities — current, previous, known-good — that
    /// nothing else on the machine remembers.
    pub deployments: Mutex<deploy::DeploymentRegistry>,
    pub deployments_path: PathBuf,
    /// Sysfs root for hardware detection (`/sys` on a live system).
    pub sys_root: PathBuf,
    /// Root for dpkg/apt-sources inspection (`/` on a live system).
    pub apt_root: PathBuf,
    /// Persistent GPU policy, separate from deployment-paired driver version metadata.
    pub hardware_state_dir: PathBuf,
    /// hostd's own `RuntimeDirectory` (`/run/templaros` in production) — the only place the
    /// `Session1` report markers `templar-boot-health` reads are ever written.
    pub run_dir: PathBuf,
    /// `/proc/cmdline`. The only thing on the machine that says which deployment actually booted.
    pub cmdline_path: PathBuf,
}

/// What [`HostdState::reconcile_boot`] found at startup.
///
/// An enum rather than a `Result<()>` because every one of these is a normal outcome that a
/// person reading the journal after a failed update needs to be able to tell apart.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BootReconciliation {
    /// No candidate staged, or a backend that counts its own attempts.
    NoCandidate,
    /// The kernel command line does not say which deployment booted.
    CannotTell,
    /// This boot *is* the trial. Nothing to account for — boot-health decides it.
    BootedTheCandidate { candidate: String },
    /// The candidate did not boot; one attempt spent, some left.
    AttemptSpent { candidate: String, remaining: u8 },
    /// The candidate ran out of attempts and the known-good deployment is the default again.
    RolledBack { candidate: String, to: String },
    /// It should have rolled back and could not.
    CouldNotRollBack { candidate: String, reason: String },
    /// The boot state could not be read at all.
    Unreadable(String),
}

impl BootReconciliation {
    /// One line for the journal.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::NoCandidate => "no candidate deployment is staged".to_owned(),
            Self::CannotTell => {
                "the kernel command line does not name a deployment; boot attempts are not \
                 being counted"
                    .to_owned()
            }
            Self::BootedTheCandidate { candidate } => {
                format!("this boot is the trial of {candidate}")
            }
            Self::AttemptSpent {
                candidate,
                remaining,
            } => format!("{candidate} did not boot; {remaining} attempt(s) left"),
            Self::RolledBack { candidate, to } => {
                format!("{candidate} never booted; rolled back to {to}")
            }
            Self::CouldNotRollBack { candidate, reason } => {
                format!("{candidate} is out of attempts and could not be rolled back: {reason}")
            }
            Self::Unreadable(reason) => format!("could not read the boot state: {reason}"),
        }
    }
}

/// The value of `templaros.deployment=` on a kernel command line.
///
/// Split out from the daemon so the parsing is testable, and written as an exact-key scan rather
/// than a substring search: `xtemplaros.deployment=` and `templaros.deployment_backup=` must not match,
/// because the answer decides whether a boot attempt is spent.
#[must_use]
pub fn booted_deployment_from_cmdline(cmdline: &str) -> Option<String> {
    cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix("templaros.deployment="))
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
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
    pub deployment_layout: crate::layout::DeploymentLayout,
    pub deployments_path: PathBuf,
    pub sys_root: PathBuf,
    pub apt_root: PathBuf,
    pub hardware_state_dir: PathBuf,
    pub run_dir: PathBuf,
    pub cmdline_path: PathBuf,
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
            config.deployment_layout,
        )?;
        let deployments = deploy::load_registry(&config.deployments_path)?;
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
            hardware_state_dir: config.hardware_state_dir,
            run_dir: config.run_dir,
            cmdline_path: config.cmdline_path,
        })
    }

    // -- Session report ---------------------------------------------------------------------

    /// Persist "the compositor's Wayland socket is accepting connections" — called only after
    /// [`crate::session_report::require_session_caller`] has verified the caller is
    /// `templar-sessiond.service`.
    pub fn report_compositor_active(&self) -> Result<()> {
        write_marker(&self.run_dir.join("compositor-active"))
    }

    /// Persist "the session's own startup sequence finished and a client frame was observed" —
    /// same caller verification as above.
    pub fn report_session_healthy(&self) -> Result<()> {
        write_marker(&self.run_dir.join("session-healthy"))
    }

    fn version_lock_path(&self) -> PathBuf {
        self.apt_root
            .join("etc/templaros/hardware/version-lock.toml")
    }

    // -- Hardware -----------------------------------------------------------------------------

    pub fn graphics_profile(&self) -> Result<templar_hwdetect::GraphicsProfile> {
        hardware::probe(&self.sys_root)
    }

    pub fn set_gpu_mode(&self, mode: GpuMode) -> Result<()> {
        let profile = self.graphics_profile()?;
        hardware::set_gpu_mode(mode, &profile, &self.hardware_state_dir)
    }

    pub fn gpu_mode(&self) -> Result<Option<GpuMode>> {
        hardware::read_gpu_mode(&self.hardware_state_dir)
    }

    // -- Drivers --------------------------------------------------------------------------------

    fn driver_violations(&self) -> Result<Vec<SupportViolation>> {
        let mut violations = drivers::detect_violations(&self.apt_root);
        let lock = drivers::read_version_lock(&self.version_lock_path())?;
        let packages =
            templar_hwdetect::dpkg::scan_status(&self.apt_root.join("var/lib/dpkg/status"));
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
        let lock = drivers::derive_lock(&self.apt_root)?;
        drivers::write_version_lock(&self.version_lock_path(), &lock)
            .context("driver baseline is deployment-paired System metadata; read-only roots require a hostd candidate update")?;
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
        let _maintenance = self.engine.maintenance_guard();
        let usage = storage::query(mount_point)?;
        let old_deployments = self.trimmable_deployments();
        let outcome = storage::enforce(
            mount_point,
            self.commands.clone(),
            self.snapshotter.clone(),
            &old_deployments,
            usage,
            self.engine.root_read_only(),
        );
        // Even a partially completed trim must not leave nonexistent account roots
        // in the registry. Filesystem inspection errors are not treated as deletion.
        let removed: Vec<_> = old_deployments
            .iter()
            .map(|(path, _)| Ok((path, !Path::new(path).try_exists()?)))
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut registry = self.deployments.lock().expect("poisoned");
        let before = registry.deployments.len();
        registry.deployments.retain(|deployment| {
            self.engine
                .mounted_deployment(&deployment.subvolume)
                .ok()
                .is_none_or(|path| {
                    !removed
                        .iter()
                        .any(|(removed, missing)| *missing && Path::new(removed) == path)
                })
        });
        if registry.deployments.len() != before {
            self.save_registry(&registry)?;
        }
        outcome
    }

    /// `(subvolume path, estimated bytes)` for every deployment the trim order is allowed to
    /// delete: never the booted one, never the known-good one, never what the registry calls
    /// current or previous, and never the newest [`storage::DEFAULT_KEEP_DEPLOYMENTS`] of the
    /// rest. Oldest first, which is the order they are deleted in.
    fn trimmable_deployments(&self) -> Vec<(String, u64)> {
        let booted = self.booted_deployment_id();
        let registry = self.deployments.lock().expect("poisoned");
        let mut protected: Vec<&str> = Vec::new();
        let candidate = self
            .engine
            .candidate_deployment()
            .map(|deployment| deployment.id);
        for id in [
            booted.as_deref(),
            registry.known_good.as_deref(),
            registry.current.as_deref(),
            registry.previous.as_deref(),
            candidate.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            protected.push(id);
        }
        // A deployment still flagged known-good by a registry that has no identities recorded —
        // one written before they existed — is protected by the flag alone.
        for deployment in registry.deployments.iter().filter(|d| d.known_good) {
            protected.push(&deployment.id);
        }
        let ordered: Vec<String> = registry.deployments.iter().map(|d| d.id.clone()).collect();
        let trimmable =
            storage::trimmable_deployments(&ordered, &protected, storage::DEFAULT_KEEP_DEPLOYMENTS);
        trimmable
            .into_iter()
            .filter_map(|id| registry.find(id))
            .filter_map(|deployment| {
                let mounted = self.engine.mounted_deployment(&deployment.subvolume).ok()?;
                let size = storage::estimate_size(&mounted);
                Some((mounted.display().to_string(), size))
            })
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
        self.engine.plan_from(changes, self.displaced_deployment())
    }

    /// The deployment a new transaction would displace: what actually booted, and failing that
    /// what the registry calls current, and failing that the known-good one. Recorded on the
    /// transaction so a failure later can name where the machine still is.
    fn displaced_deployment(&self) -> Option<String> {
        if let Some(booted) = self.booted_deployment_id() {
            return Some(booted);
        }
        let registry = self.deployments.lock().expect("poisoned");
        registry
            .current
            .clone()
            .or_else(|| registry.known_good_deployment().map(|d| d.id.clone()))
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

    /// Account for a trial boot that did not happen.
    ///
    /// GRUB has no boot-assessment counter of its own: `grub-reboot` sets a one-shot default and
    /// nothing counts the attempts, which is why hostd keeps that counter — and why hostd has to
    /// be the thing that advances it. `Grub::decrement` has existed since the boot work landed
    /// and was never called by anything; it carried a `#[allow(dead_code)]` saying as much. The
    /// consequence was a staged candidate that failed to boot at all leaving its counter at its
    /// original value for ever, so "three tries" meant three tries only if each of them got far
    /// enough to run userspace.
    ///
    /// Daemon startup is the one moment this can be observed: a candidate is staged, and the
    /// kernel command line says the machine booted something else. That means an attempt was
    /// spent — GRUB's one-shot default was consumed, the entry failed, and the firmware fell back
    /// — so the counter comes down, and at zero the candidate is abandoned for the known-good
    /// deployment.
    ///
    /// Every failure is reported rather than propagated: this runs before the daemon serves
    /// anything, and a bookkeeping problem must not stop hostd from starting.
    pub fn reconcile_boot(&self) -> BootReconciliation {
        let booted = self.booted_deployment_id();
        // Loader-owned counters do not expose a staged_candidate. The durable transaction
        // and the running kernel identity still establish whether its candidate really booted.
        if let Some(txn) = self.engine.current()
            && matches!(txn.phase, Phase::AwaitingReboot | Phase::Trial)
            && let Some(candidate) = txn.candidate_deployment
            && booted.as_deref() == Some(candidate.as_str())
        {
            if txn.phase == Phase::AwaitingReboot
                && let Err(error) = self.engine.record_trial_boot()
            {
                return BootReconciliation::Unreadable(error.to_string());
            }
            return BootReconciliation::BootedTheCandidate { candidate };
        }
        let staged = match self.boot.staged_candidate() {
            Ok(Some(staged)) => staged,
            Ok(None) => return BootReconciliation::NoCandidate,
            Err(error) => return BootReconciliation::Unreadable(error.to_string()),
        };
        let (candidate, tries) = staged;

        let Some(booted) = booted else {
            // Without knowing what booted, decrementing would be a guess — and the guess is
            // wrong in exactly the common case, the trial boot itself, where it would burn an
            // attempt on a candidate that is running fine.
            return BootReconciliation::CannotTell;
        };
        if booted == candidate {
            return BootReconciliation::BootedTheCandidate { candidate };
        }

        let remaining = match self.boot.consume_try() {
            Ok(Some(remaining)) => remaining,
            Ok(None) => return BootReconciliation::NoCandidate,
            Err(error) => return BootReconciliation::Unreadable(error.to_string()),
        };
        tracing::warn!(
            candidate,
            booted,
            tries,
            remaining,
            "a staged candidate did not boot; one attempt spent"
        );
        if remaining > 0 {
            return BootReconciliation::AttemptSpent {
                candidate,
                remaining,
            };
        }
        match self.abandon_candidate(&candidate) {
            Ok(target) => BootReconciliation::RolledBack {
                candidate,
                to: target,
            },
            Err(error) => BootReconciliation::CouldNotRollBack {
                candidate,
                reason: error.to_string(),
            },
        }
    }

    /// Make the known-good deployment the default again and close the transaction.
    fn abandon_candidate(&self, candidate: &str) -> Result<String> {
        let _maintenance = self.engine.maintenance_guard();
        let known_good = {
            let registry = self.deployments.lock().expect("poisoned");
            registry
                .known_good_deployment()
                .cloned()
                .context("no known-good deployment to return to")?
        };
        self.engine
            .check_boot_target(&self.engine.mounted_deployment(&known_good.subvolume)?)?;
        self.boot.rollback(&known_good)?;
        if let Some(txn) = self.engine.current()
            && txn.phase == Phase::Trial
        {
            self.engine.roll_back(format!(
                "candidate {candidate} exhausted its boot attempts without ever booting"
            ))?;
        }
        Ok(known_good.id)
    }

    /// The deployment id the running kernel was booted from, out of `/proc/cmdline`.
    ///
    /// TemplarOS's own boot entries carry `templaros.deployment=<id>`. Anything else — an entry someone
    /// wrote by hand, a rescue boot, a machine that has never had a deployment staged — yields
    /// `None`, and reconciliation does nothing rather than guessing.
    fn booted_deployment_id(&self) -> Option<String> {
        let cmdline = std::fs::read_to_string(&self.cmdline_path).ok()?;
        booted_deployment_from_cmdline(&cmdline)
    }

    // -- Deployments ----------------------------------------------------------------------------

    pub fn backend_name(&self) -> &'static str {
        self.boot.name()
    }

    pub fn list_deployments(&self) -> Vec<Deployment> {
        self.deployment_registry().deployments
    }

    /// The whole registry — list plus the current/previous/known-good identities.
    pub fn deployment_registry(&self) -> deploy::DeploymentRegistry {
        self.deployments.lock().expect("poisoned").clone()
    }

    fn save_registry(&self, registry: &deploy::DeploymentRegistry) -> Result<()> {
        deploy::save_registry(&self.deployments_path, registry)
    }

    /// Bring the recorded identities into line with what actually booted, and persist the result.
    ///
    /// Called at startup, right after [`Self::reconcile_boot`]: between the two, a machine that
    /// was rolled back by its bootloader while hostd was not running comes back with a registry
    /// that says so, rather than one still claiming the candidate is current.
    pub fn reconcile_registry(&self) -> Result<()> {
        let booted = self.booted_deployment_id();
        let mut registry = self.deployments.lock().expect("poisoned");
        if registry.reconcile_booted(booted.as_deref()) {
            deploy::save_registry(&self.deployments_path, &registry)?;
        }
        Ok(())
    }

    pub fn stage_deployment(&self, id: &str, tries: u8) -> Result<()> {
        let _maintenance = self.engine.maintenance_guard();
        let registry = self.deployments.lock().expect("poisoned");
        let candidate = registry.find(id).context("unknown deployment")?;
        self.engine
            .check_boot_target(&self.engine.mounted_deployment(&candidate.subvolume)?)?;
        self.boot.stage_candidate(candidate, tries)?;
        Ok(())
    }

    /// Bless the running deployment. If it is the candidate of a transaction still in
    /// [`Phase::Trial`], the transaction is promoted and the candidate joins the registry as
    /// current and known-good, with whatever was current before it kept as `previous` — the
    /// pairing ADR-0003 requires, plus the identities nothing else on the machine remembers.
    pub fn mark_good(&self) -> Result<()> {
        let _maintenance = self.engine.maintenance_guard();
        if let Some(previous) = self
            .deployments
            .lock()
            .expect("poisoned")
            .known_good_deployment()
        {
            self.engine
                .check_boot_target(&self.engine.mounted_deployment(&previous.subvolume)?)?;
        }
        self.boot.mark_good()?;
        if let Some(txn) = self.engine.current()
            && txn.phase == Phase::Trial
            && let Some(deployment) = self.engine.candidate_deployment()
        {
            let previous = self.engine.current().and_then(|t| t.previous_deployment);
            let mut registry = self.deployments.lock().expect("poisoned");
            registry.promote(deployment);
            // The transaction recorded what it displaced when it was planned, back when the
            // machine was healthy; prefer that over what the registry happened to hold.
            if let Some(previous) = previous
                && registry.current.as_deref() != Some(previous.as_str())
            {
                registry.previous = Some(previous);
            }
            self.save_registry(&registry)?;
            // Publish registry membership before closing the recoverable Trial journal.
            // A crash in between repeats the idempotent registry promotion.
            self.engine.promote()?;
        }
        Ok(())
    }

    /// Roll back to `id`. If the running deployment was a transaction's candidate still in
    /// [`Phase::Trial`], the transaction is marked rolled back to match, and the registry records
    /// that the machine is on `id` again.
    pub fn rollback(&self, id: &str) -> Result<()> {
        let _maintenance = self.engine.maintenance_guard();
        let previous = {
            let registry = self.deployments.lock().expect("poisoned");
            registry.find(id).cloned().context("unknown deployment")?
        };
        self.engine
            .check_boot_target(&self.engine.mounted_deployment(&previous.subvolume)?)?;
        self.boot.rollback(&previous)?;
        if let Some(txn) = self.engine.current()
            && txn.phase == Phase::Trial
        {
            self.engine
                .roll_back(format!("rolled back to known-good deployment {id}"))?;
        }
        let mut registry = self.deployments.lock().expect("poisoned");
        if registry.current.as_deref() != Some(id) {
            // The rolled-back-from deployment becomes `previous`: it is still on disk, still
            // stageable, and naming it is how "what did I just leave?" gets answered.
            registry.previous = registry.current.take();
            registry.current = Some(id.to_owned());
            self.save_registry(&registry)?;
        }
        Ok(())
    }
}

/// Atomic write-temp-then-rename of a one-line timestamp, matching the journal's own pattern —
/// `templar-boot-health` only cares whether the file is readable, but a timestamp costs nothing and
/// helps a human reading `cat /run/templaros/session-healthy` after the fact.
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
                sysroot: dir.join("root"),
            }),
            commands,
            clock: Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_786_960_800))),
            polkit: Arc::new(AllowAll),
            caller_identity: Arc::new(crate::session_report::FixedCallerIdentity::allow()),
            journal_dir: dir.join("journal"),
            source_root: dir.join("root"),
            deployment_layout: crate::layout::DeploymentLayout::fixture(
                dir.join("deployments"),
                dir.join("esp"),
            ),
            deployments_path: dir.join("deployments.json"),
            sys_root: dir.join("sys"),
            apt_root: dir.join("root"),
            hardware_state_dir: dir.join("var/lib/templaros/hardware"),
            run_dir: dir.join("run/templaros"),
            cmdline_path: dir.join("proc/cmdline"),
        }
    }

    /// The same configuration with the GRUB backend, whose boot counter hostd owns.
    fn grub_config(dir: &Path) -> HostdConfig {
        let mut config = config(dir);
        config.boot = Arc::new(crate::deploy::Grub {
            commands: Arc::new(FakeCommandRunner::default()),
            state_dir: dir.join("grub"),
            boot_root: dir.join("boot"),
            sysroot: dir.to_path_buf(),
        });
        config
    }

    fn write_cmdline(dir: &Path, cmdline: &str) {
        let path = dir.join("proc/cmdline");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, cmdline).unwrap();
    }

    fn deployment(id: &str, known_good: bool) -> Deployment {
        Deployment {
            id: id.to_owned(),
            subvolume: format!("@deployments/{id}"),
            esp_dir: format!("/EFI/templaros/{id}"),
            kernel: None,
            nvidia_modules: None,
            known_good,
            read_only: true,
        }
    }

    #[test]
    fn a_kernel_command_line_names_its_deployment_exactly() {
        assert_eq!(
            booted_deployment_from_cmdline("ro quiet templaros.deployment=cand1 splash").as_deref(),
            Some("cand1")
        );
        // Not a substring search: the answer decides whether a boot attempt is spent.
        assert_eq!(
            booted_deployment_from_cmdline("xtemplaros.deployment=cand1"),
            None
        );
        assert_eq!(
            booted_deployment_from_cmdline("templaros.deployment_backup=cand1"),
            None
        );
        assert_eq!(
            booted_deployment_from_cmdline("templaros.deployment="),
            None
        );
        assert_eq!(booted_deployment_from_cmdline("ro quiet"), None);
    }

    #[test]
    fn nothing_is_counted_when_no_candidate_is_staged() {
        let dir = tempfile::tempdir().unwrap();
        let state = HostdState::new(grub_config(dir.path())).unwrap();
        write_cmdline(dir.path(), "ro templaros.deployment=base1");
        assert_eq!(state.reconcile_boot(), BootReconciliation::NoCandidate);
    }

    #[test]
    fn the_trial_boot_itself_does_not_spend_an_attempt() {
        // The failure mode of guessing: decrementing here would burn an attempt on a candidate
        // that is running perfectly well, every time it booted.
        let dir = tempfile::tempdir().unwrap();
        let state = HostdState::new(grub_config(dir.path())).unwrap();
        state
            .boot
            .stage_candidate(&deployment("cand1", false), 3)
            .unwrap();
        write_cmdline(dir.path(), "ro templaros.deployment=cand1");
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::BootedTheCandidate {
                candidate: "cand1".to_owned()
            }
        );
        assert_eq!(
            state.boot.staged_candidate().unwrap(),
            Some(("cand1".to_owned(), 3)),
            "the counter must not move"
        );
    }

    #[test]
    fn a_candidate_that_did_not_boot_spends_one_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let state = HostdState::new(grub_config(dir.path())).unwrap();
        state
            .boot
            .stage_candidate(&deployment("cand1", false), 3)
            .unwrap();
        write_cmdline(dir.path(), "ro templaros.deployment=base1");
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::AttemptSpent {
                candidate: "cand1".to_owned(),
                remaining: 2
            }
        );
        assert_eq!(
            state.boot.staged_candidate().unwrap(),
            Some(("cand1".to_owned(), 2))
        );
    }

    #[test]
    fn a_candidate_out_of_attempts_is_abandoned_for_the_known_good_deployment() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = grub_config(dir.path());
        config.deployments_path = dir.path().join("deployments.json");
        crate::deploy::save_deployments(
            &config.deployments_path,
            &[deployment("base1", true), deployment("cand1", false)],
        )
        .unwrap();
        let state = HostdState::new(config).unwrap();
        state
            .boot
            .stage_candidate(&deployment("cand1", false), 1)
            .unwrap();
        write_cmdline(dir.path(), "ro templaros.deployment=base1");
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::RolledBack {
                candidate: "cand1".to_owned(),
                to: "base1".to_owned()
            }
        );
        assert_eq!(
            state.boot.staged_candidate().unwrap(),
            None,
            "a rollback clears the counter"
        );
    }

    #[test]
    fn an_unknown_command_line_counts_nothing_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let state = HostdState::new(grub_config(dir.path())).unwrap();
        state
            .boot
            .stage_candidate(&deployment("cand1", false), 3)
            .unwrap();
        write_cmdline(dir.path(), "ro quiet splash");
        assert_eq!(state.reconcile_boot(), BootReconciliation::CannotTell);
        assert_eq!(
            state.boot.staged_candidate().unwrap(),
            Some(("cand1".to_owned(), 3))
        );
    }

    #[test]
    fn systemd_boot_counts_its_own_attempts_and_this_does_nothing() {
        // The loader decrements the +tries suffix itself, before any userspace runs; counting
        // again here would spend two attempts per boot.
        let dir = tempfile::tempdir().unwrap();
        let state = HostdState::new(config(dir.path())).unwrap();
        write_cmdline(dir.path(), "ro templaros.deployment=base1");
        assert_eq!(state.reconcile_boot(), BootReconciliation::NoCandidate);
    }

    fn changes() -> ChangeSet {
        ChangeSet {
            kernel: Some("linux-image-7.2.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            nvidia_userspace: Some("nvidia-driver-580-open".into()),
            firmware: vec![],
            other_packages: vec![],
            archive: None,
        }
    }

    fn with_bootable_root(dir: &Path) {
        std::fs::create_dir_all(dir.join("root/boot")).unwrap();
        std::fs::write(dir.join("root/boot/vmlinuz-x"), b"k").unwrap();
        std::fs::write(dir.join("root/boot/initrd.img-x"), b"i").unwrap();
        std::fs::create_dir_all(dir.join("root/var/lib/dpkg")).unwrap();
        std::fs::write(dir.join("root/var/lib/dpkg/status"), "").unwrap();
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

        write_cmdline(dir, "ro templaros.deployment=base1");
        assert_eq!(state.reconcile_boot(), BootReconciliation::NoCandidate);
        assert_eq!(
            state.current_transaction().unwrap().phase,
            Phase::AwaitingReboot
        );
        drop(state);
        let state = HostdState::new(config(dir)).unwrap();
        write_cmdline(dir, &format!("ro templaros.deployment={id}"));
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::BootedTheCandidate {
                candidate: id.clone()
            }
        );
        assert_eq!(state.current_transaction().unwrap().phase, Phase::Trial);
        // Restarting hostd on the same boot must not advance the trial twice.
        let state = HostdState::new(config(dir)).unwrap();
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::BootedTheCandidate {
                candidate: id.clone()
            }
        );
        assert_eq!(state.current_transaction().unwrap().phase, Phase::Trial);
        state.mark_good().unwrap();

        let txn = state.current_transaction().unwrap();
        assert_eq!(txn.phase, Phase::Promoted);
        let deployments = state.list_deployments();
        assert_eq!(deployments.len(), 1);
        assert!(deployments[0].known_good);

        // And the identities are recorded, not merely the flag: current and known-good are the
        // deployment just promoted, and a machine with no history before it has no previous.
        let registry = state.deployment_registry();
        assert_eq!(registry.current.as_deref(), Some(id.as_str()));
        assert_eq!(registry.known_good.as_deref(), Some(id.as_str()));
        assert_eq!(registry.previous, None);

        // Persisted, not just held: a restart reads them back.
        let restarted = HostdState::new(config(dir)).unwrap();
        assert_eq!(restarted.deployment_registry(), registry);
    }

    #[test]
    fn trial_repeats_registry_publication_before_terminal_journal() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();
        let id = state.plan_update(changes()).unwrap();
        state.drive_pipeline();
        write_cmdline(dir, &format!("ro templaros.deployment={id}"));
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::BootedTheCandidate {
                candidate: id.clone()
            }
        );
        {
            let mut registry = state.deployments.lock().unwrap();
            registry.promote(state.engine.candidate_deployment().unwrap());
            state.save_registry(&registry).unwrap();
        }
        assert_eq!(state.current_transaction().unwrap().phase, Phase::Trial);
        state.mark_good().unwrap();
        assert_eq!(state.current_transaction().unwrap().phase, Phase::Promoted);
        let registry = state.deployment_registry();
        assert_eq!(registry.deployments.len(), 1);
        assert_eq!(registry.current.as_deref(), Some(id.as_str()));
        assert_eq!(registry.known_good.as_deref(), Some(id.as_str()));
        assert_eq!(registry.previous, None);
    }

    #[test]
    fn rollback_rolls_back_the_matching_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        with_bootable_root(dir);
        let state = HostdState::new(config(dir)).unwrap();
        let id = state.plan_update(changes()).unwrap();
        state.drive_pipeline();
        write_cmdline(dir, &format!("ro templaros.deployment={id}"));
        assert_eq!(
            state.reconcile_boot(),
            BootReconciliation::BootedTheCandidate {
                candidate: id.clone()
            }
        );

        // The candidate must exist in the registry to be rolled back to; register it as this
        // deployment's own previous state would have been.
        {
            let mut registry = state.deployments.lock().unwrap();
            registry
                .deployments
                .push(state.engine.candidate_deployment().unwrap());
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

        assert!(!dir.join("run/templaros/compositor-active").exists());
        state.report_compositor_active().unwrap();
        assert!(dir.join("run/templaros/compositor-active").exists());

        assert!(!dir.join("run/templaros/session-healthy").exists());
        state.report_session_healthy().unwrap();
        assert!(dir.join("run/templaros/session-healthy").exists());
    }
}
