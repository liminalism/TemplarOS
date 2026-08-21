//! apt, run as a child transient unit (ADR-0003: "apt runs as a child transient unit, never as an
//! in-process library") — so hostd's own AppArmor confinement does not have to cover everything
//! apt and its maintainer scripts touch, and so a killed apt run leaves a named unit behind for
//! `journalctl -u` rather than an orphaned child of a confined daemon.

use crate::cmd::CommandRunner;
use crate::updates::ChangeSet;
use anyhow::Result;
use std::{path::Path, sync::Arc};

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
    fn download(&self, id: &str, changes: &ChangeSet) -> Result<()>;
    fn install_into(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()>;
}

/// Runs `apt-get` inside `systemd-run --collect`, one transient unit per phase per transaction so
/// each is individually inspectable and cannot outlive hostd's tracking of it.
#[derive(Debug, Clone)]
pub struct SystemdRunApt {
    pub commands: Arc<dyn CommandRunner>,
}

impl Apt for SystemdRunApt {
    fn download(&self, id: &str, changes: &ChangeSet) -> Result<()> {
        let packages = changes.package_list();
        if packages.is_empty() {
            return Ok(());
        }
        let unit = format!("--unit=lege-hostd-apt-download-{id}");
        let mut args: Vec<&str> = vec![
            "--pipe",
            "--wait",
            "--collect",
            "--property=DynamicUser=no",
            &unit,
            "--",
            "apt-get",
            "-y",
            "--download-only",
            "install",
        ];
        args.extend(packages.iter().map(String::as_str));
        self.commands.run("systemd-run", &args)?;
        Ok(())
    }

    fn install_into(&self, id: &str, changes: &ChangeSet, target_root: &Path) -> Result<()> {
        let packages = changes.package_list();
        if packages.is_empty() {
            return Ok(());
        }
        let unit = format!("--unit=lege-hostd-apt-install-{id}");
        let root_arg = format!("--root={}", target_root.display());
        let mut args: Vec<&str> = vec![
            "--pipe",
            "--wait",
            "--collect",
            &unit,
            "--",
            "apt-get",
            "-y",
            &root_arg,
            "install",
        ];
        args.extend(packages.iter().map(String::as_str));
        self.commands.run("systemd-run", &args)?;
        Ok(())
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
    fn download(&self, id: &str, changes: &ChangeSet) -> Result<()> {
        self.calls
            .lock()
            .expect("poisoned")
            .push(AptCall::Download {
                id: id.to_owned(),
                changes: changes.clone(),
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
        };
        apt.download("t1", &changes()).unwrap();
        let calls = commands.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "systemd-run");
        assert!(calls[0].args.contains(&"--collect".to_owned()));
        assert!(
            calls[0]
                .args
                .contains(&"linux-image-7.2.0-generic".to_owned())
        );

        apt.install_into(
            "t1",
            &changes(),
            Path::new("/var/lib/legeos/hostd/candidates/t1"),
        )
        .unwrap();
        let calls = commands.calls();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[1]
                .args
                .iter()
                .any(|a| a.starts_with("--root=/var/lib/legeos/hostd/candidates/t1"))
        );
    }

    #[test]
    fn empty_change_set_runs_no_command() {
        let commands = Arc::new(FakeCommandRunner::default());
        let apt = SystemdRunApt {
            commands: commands.clone(),
        };
        apt.download("t1", &ChangeSet::default()).unwrap();
        assert!(commands.calls().is_empty());
    }
}
