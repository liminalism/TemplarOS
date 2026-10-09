//! Bounded committed replacements for an immutable root image. SQLite remains the authority;
//! a published overlay certifies one root generation and one change position from the same view.
use crate::{CallContext, EntryId, EntryKind, Generation, IndexError, Result, RootId};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub(crate) struct OverlayRow {
    pub entry_id: EntryId,
    pub written_generation: Generation,
    pub parent_id: Option<EntryId>,
    pub path: Box<[u8]>,
    pub name: Box<[u8]>,
    pub name_fold: Box<[u8]>,
    pub kind: u8,
    pub flags: u8,
    pub size: u64,
    pub mtime: i64,
}

impl OverlayRow {
    fn bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.path.len() + self.name.len() + self.name_fold.len()
    }
}
fn id_table_bytes(capacity: usize) -> usize {
    if capacity == 0 {
        0
    } else {
        (capacity + 1).next_power_of_two() * (std::mem::size_of::<EntryId>() + 1)
    }
}
// Budget both the temporary B-tree nodes and the destination boxed row structs during collection.
const COPY_ROW_BYTES: usize = std::mem::size_of::<OverlayRow>() + 96;

#[derive(Debug)]
pub(crate) struct Overlay {
    pub generation: Generation,
    pub change_seq: u64,
    /// Only IDs present in the base. An upsert replaces that base identity regardless of rank.
    pub removed: HashSet<EntryId>,
    /// One latest (entry ID, written generation) identity per live replacement.
    pub upserts: Box<[OverlayRow]>,
    bytes: usize,
    _reservation: Option<crate::arrays::Reservation>,
}
impl Overlay {
    pub(crate) fn empty(generation: Generation, change_seq: u64) -> Self {
        Self {
            generation,
            change_seq,
            removed: HashSet::new(),
            upserts: Box::new([]),
            bytes: 0,
            _reservation: None,
        }
    }
    /// A changed base row with an upsert counts once, rather than as both a deletion and an add.
    pub(crate) fn row_count(&self) -> usize {
        self.removed.len()
            + self
                .upserts
                .iter()
                .filter(|row| !self.removed.contains(&row.entry_id))
                .count()
    }
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The overlay refresh validates lineage and budgets within one SQLite read transaction."
    )]
    pub(crate) fn refresh(
        connection: &rusqlite::Connection,
        shard: &crate::arrays::RootShard,
        retained: Arc<std::sync::atomic::AtomicUsize>,
        cap: usize,
        cx: &CallContext,
    ) -> Result<Self> {
        let view = connection.unchecked_transaction()?;
        let generation = Generation(view.query_row(
            "SELECT committed_generation FROM roots WHERE root_id=?1",
            [shard.root.0],
            |row| row.get::<_, i64>(0),
        )? as u64);
        let change_seq = crate::mutation::read_latest_change_seq(&view)?;
        let previous = &shard.overlay;
        if generation < previous.generation {
            return Err(IndexError::SnapshotRequired {
                reason: "root generation moved behind its overlay",
            });
        }
        let floor: u64 = crate::schema::read_meta(&view, "min_retained_change_seq")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        if previous.change_seq < floor {
            return Err(IndexError::SnapshotRequired {
                reason: "overlay change position was pruned",
            });
        }
        let (count, reset): (i64, i64) = view.query_row(
            "SELECT COUNT(*), COALESCE(MAX(op='rescan'),0) FROM changes
             WHERE seq>?1 AND root_id=?2",
            rusqlite::params![previous.change_seq as i64, shard.root.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if reset != 0 || (generation != previous.generation && count == 0) {
            return Err(IndexError::SnapshotRequired {
                reason: "root needs a new base image",
            });
        }
        cx.check()?;
        let bound = shard.bound();
        // Directory markers deliberately summarize moves/deletes in the public change feed.
        // Indexed row generations expand them here, including overwritten identities and every
        // descendant, in the very same view. A LIMIT refuses a large subtree without decoding it.
        let sql = "SELECT e.entry_id,e.written_generation,e.parent_id,e.path_bytes,e.name_bytes,
                    e.name_fold,e.kind,e.hidden,e.coverage,e.size_bytes,e.mtime_ns,
                    e.deleted_generation IS NULL
                   FROM entries e JOIN (
                    SELECT entry_id FROM entries INDEXED BY entries_written
                     WHERE root_id=?1 AND written_generation>?2
                    UNION
                    SELECT entry_id FROM entries INDEXED BY entries_tombstoned
                     WHERE root_id=?1 AND deleted_generation>?2
                   ) changed USING(entry_id) LIMIT ?3";
        let mut statement = view.prepare(sql)?;
        let mut rows = statement.query(rusqlite::params![
            shard.root.0,
            previous.generation.0 as i64,
            (bound + 1) as i64
        ])?;
        // Reserve scratch and the copied image before allocating them. Each later replacement
        // grows the reservation before its native fields are decoded; concurrent builds and
        // reader-pinned old overlays participate in the same cap.
        let scratch_bytes = if shard.len() == 0 {
            0
        } else {
            (shard.len().saturating_mul(8).div_ceil(7) + 1).next_power_of_two()
                * (std::mem::size_of::<EntryId>() + 1)
        };
        let _scratch =
            crate::arrays::Reservation::reserve(scratch_bytes, Arc::clone(&retained), cap)?;
        let mut reservation = crate::arrays::Reservation::reserve(
            previous.bytes + previous.upserts.len() * COPY_ROW_BYTES,
            retained,
            cap,
        )?;
        let base_ids = shard.base_ids();
        let mut payload_bytes: usize = previous.upserts.iter().map(OverlayRow::bytes).sum();
        let mut removed = previous.removed.clone();
        let mut upserts: BTreeMap<EntryId, OverlayRow> = previous
            .upserts
            .iter()
            .map(|row| (row.entry_id, row.clone()))
            .collect();
        let mut changed = 0;
        let mut affected = previous.row_count();
        while let Some(row) = rows.next()? {
            changed += 1;
            if changed > bound {
                return Err(IndexError::SnapshotRequired {
                    reason: "changed subtree exceeds overlay bound",
                });
            }
            if changed % 512 == 0 {
                cx.check()?;
            }
            let id = EntryId(row.get(0)?);
            let was_affected = removed.contains(&id) || upserts.contains_key(&id);
            if base_ids.contains(&id) {
                removed.insert(id);
            }
            if !was_affected && removed.contains(&id) {
                affected += 1;
            }
            if !row.get::<_, bool>(11)? {
                if let Some(old) = upserts.remove(&id) {
                    payload_bytes -= old.bytes();
                    if !removed.contains(&id) {
                        affected -= 1;
                    }
                }
                reservation.resize(
                    id_table_bytes(removed.capacity())
                        + payload_bytes
                        + upserts.len() * COPY_ROW_BYTES,
                    cap,
                )?;
                continue;
            }
            let kind = row.get::<_, i64>(6)? as u8;
            if shard.base_directory(id) && kind != EntryKind::Directory.as_i64() as u8 {
                return Err(IndexError::SnapshotRequired {
                    reason: "base directory changed kind",
                });
            }
            if !removed.contains(&id) && !upserts.contains_key(&id) {
                affected += 1;
            }
            let path = crate::arrays::row_blob(row, 3)?;
            let name = crate::arrays::row_blob(row, 4)?;
            let name_fold = crate::arrays::row_text(row, 5)?.as_bytes();
            let incoming =
                std::mem::size_of::<OverlayRow>() + path.len() + name.len() + name_fold.len();
            let old_bytes = upserts.get(&id).map_or(0, OverlayRow::bytes);
            let new_len = upserts.len() + usize::from(!upserts.contains_key(&id));
            payload_bytes = payload_bytes - old_bytes + incoming;
            reservation.resize(
                id_table_bytes(removed.capacity()) + payload_bytes + new_len * COPY_ROW_BYTES,
                cap,
            )?;
            upserts.insert(
                id,
                OverlayRow {
                    entry_id: id,
                    written_generation: Generation(row.get::<_, i64>(1)? as u64),
                    parent_id: row.get::<_, Option<i64>>(2)?.map(EntryId),
                    path: path.into(),
                    name: name.into(),
                    name_fold: name_fold.into(),
                    kind,
                    flags: u8::from(row.get::<_, bool>(7)?) | ((row.get::<_, i64>(8)? as u8) << 1),
                    size: row.get::<_, i64>(9)? as u64,
                    mtime: row.get(10)?,
                },
            );
            if affected > bound {
                return Err(IndexError::SnapshotRequired {
                    reason: "overlay exceeds row bound",
                });
            }
        }
        if affected > bound {
            return Err(IndexError::SnapshotRequired {
                reason: "overlay exceeds row bound",
            });
        }
        let upserts: Box<[OverlayRow]> =
            upserts.into_values().collect::<Vec<_>>().into_boxed_slice();
        let bytes = id_table_bytes(removed.capacity())
            + upserts.iter().map(OverlayRow::bytes).sum::<usize>();
        reservation.resize(bytes, cap)?;
        cx.check()?;
        Ok(Self {
            generation,
            change_seq,
            removed,
            upserts,
            bytes,
            _reservation: Some(reservation),
        })
    }
}

struct UpdateClaim {
    index: crate::Index,
    root: RootId,
    finished: bool,
}
impl UpdateClaim {
    fn finish(&mut self) {
        self.finished = true;
    }
}
impl Drop for UpdateClaim {
    fn drop(&mut self) {
        if !self.finished {
            self.index.arrays_slot().abandon_update(self.root);
        }
    }
}
impl crate::Index {
    /// Used by maintenance and deterministic owned fixtures, never by an interactive handler.
    pub fn prepare_search_overlays(&self) -> Result<usize> {
        let roots = self.arrays_slot().cached_roots();
        let mut updated = 0;
        for root in roots {
            if let Some(shard) = self.arrays_slot().begin_update(root) {
                let result = self.with_read(|connection| {
                    self.arrays_slot().refresh_overlay(
                        connection,
                        &shard,
                        self.config().max_array_bytes,
                        &CallContext::internal(),
                    )
                });
                let success = result.is_ok();
                self.arrays_slot().finish_update(root, &shard, result);
                if success {
                    updated += 1;
                }
            }
        }
        Ok(updated)
    }
    pub(crate) fn request_search_overlay_updates(&self, roots: &[RootId]) {
        if !self.config().search_arrays {
            return;
        }
        for root in roots.iter().copied() {
            let Some(shard) = self.arrays_slot().begin_update(root) else {
                continue;
            };
            let index = self.clone();
            let mut claim = UpdateClaim {
                index: self.clone(),
                root,
                finished: false,
            };
            self.run_maintenance(
                "search-overlay",
                Box::new(move || {
                    let cx = CallContext::internal().with_deadline(
                        std::time::Instant::now() + std::time::Duration::from_secs(5),
                    );
                    let result = index.with_read_in(Some(&cx), |connection| {
                        index.arrays_slot().refresh_overlay(
                            connection,
                            &shard,
                            index.config().max_array_bytes,
                            &cx,
                        )
                    });
                    let again = index.arrays_slot().finish_update(root, &shard, result);
                    claim.finish();
                    index.request_arrays(&[root]);
                    if again {
                        index.request_search_overlay_updates(&[root]);
                    }
                }),
                || {},
            );
        }
    }
}
