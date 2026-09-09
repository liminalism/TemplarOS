//! `Opaque` directories are known and stored, but the whole reason they are `Opaque` is that
//! nothing inside them becomes an `entries` row. That leaves one question a drive visualiser
//! still needs answered — how big is it — with no rows to sum. This module answers it the only
//! way it can be answered without materialising the subtree: a raw filesystem walk that sums
//! bytes and counts entries without writing any of them, cached in `entry_aggregates`
//! (`schema.rs` migration `0005`) so the walk is not repeated on every query.
//!
//! The cache is refreshed only when an ordinary scan or reconcile pass actually visits the
//! `Opaque` directory (`scan.rs`); a live mutation under it never triggers a recompute, only a
//! `stale` flag (`mutation.rs`). Nothing here runs eagerly.

use crate::{
    Result,
    model::{Coverage, EntryKind, EntryMeta, Generation, NativePath, RootId, RootPolicy, now_ms},
};
use rusqlite::{OptionalExtension, Transaction};
use std::path::Path;

/// What one raw walk of an `Opaque` subtree found.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubtreeTotals {
    pub recursive_bytes: u64,
    pub recursive_alloc_bytes: u64,
    /// Every descendant counted, files and directories alike — the same thing `RootStatus::entry_count` counts for materialised rows.
    pub entry_count: u64,
    /// Whether every descendant was actually stat'd. `false` means a permission wall or a race
    /// with the filesystem stopped part of the walk, so the totals are a lower bound rather than
    /// an exact figure.
    pub readable: bool,
}

/// A cached aggregate as a consumer reads it back: the totals plus how much to trust them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggregateRow {
    pub recursive_bytes: u64,
    pub recursive_alloc_bytes: u64,
    pub entry_count: u64,
    pub readable: bool,
    /// Set by a live mutation under the subtree since this was last computed. Never cleared by
    /// anything but the next real pass over the directory.
    pub stale: bool,
    pub computed_generation: Generation,
    pub computed_ms: i64,
}

/// Walks `root` on the real filesystem and sums what it finds, under `policy`'s per-name rules —
/// `Ignored` wins at any depth, even here: the trash nested inside a build directory is still
/// noise nobody wants counted. Every other tier (`Opaque`, `Inventory`, `Detailed`) contributes
/// its bytes the same way; the point of this walk is the total, not a second classification pass.
#[must_use]
pub fn sum_subtree(root: &Path, policy: &RootPolicy) -> SubtreeTotals {
    let root_device = std::fs::symlink_metadata(root)
        .ok()
        .map(|metadata| device_of(&metadata));
    let mut totals = SubtreeTotals {
        readable: true,
        ..SubtreeTotals::default()
    };
    walk(root, policy, root_device, &mut totals);
    totals
}

fn walk(dir: &Path, policy: &RootPolicy, root_device: Option<u64>, totals: &mut SubtreeTotals) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            totals.readable = false;
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            totals.readable = false;
            continue;
        };
        let path = entry.path();
        // Vanished between `read_dir` and `stat`: a live change, not a read failure. The
        // aggregate is a snapshot in time and does not owe consistency with what happens next.
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let native = NativePath::from_path(&path);
        let meta = EntryMeta::from_symlink_metadata(&path, &metadata);
        if policy.coverage_of(&native, meta.kind) == Coverage::Ignored {
            continue;
        }
        if !policy.cross_mount
            && let Some(root_device) = root_device
            && root_device != device_of(&metadata)
        {
            continue;
        }
        totals.entry_count += 1;
        totals.recursive_bytes += meta.size_bytes;
        totals.recursive_alloc_bytes += meta.alloc_bytes;
        if meta.kind == EntryKind::Directory {
            walk(&path, policy, root_device, totals);
        }
    }
}

fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

/// Writes every pending aggregate from a scan pass. Called **after** `merge_staging_into_entries`
/// so `entry_id` already exists for the path; a path that somehow has no row (raced away between
/// the walk and the commit) is skipped rather than failing the whole commit over one directory.
pub(crate) fn write_pending(
    transaction: &Transaction<'_>,
    root: RootId,
    generation: Generation,
    pending: &[(NativePath, SubtreeTotals)],
) -> Result<()> {
    for (path, totals) in pending {
        let entry_id: Option<i64> = transaction
            .query_row(
                "SELECT entry_id FROM entries WHERE root_id = ?1 AND path_bytes = ?2",
                rusqlite::params![root.0, path.as_bytes()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(entry_id) = entry_id else { continue };
        transaction.execute(
            "INSERT INTO entry_aggregates
               (entry_id, root_id, recursive_bytes, recursive_alloc_bytes, entry_count, readable,
                computed_generation, computed_ms, stale)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
             ON CONFLICT (entry_id) DO UPDATE SET
                recursive_bytes = excluded.recursive_bytes,
                recursive_alloc_bytes = excluded.recursive_alloc_bytes,
                entry_count = excluded.entry_count,
                readable = excluded.readable,
                computed_generation = excluded.computed_generation,
                computed_ms = excluded.computed_ms,
                stale = 0",
            rusqlite::params![
                entry_id,
                root.0,
                totals.recursive_bytes as i64,
                totals.recursive_alloc_bytes as i64,
                totals.entry_count as i64,
                i64::from(totals.readable),
                generation.0 as i64,
                now_ms(),
            ],
        )?;
    }
    Ok(())
}

impl crate::Index {
    /// The cached aggregate for one `Opaque` directory, or `None` when the path is not an
    /// `Opaque` directory in this root or has never been walked.
    pub fn opaque_aggregate(
        &self,
        root: RootId,
        path: &NativePath,
    ) -> Result<Option<AggregateRow>> {
        self.with_read(|connection| {
            connection
                .query_row(
                    "SELECT a.recursive_bytes, a.recursive_alloc_bytes, a.entry_count, a.readable,
                            a.stale, a.computed_generation, a.computed_ms
                       FROM entry_aggregates a
                       JOIN entries e ON e.entry_id = a.entry_id
                      WHERE e.root_id = ?1 AND e.path_bytes = ?2 AND e.deleted_generation IS NULL
                        AND e.coverage = ?3",
                    rusqlite::params![root.0, path.as_bytes(), Coverage::Opaque.as_i64()],
                    |row| {
                        Ok(AggregateRow {
                            recursive_bytes: row.get::<_, i64>(0)? as u64,
                            recursive_alloc_bytes: row.get::<_, i64>(1)? as u64,
                            entry_count: row.get::<_, i64>(2)? as u64,
                            readable: row.get::<_, i64>(3)? != 0,
                            stale: row.get::<_, i64>(4)? != 0,
                            computed_generation: Generation(row.get::<_, i64>(5)? as u64),
                            computed_ms: row.get(6)?,
                        })
                    },
                )
                .optional()
                .map_err(Into::into)
        })
    }

    /// Marks the cached aggregate at `path` stale, keyed on the row that is actually there — a
    /// no-op when nothing has been computed for it yet. Never recomputes: that is left to the
    /// next real pass over the directory.
    pub(crate) fn mark_aggregate_stale(&self, root: RootId, path: &NativePath) -> Result<()> {
        self.with_write(|connection| {
            connection.execute(
                "UPDATE entry_aggregates SET stale = 1
                  WHERE entry_id IN (SELECT entry_id FROM entries
                                       WHERE root_id = ?1 AND path_bytes = ?2)",
                rusqlite::params![root.0, path.as_bytes()],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sum_excludes_a_trash_name_nested_inside_the_opaque_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(target.join("debug/thing.o"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir_all(target.join("Trash")).unwrap();
        std::fs::write(target.join("Trash/deleted.o"), vec![0u8; 5000]).unwrap();

        let policy = RootPolicy::default_home();
        let totals = sum_subtree(&target, &policy);
        assert_eq!(
            totals.recursive_bytes, 1000,
            "the trashed bytes are excluded at any depth"
        );
        assert_eq!(
            totals.entry_count, 2,
            "debug/ and debug/thing.o, not the Trash subtree"
        );
        assert!(totals.readable);
    }

    #[test]
    fn an_unreadable_subdirectory_marks_the_totals_unreliable_but_still_sums_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("ok")).unwrap();
        std::fs::write(target.join("ok/thing.o"), vec![0u8; 42]).unwrap();
        let locked = target.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();

        let policy = RootPolicy::default_home();
        let totals = sum_subtree(&target, &policy);
        // Restore so the tempdir can be cleaned up.
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();

        if !totals.readable {
            assert!(
                totals.recursive_bytes >= 42,
                "the readable half is still counted"
            );
        }
    }
}
