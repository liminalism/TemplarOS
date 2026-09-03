//! `LEGEIDX1`: a sealed, streamed, checksummed picture of selected roots.
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
    ENTRY_COLUMNS, Result, entry_row_from_row,
    model::{EntryKind, Generation, NativePath, RootId, RootState},
    query::FieldMask,
};
use serde::{Deserialize, Serialize};
use std::io::Write;

pub const MAGIC: &[u8; 8] = b"LEGEIDX1";
pub const FORMAT: u32 = 1;
pub const HEADER_LEN: u32 = 64;
/// Bit 0: the payload is little-endian. Always set in v1; a big-endian writer would clear it.
pub const FLAG_LITTLE_ENDIAN: u32 = 1;

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRoot {
    pub root: RootId,
    pub path_b64: String,
    pub display: String,
    pub state: String,
    pub complete: bool,
    pub generation: Generation,
}

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
        let generation = self.generation()?;
        let all = self.roots()?;
        let selected: Vec<_> = all
            .into_iter()
            .filter(|status| request.roots.is_empty() || request.roots.contains(&status.root))
            .filter(|status| request.include_offline || status.state != RootState::Offline)
            .collect();

        let mut payload = Vec::new();
        payload.extend_from_slice(&(selected.len() as u32).to_le_bytes());
        for status in &selected {
            payload.extend_from_slice(&(status.root.0 as u64).to_le_bytes());
            payload.push(state_code(status.state));
            payload.push(u8::from(status.complete));
            payload.extend_from_slice(&[0u8; 2]);
            payload.extend_from_slice(&status.generation.0.to_le_bytes());
            payload.extend_from_slice(&(status.path.as_bytes().len() as u32).to_le_bytes());
            payload.extend_from_slice(status.path.as_bytes());
        }

        let mask = request.fields;
        let mut entry_count = 0u64;
        for status in &selected {
            self.with_read(|connection| {
                let mut statement = connection.prepare(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM entries
                      WHERE root_id = ?1 AND deleted_generation IS NULL
                      ORDER BY entry_id"
                ))?;
                let rows = statement.query_map([status.root.0], entry_row_from_row)?;
                for row in rows {
                    let row = row?;
                    payload.extend_from_slice(&(row.entry_id.0 as u64).to_le_bytes());
                    payload.extend_from_slice(&(row.root.0 as u64).to_le_bytes());
                    if mask.contains(FieldMask::PARENT) {
                        payload.extend_from_slice(
                            &(row.parent.map_or(0, |id| id.0) as u64).to_le_bytes(),
                        );
                    }
                    if mask.contains(FieldMask::NAME) {
                        write_bytes(&mut payload, row.name.as_bytes());
                    }
                    if mask.contains(FieldMask::PATH) {
                        write_bytes(&mut payload, row.path.as_bytes());
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
                    entry_count += 1;
                }
                Ok(())
            })?;
        }

        let payload_crc = crc32c(&payload);
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&FORMAT.to_le_bytes());
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
            format: FORMAT,
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
                })
                .collect(),
            sealed: false,
        })
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
            return Err("not a LEGEIDX1 frame".into());
        }
        let format = u32::from_le_bytes(frame[8..12].try_into().unwrap());
        if format != FORMAT {
            return Err(format!("unknown snapshot format {format}"));
        }
        let header_crc = u32::from_le_bytes(frame[60..64].try_into().unwrap());
        if crc32c(&frame[..60]) != header_crc {
            return Err("header checksum mismatch".into());
        }
        let payload_len = u64::from_le_bytes(frame[48..56].try_into().unwrap()) as usize;
        let payload_crc = u32::from_le_bytes(frame[56..60].try_into().unwrap());
        let payload = frame
            .get(HEADER_LEN as usize..HEADER_LEN as usize + payload_len)
            .ok_or("frame shorter than its declared payload")?;
        if crc32c(payload) != payload_crc {
            return Err("payload checksum mismatch".into());
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
    pub fn entry_paths(&self) -> std::result::Result<Vec<NativePath>, String> {
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
            cursor += 4 + length;
        }

        let mask = self.field_mask;
        let mut paths = Vec::with_capacity(self.entry_count as usize);
        for _ in 0..self.entry_count {
            cursor += 16; // entry_id, root_id
            if mask.contains(FieldMask::PARENT) {
                cursor += 8;
            }
            if mask.contains(FieldMask::NAME) {
                cursor += 4 + self.length_at(cursor)?;
            }
            if mask.contains(FieldMask::PATH) {
                let length = self.length_at(cursor)?;
                cursor += 4;
                paths.push(NativePath::from_bytes(
                    self.payload
                        .get(cursor..cursor + length)
                        .ok_or("truncated entry path")?
                        .to_vec(),
                ));
                cursor += length;
            }
            if mask.contains(FieldMask::KIND) {
                cursor += 4;
            }
            if mask.contains(FieldMask::SIZE) {
                cursor += 16;
            }
            if mask.contains(FieldMask::TIMES) {
                cursor += 16;
            }
            if mask.contains(FieldMask::MODE) {
                cursor += 8;
            }
            if mask.contains(FieldMask::OBJECT_KEY) {
                cursor += 16;
            }
        }
        Ok(paths)
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
