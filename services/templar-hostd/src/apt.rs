//! apt, run as a child transient unit (ADR-0003: "apt runs as a child transient unit, never as an
//! in-process library") — so hostd's own AppArmor confinement does not have to cover everything
//! apt and its maintainer scripts touch, and so a killed apt run leaves a named unit behind for
//! `journalctl -u` rather than an orphaned child of a confined daemon.

use crate::cmd::CommandRunner;
use crate::updates::ChangeSet;
use anyhow::Result;
use std::{fmt::Write as _, path::Path, sync::Arc};

impl ChangeSet {
    /// Every package this change set names, in a stable, apt-argument-ready order.
    pub fn package_list(&self) -> Vec<String> {
        let mut packages = Vec::new();
        packages.extend(self.kernel.clone());
        packages.extend(self.nvidia_modules.clone());
        packages.extend(self.nvidia_userspace.clone());
        packages.extend(self.firmware.iter().cloned());
        packages.extend(self.other_packages.iter().cloned());
        packages
    }
}

/// What the transaction engine needs from apt. Two calls: fetch the packages (network-bound,
/// safe to retry), then install them into a target root (the candidate deployment's snapshot,
/// never the running root).
pub trait Apt: Send + Sync + std::fmt::Debug {
    fn download(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()>;
    fn install_into(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()>;
}

/// Runs `apt-get` inside `systemd-run --collect`, one transient unit per phase per transaction so
/// each is individually inspectable and cannot outlive hostd's tracking of it.
#[derive(Debug, Clone)]
pub struct SystemdRunApt {
    pub commands: Arc<dyn CommandRunner>,
    pub source_root: std::path::PathBuf,
}

impl SystemdRunApt {
    fn archive(&self, changes: &ChangeSet) -> Option<std::path::PathBuf> {
        changes.archive.as_ref().map(std::path::PathBuf::from).or_else(|| {
            let path = self.source_root.join("var/lib/templaros/devrepo");
            path.join("Release").is_file().then_some(path)
        })
    }
    fn run_in_candidate(&self, id: &str, phase: &str, changes: &ChangeSet,
        root: &Path, program: &str, command: &[&str]) -> Result<()> {
        let unit = format!("--unit=templar-hostd-apt-{phase}-{id}");
        let directory = format!("--property=RootDirectory={}", root.display());
        let destination = std::fs::read_link(root.join("etc/resolv.conf")).ok()
            .filter(|target| target.is_absolute() && (target.starts_with("/run") || target.starts_with("/var/run")))
            .map_or_else(|| "/etc/resolv.conf".to_owned(), |target| target.display().to_string());
        let mut bindings = format!("/etc/resolv.conf:{destination}");
        if let Some(path) = self.archive(changes) {
            if !path.is_absolute() { anyhow::bail!("archive path must be absolute"); }
            // The signer and set history are not package-manager inputs. Do not expose
            // the archive's private key directory to maintainer scripts.
            for name in ["pool", "Packages", "Packages.gz", "Release", "Release.gpg", "InRelease"] {
                let source = path.join(name);
                if !source.exists() { continue; }
                let source = source.to_str().ok_or_else(|| anyhow::anyhow!("non-UTF8 archive path"))?;
                bindings.push_str(" \"");
                for character in source.chars() {
                    if character.is_control() { anyhow::bail!("unsafe archive path"); }
                    if matches!(character, '\\' | '"') { bindings.push('\\'); }
                    bindings.push(character);
                }
                write!(bindings, "\":/var/lib/templaros/devrepo/{name}")?;
            }
        }
        let device_allow = if self.source_root.join(crate::layout::INSTALLED_LAYOUT).exists() {
            use std::os::unix::fs::FileTypeExt as _;
            let output = self.commands.run("findmnt", &["--noheadings", "--target", &root.to_string_lossy(), "--output", "SOURCE"])?;
            let source = String::from_utf8(output.stdout)?;
            let device = source.trim().split('[').next().unwrap_or("");
            if !device.starts_with("/dev/") || device.chars().any(|c| c.is_whitespace() || c.is_control())
                || !std::fs::metadata(device)?.file_type().is_block_device() {
                anyhow::bail!("candidate root requires one owned native block-device identity");
            }
            // GRUB's probe needs read-only access to this root device, not every
            // host device and never write permission or a mount capability.
            bindings.push(' ');
            bindings.push_str(device);
            Some(format!("--property=DeviceAllow={device} r"))
        } else { None };
        let bindings = format!("--property=BindReadOnlyPaths={bindings}");
        let mut args = vec!["--pipe", "--wait", "--collect", &unit, &directory,
            "--property=PrivateMounts=yes", "--property=MountAPIVFS=yes",
            "--property=PrivateDevices=yes", "--property=PrivatePIDs=yes",
            "--property=TemporaryFileSystem=/run", "--property=ProtectHome=yes",
            "--property=RuntimeMaxSec=240s", "--property=TimeoutStopSec=15s",
            "--property=KillMode=control-group", &bindings];
        args.push("--property=CapabilityBoundingSet=~CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_SYS_BOOT CAP_SYS_PTRACE CAP_SYS_MODULE CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_TIME CAP_SYSLOG CAP_BPF CAP_PERFMON");
        args.extend(["--property=NoNewPrivileges=yes", "--property=ProtectKernelTunables=yes",
            "--property=ProtectControlGroups=yes", "--property=ProtectKernelModules=yes"]);
        if let Some(device) = &device_allow {
            args.extend(["--property=DevicePolicy=closed", device]);
        }
        args.extend(["--", program]);
        args.extend_from_slice(command);
        self.commands.run("systemd-run", &args)?;
        Ok(())
    }
    fn configure_archive(&self, changes: &ChangeSet, root: &Path) -> Result<()> {
        let Some(archive) = self.archive(changes) else { return Ok(()) };
        if !archive.is_absolute() { anyhow::bail!("archive path must be absolute"); }
        let key = root.join("etc/apt/keyrings/templaros-dev.gpg");
        std::fs::create_dir_all(key.parent().expect("keyring parent"))?;
        std::fs::copy(archive.join("templaros-dev.gpg"), &key)?;
        let source = root.join("etc/apt/sources.list.d/templaros-dev.sources");
        std::fs::create_dir_all(source.parent().expect("source parent"))?;
        std::fs::write(source, "Types: deb\nURIs: file:/var/lib/templaros/devrepo\nSuites: ./\nSigned-By: /etc/apt/keyrings/templaros-dev.gpg\n")?;
        Ok(())
    }
}

impl Apt for SystemdRunApt {
    fn download(&self, id: &str, changes: &ChangeSet, root: &Path) -> Result<()> {
        let packages = changes.package_list();
        if packages.is_empty() { return Ok(()) }
        self.configure_archive(changes, root)?;
        self.run_in_candidate(id, "index", changes, root, "/usr/bin/apt-get", &["update"])?;
        let mut args = vec!["-y", "--download-only", "--allow-downgrades",
            "--no-install-recommends", "install", "--"];
        args.extend(packages.iter().map(String::as_str));
        self.run_in_candidate(id, "download", changes, root, "/usr/bin/apt-get", &args)
    }
    fn install_into(&self, id: &str, changes: &ChangeSet, root: &Path) -> Result<()> {
        let packages = changes.package_list();
        if packages.is_empty() { return Ok(()) }
        self.run_in_candidate(id, "configure", changes, root, "/usr/bin/dpkg", &["--configure", "-a"])?;
        let mut args = vec!["-y", "--allow-downgrades", "--no-install-recommends", "install", "--"];
        args.extend(packages.iter().map(String::as_str));
        self.run_in_candidate(id, "install", changes, root, "/usr/bin/apt-get", &args)
    }
}

/// One recorded call, for test assertions about exactly what ran (and, crucially, what did
/// *not* re-run after a resume).
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AptCall {
    Download {
        id: String,
        changes: ChangeSet,
        target_root: std::path::PathBuf,
    },
    Install {
        id: String,
        changes: ChangeSet,
        target_root: std::path::PathBuf,
    },
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeApt {
    pub calls: std::sync::Mutex<Vec<AptCall>>,
    pub fail_download: std::sync::atomic::AtomicBool,
    pub fail_install: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl FakeApt {
    pub fn calls(&self) -> Vec<AptCall> {
        self.calls.lock().expect("poisoned").clone()
    }
}

#[cfg(test)]
impl Apt for FakeApt {
    fn download(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()> {
        self.calls
            .lock()
            .expect("poisoned")
            .push(AptCall::Download {
                id: id.to_owned(),
                changes: changes.clone(),
                target_root: target_root.to_path_buf(),
            });
        if self.fail_download.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("apt download failed (fake)");
        }
        Ok(())
    }

    fn install_into(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()> {
        self.calls.lock().expect("poisoned").push(AptCall::Install {
            id: id.to_owned(),
            changes: changes.clone(),
            target_root: target_root.to_path_buf(),
        });
        if self.fail_install.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("apt install failed (fake)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeCommandRunner;

    fn changes() -> ChangeSet {
        ChangeSet {
            kernel: Some("linux-image-7.2.0-generic".into()),
            nvidia_modules: Some("linux-modules-nvidia-580-open-generic".into()),
            nvidia_userspace: Some("nvidia-driver-580-open".into()),
            firmware: vec!["linux-firmware".into()],
            other_packages: vec![],
            archive: None,
        }
    }

    #[test]
    fn package_list_collects_every_field() {
        let list = changes().package_list();
        assert_eq!(
            list,
            vec![
                "linux-image-7.2.0-generic",
                "linux-modules-nvidia-580-open-generic",
                "nvidia-driver-580-open",
                "linux-firmware",
            ]
        );
    }

    #[test]
    fn systemd_run_apt_builds_a_collected_transient_unit() {
        let commands = Arc::new(FakeCommandRunner::default());
        let apt = SystemdRunApt {
            commands: commands.clone(),
            source_root: "/missing-source-root".into(),
        };
        let root = Path::new("/private-candidate/t1");
        apt.download("t1", &changes(), root).unwrap();
        let calls = commands.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "systemd-run");
        assert!(calls[0].args.contains(&"--collect".to_owned()));
        assert!(
            calls[1]
                .args
                .contains(&"linux-image-7.2.0-generic".to_owned())
        );

        apt.install_into(
            "t1",
            &changes(),
            Path::new("/var/lib/templaros/hostd/candidates/t1"),
        )
        .unwrap();
        let calls = commands.calls();
        assert_eq!(calls.len(), 4);
        assert!(
            calls[3]
                .args
                .iter()
                .any(|a| a.starts_with("--property=RootDirectory=/var/lib/templaros/hostd/candidates/t1"))
        );
    }

    #[test]
    fn candidate_binds_public_archive_inputs_not_signer_or_history() {
        let archive = tempfile::tempdir().unwrap();
        for name in ["pool", ".gnupg", "history"] {
            std::fs::create_dir(archive.path().join(name)).unwrap();
        }
        for name in ["Release", "Packages", "templaros-dev.gpg"] {
            std::fs::write(archive.path().join(name), "").unwrap();
        }
        let commands = Arc::new(FakeCommandRunner::default());
        let apt = SystemdRunApt {
            commands: commands.clone(),
            source_root: "/missing-source-root".into(),
        };
        let changes = ChangeSet {
            archive: Some(archive.path().display().to_string()),
            ..Default::default()
        };
        apt.run_in_candidate("t1", "test", &changes, Path::new("/private-candidate/t1"),
            "/usr/bin/true", &[]).unwrap();
        let calls = commands.calls();
        let bindings = calls[0].args.iter()
            .find(|arg| arg.starts_with("--property=BindReadOnlyPaths=")).unwrap();
        assert!(bindings.contains("/pool"));
        assert!(bindings.contains("/Release"));
        assert!(bindings.contains("/Packages"));
        assert!(!bindings.contains(".gnupg"));
        assert!(!bindings.contains("history"));
        assert!(!bindings.contains("templaros-dev.gpg"));
    }

    #[test]
    fn empty_change_set_runs_no_command() {
        let commands = Arc::new(FakeCommandRunner::default());
        let apt = SystemdRunApt {
            commands: commands.clone(),
            source_root: "/missing-source-root".into(),
        };
        apt.download("t1", &ChangeSet::default(), Path::new("/private-candidate/t1")).unwrap();
        assert!(commands.calls().is_empty());
    }
}
