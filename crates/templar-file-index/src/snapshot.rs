//! `TEMPLARIDX1`: a sealed, streamed, checksummed picture of selected roots.
//!
//! Silvaview wants two million entries. Two million JSON objects over D-Bus is not a transport, it
//! is a denial of service with a schema, and the message-size limit stops it long before the
//! memory does. So a bulk consumer gets a file descriptor and reads a flat binary frame: fixed
//! header, explicit little-endian, explicit version, CRC-32C over the payload, and a field mask so
//! a caller that only wants names and sizes does not pay for modes and inodes.
//!
//! The format is deliberately readable without SQLite — that is what makes it a contract rather
//! than an implementation detail leaking out of the database.

use crate::{
    model::{Coverage, EntryKind, Generation, NativePath, RootId},
    query::FieldMask,
};
use serde::{Deserialize, Serialize};

/// Eight bytes exactly, which is why this is not spelled out in full: the frame header
/// reserves eight for it and every reader checks that width. `TMPLIDX1` is the rebrand of
/// `LEGEIDX1`; a snapshot written by the old name simply fails the magic check and is
/// rebuilt, which is what a cache is for.
pub const MAGIC: &[u8; 8] = b"TMPLIDX1";
/// A payload shaped exactly as it always was: only the eight original fields
/// (`FieldMask::ALL`, 255) may appear. `SnapshotReader` refuses anything with a wider mask under
/// this number rather than risk a byte-shape it does not know.
pub const FORMAT: u32 = 1;
/// A payload that may also carry [`FieldMask::COVERAGE`] and [`FieldMask::AGGREGATE`] — extra,
/// fixed-size blocks per entry that an old reader has no way to know to skip. Stamped whenever the
/// effective mask reaches outside `FieldMask::ALL`, so an old build fails loudly on `format !=
/// FORMAT` instead of silently misreading the rest of the frame as some other entry's fields.
pub const FORMAT_EXTENDED: u32 = 2;
pub const HEADER_LEN: u32 = 64;
/// Bit 0: the payload is little-endian. Always set in v1; a big-endian writer would clear it.
pub const FLAG_LITTLE_ENDIAN: u32 = 1;

/// Whether a snapshot's entries section holds every requested row, or one row per directory
/// carrying a cached total.
///
/// `Summary` is what a drive visualiser's first frame wants: a build directory the size of a
/// small database contributes one row with a number
/// ([`FieldMask::AGGREGATE`], `aggregate::AggregateRow`), not the row for every object file in
/// it. Regular files are omitted entirely — the whole point is to never receive the leaf rows —
/// so a summary snapshot forces [`FieldMask::COVERAGE`] and [`FieldMask::AGGREGATE`] onto the
/// effective mask regardless of what `fields` asked for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotDetail {
    #[default]
    Materialized,
    Summary,
}

/// What to put in a snapshot.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SnapshotRequest {
    /// Empty means every root.
    #[serde(default)]
    pub roots: Vec<RootId>,
    #[serde(default)]
    pub fields: FieldMask,
    /// Include entries from roots that are offline. Their rows are the last known truth and are
    /// marked as such by the root's state in the manifest.
    #[serde(default)]
    pub include_offline: bool,
    /// `Summary` for a directory-only picture carrying cached aggregates; `Materialized` (the
    /// default, and what an absent field means) for the leaf-by-leaf picture this format has
    /// always produced.
    #[serde(default)]
    pub detail: SnapshotDetail,
    /// Rebase the entries section to one subtree instead of the whole of every selected root. The
    /// roots section still names every selected root for context; only the entries section is
    /// bounded, to the subtree path itself plus everything under it.
    #[serde(default)]
    pub subtree: Option<NativePath>,
    /// Same spelling as `Query::include_coverage`, deliberately not its default: a bulk consumer
    /// picks a snapshot over `Search` precisely to see every tier, so an absent field here means
    /// "every tier", where an absent field on `Query` means "just `Detailed`". A caller that wants
    /// only what a filename search would see asks for `[Coverage::Detailed]` explicitly.
    #[serde(default)]
    pub include_coverage: Vec<Coverage>,
}

/// What a reader needs before it parses a byte.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub format: u32,
    pub generation: Generation,
    pub field_mask: u32,
    pub entry_count: u64,
    pub payload_len: u64,
    pub payload_crc32c: u32,
    pub roots: Vec<SnapshotRoot>,
    /// How the descriptor was produced, so a client knows whether it is truly sealed.
    pub sealed: bool,
    /// The change-log position this frame is consistent with: pass it straight to
    /// [`crate::Index::changes_since`] and the first change received is the first one the frame
    /// does not already contain.
    ///
    /// Without it a consumer has to *ask* for its position after taking the snapshot, and
    /// anything committed between the two calls lands in neither — the read/subscribe race
    /// `@templaros.decision.event-driven-service-authorities` exists to rule out. `changes_since(0)`
    /// is not a substitute: once the bounded log has been pruned, sequence 0 is below the
    /// retained floor and the answer is `SnapshotRequired` rather than a position.
    ///
    /// Serde-defaulted, so a manifest written before this field existed reads as 0 — which is
    /// what such a consumer was already assuming.
    #[serde(default)]
    pub snapshot_seq: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SnapshotRoot {
    pub root: RootId,
    pub path_b64: String,
    pub display: String,
    pub state: String,
    pub complete: bool,
    pub generation: Generation,
    #[serde(default)]
    pub metadata: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// [`FieldMask::AGGREGATE`]'s flags byte: an aggregate was actually cached for this row.
pub const AGGREGATE_FLAG_PRESENT: u8 = 0b0001;
/// The last computation saw every descendant — no permission wall, no read error.
pub const AGGREGATE_FLAG_READABLE: u8 = 0b0010;
/// A live mutation touched the subtree since the cached totals were computed.
pub const AGGREGATE_FLAG_STALE: u8 = 0b0100;

#[cfg(feature = "store")]
mod store {
use super::{
    SnapshotRequest, SnapshotManifest, SnapshotDetail, FieldMask, FORMAT, FORMAT_EXTENDED,
    crc32c, HEADER_LEN, MAGIC, FLAG_LITTLE_ENDIAN, SnapshotRoot, EntryKind,
    AGGREGATE_FLAG_PRESENT, AGGREGATE_FLAG_READABLE, AGGREGATE_FLAG_STALE,
};
use crate::{ENTRY_COLUMNS, Result, entry_row_from_row, model::{RootState, RootStatus}};
use rusqlite::Connection;
use std::io::Write;

impl crate::Index {
    /// Writes a snapshot into `sink` and returns the manifest describing it.
    ///
    /// One read transaction, so the frame is internally consistent: it is a picture of one
    /// generation, not a series of glimpses of several.
    pub fn write_snapshot(
        &self,
        sink: &mut impl Write,
        request: &SnapshotRequest,
    ) -> Result<SnapshotManifest> {
        // A summary is directories carrying cached totals, not leaf rows — meaningless without
        // knowing which tier each directory is at and what its aggregate says, so both are forced
        // on regardless of what the caller asked for.
        let mask = if request.detail == SnapshotDetail::Summary {
            request
                .fields
                .with(FieldMask::COVERAGE)
                .with(FieldMask::AGGREGATE)
        } else {
            request.fields
        };
        // Any bit outside the original eight changes the payload's per-entry byte shape, so it
        // has to stamp the format an old reader would refuse rather than misparse.
        let format = if mask.0 & !FieldMask::ALL.0 == 0 {
            FORMAT
        } else {
            FORMAT_EXTENDED
        };

        // The one read transaction the doc comment above promises. It used to be four and then
        // one per root, each on its own pooled connection, so a writer committing between them
        // produced a frame whose header, roots and rows never coexisted — a consumer would then
        // replay from a cursor that did not describe the rows it had. `BEGIN DEFERRED` on a
        // read-only connection takes its snapshot at the first read and holds it under WAL until
        // the transaction ends, which is exactly the view every section below reads from.
        let (generation, snapshot_seq, selected, payload, entry_count) =
            self.with_read(|connection| {
                let view = connection.unchecked_transaction()?;
                let generation = crate::read_generation(&view)?;
                // Read before the rows, not after: a change committed while the frame is being
                // written must land in the stream a consumer replays, never be skipped as
                // "already in the frame". An over-count is a change replayed; an under-count is a
                // change lost, and every mutation is idempotent.
                let snapshot_seq = crate::mutation::read_latest_change_seq(&view)?;
                let selected: Vec<_> = crate::read_roots(&view)?
                    .into_iter()
                    .filter(|status| {
                        request.roots.is_empty() || request.roots.contains(&status.root)
                    })
                    .filter(|status| request.include_offline || status.state != RootState::Offline)
                    .collect();

                let mut payload = Vec::new();
                write_roots_section(&mut payload, &selected);
                let entry_count =
                    write_entries_section(&view, &mut payload, &selected, request, mask)?;
                Ok((generation, snapshot_seq, selected, payload, entry_count))
            })?;
        // A visualiser's sizes must become measured, not remain namespace-only indefinitely.
        // Queue repair after releasing the read transaction; this frame still describes the
        // captured unverified metadata and never waits for the writer.
        if mask.0 & (FieldMask::SIZE.0 | FieldMask::TIMES.0 | FieldMask::MODE.0 | FieldMask::AGGREGATE.0) != 0
            && let Some(sink) = self.sink()
        {
            for status in &selected {
                if status.metadata == crate::MetadataFreshness::Unverified
                    && request.subtree.as_ref().is_none_or(|subtree|
                        subtree.starts_with(&status.path) || status.path.starts_with(subtree))
                {
                    sink.metadata_repair_requested(status.root);
                }
            }
        }


        let payload_crc = crc32c(&payload);
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&format.to_le_bytes());
        header.extend_from_slice(&FLAG_LITTLE_ENDIAN.to_le_bytes());
        header.extend_from_slice(&HEADER_LEN.to_le_bytes());
        header.extend_from_slice(&mask.0.to_le_bytes());
        header.extend_from_slice(&generation.0.to_le_bytes());
        header.extend_from_slice(&(selected.len() as u32).to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&entry_count.to_le_bytes());
        header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        header.extend_from_slice(&payload_crc.to_le_bytes());
        let header_crc = crc32c(&header[..60]);
        header.extend_from_slice(&header_crc.to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_LEN as usize);

        sink.write_all(&header)?;
        sink.write_all(&payload)?;
        sink.flush()?;

        Ok(SnapshotManifest {
            format,
            generation,
            field_mask: mask.0,
            entry_count,
            payload_len: payload.len() as u64,
            payload_crc32c: payload_crc,
            roots: selected
                .iter()
                .map(|status| SnapshotRoot {
                    root: status.root,
                    path_b64: crate::query::base64(status.path.as_bytes()),
                    display: status.display.clone(),
                    state: status.state.as_str().to_owned(),
                    complete: status.complete,
                    generation: status.generation,
                    metadata: status.metadata.as_str().to_owned(),
                    reason: status.reason.clone(),
                })
                .collect(),
            sealed: false,
            snapshot_seq,
        })
    }
}

/// The body of [`crate::Index::write_snapshot`]'s entry loop, pulled out only so that method stays
/// short: every selected root's live rows, on the caller's connection, streaming each row's masked
/// fields into `payload`. Returns how many entries were written.
///
/// It takes the connection rather than a pooled one of its own precisely so that every root's rows
/// come from the same view as the header that describes them.
///
/// `request.detail == Summary` narrows the query to directory rows only; `request.subtree`
/// bounds it to one subtree; `request.include_coverage` bounds it to specific tiers — each is
/// an independent, optional `WHERE` clause, composed rather than duplicated per combination.
fn write_entries_section(
    connection: &Connection,
    payload: &mut Vec<u8>,
    selected: &[RootStatus],
    request: &SnapshotRequest,
    mask: FieldMask,
) -> Result<u64> {
    let want_aggregate = mask.contains(FieldMask::AGGREGATE);
    let mut entry_count = 0u64;
    for status in selected {
        let (sql, params) = entries_query(status.root.0, request, want_aggregate);
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(sql_row) = rows.next()? {
            let row = entry_row_from_row(sql_row)?;
            write_entry_fields(payload, mask, &row);
            if want_aggregate {
                write_aggregate_field(payload, sql_row)?;
            }
            entry_count += 1;
        }
    }
    Ok(entry_count)
}

/// Builds `write_entries_section`'s query for one root: `request.detail == Summary` narrows it to
/// directory rows only; `request.subtree` bounds it to one subtree; `request.include_coverage`
/// bounds it to specific tiers — each an independent, optional `WHERE` clause, composed rather
/// than duplicated per combination. The aggregate join is a derived table renaming
/// `entry_id`/`root_id` so they do not collide with the identically-named, unqualified columns
/// `ENTRY_COLUMNS` selects from `entries` — SQLite refuses an ambiguous bare name across a join.
fn entries_query(
    root: i64,
    request: &SnapshotRequest,
    want_aggregate: bool,
) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut clauses = vec![
        "root_id = ?1".to_owned(),
        "deleted_generation IS NULL".to_owned(),
    ];
    let mut params: Vec<Value> = vec![Value::Integer(root)];
    if request.detail == SnapshotDetail::Summary {
        clauses.push(format!("kind = {}", EntryKind::Directory.as_i64()));
    }
    if !request.include_coverage.is_empty() {
        let codes: Vec<String> = request
            .include_coverage
            .iter()
            .map(|coverage| coverage.as_i64().to_string())
            .collect();
        clauses.push(format!("coverage IN ({})", codes.join(",")));
    }
    if let Some(subtree) = &request.subtree {
        let (low, high) = subtree.subtree_bounds();
        let base = params.len();
        clauses.push(format!(
            "(path_bytes = ?{} OR (path_bytes >= ?{} AND path_bytes < ?{}))",
            base + 1,
            base + 2,
            base + 3
        ));
        params.push(Value::Blob(subtree.as_bytes().to_vec()));
        params.push(Value::Blob(low));
        params.push(Value::Blob(high));
    }
    let select = if want_aggregate {
        format!(
            "SELECT {ENTRY_COLUMNS}, agg.recursive_bytes, agg.recursive_alloc_bytes, \
             agg.entry_count, agg.readable, agg.stale
               FROM entries
               LEFT JOIN (SELECT entry_id AS agg_entry_id, recursive_bytes, \
                 recursive_alloc_bytes, entry_count, readable, stale
                   FROM entry_aggregates) AS agg
                 ON agg.agg_entry_id = entries.entry_id"
        )
    } else {
        format!("SELECT {ENTRY_COLUMNS} FROM entries")
    };
    (
        format!("{select} WHERE {} ORDER BY entry_id", clauses.join(" AND ")),
        params,
    )
}

/// Every field but `AGGREGATE`, which needs the raw SQL row for the joined columns
/// (`write_aggregate_field`) rather than anything `EntryRow` carries.
fn write_entry_fields(payload: &mut Vec<u8>, mask: FieldMask, row: &crate::model::EntryRow) {
    payload.extend_from_slice(&(row.entry_id.0 as u64).to_le_bytes());
    payload.extend_from_slice(&(row.root.0 as u64).to_le_bytes());
    if mask.contains(FieldMask::PARENT) {
        payload.extend_from_slice(&(row.parent.map_or(0, |id| id.0) as u64).to_le_bytes());
    }
    if mask.contains(FieldMask::NAME) {
        write_bytes(payload, row.name.as_bytes());
    }
    if mask.contains(FieldMask::PATH) {
        write_bytes(payload, row.path.as_bytes());
    }
    if mask.contains(FieldMask::KIND) {
        payload.push(kind_code(row.meta.kind));
        payload.push(u8::from(row.meta.hidden));
        payload.extend_from_slice(&[0u8; 2]);
    }
    if mask.contains(FieldMask::SIZE) {
        payload.extend_from_slice(&row.meta.size_bytes.to_le_bytes());
        payload.extend_from_slice(&row.meta.alloc_bytes.to_le_bytes());
    }
    if mask.contains(FieldMask::TIMES) {
        payload.extend_from_slice(&row.meta.mtime_ns.to_le_bytes());
        payload.extend_from_slice(&row.meta.ctime_ns.to_le_bytes());
    }
    if mask.contains(FieldMask::MODE) {
        payload.extend_from_slice(&row.meta.mode.to_le_bytes());
        payload.extend_from_slice(&row.meta.nlink.to_le_bytes());
    }
    if mask.contains(FieldMask::OBJECT_KEY) {
        let key = row
            .meta
            .object_key
            .unwrap_or(crate::ObjectKey { dev: 0, ino: 0 });
        payload.extend_from_slice(&key.dev.to_le_bytes());
        payload.extend_from_slice(&key.ino.to_le_bytes());
    }
    if mask.contains(FieldMask::COVERAGE) {
        payload.push(row.coverage.as_i64() as u8);
        payload.extend_from_slice(&[0u8; 3]);
    }
}

/// The joined columns start right after `ENTRY_COLUMNS`'s 22 fields (indices 0..=21); all five
/// read `NULL` together when nothing joined — a `Detailed` or `Inventory` row, or an `Opaque` one
/// never walked yet.
fn write_aggregate_field(payload: &mut Vec<u8>, sql_row: &rusqlite::Row<'_>) -> Result<()> {
    let recursive_bytes: Option<i64> = sql_row.get(22)?;
    let Some(bytes) = recursive_bytes else {
        payload.extend_from_slice(&[0u8; 32]);
        return Ok(());
    };
    let alloc: i64 = sql_row.get(23)?;
    let count: i64 = sql_row.get(24)?;
    let readable: i64 = sql_row.get(25)?;
    let stale: i64 = sql_row.get(26)?;
    payload.extend_from_slice(&(bytes as u64).to_le_bytes());
    payload.extend_from_slice(&(alloc as u64).to_le_bytes());
    payload.extend_from_slice(&(count as u64).to_le_bytes());
    let mut flags = AGGREGATE_FLAG_PRESENT;
    if readable != 0 {
        flags |= AGGREGATE_FLAG_READABLE;
    }
    if stale != 0 {
        flags |= AGGREGATE_FLAG_STALE;
    }
    payload.push(flags);
    payload.extend_from_slice(&[0u8; 7]);
    Ok(())
}

/// The body of [`crate::Index::write_snapshot`]'s roots header, pulled out only so that method
/// stays short.
fn write_roots_section(payload: &mut Vec<u8>, selected: &[RootStatus]) {
    payload.extend_from_slice(&(selected.len() as u32).to_le_bytes());
    for status in selected {
        payload.extend_from_slice(&(status.root.0 as u64).to_le_bytes());
        payload.push(state_code(status.state));
        payload.push(u8::from(status.complete));
        payload.extend_from_slice(&[0u8; 2]);
        payload.extend_from_slice(&status.generation.0.to_le_bytes());
        payload.extend_from_slice(&(status.path.as_bytes().len() as u32).to_le_bytes());
        payload.extend_from_slice(status.path.as_bytes());
    }
}

fn write_bytes(payload: &mut Vec<u8>, bytes: &[u8]) {
    payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    payload.extend_from_slice(bytes);
}

fn state_code(state: RootState) -> u8 {
    match state {
        RootState::Empty => 0,
        RootState::Building => 1,
        RootState::Current => 2,
        RootState::CatchingUp => 3,
        RootState::Stale => 4,
        RootState::Offline => 5,
        RootState::PermissionLimited => 6,
    }
}

fn kind_code(kind: EntryKind) -> u8 {
    kind.as_i64() as u8
}

}

/// One decoded entry from [`SnapshotReader::entries`]. A field the mask did not request is
/// `None` (or, for [`Self::hidden`], `false`) rather than a guess at what it would have been.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotEntry {
    pub entry_id: i64,
    pub root: RootId,
    pub parent: Option<i64>,
    pub name: Option<NativePath>,
    pub path: Option<NativePath>,
    pub kind: Option<EntryKind>,
    pub hidden: bool,
    pub size_bytes: Option<u64>,
    pub alloc_bytes: Option<u64>,
    pub mtime_ns: Option<i64>,
    pub ctime_ns: Option<i64>,
    pub mode: Option<u32>,
    pub nlink: Option<u32>,
    pub coverage: Option<Coverage>,
    /// Only ever `Some` for an `Opaque` directory the writer had a cached total for — see
    /// `aggregate::AggregateRow`, the row this is decoded from.
    pub aggregate: Option<SnapshotAggregate>,
}

/// The [`FieldMask::AGGREGATE`] block, decoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotAggregate {
    pub recursive_bytes: u64,
    pub recursive_alloc_bytes: u64,
    pub entry_count: u64,
    pub readable: bool,
    pub stale: bool,
}

/// Reads a frame back. Exists so the format has one authoritative parser, and so a test can prove
/// that what went in comes out byte for byte.
#[derive(Debug)]
pub struct SnapshotReader<'a> {
    payload: &'a [u8],
    pub format: u32,
    pub generation: Generation,
    pub field_mask: FieldMask,
    pub root_count: u32,
    pub entry_count: u64,
}

impl<'a> SnapshotReader<'a> {
    /// Validates the header and both checksums. A frame that fails any of them is refused: a
    /// snapshot is only useful if a reader can tell a truncated one from a whole one.
    pub fn new(frame: &'a [u8]) -> std::result::Result<Self, String> {
        if frame.len() < HEADER_LEN as usize {
            return Err("frame shorter than its header".into());
        }
        if &frame[..8] != MAGIC {
            return Err("not a TEMPLARIDX1 frame".into());
        }
        let format = u32::from_le_bytes(frame[8..12].try_into().unwrap());
        if format != FORMAT && format != FORMAT_EXTENDED {
            return Err(format!("unknown snapshot format {format}"));
        }
        let header_crc = u32::from_le_bytes(frame[60..64].try_into().unwrap());
        if crc32c(&frame[..60]) != header_crc {
            return Err("header checksum mismatch".into());
        }
        if u32::from_le_bytes(frame[12..16].try_into().unwrap()) != FLAG_LITTLE_ENDIAN
            || u32::from_le_bytes(frame[16..20].try_into().unwrap()) != HEADER_LEN
            || frame[36..40] != [0; 4]
        {
            return Err("unsupported snapshot header layout".into());
        }
        let mask = u32::from_le_bytes(frame[20..24].try_into().unwrap());
        let allowed = if format == FORMAT_EXTENDED { 1023 } else { FieldMask::ALL.0 };
        if mask & !allowed != 0 { return Err("unsupported snapshot field mask".into()); }
        let payload_len = usize::try_from(u64::from_le_bytes(frame[48..56].try_into().unwrap()))
            .map_err(|_| "declared payload does not fit this reader")?;
        let payload_end = (HEADER_LEN as usize).checked_add(payload_len)
            .ok_or("declared payload length overflows")?;
        if payload_end != frame.len() { return Err("frame length differs from its declared payload".into()); }
        let payload_crc = u32::from_le_bytes(frame[56..60].try_into().unwrap());
        let payload = &frame[HEADER_LEN as usize..payload_end];
        if crc32c(payload) != payload_crc {
            return Err("payload checksum mismatch".into());
        }
        let root_count = u32::from_le_bytes(frame[32..36].try_into().unwrap());
        let section_count = payload.get(..4).ok_or("missing roots section")?;
        if section_count != root_count.to_le_bytes() || root_count as usize > payload.len().saturating_sub(4) / 24 {
            return Err("impossible roots section count".into());
        }
        Ok(Self {
            payload,
            format,
            generation: Generation(u64::from_le_bytes(frame[24..32].try_into().unwrap())),
            field_mask: FieldMask(u32::from_le_bytes(frame[20..24].try_into().unwrap())),
            root_count: u32::from_le_bytes(frame[32..36].try_into().unwrap()),
            entry_count: u64::from_le_bytes(frame[40..48].try_into().unwrap()),
        })
    }

    /// The roots section, in order.
    pub fn roots(&self) -> std::result::Result<Vec<(RootId, NativePath)>, String> {
        let mut cursor = 4usize;
        let mut roots = Vec::new();
        for _ in 0..self.root_count {
            let id = i64::from_le_bytes(
                self.payload
                    .get(cursor..cursor + 8)
                    .ok_or("truncated roots section")?
                    .try_into()
                    .unwrap(),
            );
            cursor += 8 + 4 + 8;
            let length = u32::from_le_bytes(
                self.payload
                    .get(cursor..cursor + 4)
                    .ok_or("truncated roots section")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            cursor += 4;
            let path = self
                .payload
                .get(cursor..cursor + length)
                .ok_or("truncated root path")?;
            cursor += length;
            roots.push((RootId(id), NativePath::from_bytes(path.to_vec())));
        }
        Ok(roots)
    }

    /// Every entry's native path, in the order they were written. The one field a bulk consumer
    /// always needs, and the one whose losslessness the whole design turns on.
    ///
    /// Refuses when the mask did not request [`FieldMask::PATH`] at all — an empty result would
    /// otherwise look identical to "every path was empty", which is not the same fact.
    pub fn entry_paths(&self) -> std::result::Result<Vec<NativePath>, String> {
        if !self.field_mask.contains(FieldMask::PATH) {
            return Err("this snapshot's field mask did not request PATH".into());
        }
        Ok(self
            .entries()?
            .into_iter()
            .filter_map(|entry| entry.path)
            .collect())
    }

    /// Every entry, decoded according to whichever fields the mask carried — the general-purpose
    /// reader every other accessor on this type could be written in terms of. A field the mask did
    /// not request comes back `None`: the caller asked not to pay for it, so it is not invented.
    pub fn entries(&self) -> std::result::Result<Vec<SnapshotEntry>, String> {
        let mut cursor = self.entries_offset()?;
        let count = usize::try_from(self.entry_count).map_err(|_| "impossible entry count")?;
        if count > self.payload.len().saturating_sub(cursor) / 16 {
            return Err("entry count exceeds its payload".into());
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..self.entry_count {
            let entry_id = i64::from_le_bytes(self.bytes_at(cursor, 8)?.try_into().unwrap());
            cursor += 8;
            let root = RootId(i64::from_le_bytes(
                self.bytes_at(cursor, 8)?.try_into().unwrap(),
            ));
            cursor += 8;
            let mut entry = SnapshotEntry {
                entry_id,
                root,
                parent: None,
                name: None,
                path: None,
                kind: None,
                hidden: false,
                size_bytes: None,
                alloc_bytes: None,
                mtime_ns: None,
                ctime_ns: None,
                mode: None,
                nlink: None,
                coverage: None,
                aggregate: None,
            };
            cursor = self.decode_entry_fields(cursor, &mut entry)?;
            entries.push(entry);
        }
        if cursor != self.payload.len() { return Err("trailing bytes after entries section".into()); }
        Ok(entries)
    }

    /// The body of [`Self::entries`]'s per-entry loop, pulled out only so that method stays
    /// short: every masked field after `entry_id`/`root_id`, in wire order. Returns the cursor
    /// just past this entry.
    fn decode_entry_fields(
        &self,
        mut cursor: usize,
        entry: &mut SnapshotEntry,
    ) -> std::result::Result<usize, String> {
        let mask = self.field_mask;
        if mask.contains(FieldMask::PARENT) {
            let raw = u64::from_le_bytes(self.bytes_at(cursor, 8)?.try_into().unwrap());
            cursor += 8;
            entry.parent = (raw != 0).then_some(raw as i64);
        }
        if mask.contains(FieldMask::NAME) {
            let (name, next) = self.read_bytes_field(cursor)?;
            entry.name = Some(NativePath::from_bytes(name));
            cursor = next;
        }
        if mask.contains(FieldMask::PATH) {
            let (path, next) = self.read_bytes_field(cursor)?;
            entry.path = Some(NativePath::from_bytes(path));
            cursor = next;
        }
        if mask.contains(FieldMask::KIND) {
            let bytes = self.bytes_at(cursor, 4)?;
            entry.kind = Some(EntryKind::from_i64(i64::from(bytes[0])));
            entry.hidden = bytes[1] != 0;
            cursor += 4;
        }
        if mask.contains(FieldMask::SIZE) {
            entry.size_bytes = Some(u64::from_le_bytes(
                self.bytes_at(cursor, 8)?.try_into().unwrap(),
            ));
            entry.alloc_bytes = Some(u64::from_le_bytes(
                self.bytes_at(cursor + 8, 8)?.try_into().unwrap(),
            ));
            cursor += 16;
        }
        if mask.contains(FieldMask::TIMES) {
            entry.mtime_ns = Some(i64::from_le_bytes(
                self.bytes_at(cursor, 8)?.try_into().unwrap(),
            ));
            entry.ctime_ns = Some(i64::from_le_bytes(
                self.bytes_at(cursor + 8, 8)?.try_into().unwrap(),
            ));
            cursor += 16;
        }
        if mask.contains(FieldMask::MODE) {
            entry.mode = Some(u32::from_le_bytes(
                self.bytes_at(cursor, 4)?.try_into().unwrap(),
            ));
            entry.nlink = Some(u32::from_le_bytes(
                self.bytes_at(cursor + 4, 4)?.try_into().unwrap(),
            ));
            cursor += 8;
        }
        if mask.contains(FieldMask::OBJECT_KEY) {
            cursor += 16;
        }
        if mask.contains(FieldMask::COVERAGE) {
            entry.coverage = Some(Coverage::from_i64(i64::from(self.bytes_at(cursor, 1)?[0])));
            cursor += 4;
        }
        if mask.contains(FieldMask::AGGREGATE) {
            entry.aggregate = self.decode_aggregate(cursor)?;
            cursor += 32;
        }
        Ok(cursor)
    }

    /// [`FieldMask::AGGREGATE`]'s 32-byte block, or `None` when the `has_aggregate` flag says
    /// nothing was cached for this row.
    fn decode_aggregate(
        &self,
        cursor: usize,
    ) -> std::result::Result<Option<SnapshotAggregate>, String> {
        let flags = self.bytes_at(cursor + 24, 1)?[0];
        if flags & AGGREGATE_FLAG_PRESENT == 0 {
            return Ok(None);
        }
        Ok(Some(SnapshotAggregate {
            recursive_bytes: u64::from_le_bytes(self.bytes_at(cursor, 8)?.try_into().unwrap()),
            recursive_alloc_bytes: u64::from_le_bytes(
                self.bytes_at(cursor + 8, 8)?.try_into().unwrap(),
            ),
            entry_count: u64::from_le_bytes(self.bytes_at(cursor + 16, 8)?.try_into().unwrap()),
            readable: flags & AGGREGATE_FLAG_READABLE != 0,
            stale: flags & AGGREGATE_FLAG_STALE != 0,
        }))
    }

    /// Where the entries section starts: right after the roots section this reader also parses.
    fn entries_offset(&self) -> std::result::Result<usize, String> {
        let mut cursor = 4usize;
        for _ in 0..self.root_count {
            cursor += 8 + 4 + 8;
            let length = u32::from_le_bytes(
                self.payload
                    .get(cursor..cursor + 4)
                    .ok_or("truncated roots section")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            cursor = cursor.checked_add(4).and_then(|cursor| cursor.checked_add(length))
                .filter(|cursor| *cursor <= self.payload.len())
                .ok_or("truncated root path")?;
        }
        Ok(cursor)
    }

    fn bytes_at(&self, cursor: usize, len: usize) -> std::result::Result<&[u8], String> {
        let end = cursor.checked_add(len).ok_or("entry length overflows")?;
        self.payload
            .get(cursor..end)
            .ok_or("truncated entry")
            .map_err(str::to_owned)
    }

    /// A length-prefixed byte field: `(length, bytes)`, returning the bytes and the cursor just
    /// past them.
    fn read_bytes_field(&self, cursor: usize) -> std::result::Result<(Vec<u8>, usize), String> {
        let length = self.length_at(cursor)?;
        let start = cursor + 4;
        let bytes = self.bytes_at(start, length)?.to_vec();
        Ok((bytes, start + length))
    }

    fn length_at(&self, cursor: usize) -> std::result::Result<usize, String> {
        Ok(u32::from_le_bytes(
            self.payload
                .get(cursor..cursor + 4)
                .ok_or("truncated entry")?
                .try_into()
                .unwrap(),
        ) as usize)
    }
}

/// CRC-32C (Castagnoli), table-driven, computed here rather than pulled in: it is thirty lines,
/// and a snapshot format should not owe its integrity check to a dependency graph.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    const POLYNOMIAL: u32 = 0x82f6_3b78;
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    fn minimal_frame(count: u64, declared_payload: u64) -> Vec<u8> {
        let payload = [0u8; 4];
        let mut header = Vec::new();
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT.to_le_bytes());
        header.extend_from_slice(&FLAG_LITTLE_ENDIAN.to_le_bytes());
        header.extend_from_slice(&HEADER_LEN.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&1u64.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&count.to_le_bytes());
        header.extend_from_slice(&declared_payload.to_le_bytes());
        header.extend_from_slice(&crc32c(&payload).to_le_bytes());
        let crc = crc32c(&header);
        header.extend_from_slice(&crc.to_le_bytes());
        header.extend_from_slice(&payload);
        header
    }

    #[test]
    fn declared_lengths_and_counts_are_bounded_before_allocation() {
        assert!(SnapshotReader::new(&minimal_frame(0, u64::MAX)).is_err());
        let frame = minimal_frame(u64::MAX, 4);
        assert!(SnapshotReader::new(&frame).unwrap().entries().is_err());
        assert!(SnapshotReader::new(&minimal_frame(0, 4)).unwrap().entries().unwrap().is_empty());
    }

    #[test]
    fn extra_bytes_and_unknown_layouts_are_not_accepted_as_a_compatible_frame() {
        let mut frame = minimal_frame(0, 4);
        frame.push(0);
        assert!(SnapshotReader::new(&frame).is_err());
        let mut frame = minimal_frame(0, 4);
        frame[20..24].copy_from_slice(&FieldMask::AGGREGATE.0.to_le_bytes());
        let crc = crc32c(&frame[..60]);
        frame[60..64].copy_from_slice(&crc.to_le_bytes());
        assert!(SnapshotReader::new(&frame).is_err());
    }
}
