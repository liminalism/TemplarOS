//! Incremental change: what the maintenance lane writes, and what every live consumer reads.
//!
//! Two types, one direction each. A caller writes [`Mutation`]s; the index emits [`Change`]s. There
//! is no third internal form, and both carry native path bytes, because a rename that has been
//! through a lossy string cannot be applied to the file it describes.
//!
//! The dedupe rule is what lets `templar-filed` apply a file operation the instant it commits and then
//! ignore the watcher's echo of the same operation seconds later: a mutation whose stored row
//! already says exactly this is a **skip** — no change, no generation bump, counted in
//! [`Applied::skipped`].

use crate::model::{EntryId, EntryKind, EntryMeta, Generation, NativePath, RootId, StaleReason};
use serde::{Deserialize, Serialize};

/// Where a batch came from. Recorded for diagnostics; it does not change the semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationSource {
    /// A filesystem watcher.
    Watcher,
    /// A `templar-filed` operation that has already committed to disk.
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

#[cfg(feature = "store")]
pub(crate) use store::{append_change, prune_changes, read_latest_change_seq};

#[cfg(feature = "store")]
mod store {
    use super::{
        Applied, Change, ChangeOp, ChangesBatch, EntryId, EntryKind, EntryMeta, Generation,
        Mutation, MutationBatch, NativePath, RootId, StaleReason,
    };
    use crate::{
        IndexError, Result,
        generation::{fold, refresh_fts},
        model::{Coverage, ObjectKey, RootPolicy, now_ms},
        schema,
    };
    use rusqlite::{Connection, Transaction};

    /// Each touched root's path and policy, read once per batch so the live-write path can stamp the
    /// same coverage tier the scanner would have.
    ///
    /// Without this, a watcher event under a build directory would land as a `Detailed` row — the
    /// column's own default — and everything the scan path carefully kept out of search would walk
    /// back in through `cargo build`.
    #[derive(Debug, Default)]
    struct Tiers(Vec<TierRoot>);

    #[derive(Debug)]
    struct TierRoot {
        root: RootId,
        base: NativePath,
        policy: RootPolicy,
        /// Registered roots strictly inside this one. Their paths belong to them, so a mutation that
        /// names one against this root (a Btrfs delta, an operation hook) writes nothing.
        boundaries: Vec<NativePath>,
    }

    impl Tiers {
        fn get(&self, root: RootId) -> Option<&TierRoot> {
            self.0.iter().find(|tier| tier.root == root)
        }

        /// The tier a row at `path` is stored at, or `None` when no row is written for it.
        ///
        /// A root this batch could not read a policy for classifies as `Detailed`: dropping a
        /// mutation because a lookup failed would lose a real change, which is worse than storing one
        /// row at too generous a tier.
        fn coverage_of(
            &self,
            root: RootId,
            path: &NativePath,
            kind: EntryKind,
        ) -> Option<Coverage> {
            let Some(tier) = self.get(root) else {
                return Some(Coverage::Detailed);
            };
            if tier
                .boundaries
                .iter()
                .any(|boundary| path.starts_with(boundary))
            {
                return None;
            }
            tier.policy.stored_coverage_within(&tier.base, path, kind)
        }

        /// Where a mutation that resolved to `None` above actually landed: the `Opaque` ancestor whose
        /// cached aggregate no longer accounts for it, or `None` when nothing at this root has such an
        /// aggregate to invalidate (an unknown root, a nested root's path, or a boundary that is
        /// `Ignored` rather than `Opaque` — the trash, which is never armed and so never reaches here
        /// in practice).
        fn opaque_boundary(&self, root: RootId, path: &NativePath) -> Option<NativePath> {
            let tier = self.get(root)?;
            if tier
                .boundaries
                .iter()
                .any(|boundary| path.starts_with(boundary))
            {
                return None;
            }
            let boundary = tier
                .policy
                .unmaterialised_boundary(Some(&tier.base), path)?;
            (tier.policy.coverage_of(&boundary, EntryKind::Directory) == Coverage::Opaque)
                .then_some(boundary)
        }
    }

    impl crate::Index {
        /// Applies a batch: one transaction, one generation, one notification.
        ///
        /// A batch larger than `max_batch` is split, and each part commits on its own — a watcher
        /// storm must not turn into one unbounded transaction. Bounded delta catch-up uses
        /// [`Self::apply_atomic`] instead, so its checkpoint has exactly one commit boundary.
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
            self.apply_atomic(&batch)
        }

        /// Apply an already bounded acquisition as one transaction/generation. The delta decoder's
        /// transfer cap bounds this batch; it must not publish only a prefix against a checkpoint.
        pub fn apply_atomic(&self, batch: &MutationBatch) -> Result<Applied> {
            let roots: std::collections::BTreeSet<_> =
                batch.mutations.iter().map(Mutation::root).collect();
            for root in roots {
                crate::volume::ensure_present(self, root)?;
            }
            let max_changes = self.config().max_changes;
            let tiers = self.tiers_for(batch);
            let (applied, invalidations, stale_aggregates) = self.with_write(|connection| {
                self.apply_transaction(connection, batch, &tiers, max_changes)
            })?;

            for (root, reason) in invalidations {
                self.mark_stale(root, &reason)?;
            }
            // A separate write, deliberately outside the transaction above: staleness is not part of
            // the generation lineage, and a batch that only touched an `Opaque` subtree rolled that
            // transaction back (nothing to publish) while still needing this recorded.
            for (root, boundary) in stale_aggregates {
                self.mark_aggregate_stale(root, &boundary)?;
            }
            if !applied.changes.is_empty() {
                self.mutation_committed();
                let roots: std::collections::BTreeSet<_> =
                    applied.changes.iter().map(|change| change.root).collect();
                self.request_search_overlay_updates(&roots.into_iter().collect::<Vec<_>>());
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
                if tiers.get(root).is_some() {
                    continue;
                }
                if let (Ok(status), Ok(policy), Ok(boundaries)) = (
                    self.root_status(root),
                    self.root_policy(root),
                    self.nested_root_paths(root),
                ) {
                    tiers.0.push(TierRoot {
                        root,
                        base: status.path,
                        policy,
                        boundaries,
                    });
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
        ) -> Result<AppliedTransaction> {
            let transaction = connection.transaction()?;
            // Allocated and published as the head in this transaction; a batch that changes nothing
            // rolls both back below.
            let generation = crate::publish_generation(&transaction)?;
            let mut changes = Vec::new();
            let mut skipped = 0usize;
            let mut moved_descendants = 0u64;
            let mut invalidations = Vec::new();
            let mut stale_aggregates: Vec<(RootId, NativePath)> = Vec::new();
            // Per root: how many live rows this transaction added or removed. Tier-agnostic, because
            // `entry_count` counts the rows the index holds and not the rows a search can see — a
            // drive visualiser asking "how many entries does this root have" wants the `Opaque` and
            // `Inventory` ones included.
            let mut touched_roots: Vec<(RootId, i64)> = Vec::new();

            for mutation in &batch.mutations {
                let mut live_delta = 0i64;
                match apply_one(&transaction, generation, tiers, mutation)? {
                    Applying::Skipped => skipped += 1,
                    Applying::SkippedUnmaterialised { root, boundary } => {
                        skipped += 1;
                        if !stale_aggregates
                            .iter()
                            .any(|(existing_root, existing_path)| {
                                *existing_root == root && *existing_path == boundary
                            })
                        {
                            stale_aggregates.push((root, boundary));
                        }
                    }
                    Applying::Changed {
                        change,
                        siblings,
                        descendants,
                        live_delta: delta,
                    } => {
                        moved_descendants += descendants;
                        live_delta = delta;
                        changes.push(change);
                        changes.extend(siblings);
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
                // advance for a batch that changed nothing. `stale_aggregates` is returned anyway —
                // a batch that only touched an unmaterialised `Opaque` subtree is exactly this case,
                // and the caller marks those aggregates stale as a separate write, outside the
                // generation lineage this rollback protects.
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
                    stale_aggregates,
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
            transaction.commit()?;

            Ok((
                Applied {
                    generation,
                    changes,
                    skipped,
                    moved_descendants,
                },
                invalidations,
                stale_aggregates,
            ))
        }

        /// The highest sequence the change log has issued, or 0 when nothing has been recorded.
        ///
        /// A consumer never has to call this: [`crate::snapshot::SnapshotManifest::snapshot_seq`]
        /// carries it, taken inside the same call that wrote the frame. It is public because that is
        /// where it is read from, and because a diagnostic wants to be able to ask.
        pub fn latest_change_seq(&self) -> Result<u64> {
            self.with_read(read_latest_change_seq)
        }

        /// A bounded page of the change log after `seq`.
        ///
        /// Below the retained floor the answer is [`IndexError::SnapshotRequired`], not a partial
        /// history dressed up as a complete one.
        pub fn changes_since(
            &self,
            seq: u64,
            limit: u32,
            cx: &crate::CallContext,
        ) -> Result<ChangesBatch> {
            let generation = self.generation()?;
            let limit = limit.clamp(1, 10_000);
            self.with_read_in(Some(cx), |connection| {
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

    /// What one transaction pass produced: the batch's own result, roots invalidated outright, and
    /// `Opaque` aggregates a mutation landed under without materialising.
    type AppliedTransaction = (
        Applied,
        Vec<(RootId, StaleReason)>,
        Vec<(RootId, NativePath)>,
    );

    enum RenameOutcome {
        Skipped,
        Applied {
            change: Change,
            siblings: Vec<Change>,
            descendants: u64,
            live_delta: i64,
        },
    }

    /// What one mutation did.
    enum Applying {
        Skipped,
        /// Skipped for a specific, actionable reason: the path lives under an `Opaque` directory's
        /// unmaterialised subtree, so that directory's cached aggregate no longer accounts for
        /// whatever just changed.
        SkippedUnmaterialised {
            root: RootId,
            boundary: NativePath,
        },
        Changed {
            change: Change,
            /// The same inode's *other* indexed names, whose rows this mutation also brought up to
            /// date. Empty for the single-link case, which is very nearly every case, and a `Vec::new`
            /// allocates nothing.
            siblings: Vec<Change>,
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
            siblings: Vec::new(),
            descendants: 0,
            live_delta,
        };
        match mutation {
            Mutation::Upsert { root, path, meta } | Mutation::Touch { root, path, meta } => {
                // No row is written for this path under the root's policy — it is the trash, or it
                // lives inside an unexpanded `Opaque` build directory. The change is real; the index
                // simply does not hold it.
                let Some(coverage) = tiers.coverage_of(*root, path, meta.kind) else {
                    return Ok(match tiers.opaque_boundary(*root, path) {
                        Some(boundary) => Applying::SkippedUnmaterialised {
                            root: *root,
                            boundary,
                        },
                        None => Applying::Skipped,
                    });
                };
                let Some((change, live_delta)) =
                    upsert(transaction, generation, *root, path, meta, coverage)?
                else {
                    return Ok(Applying::Skipped);
                };
                Ok(Applying::Changed {
                    siblings: mirror_hard_links(transaction, generation, *root, path, meta)?,
                    change,
                    descendants: 0,
                    live_delta,
                })
            }
            Mutation::Remove { root, path } => Ok(
                remove(transaction, generation, *root, path)?.map_or(Applying::Skipped, changed)
            ),
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
                            siblings,
                            descendants,
                            live_delta,
                        } => Applying::Changed {
                            change,
                            siblings,
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
                    siblings: mirror_hard_links(transaction, generation, root, to, meta)?,
                    descendants: 0,
                    live_delta,
                }),
                None => Ok(RenameOutcome::Skipped),
            };
        }
        let (entry_id, stored) = source.expect("checked above");
        let generation_id = generation.0 as i64;

        let (entry_id, mut live_delta) =
            place_at(transaction, generation, root, entry_id, to, meta, coverage)?;

        // A directory move takes its subtree's paths with it, so a folder drag costs one rewrite
        // rather than a full re-enumeration of everything under it — unless the destination tier
        // holds no descendants at all (a directory renamed to `target`), in which case the subtree
        // stops existing as far as the index is concerned and rewriting its paths would leave every
        // row under it searchable at its old tier.
        let is_directory = stored.kind == EntryKind::Directory || meta.kind == EntryKind::Directory;
        let descendants = if !is_directory {
            0
        } else if coverage.materialises_children() {
            // Whatever the destination subtree still holds is gone with the directory the move
            // replaced: `rename(2)` leaves the destination holding the source's children and nothing
            // else. Sweeping first is also what keeps the rewrite below from colliding with a live
            // row, and what makes the result equal to a fresh scan rather than a union of two trees.
            // Guarded on `from`: a move *up* into one of its own ancestors would otherwise sweep the
            // very rows the rewrite below is about to re-path. `rename(2)` cannot produce that, but a
            // caller can hand us one, and losing a subtree to it is not a survivable answer.
            if !from.starts_with(to) {
                let (low, high) = to.subtree_bounds();
                let swept = transaction.execute(
                    "UPDATE entries SET deleted_generation = ?1
                  WHERE root_id = ?2 AND deleted_generation IS NULL
                    AND path_bytes >= ?3 AND path_bytes < ?4",
                    rusqlite::params![generation_id, root.0, low, high],
                )?;
                live_delta -= swept as i64;
            }
            // Paths are rewritten in place: the same rows, somewhere else. The count does not move,
            // except where a rewritten path lands on a tombstone that has to give way.
            let (moved, collided) = rename_descendants(transaction, root, from, to, generation_id)?;
            live_delta -= collided;
            moved
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
            siblings: mirror_hard_links(transaction, generation, root, to, meta)?,
            descendants,
        })
    }

    /// Put the row `source` holds at `to`, and say what that cost the root's live-row count.
    ///
    /// Anything already sitting at the destination is gone: the move overwrote it. It cannot be
    /// tombstoned *and* left where it is, because `entries_path` is unique on (root, path) — one row
    /// per path is the oldest rule in this schema. So when the destination is occupied, *its* row is
    /// the row the moved entry now lives in, exactly as `write_entry`'s `ON CONFLICT DO UPDATE` reuses
    /// the tombstone of a file that is recreated over it, and the source row stays where it stood as a
    /// tombstone. [`occupant`] looks past `deleted_generation` on purpose: a tombstone holds its path
    /// just as firmly as a live row.
    ///
    /// The free destination is the ordinary case and keeps the row's identity: one `UPDATE`, no
    /// insert, no second lookup.
    fn place_at(
        transaction: &Transaction<'_>,
        generation: Generation,
        root: RootId,
        source: EntryId,
        to: &NativePath,
        meta: &EntryMeta,
        coverage: Coverage,
    ) -> Result<(EntryId, i64)> {
        let generation_id = generation.0 as i64;
        if let Some((victim, victim_was_live)) = occupant(transaction, root, to, source)? {
            tombstone(transaction, generation_id, source)?;
            write_entry(transaction, generation, root, to, meta, coverage)?;
            return Ok((victim, -i64::from(victim_was_live)));
        }
        let name = to.file_name().unwrap_or_else(|| to.clone());
        transaction.execute(
            "UPDATE entries
            SET path_bytes = ?1, path_display = ?2, name_bytes = ?3, name_display = ?4,
                name_fold = ?5, kind = ?6, size_bytes = ?7, alloc_bytes = ?8, mtime_ns = ?9,
                ctime_ns = ?10, mode = ?11, hidden = ?12, dev = ?13, inode = ?14, nlink = ?15,
                coverage = ?16, parent_id = ?19,
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
                source.0,
                parent_id(transaction, root, to)?,
            ],
        )?;
        Ok((source, 0))
    }

    /// The row that already holds `path` — tombstoned or not — other than `keeper`.
    ///
    /// Deliberately not `live_row`: `entries_path` is unique on (root, path) whatever
    /// `deleted_generation` says, so a tombstone occupies its path just as firmly as a live row does.
    /// The `bool` is whether that row was still live, which is exactly what the move costs the root's
    /// live-row count.
    fn occupant(
        transaction: &Transaction<'_>,
        root: RootId,
        path: &NativePath,
        keeper: EntryId,
    ) -> Result<Option<(EntryId, bool)>> {
        transaction
            .query_row(
                "SELECT entry_id, deleted_generation IS NULL FROM entries
              WHERE root_id = ?1 AND path_bytes = ?2 AND entry_id <> ?3",
                rusqlite::params![root.0, path.as_bytes(), keeper.0],
                |row| Ok((EntryId(row.get(0)?), row.get::<_, i64>(1)? != 0)),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(IndexError::Sqlite(other)),
            })
    }

    fn tombstone(transaction: &Transaction<'_>, generation_id: i64, entry: EntryId) -> Result<()> {
        transaction.execute(
            "UPDATE entries SET deleted_generation = ?1 WHERE entry_id = ?2",
            rusqlite::params![generation_id, entry.0],
        )?;
        Ok(())
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
    ) -> Result<(u64, i64)> {
        let (low, high) = from.subtree_bounds();
        let rows: Vec<(i64, Vec<u8>)> = {
            let mut statement = transaction.prepare(
                "SELECT entry_id, path_bytes FROM entries
              WHERE root_id = ?1 AND deleted_generation IS NULL
                AND path_bytes >= ?2 AND path_bytes < ?3 ORDER BY path_bytes",
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
                            last_seen_generation = ?3, parent_id = ?5
          WHERE entry_id = ?4",
        )?;
        let mut descendants = 0u64;
        let mut collided = 0i64;
        for (id, old) in rows {
            let mut rebuilt = to.as_bytes().to_vec();
            rebuilt.extend_from_slice(&old[prefix_len..]);
            let rebuilt = NativePath::from_bytes(rebuilt);
            // The destination path may still be held by a tombstone — a file deleted there before the
            // directory it lived in was replaced. One row per path, so the row that is already there
            // is the row this descendant moves into, and the descendant's own row stays behind as a
            // tombstone. The caller has already swept anything *live* under the destination, so a
            // collision here costs the live count nothing; the count is threaded out regardless
            // rather than assumed, because a caller that skips the sweep would make it wrong.
            if let Some((victim, victim_was_live)) =
                occupant(transaction, root, &rebuilt, EntryId(id))?
            {
                take_over(transaction, generation_id, victim, EntryId(id))?;
                tombstone(transaction, generation_id, EntryId(id))?;
                collided += i64::from(victim_was_live);
                transaction.execute(
                    "UPDATE entries SET parent_id = ?1 WHERE entry_id = ?2",
                    rusqlite::params![parent_id(transaction, root, &rebuilt)?, victim.0],
                )?;
            } else {
                statement.execute(rusqlite::params![
                    rebuilt.as_bytes(),
                    rebuilt.display(),
                    generation_id,
                    id,
                    parent_id(transaction, root, &rebuilt)?,
                ])?;
            }
            descendants += 1;
        }
        Ok((descendants, collided))
    }

    /// `victim` — a row that already holds a path a move is claiming — becomes `source`.
    ///
    /// Everything but the identity and the address is copied across: the two rows describe the same
    /// name in the same directory, so `name_*` and `path_*` already agree, and `parent_id` follows the
    /// path rather than the row. The alternative would be deleting one of them, and this schema
    /// deletes rows in exactly one place: [`crate::generation`]'s tombstone collector.
    fn take_over(
        transaction: &Transaction<'_>,
        generation_id: i64,
        victim: EntryId,
        source: EntryId,
    ) -> Result<()> {
        transaction.execute(
            "UPDATE entries
            SET (kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden, dev, inode,
                 nlink, mime, coverage)
              = (SELECT kind, size_bytes, alloc_bytes, mtime_ns, ctime_ns, mode, hidden, dev,
                        inode, nlink, mime, coverage FROM entries WHERE entry_id = ?2),
                written_generation = ?3, last_seen_generation = ?3, deleted_generation = NULL
          WHERE entry_id = ?1",
            rusqlite::params![victim.0, source.0, generation_id],
        )?;
        Ok(())
    }

    /// The same inode's *other* indexed names, brought up to date with the write that just landed.
    ///
    /// One inode, several paths: writing through `main.rs` changes the file behind `main-alias.rs`
    /// too, but the event that reports it — inotify's, a send stream's, or a file service's own hook —
    /// names only the path that was written. Left alone, every other name of that inode keeps a stale
    /// size, mode and mtime until the next full scan, which is how a hard-linked pair ends up
    /// disagreeing with a fresh crawl. `entries_object` makes the sibling lookup one indexed probe,
    /// and `nlink > 1` gates it so the single-link path — very nearly every path — never pays for it.
    ///
    /// The siblings' rows are genuinely distinct entries that happen to share an inode, so only the
    /// object's own attributes travel: never the name, the path, or the coverage tier.
    fn mirror_hard_links(
        transaction: &Transaction<'_>,
        generation: Generation,
        root: RootId,
        path: &NativePath,
        meta: &EntryMeta,
    ) -> Result<Vec<Change>> {
        if meta.nlink <= 1 {
            return Ok(Vec::new());
        }
        let Some(key) = meta.object_key else {
            return Ok(Vec::new());
        };
        let stale: Vec<(i64, Vec<u8>)> = {
            let mut statement = transaction.prepare(
                "SELECT entry_id, path_bytes FROM entries
              WHERE root_id = ?1 AND dev = ?2 AND inode = ?3 AND deleted_generation IS NULL
                AND path_bytes <> ?4
                AND (size_bytes <> ?5 OR alloc_bytes <> ?6 OR mtime_ns <> ?7 OR ctime_ns <> ?8
                     OR mode <> ?9 OR nlink <> ?10)",
            )?;
            let rows = statement.query_map(
                rusqlite::params![
                    root.0,
                    key.dev as i64,
                    key.ino as i64,
                    path.as_bytes(),
                    meta.size_bytes as i64,
                    meta.alloc_bytes as i64,
                    meta.mtime_ns,
                    meta.ctime_ns,
                    i64::from(meta.mode),
                    i64::from(meta.nlink),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut stale = Vec::new();
            for row in rows {
                stale.push(row?);
            }
            stale
        };
        if stale.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = transaction.prepare(
            "UPDATE entries
            SET size_bytes = ?1, alloc_bytes = ?2, mtime_ns = ?3, ctime_ns = ?4, mode = ?5,
                nlink = ?6, written_generation = ?7, last_seen_generation = ?7
          WHERE entry_id = ?8",
        )?;
        let mut changes = Vec::with_capacity(stale.len());
        for (entry_id, sibling) in stale {
            statement.execute(rusqlite::params![
                meta.size_bytes as i64,
                meta.alloc_bytes as i64,
                meta.mtime_ns,
                meta.ctime_ns,
                i64::from(meta.mode),
                i64::from(meta.nlink),
                generation.0 as i64,
                entry_id,
            ])?;
            changes.push(Change {
                seq: 0,
                generation,
                root,
                op: ChangeOp::Metadata,
                entry_id: Some(EntryId(entry_id)),
                path: NativePath::from_bytes(sibling),
                old_path: None,
                kind: meta.kind,
                size_bytes: meta.size_bytes,
                mtime_ns: meta.mtime_ns,
                at_ms: now_ms(),
            });
        }
        Ok(changes)
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

    fn parent_id(
        transaction: &Transaction<'_>,
        root: RootId,
        path: &NativePath,
    ) -> Result<Option<i64>> {
        use rusqlite::OptionalExtension as _;
        let Some(parent) = path.parent() else {
            return Ok(None);
        };
        Ok(transaction
            .query_row(
                "SELECT entry_id FROM entries
          WHERE root_id = ?1 AND path_bytes = ?2 AND deleted_generation IS NULL",
                rusqlite::params![root.0, parent.as_bytes()],
                |row| row.get(0),
            )
            .optional()?)
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
        let parent_id = parent_id(transaction, root, path)?;
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
    ///
    /// No `COUNT(*)`: the floor is the newest row outside the retained window, found by one
    /// indexed lookup, and a log that already fits costs exactly that lookup per commit.
    pub(crate) fn prune_changes(connection: &Connection, max_changes: u64) -> Result<()> {
        use rusqlite::OptionalExtension as _;
        let offset = i64::try_from(max_changes).unwrap_or(i64::MAX);
        let floor: Option<i64> = connection
            .query_row(
                "SELECT seq FROM changes ORDER BY seq DESC LIMIT 1 OFFSET ?1",
                [offset],
                |row| row.get(0),
            )
            .optional()?;
        let Some(floor) = floor else {
            return Ok(());
        };
        connection.execute("DELETE FROM changes WHERE seq <= ?1", [floor])?;
        schema::write_meta(connection, "min_retained_change_seq", &floor.to_string())?;
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

    /// The highest change sequence, on a caller-supplied connection — so a reader that must see the
    /// cursor and the rows it describes in one view (`Index::write_snapshot`) reads it inside its own
    /// transaction rather than through a second pooled connection.
    pub(crate) fn read_latest_change_seq(connection: &rusqlite::Connection) -> Result<u64> {
        Ok(connection
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM changes", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|seq| seq as u64)?)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::MutationSource;
        use crate::{Index, IndexConfig, ScanOptions, model::RootPolicy};
        use std::{
            collections::BTreeMap,
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };

        /// The columns a fresh crawl and an incrementally-maintained index have to agree on, keyed by
        /// path. The same comparison the Btrfs delta fixture makes, without needing Btrfs.
        type Fingerprint = BTreeMap<Vec<u8>, (i64, i64, i64, i64, i64, Option<i64>, i64)>;

        fn open(database: &Path) -> Index {
            Index::open(database, IndexConfig::default()).expect("opening an index")
        }

        fn scanned(database: &Path, tree: &Path) -> (Index, RootId) {
            let index = open(database);
            let root = index
                .add_root(&NativePath::from_path(tree), RootPolicy::default_home())
                .expect("adding the root")
                .root;
            index
                .scan_root(root, ScanOptions::default())
                .expect("the first pass");
            (index, root)
        }

        fn fingerprint(index: &Index, root: RootId) -> Fingerprint {
            index
                .with_read(|connection| {
                    let mut statement = connection.prepare(
                    "SELECT path_bytes, kind, size_bytes, mtime_ns, ctime_ns, mode, inode, nlink
                       FROM entries
                      WHERE root_id = ?1 AND deleted_generation IS NULL
                      ORDER BY path_bytes",
                )?;
                    let rows = statement.query_map([root.0], |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            (
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                                row.get(6)?,
                                row.get(7)?,
                            ),
                        ))
                    })?;
                    let mut out = Fingerprint::new();
                    for row in rows {
                        let (path, facts) = row?;
                        out.insert(path, facts);
                    }
                    Ok(out)
                })
                .expect("reading the rows back")
        }

        /// The only assertion that matters for an incremental writer: what it holds is what a crawl
        /// of the same tree, right now, would have written.
        ///
        /// The root's own row is left out. Creating or deleting a child restamps its parent
        /// directory's mtime, and no producer of incremental mutations claims otherwise: nothing here
        /// tells the index that the root directory itself was touched, so comparing it would be
        /// measuring the fixture rather than the writer. Every row *under* the root is compared.
        fn agrees_with_a_fresh_scan(index: &Index, root: RootId, tree: &Path, scratch: &Path) {
            let (fresh_index, fresh_root) = scanned(&scratch.join("fresh.sqlite3"), tree);
            let mut fresh = fingerprint(&fresh_index, fresh_root);
            let mut held = fingerprint(index, root);
            fresh.remove(native(tree).as_bytes());
            held.remove(native(tree).as_bytes());
            let show = |path: &[u8]| String::from_utf8_lossy(path).into_owned();
            let mut problems = Vec::new();
            for (path, facts) in &fresh {
                match held.get(path) {
                    None => problems.push(format!("missing: {}", show(path))),
                    Some(other) if other != facts => problems.push(format!(
                        "differs at {}:\n  held:  {other:?}\n  fresh: {facts:?}",
                        show(path)
                    )),
                    Some(_) => {}
                }
            }
            for path in held.keys() {
                if !fresh.contains_key(path) {
                    problems.push(format!("held but gone from disk: {}", show(path)));
                }
            }
            assert!(problems.is_empty(), "{}", problems.join("\n"));
        }

        fn meta_of(path: &Path) -> EntryMeta {
            EntryMeta::from_symlink_metadata(
                path,
                &std::fs::symlink_metadata(path).expect("stat of a fixture path"),
            )
        }

        fn native(path: &Path) -> NativePath {
            NativePath::from_path(path)
        }

        fn apply(index: &Index, mutations: Vec<Mutation>) -> Applied {
            index
                .apply(MutationBatch::new(MutationSource::Watcher, mutations))
                .expect("applying a batch")
        }

        /// `entry_count` is arithmetic on the hot path, so every new way the count can move has to be
        /// checked against the `COUNT(*)` it replaced.
        fn count_agrees(index: &Index, root: RootId) {
            let counted = index.entry_count(root).expect("counting");
            let stated = index.root_status(root).expect("status").entry_count;
            assert_eq!(stated, counted, "the maintained entry_count drifted");
        }

        struct Fixture {
            temp: tempfile::TempDir,
            tree: PathBuf,
            index: Index,
            root: RootId,
        }

        fn fixture(build: impl FnOnce(&Path)) -> Fixture {
            let temp = tempfile::tempdir().expect("a temporary directory");
            let tree = temp.path().join("tree");
            std::fs::create_dir_all(&tree).expect("the tree");
            build(&tree);
            let (index, root) = scanned(&temp.path().join("index.sqlite3"), &tree);
            Fixture {
                temp,
                tree,
                index,
                root,
            }
        }

        impl Fixture {
            fn settled(&self) {
                agrees_with_a_fresh_scan(&self.index, self.root, &self.tree, self.temp.path());
                count_agrees(&self.index, self.root);
            }
        }

        /// One inode with two indexed names. The write goes through one of them; the *other* name's
        /// row has to learn about it too, or it keeps the old size and mode until the next full scan.
        ///
        /// This is not a Btrfs problem. inotify reports the name that was opened, a send stream names
        /// the path it wrote, and `templar-filed`'s own hook names the path the operation took — every
        /// producer names one path per write, and every one of them leaves the other names stale.
        #[test]
        fn a_hard_links_other_names_do_not_keep_stale_metadata() {
            let fixture = fixture(|tree| {
                std::fs::create_dir_all(tree.join("src")).unwrap();
                std::fs::write(tree.join("src/main.rs"), b"fn main() {}").unwrap();
                std::fs::hard_link(tree.join("src/main.rs"), tree.join("src/main-alias.rs"))
                    .unwrap();
            });

            // Truncated, chmod'd and rewritten through one name only — the scenario the delta gate
            // runs, minus the delta.
            let written = fixture.tree.join("src/main.rs");
            std::fs::write(&written, b"rewritten").unwrap();
            std::fs::set_permissions(
                &written,
                std::os::unix::fs::PermissionsExt::from_mode(0o600),
            )
            .unwrap();

            let applied = apply(
                &fixture.index,
                vec![Mutation::Upsert {
                    root: fixture.root,
                    path: native(&written),
                    meta: meta_of(&written),
                }],
            );
            assert_eq!(
                applied.changes.len(),
                2,
                "both names of the inode changed, so a consumer holding a projection hears about both"
            );
            fixture.settled();
        }

        /// Two names, and the second one is written before the first one's row is written back. The
        /// mirror must not resurrect the older metadata over the newer.
        #[test]
        fn mirroring_a_hard_link_does_not_undo_a_later_write_in_the_same_batch() {
            let fixture = fixture(|tree| {
                std::fs::write(tree.join("one"), b"first").unwrap();
                std::fs::hard_link(tree.join("one"), tree.join("two")).unwrap();
            });
            let one = fixture.tree.join("one");
            let two = fixture.tree.join("two");
            std::fs::write(&one, b"rewritten through one").unwrap();
            let after = meta_of(&one);

            apply(
                &fixture.index,
                vec![
                    Mutation::Upsert {
                        root: fixture.root,
                        path: native(&one),
                        meta: after.clone(),
                    },
                    Mutation::Upsert {
                        root: fixture.root,
                        path: native(&two),
                        meta: after,
                    },
                ],
            );
            fixture.settled();
        }

        /// `rename(2)` overwrites its destination. The index has one row per path — `entries_path` is
        /// unique — so the row already there cannot be tombstoned *and* left in place; the moved entry
        /// has to take it over. Before this, the batch died on the UNIQUE constraint.
        #[test]
        fn a_move_onto_an_occupied_path_takes_the_row_over() {
            let fixture = fixture(|tree| {
                std::fs::write(tree.join("a.txt"), b"aaaaaaaa").unwrap();
                std::fs::write(tree.join("b.txt"), b"b").unwrap();
            });
            std::fs::rename(fixture.tree.join("a.txt"), fixture.tree.join("b.txt")).unwrap();
            let to = fixture.tree.join("b.txt");

            let applied = apply(
                &fixture.index,
                vec![Mutation::Move {
                    root: fixture.root,
                    from: native(&fixture.tree.join("a.txt")),
                    to: native(&to),
                    meta: meta_of(&to),
                }],
            );
            assert_eq!(applied.changes.len(), 1);
            assert_eq!(applied.changes[0].op, ChangeOp::Moved);
            fixture.settled();
        }

        /// The same thing one layer down: the destination path is held by a *tombstone* rather than a
        /// live row, which `live_row` cannot see but the unique index still enforces.
        #[test]
        fn a_move_onto_a_tombstoned_path_takes_the_row_over() {
            let fixture = fixture(|tree| {
                std::fs::write(tree.join("a.txt"), b"aaaaaaaa").unwrap();
                std::fs::write(tree.join("b.txt"), b"b").unwrap();
            });
            std::fs::remove_file(fixture.tree.join("b.txt")).unwrap();
            apply(
                &fixture.index,
                vec![Mutation::Remove {
                    root: fixture.root,
                    path: native(&fixture.tree.join("b.txt")),
                }],
            );
            std::fs::rename(fixture.tree.join("a.txt"), fixture.tree.join("b.txt")).unwrap();
            let to = fixture.tree.join("b.txt");
            apply(
                &fixture.index,
                vec![Mutation::Move {
                    root: fixture.root,
                    from: native(&fixture.tree.join("a.txt")),
                    to: native(&to),
                    meta: meta_of(&to),
                }],
            );
            fixture.settled();
        }

        /// A directory rename whose destination still has rows underneath it — live ones the index
        /// never heard were deleted, and tombstoned ones each rewritten descendant path lands on.
        /// `rename(2)` leaves the destination holding the source's children and nothing else, so the
        /// index has to converge on exactly that.
        #[test]
        fn a_directory_rename_replaces_whatever_the_destination_subtree_held() {
            let fixture = fixture(|tree| {
                std::fs::create_dir_all(tree.join("branch/leaf")).unwrap();
                std::fs::write(tree.join("branch/leaf/kept.txt"), b"kept").unwrap();
                std::fs::write(tree.join("branch/shared.txt"), b"source").unwrap();
                std::fs::create_dir_all(tree.join("destination")).unwrap();
                std::fs::write(tree.join("destination/shared.txt"), b"doomed").unwrap();
                std::fs::write(tree.join("destination/stale.txt"), b"doomed too").unwrap();
            });

            // `destination/stale.txt` is deleted behind the index's back: its row stays live, which is
            // exactly the stale state a watcher gap leaves. `destination/shared.txt` is deleted with
            // the index watching, so its path is held by a tombstone that the rewrite of
            // `branch/shared.txt` has to land on.
            std::fs::remove_file(fixture.tree.join("destination/shared.txt")).unwrap();
            apply(
                &fixture.index,
                vec![Mutation::Remove {
                    root: fixture.root,
                    path: native(&fixture.tree.join("destination/shared.txt")),
                }],
            );
            std::fs::remove_file(fixture.tree.join("destination/stale.txt")).unwrap();
            std::fs::remove_dir(fixture.tree.join("destination")).unwrap();
            std::fs::rename(
                fixture.tree.join("branch"),
                fixture.tree.join("destination"),
            )
            .unwrap();

            let to = fixture.tree.join("destination");
            let applied = apply(
                &fixture.index,
                vec![Mutation::Move {
                    root: fixture.root,
                    from: native(&fixture.tree.join("branch")),
                    to: native(&to),
                    meta: meta_of(&to),
                }],
            );
            assert_eq!(
                applied.moved_descendants, 3,
                "leaf/, leaf/kept.txt and shared.txt travelled with the directory"
            );
            fixture.settled();
        }

        /// A create and a move in one batch, addressing the same path from both ends.
        ///
        /// First the create the move then overwrites — the shape a `touch b; mv a b` burst arrives in.
        /// Then the create that lands on the tombstone the move just left behind at its source.
        #[test]
        fn a_create_and_a_move_over_the_same_path_settle_in_one_batch() {
            let fixture = fixture(|tree| {
                std::fs::write(tree.join("source"), b"source").unwrap();
            });
            std::fs::write(fixture.tree.join("target"), b"placed first").unwrap();
            let created = meta_of(&fixture.tree.join("target"));
            std::fs::rename(fixture.tree.join("source"), fixture.tree.join("target")).unwrap();
            let target = fixture.tree.join("target");
            apply(
                &fixture.index,
                vec![
                    Mutation::Upsert {
                        root: fixture.root,
                        path: native(&target),
                        meta: created,
                    },
                    Mutation::Move {
                        root: fixture.root,
                        from: native(&fixture.tree.join("source")),
                        to: native(&target),
                        meta: meta_of(&target),
                    },
                ],
            );
            fixture.settled();

            // And the other way round: the move vacates `target`, and a create fills it again in the
            // same batch — onto the tombstone the move left there.
            std::fs::rename(fixture.tree.join("target"), fixture.tree.join("moved-on")).unwrap();
            std::fs::write(fixture.tree.join("target"), b"filled again").unwrap();
            let moved_on = fixture.tree.join("moved-on");
            apply(
                &fixture.index,
                vec![
                    Mutation::Move {
                        root: fixture.root,
                        from: native(&target),
                        to: native(&moved_on),
                        meta: meta_of(&moved_on),
                    },
                    Mutation::Upsert {
                        root: fixture.root,
                        path: native(&target),
                        meta: meta_of(&target),
                    },
                ],
            );
            fixture.settled();
        }

        /// A batch bigger than `max_batch` commits in chunks, so nothing above may depend on two
        /// mutations sharing a transaction.
        #[test]
        fn a_move_onto_an_occupied_path_survives_a_chunk_boundary() {
            let temp = tempfile::tempdir().expect("a temporary directory");
            let tree = temp.path().join("tree");
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join("a"), b"aaaa").unwrap();
            std::fs::write(tree.join("b"), b"b").unwrap();
            std::fs::write(tree.join("c"), b"cc").unwrap();
            let index = Index::open(
                &temp.path().join("index.sqlite3"),
                IndexConfig {
                    max_batch: 1,
                    ..IndexConfig::default()
                },
            )
            .expect("opening the index");
            let root = index
                .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
                .expect("adding the root")
                .root;
            index.scan_root(root, ScanOptions::default()).unwrap();

            std::fs::rename(tree.join("a"), tree.join("b")).unwrap();
            std::fs::rename(tree.join("c"), tree.join("a")).unwrap();
            apply(
                &index,
                vec![
                    Mutation::Move {
                        root,
                        from: native(&tree.join("a")),
                        to: native(&tree.join("b")),
                        meta: meta_of(&tree.join("b")),
                    },
                    Mutation::Move {
                        root,
                        from: native(&tree.join("c")),
                        to: native(&tree.join("a")),
                        meta: meta_of(&tree.join("a")),
                    },
                ],
            );
            agrees_with_a_fresh_scan(&index, root, &tree, temp.path());
            count_agrees(&index, root);
        }

        /// The hard-link mirror costs an indexed probe, and it must be charged to hard links only.
        ///
        /// The measurement it protects: a warm single-mutation batch was taken to 0.3 ms at 153,326
        /// entries by making the batch cost the batch and not the tree. A per-mutation sibling lookup
        /// on the ordinary `nlink == 1` path would put a constant back on every watcher event, and a
        /// lookup that scanned instead of probing would put the tree back. So this asserts both: an
        /// absolute ceiling, and that the cost does not grow with the tree.
        ///
        /// Best-of-N rather than a mean, because a mean on a shared CI machine measures the machine.
        /// The rows are written straight through `apply`, so the whole test needs no filesystem tree.
        #[test]
        fn a_single_link_batch_costs_the_batch_and_not_the_tree() {
            fn best_single_mutation_batch(entries: usize) -> Duration {
                let temp = tempfile::tempdir().expect("a temporary directory");
                let tree = temp.path().join("tree");
                std::fs::create_dir_all(&tree).expect("the tree");
                let index = open(&temp.path().join("index.sqlite3"));
                let root = index
                    .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
                    .expect("adding the root")
                    .root;
                let synthetic = |n: usize| EntryMeta {
                    kind: EntryKind::Regular,
                    size_bytes: n as u64,
                    alloc_bytes: 4096,
                    mtime_ns: n as i64,
                    ctime_ns: n as i64,
                    mode: 0o100_644,
                    hidden: false,
                    object_key: Some(ObjectKey {
                        dev: 1,
                        ino: n as u64,
                    }),
                    nlink: 1,
                    mime: None,
                };
                let bulk = (0..entries)
                    .map(|n| Mutation::Upsert {
                        root,
                        path: native(&tree.join(format!("file{n:07}"))),
                        meta: synthetic(n),
                    })
                    .collect();
                apply(&index, bulk);

                let mut best = Duration::MAX;
                for round in 0..12 {
                    let path = tree.join("hot.txt");
                    let mutation = Mutation::Upsert {
                        root,
                        path: native(&path),
                        meta: synthetic(entries + round),
                    };
                    let started = Instant::now();
                    apply(&index, vec![mutation]);
                    // The first few rounds pay for a cold page cache, not for the tree.
                    if round >= 4 {
                        best = best.min(started.elapsed());
                    }
                }
                best
            }

            let small = best_single_mutation_batch(5_000);
            let large = best_single_mutation_batch(20_000);
            println!(
                "single-mutation batch: {:.3} ms at 5,000 entries, {:.3} ms at 20,000",
                small.as_secs_f64() * 1000.0,
                large.as_secs_f64() * 1000.0,
            );
            assert!(
                large < Duration::from_millis(20),
                "a warm single-mutation batch took {large:?}; the batch is paying for the tree again"
            );
            assert!(
                large < small * 4 + Duration::from_millis(2),
                "the batch cost grew with the tree: {small:?} at 5,000 entries, {large:?} at 20,000"
            );
        }
    }
}
