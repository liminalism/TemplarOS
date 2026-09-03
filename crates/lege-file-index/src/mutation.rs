//! Incremental change: what the maintenance lane writes, and what every live consumer reads.
//!
//! Two types, one direction each. A caller writes [`Mutation`]s; the index emits [`Change`]s. There
//! is no third internal form, and both carry native path bytes, because a rename that has been
//! through a lossy string cannot be applied to the file it describes.
//!
//! The dedupe rule is what lets `lege-filed` apply a file operation the instant it commits and then
//! ignore the watcher's echo of the same operation seconds later: a mutation whose stored row
//! already says exactly this is a **skip** — no change, no generation bump, counted in
//! [`Applied::skipped`].

use crate::{
    IndexError, Result,
    generation::{fold, refresh_fts},
    model::{
        EntryId, EntryKind, EntryMeta, Generation, NativePath, ObjectKey, RootId, StaleReason,
        now_ms,
    },
    schema,
};
use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};

/// Where a batch came from. Recorded for diagnostics; it does not change the semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationSource {
    /// A filesystem watcher.
    Watcher,
    /// A `lege-filed` operation that has already committed to disk.
    Operation,
    /// Background repair.
    Reconcile,
}

/// One change to apply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mutation {
    /// Create, or update metadata. Idempotent.
    Upsert {
        root: RootId,
        path: NativePath,
        meta: EntryMeta,
    },
    /// Remove a path; a directory takes its subtree with it.
    Remove { root: RootId, path: NativePath },
    /// Rename or move inside one root. Descendant rows are rewritten, so a folder drag is one
    /// mutation and not a subtree re-enumeration.
    Move {
        root: RootId,
        from: NativePath,
        to: NativePath,
        meta: EntryMeta,
    },
    /// Metadata only; membership is unchanged.
    Touch {
        root: RootId,
        path: NativePath,
        meta: EntryMeta,
    },
    /// Overflow, ambiguity or detected inconsistency. Marks the root stale with a reason and
    /// **deletes nothing**: an unproven absence is not a deletion.
    Invalidate {
        root: RootId,
        subtree: NativePath,
        reason: StaleReason,
    },
}

impl Mutation {
    #[must_use]
    pub fn root(&self) -> RootId {
        match self {
            Self::Upsert { root, .. }
            | Self::Remove { root, .. }
            | Self::Move { root, .. }
            | Self::Touch { root, .. }
            | Self::Invalidate { root, .. } => *root,
        }
    }
}

/// A batch applied as one transaction and one generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationBatch {
    pub source: MutationSource,
    pub mutations: Vec<Mutation>,
}

impl MutationBatch {
    #[must_use]
    pub fn new(source: MutationSource, mutations: Vec<Mutation>) -> Self {
        Self { source, mutations }
    }
}

/// What a batch did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub generation: Generation,
    pub changes: Vec<Change>,
    /// Mutations the index already agreed with. Counted, never folded into the batch length: it is
    /// the observable that proves operation-hook dedupe works.
    pub skipped: usize,
    /// Rows re-pathed underneath a moved directory.
    pub moved_descendants: u64,
}

/// What happened to one entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChangeOp {
    Added,
    Removed,
    Moved,
    Metadata,
    /// A whole root was re-enumerated. One marker instead of a million events: a consumer holding
    /// a projection of this root re-reads it rather than replaying it.
    Rescan,
}

impl ChangeOp {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Moved => "moved",
            Self::Metadata => "metadata",
            Self::Rescan => "rescan",
        }
    }

    #[must_use]
    pub fn from_str_or_metadata(value: &str) -> Self {
        match value {
            "added" => Self::Added,
            "removed" => Self::Removed,
            "moved" => Self::Moved,
            "rescan" => Self::Rescan,
            _ => Self::Metadata,
        }
    }
}

/// One entry in the bounded change log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub seq: u64,
    pub generation: Generation,
    pub root: RootId,
    pub op: ChangeOp,
    pub entry_id: Option<EntryId>,
    pub path: NativePath,
    /// Where a move came from. A client rewrites its own subtree prefix from this.
    pub old_path: Option<NativePath>,
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub mtime_ns: i64,
    pub at_ms: i64,
}

/// A page of the change log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesBatch {
    pub generation: Generation,
    pub next_seq: u64,
    pub more: bool,
    pub changes: Vec<Change>,
}

/// Where the host and the maintenance lane learn that something moved.
pub trait ChangeSink: Send + Sync + std::fmt::Debug {
    fn generation_advanced(&self, generation: Generation, changes: &[Change]);
    fn root_state_changed(&self, status: &crate::RootStatus);
}

impl crate::Index {
    /// Applies a batch: one transaction, one generation, one notification.
    ///
    /// A batch larger than `max_batch` is split, and each part commits on its own — a watcher
    /// storm must not turn into one unbounded transaction.
    pub fn apply(&self, batch: MutationBatch) -> Result<Applied> {
        let limit = self.config().max_batch;
        if batch.mutations.len() > limit {
            let mut total = Applied {
                generation: self.generation()?,
                changes: Vec::new(),
                skipped: 0,
                moved_descendants: 0,
            };
            for chunk in batch.mutations.chunks(limit) {
                let applied = self.apply(MutationBatch::new(batch.source, chunk.to_vec()))?;
                total.generation = applied.generation;
                total.skipped += applied.skipped;
                total.moved_descendants += applied.moved_descendants;
                total.changes.extend(applied.changes);
            }
            return Ok(total);
        }

        let max_changes = self.config().max_changes;
        let (applied, invalidations) = self.with_write(|connection| {
            let transaction = connection.transaction()?;
            let generation = crate::next_generation(&transaction)?;
            let mut changes = Vec::new();
            let mut skipped = 0usize;
            let mut moved_descendants = 0u64;
            let mut invalidations = Vec::new();
            let mut touched_roots: Vec<RootId> = Vec::new();

            for mutation in &batch.mutations {
                match mutation {
                    Mutation::Upsert { root, path, meta }
                    | Mutation::Touch { root, path, meta } => {
                        match upsert(&transaction, generation, *root, path, meta)? {
                            Some(change) => changes.push(change),
                            None => skipped += 1,
                        }
                    }
                    Mutation::Remove { root, path } => {
                        match remove(&transaction, generation, *root, path)? {
                            Some(change) => changes.push(change),
                            None => skipped += 1,
                        }
                    }
                    Mutation::Move {
                        root,
                        from,
                        to,
                        meta,
                    } => match rename(&transaction, generation, *root, from, to, meta)? {
                        RenameOutcome::Skipped => skipped += 1,
                        RenameOutcome::Applied {
                            change,
                            descendants,
                        } => {
                            moved_descendants += descendants;
                            changes.push(change);
                        }
                    },
                    Mutation::Invalidate {
                        root,
                        subtree,
                        reason,
                    } => {
                        tracing::debug!(
                            root = root.0,
                            subtree = %subtree.display(),
                            reason = %reason.as_string(),
                            "invalidating a subtree"
                        );
                        invalidations.push((*root, reason.clone()));
                    }
                }
                let root = mutation.root();
                if !touched_roots.contains(&root) {
                    touched_roots.push(root);
                }
            }

            if changes.is_empty() {
                // Nothing moved: roll the transaction back so the generation counter does not
                // advance for a batch that changed nothing.
                transaction.rollback()?;
                let generation = self.generation()?;
                return Ok((
                    Applied {
                        generation,
                        changes: Vec::new(),
                        skipped,
                        moved_descendants,
                    },
                    invalidations,
                ));
            }

            for change in &mut changes {
                change.seq = append_change(&transaction, change)?;
            }
            prune_changes(&transaction, max_changes)?;
            for root in &touched_roots {
                refresh_fts(&transaction, *root, generation)?;
                transaction.execute(
                    "UPDATE roots
                        SET committed_generation = ?2,
                            entry_count = (SELECT COUNT(*) FROM entries
                                            WHERE root_id = ?1 AND deleted_generation IS NULL)
                      WHERE root_id = ?1",
                    rusqlite::params![root.0, generation.0 as i64],
                )?;
            }
            schema::write_meta(
                &transaction,
                "committed_generation",
                &generation.0.to_string(),
            )?;
            transaction.commit()?;

            Ok((
                Applied {
                    generation,
                    changes,
                    skipped,
                    moved_descendants,
                },
                invalidations,
            ))
        })?;

        for (root, reason) in invalidations {
            self.mark_stale(root, &reason)?;
        }
        if !applied.changes.is_empty()
            && let Some(sink) = self.sink()
        {
            sink.generation_advanced(applied.generation, &applied.changes);
        }
        Ok(applied)
    }

    /// A bounded page of the change log after `seq`.
    ///
    /// Below the retained floor the answer is [`IndexError::SnapshotRequired`], not a partial
    /// history dressed up as a complete one.
    pub fn changes_since(&self, seq: u64, limit: u32) -> Result<ChangesBatch> {
        let generation = self.generation()?;
        let limit = limit.clamp(1, 10_000);
        self.with_read(|connection| {
            let floor: u64 = schema::read_meta(connection, "min_retained_change_seq")?
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            if seq < floor {
                return Err(IndexError::SnapshotRequired {
                    reason: "the requested change sequence has been pruned",
                });
            }
            let mut statement = connection.prepare(
                "SELECT seq, generation, root_id, op, entry_id, path_bytes, old_path_bytes, kind,
                        size_bytes, mtime_ns, at_ms
                   FROM changes WHERE seq > ?1 ORDER BY seq LIMIT ?2",
            )?;
            let rows = statement.query_map(
                rusqlite::params![seq as i64, i64::from(limit) + 1],
                change_from_row,
            )?;
            let mut changes = Vec::new();
            for row in rows {
                changes.push(row?);
            }
            let more = changes.len() > limit as usize;
            changes.truncate(limit as usize);
            let next_seq = changes.last().map_or(seq, |change| change.seq);
            Ok(ChangesBatch {
                generation,
                next_seq,
                more,
                changes,
            })
        })
    }
}

enum RenameOutcome {
    Skipped,
    Applied { change: Change, descendants: u64 },
}

fn upsert(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    path: &NativePath,
    meta: &EntryMeta,
) -> Result<Option<Change>> {
    let existing = live_row(transaction, root, path)?;
    if let Some((_, stored)) = &existing
        && stored.matches(meta)
    {
        return Ok(None);
    }
    let entry_id = write_entry(transaction, generation, root, path, meta)?;
    Ok(Some(Change {
        seq: 0,
        generation,
        root,
        op: if existing.is_some() {
            ChangeOp::Metadata
        } else {
            ChangeOp::Added
        },
        entry_id: Some(entry_id),
        path: path.clone(),
        old_path: None,
        kind: meta.kind,
        size_bytes: meta.size_bytes,
        mtime_ns: meta.mtime_ns,
        at_ms: now_ms(),
    }))
}

fn remove(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    path: &NativePath,
) -> Result<Option<Change>> {
    let Some((entry_id, meta)) = live_row(transaction, root, path)? else {
        return Ok(None);
    };
    let generation_id = generation.0 as i64;
    transaction.execute(
        "UPDATE entries SET deleted_generation = ?1 WHERE entry_id = ?2",
        rusqlite::params![generation_id, entry_id.0],
    )?;
    if meta.kind == EntryKind::Directory {
        let (low, high) = path.subtree_bounds();
        transaction.execute(
            "UPDATE entries SET deleted_generation = ?1
              WHERE root_id = ?2 AND deleted_generation IS NULL
                AND path_bytes >= ?3 AND path_bytes < ?4",
            rusqlite::params![generation_id, root.0, low, high],
        )?;
    }
    Ok(Some(Change {
        seq: 0,
        generation,
        root,
        op: ChangeOp::Removed,
        entry_id: Some(entry_id),
        path: path.clone(),
        old_path: None,
        kind: meta.kind,
        size_bytes: meta.size_bytes,
        mtime_ns: meta.mtime_ns,
        at_ms: now_ms(),
    }))
}

fn rename(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    from: &NativePath,
    to: &NativePath,
    meta: &EntryMeta,
) -> Result<RenameOutcome> {
    let source = live_row(transaction, root, from)?;
    if source.is_none() {
        // The echo of a rename the file service already applied: the source is gone because the
        // index already moved it, and the destination already says exactly this.
        if let Some((_, stored)) = live_row(transaction, root, to)?
            && stored.matches(meta)
        {
            return Ok(RenameOutcome::Skipped);
        }
        return match upsert(transaction, generation, root, to, meta)? {
            Some(change) => Ok(RenameOutcome::Applied {
                change,
                descendants: 0,
            }),
            None => Ok(RenameOutcome::Skipped),
        };
    }
    let (entry_id, stored) = source.expect("checked above");
    let generation_id = generation.0 as i64;

    // Anything already sitting at the destination is gone: the move overwrote it.
    if let Some((victim, _)) = live_row(transaction, root, to)? {
        transaction.execute(
            "UPDATE entries SET deleted_generation = ?1 WHERE entry_id = ?2",
            rusqlite::params![generation_id, victim.0],
        )?;
    }

    let name = to.file_name().unwrap_or_else(|| to.clone());
    transaction.execute(
        "UPDATE entries
            SET path_bytes = ?1, path_display = ?2, name_bytes = ?3, name_display = ?4,
                name_fold = ?5, kind = ?6, size_bytes = ?7, alloc_bytes = ?8, mtime_ns = ?9,
                ctime_ns = ?10, mode = ?11, hidden = ?12, dev = ?13, inode = ?14, nlink = ?15,
                written_generation = ?16, last_seen_generation = ?16, deleted_generation = NULL
          WHERE entry_id = ?17",
        rusqlite::params![
            to.as_bytes(),
            to.display(),
            name.as_bytes(),
            name.display(),
            fold(&name.display()),
            meta.kind.as_i64(),
            meta.size_bytes as i64,
            meta.alloc_bytes as i64,
            meta.mtime_ns,
            meta.ctime_ns,
            i64::from(meta.mode),
            i64::from(meta.hidden),
            meta.object_key.map(|key| key.dev as i64),
            meta.object_key.map(|key| key.ino as i64),
            i64::from(meta.nlink),
            generation_id,
            entry_id.0,
        ],
    )?;

    // A directory move takes its subtree's paths with it, so a folder drag costs one rewrite
    // rather than a full re-enumeration of everything under it.
    let mut descendants = 0u64;
    if stored.kind == EntryKind::Directory || meta.kind == EntryKind::Directory {
        let (low, high) = from.subtree_bounds();
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut statement = transaction.prepare(
                "SELECT entry_id, path_bytes FROM entries
                  WHERE root_id = ?1 AND deleted_generation IS NULL
                    AND path_bytes >= ?2 AND path_bytes < ?3",
            )?;
            let mapped = statement
                .query_map(rusqlite::params![root.0, low.clone(), high], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
            let mut rows = Vec::new();
            for row in mapped {
                rows.push(row?);
            }
            rows
        };
        let prefix_len = low.len() - 1; // the "from" path without its trailing separator
        let mut statement = transaction.prepare(
            "UPDATE entries SET path_bytes = ?1, path_display = ?2, written_generation = ?3,
                                last_seen_generation = ?3
              WHERE entry_id = ?4",
        )?;
        for (id, old) in rows {
            let mut rebuilt = to.as_bytes().to_vec();
            rebuilt.extend_from_slice(&old[prefix_len..]);
            let rebuilt = NativePath::from_bytes(rebuilt);
            statement.execute(rusqlite::params![
                rebuilt.as_bytes(),
                rebuilt.display(),
                generation_id,
                id
            ])?;
            descendants += 1;
        }
    }

    Ok(RenameOutcome::Applied {
        change: Change {
            seq: 0,
            generation,
            root,
            op: ChangeOp::Moved,
            entry_id: Some(entry_id),
            path: to.clone(),
            old_path: Some(from.clone()),
            kind: meta.kind,
            size_bytes: meta.size_bytes,
            mtime_ns: meta.mtime_ns,
            at_ms: now_ms(),
        },
        descendants,
    })
}

fn live_row(
    transaction: &Transaction<'_>,
    root: RootId,
    path: &NativePath,
) -> Result<Option<(EntryId, EntryMeta)>> {
    transaction
        .query_row(
            "SELECT entry_id, kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden,
                    dev, inode, nlink, mime
               FROM entries
              WHERE root_id = ?1 AND path_bytes = ?2 AND deleted_generation IS NULL",
            rusqlite::params![root.0, path.as_bytes()],
            |row| {
                let dev: Option<i64> = row.get(8)?;
                let ino: Option<i64> = row.get(9)?;
                Ok((
                    EntryId(row.get(0)?),
                    EntryMeta {
                        kind: EntryKind::from_i64(row.get(1)?),
                        size_bytes: row.get::<_, i64>(2)? as u64,
                        alloc_bytes: row.get::<_, i64>(3)? as u64,
                        mtime_ns: row.get(4)?,
                        ctime_ns: row.get(5)?,
                        mode: row.get::<_, i64>(6)? as u32,
                        hidden: row.get::<_, i64>(7)? != 0,
                        object_key: match (dev, ino) {
                            (Some(dev), Some(ino)) => Some(ObjectKey {
                                dev: dev as u64,
                                ino: ino as u64,
                            }),
                            _ => None,
                        },
                        nlink: row.get::<_, i64>(10)? as u32,
                        mime: row.get(11)?,
                    },
                ))
            },
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(IndexError::Sqlite(other)),
        })
}

fn write_entry(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    path: &NativePath,
    meta: &EntryMeta,
) -> Result<EntryId> {
    let name = path.file_name().unwrap_or_else(|| path.clone());
    let parent_id: Option<i64> = match path.parent() {
        Some(parent) => transaction
            .query_row(
                "SELECT entry_id FROM entries WHERE root_id = ?1 AND path_bytes = ?2",
                rusqlite::params![root.0, parent.as_bytes()],
                |row| row.get(0),
            )
            .ok(),
        None => None,
    };
    transaction.execute(
        "INSERT INTO entries
           (root_id, parent_id, name_bytes, name_display, name_fold, path_bytes, path_display,
            kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden, dev, inode, nlink,
            mime, written_generation, last_seen_generation, deleted_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?19, NULL)
         ON CONFLICT (root_id, path_bytes) DO UPDATE SET
            parent_id = excluded.parent_id,
            name_bytes = excluded.name_bytes,
            name_display = excluded.name_display,
            name_fold = excluded.name_fold,
            kind = excluded.kind,
            size_bytes = excluded.size_bytes,
            alloc_bytes = excluded.alloc_bytes,
            mtime_ns = excluded.mtime_ns,
            ctime_ns = excluded.ctime_ns,
            mode = excluded.mode,
            hidden = excluded.hidden,
            dev = excluded.dev,
            inode = excluded.inode,
            nlink = excluded.nlink,
            mime = excluded.mime,
            written_generation = excluded.written_generation,
            last_seen_generation = excluded.last_seen_generation,
            deleted_generation = NULL",
        rusqlite::params![
            root.0,
            parent_id,
            name.as_bytes(),
            name.display(),
            fold(&name.display()),
            path.as_bytes(),
            path.display(),
            meta.kind.as_i64(),
            meta.size_bytes as i64,
            meta.alloc_bytes as i64,
            meta.mtime_ns,
            meta.ctime_ns,
            i64::from(meta.mode),
            i64::from(meta.hidden),
            meta.object_key.map(|key| key.dev as i64),
            meta.object_key.map(|key| key.ino as i64),
            i64::from(meta.nlink),
            meta.mime,
            generation.0 as i64,
        ],
    )?;
    let entry_id: i64 = transaction.query_row(
        "SELECT entry_id FROM entries WHERE root_id = ?1 AND path_bytes = ?2",
        rusqlite::params![root.0, path.as_bytes()],
        |row| row.get(0),
    )?;
    Ok(EntryId(entry_id))
}

pub(crate) fn append_change(connection: &Connection, change: &Change) -> Result<u64> {
    connection.execute(
        "INSERT INTO changes (generation, root_id, op, entry_id, path_bytes, old_path_bytes, kind,
                              size_bytes, mtime_ns, at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            change.generation.0 as i64,
            change.root.0,
            change.op.as_str(),
            change.entry_id.map(|id| id.0),
            change.path.as_bytes(),
            change.old_path.as_ref().map(NativePath::as_bytes),
            change.kind.as_i64(),
            change.size_bytes as i64,
            change.mtime_ns,
            change.at_ms,
        ],
    )?;
    Ok(connection.last_insert_rowid() as u64)
}

/// Keeps the log bounded and records the floor, so `changes_since` can tell a client its
/// continuity is gone instead of silently starting from the middle.
pub(crate) fn prune_changes(connection: &Connection, max_changes: u64) -> Result<()> {
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))?;
    if count as u64 <= max_changes {
        return Ok(());
    }
    let excess = count as u64 - max_changes;
    let floor: i64 = connection.query_row(
        "SELECT seq FROM changes ORDER BY seq LIMIT 1 OFFSET ?1",
        [excess as i64],
        |row| row.get(0),
    )?;
    connection.execute("DELETE FROM changes WHERE seq < ?1", [floor])?;
    schema::write_meta(
        connection,
        "min_retained_change_seq",
        &(floor - 1).to_string(),
    )?;
    Ok(())
}

fn change_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    Ok(Change {
        seq: row.get::<_, i64>(0)? as u64,
        generation: Generation(row.get::<_, i64>(1)? as u64),
        root: RootId(row.get(2)?),
        op: ChangeOp::from_str_or_metadata(&row.get::<_, String>(3)?),
        entry_id: row.get::<_, Option<i64>>(4)?.map(EntryId),
        path: NativePath::from_bytes(row.get::<_, Vec<u8>>(5)?),
        old_path: row
            .get::<_, Option<Vec<u8>>>(6)?
            .map(NativePath::from_bytes),
        kind: EntryKind::from_i64(row.get(7)?),
        size_bytes: row.get::<_, i64>(8)? as u64,
        mtime_ns: row.get(9)?,
        at_ms: row.get(10)?,
    })
}
