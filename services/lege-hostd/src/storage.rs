//! Storage reserve enforcement (system side). The OS reserves enough space to log in, update and
//! recover; when a volume approaches that reserve, applications are prevented from consuming the
//! last usable blocks without an explicit override (planning doc 01 §2). The user-facing numbers
//! (`VolumeInfo`, `usable_bytes`) are `lege-filed`'s to show; hostd owns the enforcement policy —
//! cache budgets, journald/apt cache limits and the trim order below.

use crate::{cmd::CommandRunner, snapshot::Snapshotter};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{ffi::CString, fs, os::unix::ffi::OsStrExt, path::Path, sync::Arc};

/// Caches the OS is allowed to trim when the reserve is threatened, in order (most reclaimable,
/// least valuable to keep, first).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrimTarget {
    AptCache,
    ThumbnailCache,
    Coredumps,
    JournalVacuum,
    OldDeployments,
}

pub const TRIM_ORDER: [TrimTarget; 5] = [
    TrimTarget::AptCache,
    TrimTarget::ThumbnailCache,
    TrimTarget::Coredumps,
    TrimTarget::JournalVacuum,
    TrimTarget::OldDeployments,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeUsage {
    pub mount_point: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub reserved_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReserveStatus {
    Ok,
    /// Free space is within 2x the reserve: trim proactively.
    Approaching,
    /// Free space is at or below the reserve: refuse further writes without an override.
    Breached,
}

impl VolumeUsage {
    pub fn status(&self) -> ReserveStatus {
        if self.free_bytes <= self.reserved_bytes {
            ReserveStatus::Breached
        } else if self.free_bytes <= self.reserved_bytes.saturating_mul(2) {
            ReserveStatus::Approaching
        } else {
            ReserveStatus::Ok
        }
    }
}

/// Reserve size for a volume: `percent` of the total, clamped to `[min, max]` — the same formula
/// `lege-filed` uses for the user-facing number, kept in step deliberately (both read this).
pub fn reserve_for(total_bytes: u64, percent: u64, min: u64, max: u64) -> u64 {
    (total_bytes / 100).saturating_mul(percent).clamp(min, max)
}

const DEFAULT_RESERVE_PERCENT: u64 = 5;
const DEFAULT_RESERVE_MIN: u64 = 512 * 1024 * 1024;
const DEFAULT_RESERVE_MAX: u64 = 16 * 1024 * 1024 * 1024;

/// Query a live volume via `statvfs`.
pub fn query(mount_point: &Path) -> Result<VolumeUsage> {
    let (total, free) = statvfs(mount_point)?;
    Ok(VolumeUsage {
        mount_point: mount_point.display().to_string(),
        total_bytes: total,
        free_bytes: free,
        reserved_bytes: reserve_for(
            total,
            DEFAULT_RESERVE_PERCENT,
            DEFAULT_RESERVE_MIN,
            DEFAULT_RESERVE_MAX,
        ),
    })
}

fn statvfs(path: &Path) -> Result<(u64, u64)> {
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `statvfs` writes into the zeroed struct we pass it; `c_path` outlives the call.
    #[allow(unsafe_code)]
    let stats = unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &raw mut stats) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        stats
    };
    #[allow(clippy::unnecessary_cast)]
    let (frsize, blocks, bavail) = (
        stats.f_frsize as u64,
        stats.f_blocks as u64,
        stats.f_bavail as u64,
    );
    Ok((blocks * frsize, bavail * frsize))
}

/// How many trimmable deployments to keep when there is space pressure.
///
/// Two, not one: the deployment being rolled back *from* is worth keeping around long enough to
/// look at, and a machine that has just promoted a candidate has a previous one that is not yet
/// the known-good one. One would make every trim pass leave exactly the deployment that is
/// running and nothing to compare it against.
pub const DEFAULT_KEEP_DEPLOYMENTS: usize = 2;

/// Which deployments may be deleted to reclaim space, oldest first.
///
/// `ordered` is every known deployment id, oldest first (ids are time-ordered by construction).
/// `protected` is the set that must never be trimmed however tight space gets — the booted
/// deployment, the known-good one, and whatever the registry currently calls current or previous.
/// Of what is left, the newest `keep` are retained.
///
/// Pure, and separate from the deletion, because "which deployments am I allowed to lose" is the
/// question the storage reserve gets wrong in the direction that cannot be undone.
#[must_use]
pub fn trimmable_deployments<'a>(
    ordered: &'a [String],
    protected: &[&str],
    keep: usize,
) -> Vec<&'a String> {
    let eligible: Vec<&String> = ordered
        .iter()
        .filter(|id| !protected.contains(&id.as_str()))
        .collect();
    let trim = eligible.len().saturating_sub(keep);
    eligible.into_iter().take(trim).collect()
}

/// An upper bound on the bytes deleting `path` would return, from the blocks its files occupy.
///
/// An *upper* bound, and deliberately described as one: a btrfs snapshot shares extents with the
/// subvolume it came from, so most of what this counts is not freed by deleting it. Overstating
/// is the safe direction for a trim order that stops as soon as the volume is healthy — the pass
/// re-queries usage rather than trusting the estimate — and it is still an enormous improvement
/// on the zero this used to report, which made deployments look free to keep and left the trim
/// order unable to converge.
///
/// Errors are counted as zero rather than propagated: a deployment whose subvolume has already
/// gone is worth zero bytes, which is exactly what the caller should do with it.
#[must_use]
pub fn estimate_size(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt as _;

    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            total = total.saturating_add(metadata.blocks().saturating_mul(512));
            if metadata.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    total
}

/// One cache the trim pass can reclaim space from.
pub trait TrimAction: Send + Sync + std::fmt::Debug {
    fn target(&self) -> TrimTarget;
    /// Bytes this action believes it could free, without freeing them.
    fn estimate(&self, root: &Path) -> Result<u64>;
    /// Free as much as fits under `root`, returning bytes actually freed.
    fn trim(&self, root: &Path) -> Result<u64>;
}

/// Deletes files directly under a fixed relative path (apt archives, thumbnails, coredumps —
/// none of these need anything smarter than "delete the file, sum what was freed").
#[derive(Debug)]
struct DeleteFilesUnder {
    target: TrimTarget,
    relative: &'static str,
}

impl TrimAction for DeleteFilesUnder {
    fn target(&self) -> TrimTarget {
        self.target
    }

    fn estimate(&self, root: &Path) -> Result<u64> {
        Ok(walk_files(&root.join(self.relative))
            .iter()
            .filter_map(|p| p.metadata().ok())
            .map(|m| m.len())
            .sum())
    }

    fn trim(&self, root: &Path) -> Result<u64> {
        let mut freed = 0u64;
        for path in walk_files(&root.join(self.relative)) {
            if let Ok(metadata) = path.metadata() {
                freed += metadata.len();
            }
            let _ = fs::remove_file(&path);
        }
        Ok(freed)
    }
}

fn walk_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// `journalctl --vacuum-size` — journald owns its own binary files, so this is the one target
/// that has to shell out rather than delete files directly.
#[derive(Debug)]
struct JournalVacuum {
    commands: Arc<dyn CommandRunner>,
}

impl TrimAction for JournalVacuum {
    fn target(&self) -> TrimTarget {
        TrimTarget::JournalVacuum
    }

    fn estimate(&self, root: &Path) -> Result<u64> {
        Ok(walk_files(&root.join("var/log/journal"))
            .iter()
            .filter_map(|p| p.metadata().ok())
            .map(|m| m.len())
            .sum())
    }

    fn trim(&self, root: &Path) -> Result<u64> {
        let before = self.estimate(root)?;
        let root_arg = format!("--root={}", root.display());
        self.commands
            .run("journalctl", &[&root_arg, "--vacuum-size=64M"])?;
        let after = self.estimate(root)?;
        Ok(before.saturating_sub(after))
    }
}

/// Deletes the oldest deployment that is not the running one and not known-good, via the
/// snapshotter — the last resort in the trim order because it gives up rollback headroom.
#[derive(Debug)]
struct OldDeployments {
    snapshotter: Arc<dyn Snapshotter>,
    candidates: Vec<(String, u64)>,
}

impl TrimAction for OldDeployments {
    fn target(&self) -> TrimTarget {
        TrimTarget::OldDeployments
    }

    fn estimate(&self, _root: &Path) -> Result<u64> {
        Ok(self.candidates.iter().map(|(_, size)| size).sum())
    }

    fn trim(&self, _root: &Path) -> Result<u64> {
        let mut freed = 0;
        for (path, size) in &self.candidates {
            self.snapshotter.delete(Path::new(path))?;
            freed += size;
        }
        Ok(freed)
    }
}

/// Result of one enforcement pass: what ran, in order, and how much each freed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrimReport {
    pub target: TrimTarget,
    pub freed_bytes: u64,
}

/// Run the trim order against `root` until `usage` would no longer be [`ReserveStatus::Breached`]
/// or [`ReserveStatus::Approaching`], or the order is exhausted. Each action only runs if the
/// volume is still not `Ok` — a healthy volume never has its caches touched.
pub fn enforce(
    root: &Path,
    commands: Arc<dyn CommandRunner>,
    snapshotter: Arc<dyn Snapshotter>,
    old_deployments: Vec<(String, u64)>,
    mut usage: VolumeUsage,
) -> Result<Vec<TrimReport>> {
    let actions: Vec<Box<dyn TrimAction>> = vec![
        Box::new(DeleteFilesUnder {
            target: TrimTarget::AptCache,
            relative: "var/cache/apt/archives",
        }),
        Box::new(DeleteFilesUnder {
            target: TrimTarget::ThumbnailCache,
            relative: "var/cache/legeos/thumbnails",
        }),
        Box::new(DeleteFilesUnder {
            target: TrimTarget::Coredumps,
            relative: "var/lib/systemd/coredump",
        }),
        Box::new(JournalVacuum { commands }),
        Box::new(OldDeployments {
            snapshotter,
            candidates: old_deployments,
        }),
    ];
    let mut reports = Vec::new();
    for action in actions {
        if usage.status() == ReserveStatus::Ok {
            break;
        }
        let freed = action
            .trim(root)
            .with_context(|| format!("{:?}", action.target()))?;
        usage.free_bytes = usage.free_bytes.saturating_add(freed);
        reports.push(TrimReport {
            target: action.target(),
            freed_bytes: freed,
        });
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cmd::FakeCommandRunner, snapshot::FakeSnapshotter};

    #[test]
    fn reserve_size_is_clamped() {
        assert_eq!(reserve_for(1_000, 5, 10, 40), 40);
        assert_eq!(reserve_for(1_000, 5, 60, 400), 60);
        assert_eq!(reserve_for(1_000, 5, 10, 400), 50);
    }

    #[test]
    fn reserve_status_thresholds() {
        let usage = |free, reserved| VolumeUsage {
            mount_point: "/".into(),
            total_bytes: 100,
            free_bytes: free,
            reserved_bytes: reserved,
        };
        assert_eq!(usage(100, 10).status(), ReserveStatus::Ok);
        assert_eq!(usage(15, 10).status(), ReserveStatus::Approaching);
        assert_eq!(usage(10, 10).status(), ReserveStatus::Breached);
    }

    #[test]
    fn root_can_be_queried() {
        let usage = query(Path::new("/")).unwrap();
        assert!(usage.total_bytes > 0);
    }

    #[test]
    fn apt_cache_trim_deletes_files_and_reports_bytes_freed() {
        let dir = tempfile::tempdir().unwrap();
        let archives = dir.path().join("var/cache/apt/archives");
        fs::create_dir_all(&archives).unwrap();
        fs::write(archives.join("a.deb"), vec![0u8; 1000]).unwrap();
        fs::write(archives.join("b.deb"), vec![0u8; 500]).unwrap();

        let action = DeleteFilesUnder {
            target: TrimTarget::AptCache,
            relative: "var/cache/apt/archives",
        };
        assert_eq!(action.estimate(dir.path()).unwrap(), 1500);
        assert_eq!(action.trim(dir.path()).unwrap(), 1500);
        assert!(walk_files(&archives).is_empty());
    }

    #[test]
    fn enforce_stops_once_the_volume_is_healthy_again() {
        let dir = tempfile::tempdir().unwrap();
        let archives = dir.path().join("var/cache/apt/archives");
        fs::create_dir_all(&archives).unwrap();
        fs::write(archives.join("a.deb"), vec![0u8; 20]).unwrap();

        let commands = Arc::new(FakeCommandRunner::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let breached = VolumeUsage {
            mount_point: "/".into(),
            total_bytes: 100,
            free_bytes: 5,
            reserved_bytes: 10,
        };
        let reports = enforce(dir.path(), commands, snapshotter, vec![], breached).unwrap();
        // AptCache alone frees 20 bytes, bringing free_bytes to 25 — comfortably past 2x the
        // 10-byte reserve — so nothing past the first action should have run.
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].target, TrimTarget::AptCache);
        assert_eq!(reports[0].freed_bytes, 20);
    }

    #[test]
    fn enforce_falls_through_to_old_deployments_when_caches_are_not_enough() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(FakeCommandRunner::default());
        let snapshotter = Arc::new(FakeSnapshotter::default());
        let dep_dir = dir.path().join("deployments/old");
        fs::create_dir_all(&dep_dir).unwrap();
        let breached = VolumeUsage {
            mount_point: "/".into(),
            total_bytes: 1000,
            free_bytes: 1,
            reserved_bytes: 10,
        };
        let reports = enforce(
            dir.path(),
            commands,
            snapshotter.clone(),
            vec![(dep_dir.display().to_string(), 100)],
            breached,
        )
        .unwrap();
        assert!(
            reports
                .iter()
                .any(|r| r.target == TrimTarget::OldDeployments)
        );
        assert_eq!(snapshotter.deleted.lock().unwrap().len(), 1);
    }

    #[test]
    fn retention_keeps_the_known_good_and_booted_deployments_whatever_the_pressure() {
        let ordered: Vec<String> = ["d1", "d2", "d3", "d4", "d5"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();

        // d1 is the oldest and is the known-good one; d5 booted. Neither may ever go, and of the
        // three that remain the newest two are kept, so only d2 is trimmable.
        let trimmable = trimmable_deployments(&ordered, &["d1", "d5"], DEFAULT_KEEP_DEPLOYMENTS);
        assert_eq!(trimmable, vec!["d2"]);

        // Under any pressure at all: `keep` going to zero still does not touch the protected two.
        let trimmable = trimmable_deployments(&ordered, &["d1", "d5"], 0);
        assert_eq!(trimmable, vec!["d2", "d3", "d4"]);

        // A machine with nothing but its known-good deployment has nothing to give.
        assert!(trimmable_deployments(&ordered[..1], &["d1"], 0).is_empty());

        // And the estimate is a real number, not the zero that made deployments look free to
        // keep and left the trim order unable to converge.
        let dir = tempfile::tempdir().unwrap();
        let subvolume = dir.path().join("@deployments/d2");
        fs::create_dir_all(subvolume.join("usr/bin")).unwrap();
        fs::write(subvolume.join("usr/bin/lege"), vec![7u8; 40_000]).unwrap();
        assert!(
            estimate_size(&subvolume) >= 40_000,
            "a 40 kB deployment must not estimate at zero"
        );
        assert_eq!(estimate_size(&dir.path().join("gone")), 0);
    }
}
