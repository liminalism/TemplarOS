//! Staged system and driver updates as one transaction (planning doc 01 §4).
//!
//! 1. download candidate kernel, NVIDIA modules, userspace, firmware;
//! 2. install into a non-default boot deployment;
//! 3. verify module signatures, version matching, initramfs contents;
//! 4. preserve the complete previous deployment;
//! 5. boot the candidate with a watchdog (systemd boot-assessment counter);
//! 6. `lege-boot-health` verifies module loaded, DRM modeset, compositor started, Vulkan/GL probes;
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub changes: ChangeSet,
    pub phase: Phase,
    /// Deployment identifier of the candidate (snapshot / boot entry).
    pub candidate_deployment: Option<String>,
    pub previous_deployment: Option<String>,
    pub log: Vec<String>,
}

impl Transaction {
    pub fn new(id: impl Into<String>, changes: ChangeSet) -> Self {
        Self {
            id: id.into(),
            changes,
            phase: Phase::Planned,
            candidate_deployment: None,
            previous_deployment: None,
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
    /// Where candidate snapshots live (e.g. `<root>/@deployments`).
    deployments_dir: PathBuf,
    /// The hostd-owned ESP directory boot files are copied into.
    esp_root: PathBuf,
    current: Mutex<Option<Transaction>>,
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
        deployments_dir: PathBuf,
        esp_root: PathBuf,
    ) -> Result<Self> {
        let resumed = journal.unresolved()?;
        if let Some(txn) = &resumed {
            tracing::info!(id = %txn.id, phase = ?txn.phase, "resuming transaction from journal");
        }
        Ok(Self {
            journal,
            apt,
            snapshotter,
            boot,
            clock,
            root,
            deployments_dir,
            esp_root,
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

    /// Plan a new transaction. Fails if one is already in flight and not terminal.
    pub fn plan(&self, changes: ChangeSet) -> Result<String> {
        {
            let guard = self.current.lock().expect("poisoned");
            if let Some(existing) = guard.as_ref()
                && !existing.phase.is_terminal()
            {
                bail!(
                    "transaction {} is in phase {:?}",
                    existing.id,
                    existing.phase
                );
            }
        }
        let id = format!("txn-{}", clock::stamp(self.clock.now()));
        let mut txn = Transaction::new(id.clone(), changes);
        txn.advance(Phase::Downloading, "planned")
            .map_err(anyhow::Error::msg)?;
        self.persist(&txn)?;
        *self.current.lock().expect("poisoned") = Some(txn);
        Ok(id)
    }

    fn candidate_dir(&self, id: &str) -> PathBuf {
        self.deployments_dir.join(id)
    }

    /// Drive the current transaction from wherever it is up to [`Phase::AwaitingReboot`].
    /// Idempotent: each phase's side effect runs only if the transaction has not already recorded
    /// moving past it, so this is exactly what a restart calls to resume.
    pub fn run_to_awaiting_reboot(&self) -> Result<()> {
        let Some(mut txn) = self.current() else {
            bail!("no current transaction");
        };

        if txn.phase == Phase::Downloading {
            match self.apt.download(&txn.id, &txn.changes) {
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
            let result = self
                .snapshotter
                .create_rw(&self.root, &candidate)
                .and_then(|()| self.apt.install_into(&txn.id, &txn.changes, &candidate));
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
                .and_then(|()| self.snapshotter.seal_read_only(&candidate))
                .and_then(|()| {
                    deploy::place_boot_files(&candidate, &self.esp_root, &txn.id).map(|_| ())
                })
                .and_then(|()| {
                    let deployment = deploy::Deployment {
                        id: txn.id.clone(),
                        subvolume: candidate.display().to_string(),
                        esp_dir: self
                            .esp_root
                            .join("EFI/legeos")
                            .join(&txn.id)
                            .display()
                            .to_string(),
                        kernel: txn.changes.kernel.clone(),
                        nvidia_modules: txn.changes.nvidia_modules.clone(),
                        known_good: false,
                    };
                    self.boot.stage_candidate(&deployment, 3)
                });
            match result {
                Ok(()) => {
                    txn.advance(Phase::AwaitingReboot, "verified, sealed, staged for boot")
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
        let reason = error.to_string();
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
            subvolume: self.candidate_dir(&id).display().to_string(),
            esp_dir: self
                .esp_root
                .join("EFI/legeos")
                .join(&id)
                .display()
                .to_string(),
            id,
            kernel: txn.changes.kernel,
            nvidia_modules: txn.changes.nvidia_modules,
            known_good: false,
        })
    }

    /// The candidate has booted and `lege-boot-health` is now probing it.
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
    if changes.kernel.is_some() && !deploy::has_bootable_files(&candidate_root.join("boot")) {
        bail!(
            "candidate is missing a kernel image or initramfs under {}/boot",
            candidate_root.display()
        );
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

    fn harness(dir: &Path) -> Harness {
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-7.2.0-generic"), b"kernel").unwrap();
        fs::write(root.join("boot/initrd.img-7.2.0-generic"), b"initrd").unwrap();

        let journal_dir = dir.join("journal");
        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands: commands.clone(),
            esp_root: dir.join("esp"),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_786_960_800)));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt.clone(),
            snapshotter.clone(),
            boot,
            clock,
            root.clone(),
            dir.join("deployments"),
            dir.join("esp"),
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
        }
    }

    #[test]
    fn plan_then_run_reaches_awaiting_reboot_and_stages_boot() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path());
        let id = h.engine.plan(changes()).unwrap();
        h.engine.run_to_awaiting_reboot().unwrap();

        let txn = h.engine.current().unwrap();
        assert_eq!(txn.id, id);
        assert_eq!(txn.phase, Phase::AwaitingReboot);
        assert_eq!(txn.candidate_deployment.as_deref(), Some(id.as_str()));

        assert_eq!(h.apt.calls().len(), 2);
        assert!(
            h.snapshotter
                .sealed
                .lock()
                .unwrap()
                .contains(&h.engine.candidate_dir(&id))
        );
        assert!(
            h.commands
                .calls()
                .iter()
                .any(|c| c.program == "bootctl" && c.args[0] == "set-default")
        );

        // A second transaction cannot start while this one is in flight.
        assert!(h.engine.plan(changes()).is_err());
    }

    #[test]
    fn a_process_restart_resumes_from_staging_without_repeating_download() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(root.join("boot/initrd.img-x"), b"i").unwrap();

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
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt.clone(),
            snapshotter,
            boot,
            clock,
            root,
            dir.join("deployments"),
            dir.join("esp"),
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
    fn resuming_staging_with_a_partially_populated_candidate_directory_succeeds() {
        // The scenario a crash is most likely to land in: Staging is create_rw() followed by a
        // multi-minute apt install, so hostd dying mid-install leaves the candidate directory on
        // disk with the journal still saying Staging. Nothing has sealed that directory or
        // pointed a boot entry at it, so it is safe to discard — and `create_rw` must do exactly
        // that rather than fail with "File exists" the way a raw `btrfs subvolume snapshot` would.
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let root = dir.join("root");
        fs::create_dir_all(root.join("boot")).unwrap();
        fs::write(root.join("boot/vmlinuz-x"), b"k").unwrap();
        fs::write(root.join("boot/initrd.img-x"), b"i").unwrap();

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
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt,
            snapshotter,
            boot,
            clock,
            root,
            dir.join("deployments"),
            dir.join("esp"),
        )
        .unwrap();

        assert_eq!(engine.current().unwrap().phase, Phase::Staging);
        engine.run_to_awaiting_reboot().unwrap();
        assert_eq!(engine.current().unwrap().phase, Phase::AwaitingReboot);
        assert!(
            !candidate.join("stale-partial-file").exists(),
            "the stale leftover must have been discarded, not merged into"
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

        let journal_dir = dir.join("journal");
        let journal = Journal::new(&journal_dir).unwrap();
        let mut txn = Transaction::new("txn-resume-verifying", changes());
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

        // A previous attempt already got as far as writing the ESP entry before crashing.
        let esp = dir.join("esp");
        deploy::place_boot_files(&candidate, &esp, &txn.id).unwrap();
        let entries_dir = esp.join("loader/entries");
        fs::create_dir_all(&entries_dir).unwrap();
        fs::write(
            entries_dir.join(format!("legeos-{}+3.conf", txn.id)),
            "stale entry from the previous attempt\n",
        )
        .unwrap();

        let apt = Arc::new(FakeApt::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let commands = Arc::new(FakeCommandRunner::default());
        let boot = Arc::new(SystemdBoot {
            commands,
            esp_root: esp.clone(),
        });
        let clock = Arc::new(FixedClock(UNIX_EPOCH));
        let engine = Engine::new(
            Journal::new(&journal_dir).unwrap(),
            apt,
            snapshotter,
            boot,
            clock,
            root,
            dir.join("deployments"),
            esp,
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
        h.engine.plan(changes()).unwrap();
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
        h.engine.plan(changes()).unwrap();
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
        let id = h.engine.plan(changes()).unwrap();
        assert_eq!(id, "txn-20260817T100000Z");
    }
}
