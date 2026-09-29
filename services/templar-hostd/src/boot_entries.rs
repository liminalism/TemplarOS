//! GRUB boot entries that carry the deployment id.
//!
//! ADR-0004 resolved the bare-metal Secure-Boot posture to Canonical-signed shim → Canonical-signed
//! GRUB → Ubuntu-signed kernel → Canonical-signed matching NVIDIA LRM. That path has no TemplarOS
//! signing infrastructure in it, which is the whole point — but it also means GRUB, not
//! systemd-boot, is what reads a menu on the reference laptop, and GRUB has no equivalent of the
//! Boot Loader Specification entries hostd already writes for systemd-boot.
//!
//! So hostd writes them. `/boot/grub/templaros-entries.cfg` is generated whole on every stage,
//! promotion and rollback, and `/etc/grub.d/09_templaros` (shipped as `distro/deploy/grub.d/09_templaros`)
//! makes `grub.cfg` `source` it at boot. That indirection is deliberate: regenerating `grub.cfg`
//! means running `grub-mkconfig`, which reruns Ubuntu's own generators and would put this daemon in
//! the business of owning the whole menu. Sourcing one generated file leaves Ubuntu's entries
//! exactly as they are and keeps TemplarOS's to a file nothing else writes.
//!
//! Two properties matter to the rest of the system:
//!
//! * every generated entry carries `templaros.deployment=<id>` on its kernel command line, which is
//!   how [`crate::state::HostdState::reconcile_boot`] can tell whether the staged candidate is the
//!   thing that actually booted. Without it that function reports
//!   [`BootReconciliation::CannotTell`](crate::state::BootReconciliation) and counts nothing,
//!   because guessing would burn a trial attempt on a candidate that is running fine;
//! * the menu entry id *is* the deployment id, unadorned. `GRUB_DEFAULT=saved` plus
//!   `grub-reboot <id>` and `grub-set-default <id>` — which the [`Grub`] backend already calls with
//!   the bare id — then address exactly these entries with no translation layer to get wrong.
//!
//! The layout constants below (the command-line key, the ESP directory, the id grammar) are pinned
//! to `distro/deploy/layout.toml` by `layout_declaration_parses_and_matches_repart`.

use crate::deploy::{Deployment, Grub};
use anyhow::{Context, Result, bail};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// The kernel command-line key that names the running deployment.
pub const CMDLINE_KEY: &str = "templaros.deployment=";

/// The root filesystem label. Identical for every deployment by construction — which is why
/// [`CMDLINE_KEY`] has to exist.
///
/// It is also identical on every *machine*, which is why it is now the fallback rather than the
/// answer: a laptop with a second TemplarOS disk attached would boot whichever the kernel enumerated
/// first. [`RootSpec::read`] prefers the UUID the provisioner recorded.
pub const ROOT_FS_LABEL: &str = "templaros-root";

/// Where the provisioner records this installation's filesystem identities.
pub const INSTALL_TOML: &str = "etc/templaros/install.toml";

/// How a generated boot entry names the root filesystem.
///
/// `installer/provision.sh` writes `/etc/templaros/install.toml` with the UUID `mkfs.btrfs` minted,
/// which is unique per filesystem and cannot collide. Every machine provisioned before that file
/// existed — and every mkosi image, which is built by `systemd-repart` and has no such file — has
/// only the label, so the label is what an absent or unreadable file falls back to. A boot entry
/// is not the place to discover that a configuration file is missing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootSpec {
    Uuid(String),
    Label(String),
}

impl RootSpec {
    /// Read `root_uuid` from `<root>/etc/templaros/install.toml`, falling back to the label.
    #[must_use]
    pub fn read(root: &Path) -> Self {
        std::fs::read_to_string(root.join(INSTALL_TOML))
            .ok()
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or_else(|| Self::Label(ROOT_FS_LABEL.to_owned()))
    }

    /// `root_uuid = "…"` out of the file. A deliberately small reader: the file is flat, and the
    /// boot path is the last place to add a dependency that can be missing on a rescue shell.
    #[must_use]
    fn parse(text: &str) -> Option<Self> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| line.strip_prefix("root_uuid"))
            .and_then(|rest| rest.trim_start().strip_prefix('='))
            .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|uuid| !uuid.is_empty())
            .map(Self::Uuid)
    }

    /// The `root=` word for a kernel command line.
    #[must_use]
    pub fn cmdline_arg(&self) -> String {
        match self {
            Self::Uuid(uuid) => format!("root=UUID={uuid}"),
            Self::Label(label) => format!("root=LABEL={label}"),
        }
    }
}

/// The generated entries file, relative to the boot directory (`/boot`).
pub const ENTRIES_FILE: &str = "grub/templaros-entries.cfg";

/// The ESP directory hostd owns for each deployment, as GRUB addresses it (the ESP is GRUB's
/// `root` for these entries, so the path is relative to the ESP, not to `/`).
pub const ESP_DEPLOYMENT_DIR: &str = "/EFI/templaros";

/// Longest acceptable deployment id.
pub const MAX_ID_LEN: usize = 64;

/// One generated GRUB menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootEntry {
    /// The deployment id — also the menu entry id GRUB is asked to boot by name.
    pub id: String,
    /// What a human reads in the menu.
    pub title: String,
    /// Btrfs subvolume mounted as `/` for this deployment.
    pub subvolume: String,
    /// Recorded as a comment, the same way the systemd-boot entry does it: not a kernel argument,
    /// but the one place a person looking at a boot entry can see which module build goes with it.
    pub nvidia_modules: Option<String>,
}

impl BootEntry {
    /// The entry for `deployment`, titled by its role.
    pub fn for_deployment(deployment: &Deployment) -> Self {
        Self {
            id: deployment.id.clone(),
            title: if deployment.known_good {
                format!("TemplarOS ({}) — known good", deployment.id)
            } else {
                format!("TemplarOS ({})", deployment.id)
            },
            subvolume: deployment.subvolume.clone(),
            nvidia_modules: deployment.nvidia_modules.clone(),
        }
    }

    /// The entry for a deployment known only by id — the known-good deployment recorded by an
    /// earlier promotion, whose full record belongs to the registry, not to the boot backend.
    #[cfg(test)]
    pub fn known_good(id: &str) -> Self {
        Self::known_good_at(id, "@")
    }

    fn known_good_at(id: &str, subvolume: &str) -> Self {
        Self {
            id: id.to_owned(),
            title: format!("TemplarOS ({id}) — known good"),
            subvolume: subvolume.to_owned(),
            nvidia_modules: None,
        }
    }
}

/// Whether `id` is safe as a subvolume name, a vfat directory name, a GRUB menu entry id and a
/// single word on the kernel command line (`distro/deploy/layout.toml`).
pub fn is_valid_deployment_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    id.len() <= MAX_ID_LEN
        && first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Render the generated GRUB fragment for `entries`, in menu order, naming the root by `root`.
pub fn render(entries: &[BootEntry], root: &RootSpec) -> String {
    let mut out = String::from(
        "# GENERATED by templar-hostd (services/templar-hostd/src/boot_entries.rs) — do not edit.\n\
         # Sourced by /etc/grub.d/09_templaros. Rewritten whole on every stage, promotion and\n\
         # rollback; GRUB_DEFAULT=saved selects among these by deployment id.\n",
    );
    for entry in entries {
        let id = &entry.id;
        // `search --file` rather than a filesystem label: it finds the ESP by something that is
        // true of it in every image we produce, including the mkosi one, whose repart
        // configuration sets no filesystem label at all.
        let _ = write!(
            out,
            "\nmenuentry '{title}' --id '{id}' {{\n    \
             search --no-floppy --set=root --file {dir}/{id}/vmlinuz\n    \
             linux {dir}/{id}/vmlinuz {root} rootflags=subvol={subvolume} rw {key}{id}\n    \
             initrd {dir}/{id}/initrd.img\n",
            title = entry.title,
            dir = ESP_DEPLOYMENT_DIR,
            root = root.cmdline_arg(),
            subvolume = entry.subvolume,
            key = CMDLINE_KEY,
        );
        if let Some(modules) = &entry.nvidia_modules {
            let _ = writeln!(out, "    # nvidia-modules {modules}");
        }
        out.push_str("}\n");
    }
    out
}

/// Write the generated fragment under `boot_root` (`/boot` in production), atomically.
///
/// Atomic because the alternative is a torn config file on the one path that has to work when
/// everything else has already gone wrong.
pub fn write_entries(boot_root: &Path, entries: &[BootEntry], root: &RootSpec) -> Result<PathBuf> {
    for entry in entries {
        if !is_valid_deployment_id(&entry.id) {
            bail!(
                "refusing to write a GRUB entry for invalid deployment id {:?}",
                entry.id
            );
        }
        if entry.subvolume.is_empty()
            || !entry.subvolume.starts_with('@')
            || !entry
                .subvolume
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '.' | '_' | '-'))
        {
            bail!("refusing unsafe root subvolume {:?}", entry.subvolume);
        }
    }
    let path = boot_root.join(ENTRIES_FILE);
    let parent = path
        .parent()
        .context("entries path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = path.with_extension("cfg.tmp");
    fs::write(&tmp, render(entries, root)).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(path)
}

/// The boot-entry half of the GRUB backend.
///
/// It lives here rather than beside the rest of [`Grub`] so that entry generation — which is D3's
/// contract C3 — stays in one file, while the transaction engine's use of the backend stays in
/// `deploy.rs`.
impl Grub {
    /// Where the id of the last deployment blessed by boot-health is remembered.
    ///
    /// The `BootManager` trait hands the backend a candidate to stage and a previous deployment to
    /// roll back to, but never the known-good deployment as a standing fact — and the generated
    /// file is rewritten whole, so the backend has to remember it or lose it the first time a
    /// candidate is staged.
    fn known_good_path(&self) -> PathBuf {
        self.state_dir.join("known-good")
    }

    /// The known-good deployment id, if one has been recorded.
    fn known_good_entry(&self) -> Option<BootEntry> {
        let text = fs::read_to_string(self.known_good_path()).ok()?;
        let mut lines = text.lines();
        let id = lines.next()?.trim();
        let subvolume = lines
            .next()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("@");
        (is_valid_deployment_id(id)).then(|| BootEntry::known_good_at(id, subvolume))
    }

    /// Remember the deployment and its boot-root subvolume together.
    pub fn record_known_good(&self, deployment: &Deployment) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("creating {}", self.state_dir.display()))?;
        fs::write(
            self.known_good_path(),
            format!("{}\n{}\n", deployment.id, deployment.subvolume),
        )
        .with_context(|| format!("recording known-good deployment {}", deployment.id))
    }

    /// Regenerate the entries file: the known-good deployment first, so a person reading the menu
    /// sees the safe choice at the top, then `candidate` when one is staged.
    pub fn refresh_entries(&self, candidate: Option<&Deployment>) -> Result<()> {
        let mut entries = Vec::new();
        if let Some(entry) = self.known_good_entry() {
            entries.push(entry);
        }
        if let Some(candidate) = candidate
            && entries.iter().all(|e| e.id != candidate.id)
        {
            entries.push(BootEntry::for_deployment(candidate));
        }
        if entries.is_empty() {
            return Ok(());
        }
        // Read per refresh rather than cached at construction: a provisioner or a rescue shell
        // can write install.toml after hostd started, and a stale cached label would keep
        // producing entries that name the wrong disk.
        write_entries(&self.boot_root, &entries, &RootSpec::read(&self.sysroot)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeCommandRunner;
    use crate::deploy::BootManager;
    use std::sync::Arc;

    fn deployment(id: &str, known_good: bool) -> Deployment {
        Deployment {
            id: id.to_owned(),
            subvolume: format!("@deployments/{id}"),
            esp_dir: format!("EFI/templaros/{id}"),
            kernel: Some("linux-image-7.0.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            known_good,
        }
    }

    #[test]
    fn a_boot_entry_names_the_root_by_the_uuid_the_provisioner_recorded() {
        // The label is identical on every TemplarOS disk, so a laptop with a second one attached
        // boots whichever the kernel enumerated first. The UUID is minted by mkfs and cannot
        // collide.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc/templaros")).unwrap();
        std::fs::write(
            dir.path().join(INSTALL_TOML),
            "# GENERATED by installer/provision.sh\nroot_uuid = \"7f3c1e2a-0b44-4d19-9a75-2c8e6f0d1b53\"\nesp_partuuid = \"1111-2222\"\n",
        )
        .unwrap();

        let spec = RootSpec::read(dir.path());
        assert_eq!(
            spec,
            RootSpec::Uuid("7f3c1e2a-0b44-4d19-9a75-2c8e6f0d1b53".to_owned())
        );
        let rendered = render(&[BootEntry::known_good("install-20260907T000000Z")], &spec);
        assert!(
            rendered.contains(
                "root=UUID=7f3c1e2a-0b44-4d19-9a75-2c8e6f0d1b53 rootflags=subvol=@ rw \
                 templaros.deployment=install-20260907T000000Z"
            ),
            "{rendered}"
        );
        assert!(
            !rendered.contains("LABEL="),
            "the label survived alongside the UUID:\n{rendered}"
        );
    }

    #[test]
    fn an_installation_with_no_recorded_identity_still_boots_by_label() {
        // Every machine provisioned before install.toml existed, and every mkosi image — which is
        // built by systemd-repart and has no such file. A missing configuration file must not be
        // discovered on the boot path.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            RootSpec::read(dir.path()),
            RootSpec::Label(ROOT_FS_LABEL.to_owned())
        );

        // ...and so must a file that is there but says nothing useful.
        std::fs::create_dir_all(dir.path().join("etc/templaros")).unwrap();
        for content in [
            "",
            "# only a comment\n",
            "esp_partuuid = \"1111-2222\"\n",
            "root_uuid = \"\"\n",
        ] {
            std::fs::write(dir.path().join(INSTALL_TOML), content).unwrap();
            assert_eq!(
                RootSpec::read(dir.path()),
                RootSpec::Label(ROOT_FS_LABEL.to_owned()),
                "{content:?} was read as an identity"
            );
        }

        let rendered = render(
            &[BootEntry::known_good("install-1")],
            &RootSpec::read(dir.path()),
        );
        assert!(rendered.contains(
            "root=LABEL=templaros-root rootflags=subvol=@ rw templaros.deployment=install-1"
        ));
    }

    #[test]
    fn grub_entries_carry_deployment_id() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let backend = Grub {
            commands: commands.clone(),
            state_dir: dir.path().join("state"),
            boot_root: dir.path().join("boot"),
            sysroot: dir.path().to_path_buf(),
        };

        // A promotion records the known-good deployment and generates its entry.
        backend
            .stage_candidate(&deployment("install-1", false), 3)
            .unwrap();
        backend.mark_good().unwrap();
        // Then a new candidate is staged on top of it.
        backend
            .stage_candidate(&deployment("txn-20260903T120000Z", false), 3)
            .unwrap();

        let generated = dir.path().join("boot").join(ENTRIES_FILE);
        let body = fs::read_to_string(&generated).unwrap();

        // Both deployments are bootable by name, and each carries its own id on the command line.
        for id in ["install-1", "txn-20260903T120000Z"] {
            assert!(
                body.contains(&format!("--id '{id}'")),
                "no menu entry id for {id} in:\n{body}"
            );
            assert!(
                body.contains(&format!("{CMDLINE_KEY}{id}")),
                "no {CMDLINE_KEY}{id} in:\n{body}"
            );
            assert!(
                body.contains(&format!("linux {ESP_DEPLOYMENT_DIR}/{id}/vmlinuz")),
                "no kernel line for {id} in:\n{body}"
            );
            assert!(
                body.contains(&format!("rootflags=subvol=@deployments/{id}")),
                "no deployment-specific root subvolume for {id} in:\n{body}"
            );
        }
        // The known-good entry comes first, and the file is regenerated rather than appended to.
        assert_eq!(body.matches("menuentry ").count(), 2, "in:\n{body}");
        assert!(body.find("install-1").unwrap() < body.find("txn-20260903T120000Z").unwrap());
        // What GRUB is asked to boot is exactly what the entry is named.
        assert_eq!(
            commands.calls().last().unwrap().args,
            ["txn-20260903T120000Z"]
        );

        // A rollback leaves the previous deployment as the only entry.
        backend.rollback(&deployment("install-1", true)).unwrap();
        let body = fs::read_to_string(&generated).unwrap();
        assert_eq!(body.matches("menuentry ").count(), 1, "in:\n{body}");
        assert!(body.contains("--id 'install-1'"));
        assert!(!body.contains("txn-20260903T120000Z"));

        // An id that would break out of the generated config is refused rather than written.
        assert!(!is_valid_deployment_id("bad'id"));
        assert!(!is_valid_deployment_id("a/b"));
        assert!(!is_valid_deployment_id("has space"));
        assert!(!is_valid_deployment_id(""));
        assert!(is_valid_deployment_id("txn-20260903T120000Z"));
        assert!(
            write_entries(
                dir.path(),
                &[BootEntry::known_good("bad'id")],
                &RootSpec::Label(ROOT_FS_LABEL.to_owned())
            )
            .is_err()
        );
    }

    // -- The layout declaration ------------------------------------------------------------------

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("repository root")
    }

    /// `Key=value` from a repart drop-in (systemd-style INI, one section, no continuations).
    fn repart_value(text: &str, key: &str) -> Option<String> {
        text.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .map(str::to_owned)
    }

    #[test]
    fn layout_declaration_parses_and_matches_repart() {
        let root = repo_root();
        let layout: toml::Value =
            toml::from_str(&fs::read_to_string(root.join("distro/deploy/layout.toml")).unwrap())
                .expect("distro/deploy/layout.toml parses");

        // The compiled-in constants are the declaration, not a second opinion about it.
        let deployment = &layout["deployment"];
        assert_eq!(deployment["cmdline"]["key"].as_str().unwrap(), CMDLINE_KEY);
        assert_eq!(
            deployment["esp_dir_template"].as_str().unwrap(),
            format!("{ESP_DEPLOYMENT_DIR}/{{id}}")
        );
        assert_eq!(
            deployment["cmdline"]["template"].as_str().unwrap(),
            format!("root=LABEL={ROOT_FS_LABEL} rootflags=subvol=@ rw {CMDLINE_KEY}{{id}}")
        );
        assert_eq!(
            layout["boot"]["grub"]["entries_file"].as_str().unwrap(),
            format!("/boot/{ENTRIES_FILE}")
        );
        assert_eq!(
            layout["boot"]["grub"]["menuentry_id_template"]
                .as_str()
                .unwrap(),
            "{id}"
        );
        assert_eq!(layout["root"]["label"].as_str().unwrap(), ROOT_FS_LABEL);

        // Every character the declaration forbids is in fact refused, and the sample ids the two
        // id-minting sites produce are in fact accepted.
        for forbidden in deployment["id_forbidden"].as_array().unwrap() {
            let id = format!("a{}b", forbidden.as_str().unwrap());
            assert!(!is_valid_deployment_id(&id), "{id:?} should be refused");
        }
        for prefix in ["transaction_id_prefix", "install_id_prefix"] {
            let id = format!("{}20260903T120000Z", deployment[prefix].as_str().unwrap());
            assert!(is_valid_deployment_id(&id), "{id:?} should be accepted");
        }
        assert!(!is_valid_deployment_id(&"a".repeat(MAX_ID_LEN + 1)));

        // repart builds the image from the same layout.
        let repart = root.join("distro/image/mkosi.repart");
        let esp = fs::read_to_string(repart.join("00-esp.conf")).unwrap();
        assert_eq!(
            repart_value(&esp, "Type").as_deref(),
            layout["esp"]["type"].as_str()
        );
        assert_eq!(
            repart_value(&esp, "Format").as_deref(),
            layout["esp"]["format"].as_str()
        );
        assert_eq!(
            repart_value(&esp, "SizeMinBytes").as_deref(),
            layout["esp"]["size_min"].as_str()
        );
        assert_eq!(
            repart_value(&esp, "SizeMaxBytes").as_deref(),
            layout["esp"]["size_max"].as_str()
        );

        let root_conf = fs::read_to_string(repart.join("10-root.conf")).unwrap();
        assert_eq!(
            repart_value(&root_conf, "Type").as_deref(),
            layout["root"]["type"].as_str()
        );
        assert_eq!(
            repart_value(&root_conf, "Format").as_deref(),
            layout["root"]["format"].as_str()
        );
        assert_eq!(
            repart_value(&root_conf, "SizeMinBytes").as_deref(),
            layout["root"]["size_min"].as_str()
        );

        let declared: Vec<&str> = layout["root"]["subvolumes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let subvolumes =
            repart_value(&root_conf, "Subvolumes").expect("10-root.conf declares Subvolumes=");
        let in_repart: Vec<&str> = subvolumes.split_whitespace().collect();
        assert_eq!(
            declared, in_repart,
            "distro/deploy/layout.toml and mkosi.repart/10-root.conf disagree about subvolumes"
        );
        // The one the update engine stages candidates into is not optional.
        assert!(in_repart.contains(&"/@deployments"));
    }
}
