//! Boot deployments and rollback. The `BootManager` trait abstracts the two backends decided in
//! ADR-0004: `systemd-boot` (boot-assessment counters + `systemd-bless-boot`) and `grub`
//! (env-block counter maintained by hostd). Root is btrfs subvolumes; a deployment is a read-only
//! snapshot plus a hostd-owned copy of kernel + initrd + entry on the ESP
//! (`distro/deploy/README.md`), placed here rather than left where apt's maintainer scripts put
//! them so Ubuntu's initramfs post-update hooks cannot rewrite the previous entry out from under
//! a deployment that is supposed to be immutable.
//!
//! ADR-0004 is settled (`@legeos.decision.boot-deploy-secure-boot`): bare metal boots through
//! Canonical-signed shim → GRUB → an Ubuntu-signed kernel → Canonical LRM, and the mkosi
//! developer/VM image keeps systemd-boot. Both backends therefore stay, and [`detect_backend`]
//! remains the one place that decides between them — by reading the installation's actual loader
//! rather than assuming which kind of machine this is.

use crate::cmd::CommandRunner;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    /// btrfs subvolume path (e.g. `@deployments/20260817T100000Z`).
    pub subvolume: String,
    /// ESP directory holding this deployment's kernel/initrd/entry.
    pub esp_dir: String,
    pub kernel: Option<String>,
    pub nvidia_modules: Option<String>,
    pub known_good: bool,
}

/// What every backend must provide.
pub trait BootManager: Send + Sync + std::fmt::Debug {
    /// Register `candidate` as the next boot with `tries` attempts before automatic fallback.
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()>;
    /// Mark the currently booted deployment good (called by boot-health via hostd).
    fn mark_good(&self) -> Result<()>;
    /// Make `previous` the default again and drop the candidate's tries.
    fn rollback(&self, previous: &Deployment) -> Result<()>;
    /// Human-readable backend name for `lege doctor`.
    fn name(&self) -> &'static str;

    /// `(candidate id, tries remaining)` for a backend whose boot counter *this daemon* keeps.
    ///
    /// systemd-boot returns `None`: its counter lives in the entry's file name and the loader
    /// decrements it itself, on every attempt, before any userspace runs. GRUB has no equivalent
    /// — `grub-reboot` sets a one-shot default and nothing counts the attempts — which is why
    /// hostd keeps that counter, and why it has to be the thing that advances it.
    fn staged_candidate(&self) -> Result<Option<(String, u8)>> {
        Ok(None)
    }

    /// Record that one trial attempt was spent without a `mark_good`, returning the tries left.
    ///
    /// `None` from a backend that counts its own attempts.
    fn consume_try(&self) -> Result<Option<u8>> {
        Ok(None)
    }
}

/// Copy a candidate deployment's kernel image and initramfs from its snapshot into the
/// hostd-owned ESP directory. Ubuntu-signed kernels only (Type #1 entries, no self-built UKI).
pub fn place_boot_files(candidate_root: &Path, esp_root: &Path, id: &str) -> Result<PathBuf> {
    let dest = esp_root.join("EFI/legeos").join(id);
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let boot = candidate_root.join("boot");
    let kernel = find_entry_matching(&boot, "vmlinuz")
        .with_context(|| format!("candidate has no kernel image under {}", boot.display()))?;
    let initrd = find_entry_matching(&boot, "initrd.img")
        .with_context(|| format!("candidate has no initramfs under {}", boot.display()))?;
    fs::copy(&kernel, dest.join("vmlinuz")).context("copying kernel image")?;
    fs::copy(&initrd, dest.join("initrd.img")).context("copying initramfs")?;
    Ok(dest)
}

/// Whether `dir` has both a kernel image and an initramfs — the minimum for a bootable
/// deployment, checked during transaction verification.
pub fn has_bootable_files(dir: &Path) -> bool {
    find_entry_matching(dir, "vmlinuz").is_some()
        && find_entry_matching(dir, "initrd.img").is_some()
}

fn find_entry_matching(dir: &Path, prefix: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(prefix))
        })
}

/// systemd-boot backend: Boot Loader Spec "automatic boot assessment" — the entry filename
/// carries a `+tries` suffix that systemd-boot itself decrements on every boot attempt, and
/// `systemd-bless-boot` strips it once boot-health has passed.
#[derive(Debug, Clone)]
pub struct SystemdBoot {
    pub commands: Arc<dyn CommandRunner>,
    pub esp_root: PathBuf,
}

impl SystemdBoot {
    fn entry_path(&self, id: &str, tries: Option<u8>) -> PathBuf {
        let name = match tries {
            Some(n) => format!("legeos-{id}+{n}.conf"),
            None => format!("legeos-{id}.conf"),
        };
        self.esp_root.join("loader/entries").join(name)
    }
}

impl BootManager for SystemdBoot {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()> {
        let entry = self.entry_path(&candidate.id, Some(tries));
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent)?;
        }
        // `legeos.deployment=` is what lets userspace answer "which deployment am I running?"
        // after the fact — the question `HostdState::reconcile_boot` has to answer to decide
        // whether a staged candidate ever booted. Nothing else on the command line carries it:
        // the root filesystem label is the same for every deployment by construction.
        let mut body = format!(
            "title LegeOS ({id})\nlinux /EFI/legeos/{id}/vmlinuz\ninitrd /EFI/legeos/{id}/initrd.img\noptions root=LABEL=legeos-root rw legeos.deployment={id}\n",
            id = candidate.id,
        );
        if let Some(modules) = &candidate.nvidia_modules {
            use std::fmt::Write as _;
            let _ = writeln!(body, "# nvidia-modules {modules}");
        }
        fs::write(&entry, body).with_context(|| format!("writing {}", entry.display()))?;
        let file_name = entry
            .file_name()
            .and_then(|n| n.to_str())
            .context("entry path has no file name")?;
        self.commands.run("bootctl", &["set-default", file_name])?;
        Ok(())
    }

    fn mark_good(&self) -> Result<()> {
        self.commands.run("systemd-bless-boot", &["good"])?;
        Ok(())
    }

    fn rollback(&self, previous: &Deployment) -> Result<()> {
        let entry = self.entry_path(&previous.id, None);
        let file_name = entry
            .file_name()
            .and_then(|n| n.to_str())
            .context("entry path has no file name")?;
        self.commands.run("bootctl", &["set-default", file_name])?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "systemd-boot"
    }
}

/// GRUB backend: `GRUB_DEFAULT=saved`. `grub-reboot` only affects the *next* boot, so hostd owns
/// the multi-try counter itself (ADR-0004) in a small state file rather than GRUB's environment
/// block, which Ubuntu's `grub-mkconfig` can rewrite.
#[derive(Debug, Clone)]
pub struct Grub {
    pub commands: Arc<dyn CommandRunner>,
    pub state_dir: PathBuf,
    /// The boot directory (`/boot`), where the generated entries file lives. Parameterized so
    /// tests never write to the developer's real one.
    pub boot_root: PathBuf,
}

impl Grub {
    fn counter_path(&self) -> PathBuf {
        self.state_dir.join("grub-tries")
    }

    /// `(candidate id, tries remaining)`, if a candidate is currently staged.
    pub fn staged(&self) -> Result<Option<(String, u8)>> {
        match fs::read_to_string(self.counter_path()) {
            Ok(content) => {
                let mut lines = content.lines();
                let id = lines.next().context("empty grub-tries state file")?;
                let tries: u8 = lines
                    .next()
                    .context("grub-tries state file missing tries count")?
                    .parse()
                    .context("grub-tries count is not a number")?;
                Ok(Some((id.to_owned(), tries)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// One more failed trial boot happened without a `mark_good`. Returns the tries remaining
    /// after decrementing (0 means the caller should roll back).
    ///
    /// Driven by `HostdState::reconcile_boot` at daemon startup: the only moment userspace can
    /// observe "a candidate was staged, and this is not it".
    pub fn decrement(&self) -> Result<u8> {
        let Some((id, tries)) = self.staged()? else {
            bail!("no candidate is staged");
        };
        let remaining = tries.saturating_sub(1);
        fs::write(self.counter_path(), format!("{id}\n{remaining}\n"))?;
        if remaining > 0 {
            self.commands.run("grub-reboot", &[&id])?;
        }
        Ok(remaining)
    }
}

impl BootManager for Grub {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        fs::write(
            self.counter_path(),
            format!("{}\n{}\n", candidate.id, tries),
        )?;
        // The entries have to exist before `grub-reboot` names one of them: `GRUB_DEFAULT=saved`
        // selects a menu entry by id, and an id GRUB cannot find is a boot into whatever the
        // firmware picks instead. Written whole, so the known-good entry (which
        // `refresh_entries` reads from this backend's own record) survives every restage.
        self.refresh_entries(Some(candidate))?;
        self.commands.run("grub-reboot", &[&candidate.id])?;
        Ok(())
    }

    fn mark_good(&self) -> Result<()> {
        let Some((id, _)) = self.staged()? else {
            bail!("no candidate is staged to promote");
        };
        self.record_known_good(&id)?;
        self.refresh_entries(None)?;
        self.commands.run("grub-set-default", &[&id])?;
        let _ = fs::remove_file(self.counter_path());
        Ok(())
    }

    fn rollback(&self, previous: &Deployment) -> Result<()> {
        // The candidate's entry goes away with the candidate: leaving it in the menu invites a
        // person at the console to hand-pick the deployment that just failed its health gate.
        self.record_known_good(&previous.id)?;
        self.refresh_entries(None)?;
        self.commands.run("grub-set-default", &[&previous.id])?;
        let _ = fs::remove_file(self.counter_path());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "grub"
    }

    fn staged_candidate(&self) -> Result<Option<(String, u8)>> {
        self.staged()
    }

    fn consume_try(&self) -> Result<Option<u8>> {
        self.decrement().map(Some)
    }
}

/// The known-deployment registry as persisted at `<state-dir>/deployments.json`.
///
/// The three identities are held beside the list rather than derived from it because only one of
/// them can be recovered from the machine afterwards: `known_good` is a property of a deployment
/// (and stays mirrored on it, so an old reader still sees it), but *current* and *previous* are
/// facts about this installation's history — which deployment the update engine last promoted,
/// and which one it displaced — and nothing on disk remembers them once the transaction journal's
/// entry ages out. `/proc/cmdline` says what booted, never what the alternative was.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRegistry {
    #[serde(default)]
    pub deployments: Vec<Deployment>,
    /// The deployment most recently promoted — what a healthy machine is running.
    #[serde(default)]
    pub current: Option<String>,
    /// The deployment `current` displaced: the boot-side rollback target.
    #[serde(default)]
    pub previous: Option<String>,
    /// The deployment that has passed boot-health and may always be returned to.
    #[serde(default)]
    pub known_good: Option<String>,
}

/// Either shape `deployments.json` can have on disk: the object written since the identities
/// landed, or the bare array every installation before it wrote. A registry file is not something
/// an update may invalidate — the file that says where to roll back to must survive being read by
/// the version that finds it — so the older form is read, not rejected.
#[derive(Deserialize)]
#[serde(untagged)]
enum PersistedRegistry {
    Modern(DeploymentRegistry),
    Legacy(Vec<Deployment>),
}

impl DeploymentRegistry {
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Deployment> {
        self.deployments.iter().find(|d| d.id == id)
    }

    /// The known-good deployment, preferring the recorded identity and falling back to the flag on
    /// the deployments themselves — which is all a registry migrated from the legacy array has.
    #[must_use]
    pub fn known_good_deployment(&self) -> Option<&Deployment> {
        self.known_good
            .as_deref()
            .and_then(|id| self.find(id))
            .or_else(|| self.deployments.iter().find(|d| d.known_good))
    }

    /// Record `deployment` as promoted: it becomes current and known-good, and whatever was
    /// current before it becomes the rollback target.
    ///
    /// Promoting the deployment that is already current is not a no-op only in the boring sense:
    /// it must not make a deployment its own predecessor, which would name a rollback target that
    /// is the very thing being rolled back from.
    pub fn promote(&mut self, deployment: Deployment) {
        if self.current.as_deref() != Some(deployment.id.as_str()) {
            self.previous = self.current.take();
        }
        self.current = Some(deployment.id.clone());
        self.known_good = Some(deployment.id.clone());
        for existing in &mut self.deployments {
            existing.known_good = false;
        }
        match self.deployments.iter_mut().find(|d| d.id == deployment.id) {
            Some(existing) => {
                *existing = Deployment {
                    known_good: true,
                    ..deployment
                };
            }
            None => self.deployments.push(Deployment {
                known_good: true,
                ..deployment
            }),
        }
    }

    /// Reconcile the recorded identities against what the machine actually booted.
    ///
    /// Returns whether anything changed, so a startup that learned nothing does not rewrite the
    /// file. A registry written by an older version has no identities at all; a machine that was
    /// rolled back by the bootloader itself, or provisioned outside the update engine, boots
    /// something the registry never called current. In both cases the booted deployment is the
    /// truth and the registry follows it, keeping the displaced identity as `previous`.
    pub fn reconcile_booted(&mut self, booted: Option<&str>) -> bool {
        let mut changed = false;
        if self.known_good.is_none()
            && let Some(flagged) = self.deployments.iter().find(|d| d.known_good)
        {
            self.known_good = Some(flagged.id.clone());
            changed = true;
        }
        let Some(booted) = booted else {
            return changed;
        };
        if self.current.as_deref() == Some(booted) {
            return changed;
        }
        if self.current.is_some() {
            self.previous = self.current.take();
        }
        self.current = Some(booted.to_owned());
        true
    }
}

/// Load the known-deployment registry (`<state-dir>/deployments.json`) — empty if it does not
/// exist yet (a fresh install has no deployments beyond the one it booted from).
pub fn load_registry(path: &Path) -> Result<DeploymentRegistry> {
    match fs::read(path) {
        Ok(bytes) => {
            let persisted: PersistedRegistry = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(match persisted {
                PersistedRegistry::Modern(registry) => registry,
                PersistedRegistry::Legacy(deployments) => {
                    let known_good = deployments
                        .iter()
                        .find(|d| d.known_good)
                        .map(|d| d.id.clone());
                    DeploymentRegistry {
                        deployments,
                        current: None,
                        previous: None,
                        known_good,
                    }
                }
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DeploymentRegistry::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Persist the registry atomically (write-temp-then-rename, matching the transaction journal).
pub fn save_registry(path: &Path, registry: &DeploymentRegistry) -> Result<()> {
    write_atomically(path, &serde_json::to_vec_pretty(registry)?)
}

/// Write the legacy bare-array form. Kept so the reader above has something to be tested against
/// that was not produced by the writer that shares its assumptions.
#[cfg(test)]
pub fn save_deployments(path: &Path, deployments: &[Deployment]) -> Result<()> {
    write_atomically(path, &serde_json::to_vec_pretty(deployments)?)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))
}

/// Where the ESP is mounted under `root` — `boot/efi` on Ubuntu's default layout, `efi` on a
/// minimal mkosi image. The one place this is decided, so [`detect_backend`] and the transaction
/// engine's boot-file placement can never disagree about it.
pub fn esp_root(root: &Path) -> PathBuf {
    for candidate in ["boot/efi", "efi"] {
        let esp = root.join(candidate);
        if esp.join("loader/loader.conf").exists() {
            return esp;
        }
    }
    root.join("boot/efi")
}

/// Select the backend for this installation (ADR-0004). systemd-boot when its loader is present
/// under the given root's ESP, GRUB otherwise. `root` is parameterized so tests never touch `/`.
pub fn detect_backend(
    root: &Path,
    commands: Arc<dyn CommandRunner>,
    state_dir: PathBuf,
) -> Box<dyn BootManager> {
    let esp = esp_root(root);
    if esp.join("loader/loader.conf").exists() {
        Box::new(SystemdBoot {
            commands,
            esp_root: esp,
        })
    } else {
        Box::new(Grub {
            commands,
            state_dir,
            boot_root: root.join("boot"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeCommandRunner;

    fn deployment(id: &str) -> Deployment {
        Deployment {
            id: id.to_owned(),
            subvolume: format!("@deployments/{id}"),
            esp_dir: format!("EFI/legeos/{id}"),
            kernel: Some("linux-image-7.2.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            known_good: false,
        }
    }

    #[test]
    fn detect_backend_prefers_systemd_boot_when_loader_conf_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("boot/efi/loader")).unwrap();
        fs::write(dir.path().join("boot/efi/loader/loader.conf"), "").unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = detect_backend(dir.path(), commands, dir.path().join("state"));
        assert_eq!(backend.name(), "systemd-boot");
    }

    #[test]
    fn detect_backend_falls_back_to_grub() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = detect_backend(dir.path(), commands, dir.path().join("state"));
        assert_eq!(backend.name(), "grub");
    }

    #[test]
    fn esp_root_agrees_with_detect_backend_on_a_minimal_efi_layout() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("efi/loader")).unwrap();
        fs::write(dir.path().join("efi/loader/loader.conf"), "").unwrap();

        assert_eq!(esp_root(dir.path()), dir.path().join("efi"));

        let commands = Arc::new(FakeCommandRunner::default());
        let backend = detect_backend(dir.path(), commands, dir.path().join("state"));
        assert_eq!(backend.name(), "systemd-boot");
    }

    #[test]
    fn esp_root_defaults_to_boot_efi_when_nothing_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(esp_root(dir.path()), dir.path().join("boot/efi"));
    }

    #[test]
    fn systemd_boot_writes_a_tries_suffixed_entry_and_sets_default() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = SystemdBoot {
            commands: commands.clone(),
            esp_root: dir.path().to_path_buf(),
        };
        backend.stage_candidate(&deployment("cand1"), 3).unwrap();
        let entry = dir.path().join("loader/entries/legeos-cand1+3.conf");
        assert!(entry.exists());
        let body = fs::read_to_string(&entry).unwrap();
        assert!(body.contains("linux /EFI/legeos/cand1/vmlinuz"));

        let calls = commands.calls();
        assert_eq!(calls[0].program, "bootctl");
        assert_eq!(calls[0].args, ["set-default", "legeos-cand1+3.conf"]);

        backend.mark_good().unwrap();
        assert_eq!(commands.calls()[1].program, "systemd-bless-boot");

        backend.rollback(&deployment("prev1")).unwrap();
        assert_eq!(
            commands.calls()[2].args,
            ["set-default", "legeos-prev1.conf"]
        );
    }

    #[test]
    fn grub_owns_its_own_tries_counter() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = Grub {
            commands: commands.clone(),
            state_dir: dir.path().to_path_buf(),
            boot_root: dir.path().join("boot"),
        };
        backend.stage_candidate(&deployment("cand1"), 3).unwrap();
        assert_eq!(backend.staged().unwrap(), Some(("cand1".to_owned(), 3)));
        assert_eq!(commands.calls()[0].args, ["cand1"]);

        assert_eq!(backend.decrement().unwrap(), 2);
        assert_eq!(backend.staged().unwrap(), Some(("cand1".to_owned(), 2)));

        backend.mark_good().unwrap();
        assert_eq!(backend.staged().unwrap(), None);
        assert_eq!(commands.calls().last().unwrap().program, "grub-set-default");
    }

    #[test]
    fn grub_rollback_clears_the_counter() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = Grub {
            commands: commands.clone(),
            state_dir: dir.path().to_path_buf(),
            boot_root: dir.path().join("boot"),
        };
        backend.stage_candidate(&deployment("cand1"), 1).unwrap();
        backend.rollback(&deployment("prev1")).unwrap();
        assert!(backend.staged().unwrap().is_none());
        assert_eq!(
            commands.calls().last().unwrap().args,
            vec!["prev1".to_owned()]
        );
    }

    #[test]
    fn place_boot_files_copies_kernel_and_initrd() {
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("candidate/boot");
        fs::create_dir_all(&candidate).unwrap();
        fs::write(candidate.join("vmlinuz-7.2.0-generic"), b"kernel").unwrap();
        fs::write(candidate.join("initrd.img-7.2.0-generic"), b"initrd").unwrap();

        assert!(has_bootable_files(&candidate));

        let esp = dir.path().join("esp");
        let placed = place_boot_files(&dir.path().join("candidate"), &esp, "cand1").unwrap();
        assert!(placed.join("vmlinuz").exists());
        assert!(placed.join("initrd.img").exists());
    }

    #[test]
    fn place_boot_files_fails_without_a_kernel() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("candidate/boot")).unwrap();
        let esp = dir.path().join("esp");
        assert!(place_boot_files(&dir.path().join("candidate"), &esp, "cand1").is_err());
    }

    #[test]
    fn deployment_registry_round_trips_and_defaults_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deployments.json");
        assert!(load_registry(&path).unwrap().deployments.is_empty());

        let deployments = vec![deployment("a"), deployment("b")];
        save_deployments(&path, &deployments).unwrap();
        assert_eq!(load_registry(&path).unwrap().deployments, deployments);
    }

    #[test]
    fn a_legacy_array_registry_still_loads_with_its_known_good_recovered() {
        // The file that says where to roll back to must survive being read by the version that
        // finds it: the bare array every installation before the identities landed wrote, read
        // back with `known_good` recovered from the flag on the deployment itself.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deployments.json");
        let mut good = deployment("a");
        good.known_good = true;
        save_deployments(&path, &[good.clone(), deployment("b")]).unwrap();

        let registry = load_registry(&path).unwrap();
        assert_eq!(registry.known_good.as_deref(), Some("a"));
        assert_eq!(registry.known_good_deployment(), Some(&good));
        assert_eq!(registry.current, None, "a legacy file records no history");

        // And the object form round-trips whole.
        save_registry(&path, &registry).unwrap();
        assert_eq!(load_registry(&path).unwrap(), registry);
    }
}
