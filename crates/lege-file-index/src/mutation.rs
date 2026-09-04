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
        Coverage, EntryId, EntryKind, EntryMeta, Generation, NativePath, ObjectKey, RootId,
        RootPolicy, StaleReason, now_ms,
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

    /// A query wanted metadata this root cannot vouch for.
    ///
    /// Advisory, and deliberately not a request the query waits on: a `sort:size` over an
    /// `Unverified` root serves the rows it has, names the root in its answer, and asks the host
    /// to queue a repair. Blocking the query on a walk of a home directory would be a worse answer
    /// than a labelled one. The default does nothing, so a sink that cannot repair need not say so.
    fn metadata_repair_requested(&self, _root: RootId) {}
}

/// Each touched root's path and policy, read once per batch so the live-write path can stamp the
/// same coverage tier the scanner would have.
///
/// Without this, a watcher event under a build directory would land as a `Detailed` row — the
/// column's own default — and everything the scan path carefully kept out of search would walk
/// back in through `cargo build`.
#[derive(Debug, Default)]
struct Tiers(Vec<(RootId, NativePath, RootPolicy)>);

impl Tiers {
    /// The tier a row at `path` is stored at, or `None` when no row is written for it.
    ///
    /// A root this batch could not read a policy for classifies as `Detailed`: dropping a
    /// mutation because a lookup failed would lose a real change, which is worse than storing one
    /// row at too generous a tier.
    fn coverage_of(&self, root: RootId, path: &NativePath, kind: EntryKind) -> Option<Coverage> {
        let Some((_, base, policy)) = self.0.iter().find(|(id, _, _)| *id == root) else {
            return Some(Coverage::Detailed);
        };
        policy.stored_coverage_within(base, path, kind)
    }
}

impl crate::Index {
    /// Applies a batch: one transaction, one generation, one notification.
    ///
    /// A batch larger than `max_batch` is split, and each part commits on its own — a watcher
    /// storm must not turn into one unbounded transaction.
    // `batch` is taken by value: this is the frozen C5 contract signature (F1-contract.md §7),
    // which F2/Files/CLI already code against. The chunk-splitting path below needs an owned
    // `MutationSource` to build each sub-batch; the transaction path only ever borrows it.
    #[allow(clippy::needless_pass_by_value)]
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
        let tiers = self.tiers_for(&batch);
        let (applied, invalidations) = self.with_write(|connection| {
            self.apply_transaction(connection, &batch, &tiers, max_changes)
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

    /// Each root the batch touches, with its path and policy, read **outside** the write lock:
    /// `root_policy` takes a reader, and a batch touches a handful of roots however many
    /// mutations it holds.
    fn tiers_for(&self, batch: &MutationBatch) -> Tiers {
        let mut tiers = Tiers::default();
        for mutation in &batch.mutations {
            let root = mutation.root();
            if tiers.0.iter().any(|(id, _, _)| *id == root) {
                continue;
            }
            if let (Ok(status), Ok(policy)) = (self.root_status(root), self.root_policy(root)) {
                tiers.0.push((root, status.path, policy));
            } else {
                tracing::debug!(
                    root = root.0,
                    "no policy for a mutated root; its rows keep the default tier"
                );
            }
        }
        tiers
    }

    /// The body of [`Self::apply`]'s one transaction, pulled out only so the method above stays
    /// short: apply every mutation in `batch`, log the changes, and move every touched root's
    /// generation forward. An empty result rolls the transaction back rather than advancing the
    /// generation counter for a batch that changed nothing.
    fn apply_transaction(
        &self,
        connection: &mut Connection,
        batch: &MutationBatch,
        tiers: &Tiers,
        max_changes: u64,
    ) -> Result<(Applied, Vec<(RootId, StaleReason)>)> {
        let transaction = connection.transaction()?;
        let generation = crate::next_generation(&transaction)?;
        let mut changes = Vec::new();
        let mut skipped = 0usize;
        let mut moved_descendants = 0u64;
        let mut invalidations = Vec::new();
        // Per root: how many live rows this transaction added or removed. Tier-agnostic, because
        // `entry_count` counts the rows the index holds and not the rows a search can see — a
        // drive visualiser asking "how many entries does this root have" wants the `Opaque` and
        // `Inventory` ones included.
        let mut touched_roots: Vec<(RootId, i64)> = Vec::new();

        for mutation in &batch.mutations {
            let mut live_delta = 0i64;
            match apply_one(&transaction, generation, tiers, mutation)? {
                Applying::Skipped => skipped += 1,
                Applying::Changed {
                    change,
                    descendants,
                    live_delta: delta,
                } => {
                    moved_descendants += descendants;
                    live_delta = delta;
                    changes.push(change);
                }
                Applying::Invalidated(root, reason) => invalidations.push((root, reason)),
            }
            let root = mutation.root();
            match touched_roots.iter_mut().find(|(id, _)| *id == root) {
                Some((_, total)) => *total += live_delta,
                None => touched_roots.push((root, live_delta)),
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
        for (root, live_delta) in &touched_roots {
            refresh_fts(&transaction, *root, generation)?;
            // Adjusted, not recounted. The `COUNT(*)` this replaces ran over every live row of the
            // root on every batch commit — ~8 ms at 15,338 entries and ~51 ms at 153,326, almost
            // all CPU, which extrapolates to ~330 ms per coalesced watcher batch on a
            // million-entry home. `MAX(0, ...)` is a floor, not a correction: a scan commit still
            // recounts exactly, so any drift is bounded by the next full pass rather than
            // permanent.
            transaction.execute(
                "UPDATE roots
                    SET committed_generation = ?2,
                        entry_count = MAX(0, entry_count + ?3)
                  WHERE root_id = ?1",
                rusqlite::params![root.0, generation.0 as i64, live_delta],
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
    Applied {
        change: Change,
        descendants: u64,
        live_delta: i64,
    },
}

/// What one mutation did.
enum Applying {
    Skipped,
    Changed {
        change: Change,
        descendants: u64,
        /// How the root's live-row count moved. Maintained here rather than recounted, because
        /// the recount was `COUNT(*)` over every live row of the root on **every** batch commit —
        /// measured at ~0.33 us/entry, so ~330 ms per coalesced watcher batch on a million-entry
        /// home directory, for a number the transaction already knows.
        live_delta: i64,
    },
    /// Not a row change at all: a subtree the caller can no longer vouch for.
    Invalidated(RootId, StaleReason),
}

/// One mutation, tier included. Split out of `apply_transaction` so the transaction body stays
/// about the transaction.
fn apply_one(
    transaction: &Transaction<'_>,
    generation: Generation,
    tiers: &Tiers,
    mutation: &Mutation,
) -> Result<Applying> {
    let changed = |(change, live_delta): (Change, i64)| Applying::Changed {
        change,
        descendants: 0,
        live_delta,
    };
    match mutation {
        Mutation::Upsert { root, path, meta } | Mutation::Touch { root, path, meta } => {
            // No row is written for this path under the root's policy — it is the trash, or it
            // lives inside an unexpanded `Opaque` build directory. The change is real; the index
            // simply does not hold it.
            let Some(coverage) = tiers.coverage_of(*root, path, meta.kind) else {
                return Ok(Applying::Skipped);
            };
            Ok(
                upsert(transaction, generation, *root, path, meta, coverage)?
                    .map_or(Applying::Skipped, changed),
            )
        }
        Mutation::Remove { root, path } => {
            Ok(remove(transaction, generation, *root, path)?.map_or(Applying::Skipped, changed))
        }
        Mutation::Move {
            root,
            from,
            to,
            meta,
        } => {
            // Moved somewhere the index holds no rows — dragged into `target/`, or into the
            // trash. That is a deletion as far as the index is concerned, and treating it as one
            // is what stops the row surviving at its old tier under a new path.
            let Some(coverage) = tiers.coverage_of(*root, to, meta.kind) else {
                return Ok(remove(transaction, generation, *root, from)?
                    .map_or(Applying::Skipped, changed));
            };
            Ok(
                match rename(transaction, generation, *root, from, to, meta, coverage)? {
                    RenameOutcome::Skipped => Applying::Skipped,
                    RenameOutcome::Applied {
                        change,
                        descendants,
                        live_delta,
                    } => Applying::Changed {
                        change,
                        descendants,
                        live_delta,
                    },
                },
            )
        }
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
            Ok(Applying::Invalidated(*root, reason.clone()))
        }
    }
}

fn upsert(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    path: &NativePath,
    meta: &EntryMeta,
    coverage: Coverage,
) -> Result<Option<(Change, i64)>> {
    let existing = live_row(transaction, root, path)?;
    if let Some((_, stored)) = &existing
        && stored.matches(meta)
    {
        // The echo of an operation the file service already applied. No row moves, so the count
        // must not move either — this is the case that silently drifts if the delta is assumed
        // from the mutation's *kind* rather than from what the write actually did.
        return Ok(None);
    }
    let entry_id = write_entry(transaction, generation, root, path, meta, coverage)?;
    // `write_entry` upserts and clears `deleted_generation`, so a path with no live row gains one
    // whether it was absent or a tombstone. A revived row is a gain of exactly one, not two.
    let live_delta = i64::from(existing.is_none());
    Ok(Some((
        Change {
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
        },
        live_delta,
    )))
}

fn remove(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    path: &NativePath,
) -> Result<Option<(Change, i64)>> {
    let Some((entry_id, meta)) = live_row(transaction, root, path)? else {
        return Ok(None);
    };
    let generation_id = generation.0 as i64;
    transaction.execute(
        "UPDATE entries SET deleted_generation = ?1 WHERE entry_id = ?2",
        rusqlite::params![generation_id, entry_id.0],
    )?;
    // A directory takes its subtree with it. The `deleted_generation IS NULL` predicate is what
    // makes the affected-row count the *live* rows lost, so a subtree that already held
    // tombstones is not counted twice.
    let mut live_delta = -1i64;
    if meta.kind == EntryKind::Directory {
        let (low, high) = path.subtree_bounds();
        let swept = transaction.execute(
            "UPDATE entries SET deleted_generation = ?1
              WHERE root_id = ?2 AND deleted_generation IS NULL
                AND path_bytes >= ?3 AND path_bytes < ?4",
            rusqlite::params![generation_id, root.0, low, high],
        )?;
        live_delta -= swept as i64;
    }
    Ok(Some((
        Change {
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
        },
        live_delta,
    )))
}

fn rename(
    transaction: &Transaction<'_>,
    generation: Generation,
    root: RootId,
    from: &NativePath,
    to: &NativePath,
    meta: &EntryMeta,
    coverage: Coverage,
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
        return match upsert(transaction, generation, root, to, meta, coverage)? {
            Some((change, live_delta)) => Ok(RenameOutcome::Applied {
                change,
                descendants: 0,
                live_delta,
            }),
            None => Ok(RenameOutcome::Skipped),
        };
    }
    let (entry_id, stored) = source.expect("checked above");
    let generation_id = generation.0 as i64;

    // Anything already sitting at the destination is gone: the move overwrote it.
    let mut live_delta = 0i64;
    if let Some((victim, _)) = live_row(transaction, root, to)? {
        transaction.execute(
            "UPDATE entries SET deleted_generation = ?1 WHERE entry_id = ?2",
            rusqlite::params![generation_id, victim.0],
        )?;
        live_delta -= 1;
    }

    let name = to.file_name().unwrap_or_else(|| to.clone());
    transaction.execute(
        "UPDATE entries
            SET path_bytes = ?1, path_display = ?2, name_bytes = ?3, name_display = ?4,
                name_fold = ?5, kind = ?6, size_bytes = ?7, alloc_bytes = ?8, mtime_ns = ?9,
                ctime_ns = ?10, mode = ?11, hidden = ?12, dev = ?13, inode = ?14, nlink = ?15,
                coverage = ?16,
                written_generation = ?17, last_seen_generation = ?17, deleted_generation = NULL
          WHERE entry_id = ?18",
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
            coverage.as_i64(),
            generation_id,
            entry_id.0,
        ],
    )?;

    // A directory move takes its subtree's paths with it, so a folder drag costs one rewrite
    // rather than a full re-enumeration of everything under it — unless the destination tier
    // holds no descendants at all (a directory renamed to `target`), in which case the subtree
    // stops existing as far as the index is concerned and rewriting its paths would leave every
    // row under it searchable at its old tier.
    let is_directory = stored.kind == EntryKind::Directory || meta.kind == EntryKind::Directory;
    let descendants = if !is_directory {
        0
    } else if coverage.materialises_children() {
        // Paths are rewritten in place: the same rows, somewhere else. The count does not move.
        rename_descendants(transaction, root, from, to, generation_id)?
    } else {
        let (low, high) = from.subtree_bounds();
        let swept = transaction.execute(
            "UPDATE entries SET deleted_generation = ?1
              WHERE root_id = ?2 AND deleted_generation IS NULL
                AND path_bytes >= ?3 AND path_bytes < ?4",
            rusqlite::params![generation_id, root.0, low, high],
        )?;
        live_delta -= swept as i64;
        0
    };

    Ok(RenameOutcome::Applied {
        live_delta,
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

/// The body of [`rename`]'s directory-subtree rewrite, pulled out only so that function stays
/// short: every live descendant of `from` is rewritten to sit under `to` instead, in one prepared
/// statement per row rather than a full re-enumeration of the moved subtree.
fn rename_descendants(
    transaction: &Transaction<'_>,
    root: RootId,
    from: &NativePath,
    to: &NativePath,
    generation_id: i64,
) -> Result<u64> {
    let (low, high) = from.subtree_bounds();
    let rows: Vec<(i64, Vec<u8>)> = {
        let mut statement = transaction.prepare(
            "SELECT entry_id, path_bytes FROM entries
              WHERE root_id = ?1 AND deleted_generation IS NULL
                AND path_bytes >= ?2 AND path_bytes < ?3",
        )?;
        let mapped = statement.query_map(rusqlite::params![root.0, low.clone(), high], |row| {
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
    let mut descendants = 0u64;
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
    Ok(descendants)
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
    coverage: Coverage,
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
            mime, coverage, written_generation, last_seen_generation, deleted_generation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?20, NULL)
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
            coverage = excluded.coverage,
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
            coverage.as_i64(),
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
