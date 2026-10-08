//! Boot deployments and rollback. The `BootManager` trait abstracts the two backends decided in
//! ADR-0004: `systemd-boot` (boot-assessment counters + `systemd-bless-boot`) and `grub`
//! (env-block counter maintained by hostd). Root is btrfs subvolumes; a deployment is a read-only
//! snapshot plus a hostd-owned copy of kernel + initrd + entry on the ESP
//! (`distro/deploy/README.md`), placed here rather than left where apt's maintainer scripts put
//! them so Ubuntu's initramfs post-update hooks cannot rewrite the previous entry out from under
//! a deployment that is supposed to be immutable.
//!
//! ADR-0004 is settled (`@templaros.decision.boot-deploy-secure-boot`): bare metal boots through
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
    #[serde(default)]
    pub read_only: bool,
}

/// What every backend must provide.
pub trait BootManager: Send + Sync + std::fmt::Debug {
    /// Register `candidate` as the next boot with `tries` attempts before automatic fallback.
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()>;
    /// Mark the currently booted deployment good (called by boot-health via hostd).
    fn mark_good(&self) -> Result<()>;
    /// Make `previous` the default again and drop the candidate's tries.
    fn rollback(&self, previous: &Deployment) -> Result<()>;
    /// Human-readable backend name for `templar doctor`.
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
    if !crate::boot_entries::is_valid_deployment_id(id) { bail!("unsafe deployment id"); }
    let dest = esp_root.join("EFI/templaros").join(id);
    fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let boot = candidate_root.join("boot");
    let (kernel, initrd) = matching_kernel_pair(&boot).with_context(|| {
        format!(
            "candidate has no matching kernel/initramfs pair under {}",
            boot.display()
        )
    })?;
    fs::copy(&kernel, dest.join("vmlinuz")).context("copying kernel image")?;
    fs::copy(&initrd, dest.join("initrd.img")).context("copying initramfs")?;
    Ok(dest)
}

/// Whether `dir` has both a kernel image and an initramfs — the minimum for a bootable
/// deployment, checked during transaction verification.
pub fn has_bootable_files(dir: &Path) -> bool {
    matching_kernel_pair(dir).is_some()
}

pub(crate) fn matching_kernel_pair(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    fs::read_dir(dir).ok()?.filter_map(Result::ok).filter_map(|entry| {
        let path = entry.path();
        let version = path.file_name()?.to_str()?.strip_prefix("vmlinuz-")?;
        let initrd = dir.join(format!("initrd.img-{version}"));
        initrd.is_file().then_some((path, initrd))
    }).max_by(|(left, _), (right, _)| {
        kernel_version_cmp(left.file_name().unwrap().to_str().unwrap(), right.file_name().unwrap().to_str().unwrap())
    })
}

fn kernel_version_cmp(mut left: &str, mut right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    while !left.is_empty() && !right.is_empty() {
        let a = left.as_bytes()[0];
        let b = right.as_bytes()[0];
        if a.is_ascii_digit() && b.is_ascii_digit() {
            let a_end = left.bytes().take_while(u8::is_ascii_digit).count();
            let b_end = right.bytes().take_while(u8::is_ascii_digit).count();
            let a_number = left[..a_end].trim_start_matches('0');
            let b_number = right[..b_end].trim_start_matches('0');
            let order = a_number.len().cmp(&b_number.len()).then_with(|| a_number.cmp(b_number));
            if order != Ordering::Equal { return order; }
            left = &left[a_end..];
            right = &right[b_end..];
        } else {
            let a = left.chars().next().unwrap();
            let b = right.chars().next().unwrap();
            let order = a.cmp(&b);
            if order != Ordering::Equal { return order; }
            left = &left[a.len_utf8()..];
            right = &right[b.len_utf8()..];
        }
    }
    left.len().cmp(&right.len())
}

/// systemd-boot backend: Boot Loader Spec "automatic boot assessment" — the entry filename
/// carries a `+tries` suffix that systemd-boot itself decrements on every boot attempt, and
/// `systemd-bless-boot` strips it once boot-health has passed.
#[derive(Debug, Clone)]
pub struct SystemdBoot {
    pub commands: Arc<dyn CommandRunner>,
    pub esp_root: PathBuf,
    pub sysroot: PathBuf,
}

impl SystemdBoot {
    fn entry_path(&self, id: &str, tries: Option<u8>) -> PathBuf {
        let name = match tries {
            Some(n) => format!("templaros-{id}+{n}.conf"),
            None => format!("templaros-{id}.conf"),
        };
        self.esp_root.join("loader/entries").join(name)
    }
}

impl BootManager for SystemdBoot {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()> {
        if !crate::boot_entries::is_valid_deployment_id(&candidate.id)
            || !crate::layout::safe_subvolume(&candidate.subvolume) { bail!("unsafe boot identity"); }
        let entry = self.entry_path(&candidate.id, Some(tries));
        if let Some(parent) = entry.parent() {
            fs::create_dir_all(parent)?;
        }
        let root = crate::boot_entries::RootSpec::read(&self.sysroot).cmdline_arg();
        let mut body = format!(
            "title TemplarOS ({id})\nlinux /EFI/templaros/{id}/vmlinuz\ninitrd /EFI/templaros/{id}/initrd.img\noptions {root} rootflags=subvol={subvolume} {mode} templaros.deployment={id}\n",
            id = candidate.id,
            subvolume = candidate.subvolume,
            mode = if candidate.read_only { "ro" } else { "rw" },
        );
        if let Some(modules) = &candidate.nvidia_modules {
            use std::fmt::Write as _;
            let _ = writeln!(body, "# nvidia-modules {modules}");
        }
        write_atomically(&entry, body.as_bytes()).with_context(|| format!("writing {}", entry.display()))?;
        // systemd-boot normalizes boot counters out of the selectable entry ID.
        // Keep +tries in the file name, but select the stable ID the loader advertises.
        let stable_id = format!("templaros-{}.conf", candidate.id);
        let esp = format!("--esp-path={}", self.esp_root.display());
        self.commands.run("bootctl", &[&esp, "set-default", &stable_id])?;
        Ok(())
    }

    fn mark_good(&self) -> Result<()> {
        self.commands.run("/usr/lib/systemd/systemd-bless-boot", &["good"])?;
        Ok(())
    }

    fn rollback(&self, previous: &Deployment) -> Result<()> {
        if !crate::boot_entries::is_valid_deployment_id(&previous.id)
            || !crate::layout::safe_subvolume(&previous.subvolume) { bail!("unsafe rollback boot identity"); }
        let entry = self.entry_path(&previous.id, None);
        let file_name = entry
            .file_name()
            .and_then(|n| n.to_str())
            .context("entry path has no file name")?;
        let esp = format!("--esp-path={}", self.esp_root.display());
        self.commands.run("bootctl", &[&esp, "set-default", file_name])?;
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
    /// The system root (`/`), where `etc/templaros/install.toml` records this installation's
    /// filesystem identity. Parameterized for the same reason as `boot_root`.
    pub sysroot: PathBuf,
}

impl Grub {
    fn select_entry(&self, command: &str, id: &str) -> Result<()> {
        let directory = format!("--boot-directory={}", self.boot_root.display());
        self.commands.run(command, &[&directory, id])?;
        Ok(())
    }
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
        let text = fs::read_to_string(self.counter_path())?;
        let identity = text.lines().skip(2).collect::<Vec<_>>().join("\n");
        write_atomically(&self.counter_path(), format!("{id}\n{remaining}\n{identity}\n").as_bytes())?;
        if remaining > 0 {
            self.select_entry("grub-reboot", &id)?;
        }
        Ok(remaining)
    }
}

impl BootManager for Grub {
    fn stage_candidate(&self, candidate: &Deployment, tries: u8) -> Result<()> {
        fs::create_dir_all(&self.state_dir)?;
        write_atomically(&self.counter_path(),
            format!("{}\n{}\n{}\n{}\n", candidate.id, tries, candidate.subvolume, candidate.read_only).as_bytes())?;
        // The entries have to exist before `grub-reboot` names one of them: `GRUB_DEFAULT=saved`
        // selects a menu entry by id, and an id GRUB cannot find is a boot into whatever the
        // firmware picks instead. Written whole, so the known-good entry (which
        // `refresh_entries` reads from this backend's own record) survives every restage.
        self.refresh_entries(Some(candidate))?;
        self.select_entry("grub-reboot", &candidate.id)?;
        Ok(())
    }

    fn mark_good(&self) -> Result<()> {
        let Some((id, _)) = self.staged()? else {
            let booted = fs::read_to_string(self.sysroot.join("proc/cmdline"))?;
            if self.known_good_entry()?.is_some_and(|entry| {
                booted.split_whitespace().any(|word| word.strip_prefix("templaros.deployment=") == Some(entry.id.as_str()))
            }) { return Ok(()); }
            bail!("no candidate is staged to promote");
        };
        let subvolume = fs::read_to_string(self.counter_path())?
            .lines()
            .nth(2)
            .unwrap_or("@")
            .to_owned();
        self.record_known_good(&Deployment {
            id: id.clone(),
            subvolume,
            esp_dir: String::new(),
            kernel: None,
            nvidia_modules: None,
            known_good: true,
            read_only: fs::read_to_string(self.counter_path())?.lines().nth(3) == Some("true"),
        })?;
        self.refresh_entries(None)?;
        self.select_entry("grub-set-default", &id)?;
        match fs::remove_file(self.counter_path()) {
            Ok(()) => crate::persistence::sync_parent(&self.counter_path())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn rollback(&self, previous: &Deployment) -> Result<()> {
        // The candidate's entry goes away with the candidate: leaving it in the menu invites a
        // person at the console to hand-pick the deployment that just failed its health gate.
        self.record_known_good(previous)?;
        self.refresh_entries(None)?;
        self.select_entry("grub-set-default", &previous.id)?;
        match fs::remove_file(self.counter_path()) {
            Ok(()) => crate::persistence::sync_parent(&self.counter_path())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRegistry {
    #[serde(default = "crate::persistence::schema", deserialize_with = "crate::persistence::supported_schema")]
    pub schema: u32,
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

impl Default for DeploymentRegistry {
    fn default() -> Self {
        Self { schema: crate::persistence::SCHEMA, deployments: Vec::new(),
            current: None, previous: None, known_good: None }
    }
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
                        schema: crate::persistence::SCHEMA,
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
    if registry.schema != crate::persistence::SCHEMA { bail!("refusing unsupported registry schema"); }
    // Reading first rejects a newer file even if a caller supplies an older in-memory value.
    load_registry(path)?;
    crate::persistence::backup_before_write(path)?;
    write_atomically(path, &serde_json::to_vec_pretty(registry)?)
}

/// Write the legacy bare-array form. Kept so the reader above has something to be tested against
/// that was not produced by the writer that shares its assumptions.
#[cfg(test)]
pub fn save_deployments(path: &Path, deployments: &[Deployment]) -> Result<()> {
    write_atomically(path, &serde_json::to_vec_pretty(deployments)?)
}

pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    crate::persistence::sync_parent(path)
}

/// Select the backend for this installation (ADR-0004). systemd-boot when its loader is present
/// under the given root's ESP, GRUB otherwise. `root` is parameterized so tests never touch `/`.
pub fn detect_backend(
    root: &Path,
    commands: Arc<dyn CommandRunner>,
    state_dir: PathBuf,
    read_only: bool,
    esp: PathBuf,
) -> Box<dyn BootManager> {
    if esp.join("loader/loader.conf").exists() {
        Box::new(SystemdBoot {
            commands,
            esp_root: esp,
            sysroot: root.to_path_buf(),
        })
    } else {
        Box::new(Grub {
            commands,
            state_dir,
            boot_root: if read_only { esp.join("EFI/templaros") } else { root.join("boot") },
            sysroot: root.to_path_buf(),
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
            esp_dir: format!("EFI/templaros/{id}"),
            kernel: Some("linux-image-7.2.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            known_good: false,
            read_only: true,
        }
    }

    #[test]
    fn detect_backend_prefers_systemd_boot_when_loader_conf_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("boot/efi/loader")).unwrap();
        fs::write(dir.path().join("boot/efi/loader/loader.conf"), "").unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let layout = crate::layout::DeploymentLayout::read(dir.path()).unwrap();
        let backend = detect_backend(dir.path(), commands, dir.path().join("state"), layout.read_only, layout.mounted_esp);
        assert_eq!(backend.name(), "systemd-boot");
    }

    #[test]
    fn detect_backend_falls_back_to_grub() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let layout = crate::layout::DeploymentLayout::read(dir.path()).unwrap();
        let backend = detect_backend(dir.path(), commands, dir.path().join("state"), layout.read_only, layout.mounted_esp);
        assert_eq!(backend.name(), "grub");
    }


    #[test]
    fn grub_owns_its_own_tries_counter() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = Grub {
            commands: commands.clone(),
            state_dir: dir.path().to_path_buf(),
            boot_root: dir.path().join("boot"),
            sysroot: dir.path().to_path_buf(),
        };
        backend.stage_candidate(&deployment("cand1"), 3).unwrap();
        assert_eq!(backend.staged().unwrap(), Some(("cand1".to_owned(), 3)));
        assert_eq!(commands.calls()[0].args, [format!("--boot-directory={}", dir.path().join("boot").display()), "cand1".into()]);

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
            sysroot: dir.path().to_path_buf(),
        };
        backend.stage_candidate(&deployment("cand1"), 1).unwrap();
        backend.rollback(&deployment("prev1")).unwrap();
        assert!(backend.staged().unwrap().is_none());
        assert_eq!(
            commands.calls().last().unwrap().args,
            vec![format!("--boot-directory={}", dir.path().join("boot").display()), "prev1".to_owned()]
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
    fn place_boot_files_selects_one_matching_kernel_pair() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("candidate/boot");
        fs::create_dir_all(&boot).unwrap();
        fs::write(boot.join("vmlinuz-7.1"), b"old-kernel").unwrap();
        fs::write(boot.join("initrd.img-7.1"), b"old-initrd").unwrap();
        fs::write(boot.join("vmlinuz-7.2"), b"new-kernel").unwrap();
        fs::write(boot.join("initrd.img-9.9"), b"mismatched").unwrap();

        let dest = place_boot_files(
            &dir.path().join("candidate"),
            &dir.path().join("esp"),
            "cand1",
        )
        .unwrap();
        assert_eq!(fs::read(dest.join("vmlinuz")).unwrap(), b"old-kernel");
        assert_eq!(fs::read(dest.join("initrd.img")).unwrap(), b"old-initrd");
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

    #[test]
    fn future_registry_is_not_overwritten_and_legacy_upgrade_keeps_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deployments.json");
        let future = br#"{"schema":2,"deployments":[]}"#;
        fs::write(&path, future).unwrap();
        assert!(load_registry(&path).is_err());
        assert!(save_registry(&path, &DeploymentRegistry::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), future);
        fs::write(&path, b"[]").unwrap();
        save_registry(&path, &load_registry(&path).unwrap()).unwrap();
        assert_eq!(fs::read(path.with_extension("pre-schema-1")).unwrap(), b"[]");
    }

    #[test]
    fn boot_pair_uses_numeric_kernel_order_and_requires_matching_initrd() {
        let dir = tempfile::tempdir().unwrap();
        for version in ["7.2.0-9", "7.2.0-10"] {
            fs::write(dir.path().join(format!("vmlinuz-{version}")), b"kernel").unwrap();
            fs::write(dir.path().join(format!("initrd.img-{version}")), b"initrd").unwrap();
        }
        fs::write(dir.path().join("vmlinuz-99.0"), b"unpaired").unwrap();
        assert_eq!(matching_kernel_pair(dir.path()).unwrap().0.file_name().unwrap(), "vmlinuz-7.2.0-10");
    }
}
