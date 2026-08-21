//! Deployment snapshots. A deployment is a read-only btrfs snapshot of `@` plus a hostd-owned ESP
//! directory (`distro/deploy/README.md`); this module owns the subvolume side. Root on btrfs
//! subvolumes from the first image (ADR-0004) — no fallback filesystem path is implemented,
//! because a deployment that silently degraded to "just a directory" on non-btrfs root would
//! lose the one property that makes rollback safe: an atomic, space-efficient, read-only copy.

use crate::cmd::CommandRunner;
use anyhow::{Context, Result};
use std::{path::Path, sync::Arc};

/// What the transaction engine and storage reserve enforcement need from btrfs.
pub trait Snapshotter: Send + Sync + std::fmt::Debug {
    /// Snapshot `source` (read-write, so the candidate can be installed into) at `dest`.
    ///
    /// Must be idempotent when `dest` already exists: the `Staging` phase this backs is the
    /// longest-running one (snapshot + a multi-minute `apt install`), so it is the phase most
    /// likely to be interrupted, and resuming it re-enters `create_rw` with the same `dest`. A
    /// candidate directory that exists but was never sealed read-only and never had a boot entry
    /// written for it is by definition incomplete and unreferenced — nothing outside the
    /// transaction that created it knows it exists — so an implementation must discard and
    /// recreate rather than fail on "destination exists".
    fn create_rw(&self, source: &Path, dest: &Path) -> Result<()>;
    /// Flip a previously-created snapshot to read-only once staging and verification pass —
    /// the point at which it becomes trustworthy as "the complete previous deployment" for the
    /// *next* transaction.
    fn seal_read_only(&self, path: &Path) -> Result<()>;
    /// Permanently remove a snapshot (used by storage-reserve trim, oldest non-good first).
    fn delete(&self, path: &Path) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct BtrfsSnapshotter {
    pub commands: Arc<dyn CommandRunner>,
}

impl Snapshotter for BtrfsSnapshotter {
    fn create_rw(&self, source: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {} for snapshot", parent.display()))?;
        }
        if dest.exists() {
            // A leftover from an interrupted Staging attempt (see the trait doc). `btrfs
            // subvolume snapshot` itself refuses to write into an existing directory, so a
            // resumed transaction would otherwise fail on "File exists" forever instead of
            // retrying — clear it first.
            self.delete(dest)
                .with_context(|| format!("clearing incomplete candidate at {}", dest.display()))?;
        }
        self.commands.run(
            "btrfs",
            &[
                "subvolume",
                "snapshot",
                &source.to_string_lossy(),
                &dest.to_string_lossy(),
            ],
        )?;
        Ok(())
    }

    fn seal_read_only(&self, path: &Path) -> Result<()> {
        self.commands.run(
            "btrfs",
            &[
                "property",
                "set",
                "-ts",
                &path.to_string_lossy(),
                "ro",
                "true",
            ],
        )?;
        Ok(())
    }

    fn delete(&self, path: &Path) -> Result<()> {
        self.commands
            .run("btrfs", &["subvolume", "delete", &path.to_string_lossy()])?;
        Ok(())
    }
}

/// Plain directories standing in for subvolumes — enough to exercise the transaction and
/// storage-reserve state machines on a machine with no btrfs (rule 4).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeSnapshotter {
    pub sealed: std::sync::Mutex<Vec<std::path::PathBuf>>,
    pub deleted: std::sync::Mutex<Vec<std::path::PathBuf>>,
}

#[cfg(test)]
impl Snapshotter for FakeSnapshotter {
    fn create_rw(&self, source: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if dest.exists() {
            // Mirrors BtrfsSnapshotter: discard a leftover from an interrupted attempt rather
            // than merging into it, so tests exercise the same "clear then recreate" semantics.
            self.delete(dest)?;
        }
        if source.exists() {
            copy_dir(source, dest)?;
        } else {
            std::fs::create_dir_all(dest)?;
        }
        Ok(())
    }

    fn seal_read_only(&self, path: &Path) -> Result<()> {
        self.sealed
            .lock()
            .expect("poisoned")
            .push(path.to_path_buf());
        Ok(())
    }

    fn delete(&self, path: &Path) -> Result<()> {
        self.deleted
            .lock()
            .expect("poisoned")
            .push(path.to_path_buf());
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::FakeCommandRunner;

    #[test]
    fn btrfs_snapshotter_issues_expected_commands() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let snapshotter = BtrfsSnapshotter {
            commands: commands.clone(),
        };
        let source = dir.path().join("@");
        let dest = dir.path().join("@deployments/t1");
        snapshotter.create_rw(&source, &dest).unwrap();
        snapshotter.seal_read_only(&dest).unwrap();
        snapshotter.delete(&dest).unwrap();

        let calls = commands.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].args[0..2], ["subvolume", "snapshot"]);
        assert_eq!(calls[1].args[0..2], ["property", "set"]);
        assert_eq!(calls[2].args[0..2], ["subvolume", "delete"]);
    }

    #[test]
    fn fake_snapshotter_copies_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("root");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("marker"), b"hi").unwrap();

        let dest = dir.path().join("deployments/t1");
        let snapshotter = FakeSnapshotter::default();
        snapshotter.create_rw(&source, &dest).unwrap();
        assert_eq!(std::fs::read(dest.join("marker")).unwrap(), b"hi");

        snapshotter.seal_read_only(&dest).unwrap();
        assert_eq!(snapshotter.sealed.lock().unwrap().len(), 1);

        snapshotter.delete(&dest).unwrap();
        assert!(!dest.exists());
    }

    #[test]
    fn btrfs_create_rw_clears_a_pre_existing_destination_first() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let snapshotter = BtrfsSnapshotter {
            commands: commands.clone(),
        };
        let source = dir.path().join("@");
        let dest = dir.path().join("@deployments/t1");
        // Simulate a leftover from an interrupted attempt: the directory already exists.
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("stale"), b"leftover").unwrap();

        snapshotter.create_rw(&source, &dest).unwrap();

        let calls = commands.calls();
        assert_eq!(
            calls.len(),
            2,
            "expected a delete before the snapshot: {calls:?}"
        );
        assert_eq!(calls[0].args[0..2], ["subvolume", "delete"]);
        assert_eq!(calls[1].args[0..2], ["subvolume", "snapshot"]);
    }

    #[test]
    fn fake_create_rw_discards_a_partially_populated_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("root");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("marker"), b"hi").unwrap();

        let dest = dir.path().join("deployments/t1");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(
            dest.join("stale-partial-file"),
            b"leftover from a crashed attempt",
        )
        .unwrap();

        let snapshotter = FakeSnapshotter::default();
        snapshotter.create_rw(&source, &dest).unwrap();

        assert_eq!(std::fs::read(dest.join("marker")).unwrap(), b"hi");
        assert!(!dest.join("stale-partial-file").exists());
    }
}
