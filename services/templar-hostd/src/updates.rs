//! Staged system and driver updates as one transaction (planning doc 01 §4).
//!
//! 1. download candidate kernel, NVIDIA modules, userspace, firmware;
//! 2. install into a non-default boot deployment;
//! 3. verify module signatures, version matching, initramfs contents;
//! 4. preserve the complete previous deployment;
//! 5. boot the candidate with a watchdog (systemd boot-assessment counter);
//! 6. `templar-boot-health` verifies module loaded, DRM modeset, compositor started, Vulkan/GL probes;
//! 7. promote only after validation (`systemd-bless-boot`);
//! 8. roll back automatically otherwise.
//!
//! [`Engine`] is the resumable state machine: every phase transition is written to the on-disk
//! [`crate::journal::Journal`] before the in-memory state changes, and [`Engine::run_to_awaiting_reboot`]
//! only performs a phase's side effect when the transaction has not already recorded moving past
//! it — so calling it again after a restart resumes mid-apt rather than repeating work (ADR-0003).
//! apt runs as a child transient unit ([`crate::apt`]), never as an in-process library.

use crate::{apt::Apt, clock, clock::Clock, deploy, journal::Journal, snapshot::Snapshotter};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Transaction phases. Transitions are validated by [`Transaction::advance`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Planned,
    Downloading,
    Staging,
    Verifying,
    AwaitingReboot,
    Trial,
    Promoted,
    RolledBack,
    Failed,
}

impl Phase {
    /// The legal next phases from `self`.
    pub fn successors(self) -> &'static [Phase] {
        match self {
            Self::Planned => &[Self::Downloading, Self::Failed],
            Self::Downloading => &[Self::Staging, Self::Failed],
            Self::Staging => &[Self::Verifying, Self::Failed],
            Self::Verifying => &[Self::AwaitingReboot, Self::Failed],
            Self::AwaitingReboot => &[Self::Trial, Self::Failed],
            Self::Trial => &[Self::Promoted, Self::RolledBack],
            Self::Promoted | Self::RolledBack | Self::Failed => &[],
        }
    }

    pub fn is_terminal(self) -> bool {
        self.successors().is_empty()
    }
}

/// What is being changed together (kernel + modules + userspace as one tested set).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSet {
    pub kernel: Option<String>,
    pub nvidia_modules: Option<String>,
    pub nvidia_userspace: Option<String>,
    pub firmware: Vec<String>,
    pub other_packages: Vec<String>,
    #[serde(default)]
    pub archive: Option<String>,
}

/// Why a transaction failed, when the reason is one a person can act on differently.
///
/// Only distinctions that change the advice are worth making: "the disk filled up" means delete
/// something and try again, and it is the one failure a staged deployment can cause *by
/// succeeding* at everything else — the snapshot and the install are what consume the space.
/// Everything else is `Other`, described by the log line rather than by a taxonomy nobody
/// maintains.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureClass {
    #[default]
    Other,
    /// The volume ran out of space while staging the candidate.
    DiskExhausted,
}

impl FailureClass {
    /// Classify a staging error. Matches on the message as well as the `io::Error` kind because
    /// the failure usually arrives from `apt` as an exit status with its output attached, not as a
    /// Rust I/O error at all.
    #[must_use]
    pub fn of(error: &anyhow::Error) -> Self {
        let by_kind = error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
            .any(|io| io.raw_os_error() == Some(libc::ENOSPC));
        if by_kind {
            return Self::DiskExhausted;
        }
        let text = format!("{error:#}").to_ascii_lowercase();
        if text.contains("no space left on device")
            || text.contains("insufficient free space")
            || text.contains("not enough free disk space")
        {
            return Self::DiskExhausted;
        }
        Self::Other
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentIdentity {
    mount: PathBuf,
    subvolume: String,
    esp_mount: PathBuf,
    esp_dir: String,
    read_only: bool,
}

fn identity(layout: &crate::layout::DeploymentLayout, id: &str) -> DeploymentIdentity {
    DeploymentIdentity {
        mount: layout.candidate_mount(id),
        subvolume: layout.subvolume(id),
        esp_mount: layout.mounted_esp.clone(),
        esp_dir: layout.esp_identity(id),
        read_only: layout.read_only,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    #[serde(
        default = "crate::persistence::schema",
        deserialize_with = "crate::persistence::supported_schema"
    )]
    pub schema: u32,
    pub id: String,
    pub changes: ChangeSet,
    #[serde(default)]
    pub deployment_identity: Option<DeploymentIdentity>,
    pub phase: Phase,
    /// Deployment identifier of the candidate (snapshot / boot entry).
    pub candidate_deployment: Option<String>,
    pub previous_deployment: Option<String>,
    /// Set when `phase` is [`Phase::Failed`]. `#[serde(default)]` so a journal written before
    /// classification existed still loads — a resume must never be blocked by its own history.
    #[serde(default)]
    pub failure: Option<FailureClass>,
    pub log: Vec<String>,
}

impl Transaction {
    pub fn new(id: impl Into<String>, changes: ChangeSet) -> Self {
        Self {
            schema: crate::persistence::SCHEMA,
            id: id.into(),
            changes,
            deployment_identity: None,
            phase: Phase::Planned,
            candidate_deployment: None,
            previous_deployment: None,
            failure: None,
            log: Vec::new(),
        }
    }

    /// Move to `next` if legal, recording why.
    pub fn advance(&mut self, next: Phase, note: impl Into<String>) -> Result<(), String> {
        if !self.phase.successors().contains(&next) {
            return Err(format!("illegal transition {:?} → {:?}", self.phase, next));
        }
        self.log
            .push(format!("{:?} → {:?}: {}", self.phase, next, note.into()));
        self.phase = next;
        Ok(())
    }
}

/// The resumable transaction state machine. Everything that touches the system is a trait object
/// so the whole pipeline runs against fakes in tests (rule 4).
#[derive(Debug)]
pub struct Engine {
    journal: Journal,
    apt: Arc<dyn Apt>,
    snapshotter: Arc<dyn Snapshotter>,
    boot: Arc<dyn deploy::BootManager>,
    clock: Arc<dyn Clock>,
    /// Source root snapshotted into each candidate (`/` on a live system).
    root: PathBuf,
    layout: crate::layout::DeploymentLayout,
    current: Mutex<Option<Transaction>>,
    mutation: parking_lot::Mutex<()>,
}

impl Engine {
    /// Loads whatever unresolved transaction the journal remembers — the resume path runs
    /// automatically every time hostd starts, not just after an explicit crash.
    pub fn new(
        journal: Journal,
        apt: Arc<dyn Apt>,
        snapshotter: Arc<dyn Snapshotter>,
        boot: Arc<dyn deploy::BootManager>,
        clock: Arc<dyn Clock>,
        root: PathBuf,
        layout: crate::layout::DeploymentLayout,
    ) -> Result<Self> {
        let mut resumed = journal.unresolved()?;
        if let Some(txn) = &mut resumed {
            let expected = identity(&layout, &txn.id);
            match &txn.deployment_identity {
                Some(recorded) if recorded != &expected => bail!(
                    "deployment layout changed during {}; refusing to reinterpret a staged root",
                    txn.id
                ),
                Some(_) => {}
                None if matches!(txn.phase, Phase::Downloading | Phase::Staging) => {
                    txn.deployment_identity = Some(expected);
                    journal.save(txn)?;
                }
                None => bail!(
                    "legacy staged transaction lacks a boot identity; reviewed recovery is required"
                ),
            }
            tracing::info!(id = %txn.id, phase = ?txn.phase, "resuming transaction from journal");
        }
        Ok(Self {
            journal,
            apt,
            snapshotter,
            boot,
            clock,
            root,
            layout,
            mutation: parking_lot::Mutex::new(()),
            current: Mutex::new(resumed),
        })
    }

    pub fn current(&self) -> Option<Transaction> {
        self.current.lock().expect("poisoned").clone()
    }

    /// Every transaction the journal remembers — the durable "why did that happen" record for
    /// updates (planning doc 01 §7's system-wide change history, for this domain).
    pub fn history(&self) -> Result<Vec<Transaction>> {
        self.journal.list()
    }

    /// Look up one past (or current) transaction by id.
    pub fn transaction(&self, id: &str) -> Result<Option<Transaction>> {
        self.journal.load(id)
    }

    fn persist(&self, txn: &Transaction) -> Result<()> {
        self.journal.save(txn)
    }

    pub fn with_maintenance<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self.mutation.lock();
        operation()
    }

    pub fn mounted_deployment(&self, subvolume: &str) -> Result<PathBuf> {
        self.layout.mount_for_subvolume(subvolume)
    }
    pub fn root_read_only(&self) -> bool {
        self.layout.read_only
    }
    pub fn check_boot_target(&self, target: &Path) -> Result<()> {
        crate::accounts::ensure_compatible(&self.root, target)?;
        self.layout.check_readers(&self.root, target)
    }
    pub fn maintenance_guard(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.mutation.lock()
    }
    /// Plan a new transaction, recording the deployment it would displace.
    ///
    /// The predecessor is recorded at planning time, not at promotion time, because the moment it
    /// is needed — a candidate that failed to boot, or a staging failure with a full disk — is
    /// after the machine has stopped cooperating. A rollback target worked out from the registry
    /// at that point is a target worked out from state the failure may have been about; one
    /// written down when everything was still healthy is a fact.
    ///
    /// Fails if a transaction is already in flight and not terminal.
    pub fn plan_from(
        &self,
        changes: ChangeSet,
        previous_deployment: Option<String>,
    ) -> Result<String> {
        let mut guard = self.current.lock().expect("poisoned");
        if let Some(existing) = guard.as_ref()
            && !existing.phase.is_terminal()
        {
            bail!(
                "transaction {} is in phase {:?}",
                existing.id,
                existing.phase
            );
        }
        // Keep the guard through persistence: concurrent planners cannot replace each other.
        let stamp = clock::stamp(self.clock.now());
        let mut id = format!("txn-{stamp}");
        let mut collision = 0u64;
        while self.journal.load(&id)?.is_some() {
            collision += 1;
            id = format!("txn-{stamp}-{collision}");
        }
        let mut txn = Transaction::new(id.clone(), changes);
        txn.deployment_identity = Some(identity(&self.layout, &id));
        txn.previous_deployment = previous_deployment;
        txn.advance(Phase::Downloading, "planned")
            .map_err(anyhow::Error::msg)?;
        self.persist(&txn)?;
        *guard = Some(txn);
        Ok(id)
    }

    fn candidate_dir(&self, id: &str) -> PathBuf {
        self.layout.candidate_mount(id)
    }

    /// Drive the current transaction from wherever it is up to [`Phase::AwaitingReboot`].
    /// Idempotent: each phase's side effect runs only if the transaction has not already recorded
    /// moving past it, so this is exactly what a restart calls to resume.
    pub fn run_to_awaiting_reboot(&self) -> Result<()> {
        let _mutation = self.mutation.lock();
        let Some(mut txn) = self.current() else {
            bail!("no current transaction");
        };

        if txn.phase == Phase::Downloading {
            let candidate = self.candidate_dir(&txn.id);
            let result = self
                .snapshotter
                .create_rw(&self.root, &candidate)
                .and_then(|()| self.apt.download(&txn.id, &txn.changes, &candidate));
            match result {
                Ok(()) => {
                    txn.advance(Phase::Staging, "apt download complete")
                        .map_err(anyhow::Error::msg)?;
                    self.persist(&txn)?;
                }
                Err(error) => return self.fail(txn, error),
            }
        }

        if txn.phase == Phase::Staging {
            let candidate = self.candidate_dir(&txn.id);
            // Downloading already owns the snapshot/cache. A resume continues
            // dpkg inside that candidate, never merges new files into the live root.
            let result = if candidate.exists() {
                self.apt.install_into(&txn.id, &txn.changes, &candidate)
            } else {
                self.snapshotter
                    .create_rw(&self.root, &candidate)
                    .and_then(|()| self.apt.install_into(&txn.id, &txn.changes, &candidate))
            };
            match result {
                Ok(()) => {
                    txn.candidate_deployment = Some(txn.id.clone());
                    txn.advance(Phase::Verifying, "installed into candidate snapshot")
                        .map_err(anyhow::Error::msg)?;
                    self.persist(&txn)?;
                }
                Err(error) => return self.fail(txn, error),
            }
        }

        if txn.phase == Phase::Verifying {
            // Resuming here re-runs all four steps from the top, same as Staging — each is
            // safe to repeat:
            //   - `verify_candidate` only reads; it has no side effect to repeat.
            //   - `seal_read_only` sets a btrfs property to `true`; setting an already-true
            //     property again is a no-op success, not an error.
            //   - `place_boot_files` creates the ESP directory (`create_dir_all`, already
            //     idempotent) and copies the kernel/initrd with `fs::copy`, which overwrites an
            //     existing destination file rather than failing on it.
            //   - `stage_candidate` writes the boot entry with `fs::write` (same overwrite
            //     semantics) and then re-issues `bootctl set-default` / `grub-reboot` for the
            //     same candidate id, both of which succeed when re-run with the same argument.
            // None of the four needs the `create_rw`-style "delete first" treatment: unlike a
            // btrfs subvolume snapshot, none of these operations refuses to write over what is
            // already there.
            let candidate = self.candidate_dir(&txn.id);
            let result = verify_candidate(&candidate, &txn.changes)
                .and_then(|()| crate::accounts::ensure_compatible(&self.root, &candidate))
                .and_then(|()| self.layout.check_readers(&self.root, &candidate))
                .and_then(|()| crate::drivers::derive_candidate_lock(&candidate))
                .and_then(|()| self.layout.preserve_gpu_mode(&self.root))
                .and_then(|()| self.layout.prepare_candidate(&candidate, &txn.id))
                .and_then(|()| {
                    deploy::place_boot_files(&candidate, &self.layout.mounted_esp, &txn.id)
                        .map(|_| ())
                })
                .and_then(|()| {
                    if self.layout.read_only {
                        self.snapshotter.seal_read_only(&candidate)
                    } else {
                        Ok(())
                    }
                })
                .and_then(|()| {
                    let deployment = deploy::Deployment {
                        id: txn.id.clone(),
                        subvolume: self.layout.subvolume(&txn.id),
                        esp_dir: self.layout.esp_identity(&txn.id),
                        kernel: txn.changes.kernel.clone(),
                        nvidia_modules: txn.changes.nvidia_modules.clone(),
                        known_good: false,
                        read_only: self.layout.read_only,
                    };
                    self.boot.stage_candidate(&deployment, 3)
                });
            match result {
                Ok(()) => {
                    txn.advance(
                        Phase::AwaitingReboot,
                        "verified, root policy applied, boot artifacts placed, staged for boot",
                    )
                    .map_err(anyhow::Error::msg)?;
                    self.persist(&txn)?;
                }
                Err(error) => return self.fail(txn, error),
            }
        }

        *self.current.lock().expect("poisoned") = Some(txn);
        Ok(())
    }

    fn fail(&self, mut txn: Transaction, error: anyhow::Error) -> Result<()> {
        let class = FailureClass::of(&error);
        let mut reason = error.to_string();
        if class == FailureClass::DiskExhausted {
            // The one thing a person needs told at this moment is that nothing was lost: the
            // deployment they are running was never touched, and it is still what boots. Naming
            // it turns "the update failed" into "you are still on X, free some space".
            let previous = txn.previous_deployment.clone();
            reason = match &previous {
                Some(previous) => format!(
                    "{reason} — out of disk space while staging; nothing was changed and \
                     deployment {previous} still boots"
                ),
                None => format!(
                    "{reason} — out of disk space while staging; nothing was changed and the \
                     running deployment is unaffected"
                ),
            };
        }
        txn.failure = Some(class);
        txn.advance(Phase::Failed, reason)
            .map_err(anyhow::Error::msg)?;
        self.persist(&txn)?;
        *self.current.lock().expect("poisoned") = Some(txn);
        Err(error.context("transaction failed"))
    }

    /// Reconstructs the [`deploy::Deployment`] the engine most recently staged, if any — used to
    /// register it in the deployment registry once boot-health blesses it.
    pub fn candidate_deployment(&self) -> Option<deploy::Deployment> {
        let txn = self.current()?;
        let id = txn.candidate_deployment?;
        Some(deploy::Deployment {
            subvolume: self.layout.subvolume(&id),
            esp_dir: self.layout.esp_identity(&id),
            id,
            kernel: txn.changes.kernel,
            nvidia_modules: txn.changes.nvidia_modules,
            known_good: false,
            read_only: self.layout.read_only,
        })
    }

    /// The candidate has booted and `templar-boot-health` is now probing it.
    pub fn record_trial_boot(&self) -> Result<()> {
        self.transition(Phase::Trial, "candidate booted, boot-health probing")
    }

    /// Boot-health passed — call after [`deploy::BootManager::mark_good`] succeeds.
    pub fn promote(&self) -> Result<()> {
        self.transition(Phase::Promoted, "boot-health passed, blessed")
    }

    /// Boot-health failed, or the boot counter ran out — call after
    /// [`deploy::BootManager::rollback`] succeeds.
    pub fn roll_back(&self, reason: impl Into<String>) -> Result<()> {
        self.transition(Phase::RolledBack, reason)
    }

    fn transition(&self, phase: Phase, note: impl Into<String>) -> Result<()> {
        let mut guard = self.current.lock().expect("poisoned");
        let Some(txn) = guard.as_mut() else {
            bail!("no current transaction");
        };
        txn.advance(phase, note).map_err(anyhow::Error::msg)?;
        self.persist(txn)?;
        Ok(())
    }
}

/// Verify module signatures, version matching and initramfs contents (planning doc 01 §4 step 3).
/// The signature check is a structural proxy, not a cryptographic one: it confirms no DKMS-built
/// module shadows the LRM-signed ones (the actual invariant "never DKMS" needs), not that every
/// `.ko`'s embedded signature verifies — that needs `modinfo`/kernel keyring inspection this
/// crate does not yet do.
fn verify_candidate(candidate_root: &Path, changes: &ChangeSet) -> Result<()> {
    if !deploy::has_bootable_files(&candidate_root.join("boot")) {
        bail!(
            "candidate is missing a kernel image or initramfs under {}/boot",
            candidate_root.display()
        );
    }
    if candidate_root
        .join(crate::layout::INSTALLED_LAYOUT)
        .exists()
    {
        let packages =
            templar_hwdetect::dpkg::scan_status(&candidate_root.join("var/lib/dpkg/status"));
        for requested in changes.package_list() {
            let (name, version) = requested
                .split_once('=')
                .map_or((requested.as_str(), None), |(name, version)| {
                    (name, Some(version))
                });
            let name = name.split_once(':').map_or(name, |(name, _)| name);
            if !packages.iter().any(|package| {
                package.name == name
                    && package.installed
                    && version.is_none_or(|version| package.version == version)
            }) {
                bail!("candidate did not install requested package/version {requested}");
            }
        }
    }

    if let (Some(modules), Some(userspace)) = (&changes.nvidia_modules, &changes.nvidia_userspace) {
        let module_branch = nvidia_branch(modules);
        let userspace_branch = nvidia_branch(userspace);
        if module_branch.is_none() || module_branch != userspace_branch {
            bail!(
                "nvidia module/userspace version mismatch: {modules} (branch {module_branch:?}) \
                 vs {userspace} (branch {userspace_branch:?}) — kernel and NVIDIA must move as a pair"
            );
        }
    }

    if changes.nvidia_modules.is_some() {
        let modules_dir = candidate_root.join("lib/modules");
        if any_path_contains(&modules_dir, "dkms") {
            bail!(
                "candidate has DKMS-built nvidia modules under {} — \
                 the open/proprietary flavours come from Canonical-signed LRM only, never DKMS",
                modules_dir.display()
            );
        }
    }
    Ok(())
}

/// The numeric driver branch embedded in an apt package name (e.g. `580` in
/// `linux-modules-nvidia-580-open-generic` and in `nvidia-driver-580-open`).
fn nvidia_branch(package: &str) -> Option<&str> {
    package
        .split('-')
        .find(|segment| !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
}

fn any_path_contains(root: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    let mut stack: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    while let Some(path) = stack.pop() {
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().contains(needle))
        {
            return true;
        }
        if path.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            stack.extend(entries.filter_map(Result::ok).map(|e| e.path()));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        apt::FakeApt, clock::FixedClock, cmd::FakeCommandRunner, deploy::SystemdBoot,
        snapshot::FakeSnapshotter,
    };
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn happy_path_and_rollback_are_legal_and_shortcuts_are_not() {
        let mut t = Transaction::new(
            "t1",
            ChangeSet {
                kernel: Some("7.0.0-14-generic".into()),
                ..Default::default()
            },
        );
        for (phase, note) in [
            (Phase::Downloading, "apt-get download"),
            (Phase::Staging, "into snapshot"),
            (Phase::Verifying, "signatures ok"),
            (Phase::AwaitingReboot, "entry +3"),
            (Phase::Trial, "booted"),
        ] {
            t.advance(phase, note).unwrap();
        }
        assert!(t.advance(Phase::Promoted, "bless-boot").is_ok());
        assert!(t.phase.is_terminal());
        assert_eq!(t.log.len(), 6);

        let mut u = Transaction::new("t2", ChangeSet::default());
        assert!(u.advance(Phase::Promoted, "skip everything").is_err());
        u.advance(Phase::Downloading, "").unwrap();
        u.advance(Phase::Failed, "mirror down").unwrap();
        assert!(u.advance(Phase::Staging, "").is_err());
    }

    struct Harness {
        engine: Engine,
        apt: Arc<FakeApt>,
        snapshotter: Arc<FakeSnapshotter>,
        commands: Arc<FakeCommandRunner>,
        root: PathBuf,
        journal_dir: PathBuf,
    }

    fn package_metadata(root: &Path) {
        fs::create_dir_all(root.join("var/lib/dpkg")).unwrap();
        fs::write(root.join("var/lib/dpkg/status"), "").unwrap();
    }

    fn harness(dir: &Path) -> Harness {
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-7.2.0-generic"), b"kernel").unwrap();
        fs::write(root.join("boot/initrd.img-7.2.0-generic"), b"initrd").unwrap();
        package_metadata(&root);

        let journal_dir = dir.join("journal");
        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands: commands.clone(),
            esp_root: dir.join("esp"),
            sysroot: root.clone(),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_786_960_800)));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt.clone(),
            snapshotter.clone(),
            boot,
            clock,
            root.clone(),
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();
        Harness {
            engine,
            apt,
            snapshotter,
            commands,
            root,
            journal_dir,
        }
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

    #[test]
    fn plan_then_run_reaches_awaiting_reboot_and_stages_boot() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let id = h.engine.plan_from(changes(), None).unwrap();
        h.engine.run_to_awaiting_reboot().unwrap();

        let txn = h.engine.current().unwrap();
        assert_eq!(txn.id, id);
        assert_eq!(txn.phase, Phase::AwaitingReboot);
        assert_eq!(txn.candidate_deployment.as_deref(), Some(id.as_str()));
        let lock = crate::drivers::read_version_lock(
            &h.engine
                .candidate_dir(&id)
                .join("etc/templaros/hardware/version-lock.toml"),
        )
        .unwrap();
        assert_eq!(lock.kernel.as_deref(), Some("7.2.0-generic"));
        assert_eq!(lock.nvidia_lrm, None);
        assert_eq!(lock.nvidia_userspace, None);

        // A second transaction cannot start while this one is in flight.
        assert!(h.engine.plan_from(changes(), None).is_err());
    }

    #[test]
    fn a_process_restart_resumes_from_staging_without_repeating_download() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(root.join("boot/initrd.img-x"), b"i").unwrap();
        package_metadata(&root);

        let journal_dir = dir.join("journal");
        let journal = Journal::new(&journal_dir).unwrap();
        let mut txn = Transaction::new("txn-resume-staging", changes());
        txn.advance(Phase::Downloading, "planned").unwrap();
        txn.advance(Phase::Staging, "apt download complete")
            .unwrap();
        journal.save(&txn).unwrap();

        // A brand-new process: nothing has called `plan`, so a fresh FakeApt with zero calls
        // recorded proves resume does not re-download.
        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands,
            esp_root: dir.join("esp"),
            sysroot: root.clone(),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt.clone(),
            snapshotter,
            boot,
            clock,
            root,
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();

        assert_eq!(engine.current().unwrap().phase, Phase::Staging);
        engine.run_to_awaiting_reboot().unwrap();

        assert_eq!(engine.current().unwrap().phase, Phase::AwaitingReboot);
        assert_eq!(
            apt.calls().len(),
            1,
            "download must not run again after resume"
        );
        assert!(matches!(
            apt.calls()[0],
            crate::apt::AptCall::Install { .. }
        ));

        // The journal on disk reflects the resumed progress too.
        let reloaded = journal.load("txn-resume-staging").unwrap().unwrap();
        assert_eq!(reloaded.phase, Phase::AwaitingReboot);
    }

    #[test]
    fn resuming_an_incomplete_candidate_refuses_boot_without_merging_the_live_root() {
        // Downloading already owns this candidate. A failed/interrupted install
        // must not discard its cache or copy a newer live root into it; if the
        // resumed install leaves it without boot images, validation fails closed.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(root.join("boot/initrd.img-x"), b"i").unwrap();
        package_metadata(&root);

        let journal_dir = dir.join("journal");
        let journal = Journal::new(&journal_dir).unwrap();
        let mut txn = Transaction::new("txn-resume-staging-partial", changes());
        txn.advance(Phase::Downloading, "planned").unwrap();
        txn.advance(Phase::Staging, "apt download complete")
            .unwrap();
        journal.save(&txn).unwrap();

        let candidate = dir.join("deployments").join(&txn.id);
        fs::create_dir_all(&candidate).unwrap();
        fs::write(
            candidate.join("stale-partial-file"),
            b"leftover from the crashed attempt",
        )
        .unwrap();

        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands,
            esp_root: dir.join("esp"),
            sysroot: root.clone(),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt,
            snapshotter,
            boot,
            clock,
            root,
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();

        assert_eq!(engine.current().unwrap().phase, Phase::Staging);
        assert!(engine.run_to_awaiting_reboot().is_err());
        assert_eq!(engine.current().unwrap().phase, Phase::Failed);
        assert_eq!(
            fs::read(candidate.join("stale-partial-file")).unwrap(),
            b"leftover from the crashed attempt",
        );
        assert_eq!(fs::read(engine.root.join("boot/vmlinuz-x")).unwrap(), b"k");
        assert_eq!(
            fs::read(engine.root.join("boot/initrd.img-x")).unwrap(),
            b"i"
        );
    }

    #[test]
    fn resuming_verifying_with_already_written_boot_files_succeeds() {
        // The same crash-recovery question one phase later: Verifying re-runs seal_read_only,
        // place_boot_files and stage_candidate from the top too. All three must tolerate their
        // own prior output already being there (see the comment on the Verifying block in
        // `run_to_awaiting_reboot`) rather than erroring on it.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(root.join("boot/initrd.img-x"), b"i").unwrap();
        package_metadata(&root);

        let journal_dir = dir.join("journal");
        let journal = Journal::new(&journal_dir).unwrap();
        let mut txn = Transaction::new("txn-resume-verifying", changes());
        txn.deployment_identity = Some(identity(
            &crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
            &txn.id,
        ));
        txn.advance(Phase::Downloading, "planned").unwrap();
        txn.advance(Phase::Staging, "apt download complete")
            .unwrap();
        txn.candidate_deployment = Some(txn.id.clone());
        txn.advance(Phase::Verifying, "installed into candidate snapshot")
            .unwrap();
        journal.save(&txn).unwrap();

        // The candidate snapshot itself, already installed and bootable.
        let candidate = dir.join("deployments").join(&txn.id);
        fs::create_dir_all(candidate.join("boot")).unwrap();
        fs::write(candidate.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(candidate.join("boot/initrd.img-x"), b"i").unwrap();
        package_metadata(&candidate);

        // A previous attempt already got as far as writing the ESP entry before crashing.
        let esp = dir.join("esp");
        deploy::place_boot_files(&candidate, &esp, &txn.id).unwrap();
        let entries_dir = esp.join("loader/entries");
        fs::create_dir_all(&entries_dir).unwrap();
        fs::write(
            entries_dir.join(format!("templaros-{}+3.conf", txn.id)),
            "stale entry from the previous attempt\n",
        )
        .unwrap();

        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands,
            esp_root: esp.clone(),
            sysroot: root.clone(),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt,
            snapshotter,
            boot,
            clock,
            root,
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), esp),
        )
        .unwrap();

        assert_eq!(engine.current().unwrap().phase, Phase::Verifying);
        engine.run_to_awaiting_reboot().unwrap();
        assert_eq!(engine.current().unwrap().phase, Phase::AwaitingReboot);
    }

    #[test]
    fn resuming_an_already_awaiting_reboot_transaction_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        h.engine.plan_from(changes(), None).unwrap();
        h.engine.run_to_awaiting_reboot().unwrap();
        let before = h.apt.calls().len();

        // Resuming again (as a fresh process would after another restart) must be a no-op.
        h.engine.run_to_awaiting_reboot().unwrap();
        assert_eq!(h.apt.calls().len(), before);
        assert_eq!(h.engine.current().unwrap().phase, Phase::AwaitingReboot);
        let _ = h.journal_dir;
        let _ = h.root;
    }

    #[test]
    fn a_failed_download_moves_to_failed_and_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        h.apt
            .fail_download
            .store(true, std::sync::atomic::Ordering::SeqCst);
        h.engine.plan_from(changes(), None).unwrap();
        assert!(h.engine.run_to_awaiting_reboot().is_err());
        let txn = h.engine.current().unwrap();
        assert_eq!(txn.phase, Phase::Failed);
        assert!(txn.log.last().unwrap().contains("apt download failed"));
    }

    #[test]
    fn verification_rejects_mismatched_nvidia_versions() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("boot")).unwrap();
        fs::write(dir.path().join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(dir.path().join("boot/initrd.img-x"), b"i").unwrap();
        let mismatched = ChangeSet {
            kernel: None,
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            nvidia_userspace: Some("nvidia-driver-575-open".into()),
            firmware: vec![],
            other_packages: vec![],
            archive: None,
        };
        let error = verify_candidate(dir.path(), &mismatched).unwrap_err();
        assert!(error.to_string().contains("mismatch"));
    }

    #[test]
    fn verification_rejects_dkms_shadow_modules() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("boot")).unwrap();
        fs::write(dir.path().join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(dir.path().join("boot/initrd.img-x"), b"i").unwrap();
        fs::create_dir_all(dir.path().join("lib/modules/7.2.0/updates/dkms")).unwrap();
        let with_nvidia = ChangeSet {
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            ..Default::default()
        };
        let error = verify_candidate(dir.path(), &with_nvidia).unwrap_err();
        assert!(error.to_string().contains("DKMS"));
    }

    #[test]
    fn plan_ids_are_clock_derived_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let id = h.engine.plan_from(changes(), None).unwrap();
        assert_eq!(id, "txn-20260817T100000Z");
    }

    /// The trial phase is the one a restart can land in for hours: the candidate is booted and
    /// boot-health has not yet said anything. Both ways out of it have to work in a process that
    /// did not plan the transaction — which is the only kind of process there is by then, since
    /// deciding a trial requires the reboot that ended the planning one.
    #[test]
    fn a_restart_during_the_trial_can_still_promote_or_roll_back() {
        for (promote, expected) in [(true, Phase::Promoted), (false, Phase::RolledBack)] {
            let dir = tempfile::tempdir().unwrap();
            let dir = dir.path();
            let journal_dir = dir.join("journal");

            // What the process that rebooted the machine left behind.
            {
                let journal = Journal::new(&journal_dir).unwrap();
                let mut txn = Transaction::new("txn-trial", changes());
                txn.deployment_identity = Some(identity(
                    &crate::layout::DeploymentLayout::fixture(
                        dir.join("deployments"),
                        dir.join("esp"),
                    ),
                    &txn.id,
                ));
                txn.previous_deployment = Some("txn-earlier".to_owned());
                for (phase, note) in [
                    (Phase::Downloading, "planned"),
                    (Phase::Staging, "downloaded"),
                    (Phase::Verifying, "installed"),
                    (Phase::AwaitingReboot, "staged"),
                    (Phase::Trial, "candidate booted"),
                ] {
                    txn.advance(phase, note).unwrap();
                }
                txn.candidate_deployment = Some("txn-trial".to_owned());
                journal.save(&txn).unwrap();
            }

            // A brand-new process over the same journal.
            let resumed = harness_over(dir, &journal_dir);
            let recovered = resumed.engine.current().expect("the trial is resumed");
            assert_eq!(recovered.phase, Phase::Trial);
            assert_eq!(
                recovered.previous_deployment.as_deref(),
                Some("txn-earlier"),
                "the displaced deployment survives the restart that has to name it"
            );

            if promote {
                resumed.engine.promote().unwrap();
            } else {
                resumed.engine.roll_back("boot-health failed").unwrap();
            }

            // And a third process reads the outcome, not the trial.
            let after = harness_over(dir, &journal_dir);
            assert_eq!(after.engine.current(), None, "a decided trial is resolved");
            let txn = after.engine.transaction("txn-trial").unwrap().unwrap();
            assert_eq!(txn.phase, expected);
            assert_eq!(txn.previous_deployment.as_deref(), Some("txn-earlier"));
        }
    }

    /// A `Harness` over an existing journal directory — a restart, rather than a fresh machine.
    fn harness_over(dir: &Path, journal_dir: &Path) -> Harness {
        let mut h = harness(dir);
        h.engine = Engine::new(
            Journal::new(journal_dir).unwrap(),
            h.apt.clone(),
            h.snapshotter.clone(),
            Arc::new(SystemdBoot {
                commands: h.commands.clone(),
                esp_root: dir.join("esp"),
                sysroot: h.root.clone(),
            }),
            Arc::new(FixedClock(UNIX_EPOCH)),
            h.root.clone(),
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();
        h.journal_dir = journal_dir.to_path_buf();
        h
    }

    /// An apt that fills the disk instead of installing — the failure that arrives as an exit
    /// status with output attached rather than as a Rust I/O error.
    #[derive(Debug)]
    struct FullDiskApt;

    impl Apt for FullDiskApt {
        fn download(&self, _id: &str, _changes: &ChangeSet, _target_root: &Path) -> Result<()> {
            Ok(())
        }

        fn install_into(&self, _id: &str, _changes: &ChangeSet, _target: &Path) -> Result<()> {
            bail!(
                "apt-get exited 100: E: You don't have enough free space in /var/cache/apt/archives/.\n\
                 No space left on device"
            )
        }
    }

    #[test]
    fn running_out_of_disk_while_staging_is_classified_and_names_where_the_machine_still_is() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let h = harness(dir);
        let engine = Engine::new(
            Journal::new(dir.join("journal")).unwrap(),
            Arc::new(FullDiskApt),
            h.snapshotter.clone(),
            Arc::new(SystemdBoot {
                commands: h.commands.clone(),
                esp_root: dir.join("esp"),
                sysroot: h.root.clone(),
            }),
            Arc::new(FixedClock(UNIX_EPOCH)),
            h.root.clone(),
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();

        engine
            .plan_from(changes(), Some("txn-earlier".to_owned()))
            .unwrap();
        let error = engine.run_to_awaiting_reboot().unwrap_err();
        assert!(format!("{error:#}").contains("No space left on device"));

        let txn = engine.current().unwrap();
        assert_eq!(txn.phase, Phase::Failed);
        assert_eq!(
            txn.failure,
            Some(FailureClass::DiskExhausted),
            "a full disk is not just any failure: it is the one with an obvious next step"
        );
        let last = txn.log.last().expect("the failure is logged");
        assert!(
            last.contains("txn-earlier"),
            "the failure must say where the machine still is: {last}"
        );

        // And the classification is durable, because the advice is needed after the restart the
        // person does when they think the machine is broken.
        let after = Engine::new(
            Journal::new(dir.join("journal")).unwrap(),
            Arc::new(FullDiskApt),
            h.snapshotter.clone(),
            Arc::new(SystemdBoot {
                commands: h.commands.clone(),
                esp_root: dir.join("esp"),
                sysroot: h.root.clone(),
            }),
            Arc::new(FixedClock(UNIX_EPOCH)),
            h.root.clone(),
            crate::layout::DeploymentLayout::fixture(dir.join("deployments"), dir.join("esp")),
        )
        .unwrap();
        let recorded = after.transaction(&txn.id).unwrap().unwrap();
        assert_eq!(recorded.failure, Some(FailureClass::DiskExhausted));
    }
}
