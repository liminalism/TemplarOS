//! Btrfs metadata deltas: bridging a dormant period without walking the tree.
//!
//! Linux gives no persistent per-user change journal, which is why [`crate::reconcile`] exists and
//! why its cost is proportional to the *size of the tree* rather than to the amount that changed.
//! Btrfs can do better. Two read-only snapshots of the same subvolume have a provable difference,
//! and `btrfs send --no-data -p <parent> <child>` writes that difference out as a stream of
//! namespace and metadata operations — create, rename, unlink, chmod, truncate, utimes — without
//! reading a byte of file data. This module decodes that stream and turns it into a
//! [`MutationBatch`], so a daemon that was not running can learn what happened while it was away
//! at a cost that tracks the change, not the tree.
//!
//! **This is an unadopted backend behind a flag.** It is here to be measured
//! (`docs/measurements/`), not to be switched on. Nothing calls it by default.
//!
//! Four decisions shape everything below.
//!
//! **The stream is a change oracle, not a metadata source.** A send stream carries `chmod`,
//! `utimes` and `truncate`, but reconstructing an [`EntryMeta`] from them would mean reimplementing
//! btrfs's own idea of an inode and getting it exactly right on every command — and being wrong
//! anywhere would put a lie into the index that no later pass would notice. So the stream is used
//! only to answer *which paths changed*, and the metadata for each of them is `lstat`ed exactly as
//! a scan would. That is what makes `baseline + delta == fresh scan` a property rather than a hope,
//! and it costs one stat per changed path — still proportional to the change.
//!
//! **The stream's namespace is not the filesystem's namespace.** `btrfs send` routinely renames a
//! file to a temporary name (`o<ino>-<gen>-<seq>`) and then to its real one, because an incremental
//! stream has to be replayable in order and rename cycles otherwise deadlock. A decoder that
//! emitted one mutation per command would write those temporary names into the index. So the
//! commands are replayed over a small in-memory namespace model first, and only the *net* effect —
//! where each touched inode started and where it ended — becomes a mutation. Temporary names never
//! escape this module.
//!
//! **A partial stream is not a partial answer.** A delta that was interrupted describes a state
//! that never existed. [`decode`] refuses any stream that does not end in `BTRFS_SEND_C_END`, and
//! verifies every command's CRC32C, so the caller's only options are "apply all of it" or "fall
//! back to a reconcile" — never "apply the first two thirds".
//!
//! **A stream's paths are relative to the subvolume, not to the indexed root.** `btrfs send`
//! spells every path from the root of the subvolume it snapshotted, and the subvolume is normally
//! *larger* than what any one index root covers: `@home` mounts at `/home`, so a delta of it names
//! `dk/notes.md` and also `other-user/secret`. So [`DeltaTarget`] carries both paths. Every stream
//! path is resolved against [`DeltaTarget::subvolume_path`] to get an absolute one, and is applied
//! only if that absolute path lies within [`DeltaTarget::root_path`] — a component-bounded test,
//! so `/home/dkx` is not mistaken for something under `/home/dk`. Paths outside the root are
//! counted in [`DeltaReport::outside_root`] rather than folded into `vanished`: one means "another
//! root's business", the other means "ours, and gone", and treating them alike is how a delta that
//! silently applies nothing looks like a delta that had nothing to do.

use std::{collections::BTreeMap, path::Path};

use crate::{EntryMeta, Mutation, MutationBatch, MutationSource, NativePath, RootId, RootPolicy};

/// `struct btrfs_stream_header`: 13 magic bytes then a little-endian version.
const MAGIC: &[u8; 13] = b"btrfs-stream\0";
const HEADER_LEN: usize = 17;
/// `struct btrfs_cmd_header`: `__le32 len`, `__le16 cmd`, `__le32 crc32c`.
const CMD_HEADER_LEN: usize = 10;
/// `struct btrfs_tlv_header`: `__le16 type`, `__le16 len`.
const TLV_HEADER_LEN: usize = 4;

/// `enum btrfs_send_attrs` (`fs/btrfs/send.h`). Only the ones this module reads are named.
mod attr {
    pub const UUID: u16 = 1;
    pub const CTRANSID: u16 = 2;
    pub const INO: u16 = 3;
    pub const SIZE: u16 = 4;
    pub const MODE: u16 = 5;
    pub const PATH: u16 = 15;
    pub const PATH_TO: u16 = 16;
    pub const PATH_LINK: u16 = 17;
    pub const DATA: u16 = 19;
    pub const CLONE_UUID: u16 = 20;
    pub const CLONE_CTRANSID: u16 = 21;
}

/// `enum btrfs_send_cmd` (`fs/btrfs/send.h`), plus a catch-all so a stream from a newer kernel
/// parses instead of failing — an unknown command is reported, never silently dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendCmd {
    Subvol,
    Snapshot,
    MkFile,
    MkDir,
    MkNod,
    MkFifo,
    MkSock,
    Symlink,
    Rename,
    Link,
    Unlink,
    RmDir,
    SetXattr,
    RemoveXattr,
    Write,
    Clone,
    Truncate,
    Chmod,
    Chown,
    Utimes,
    End,
    UpdateExtent,
    Fallocate,
    FileAttr,
    EncodedWrite,
    Unknown(u16),
}

impl SendCmd {
    fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Subvol,
            2 => Self::Snapshot,
            3 => Self::MkFile,
            4 => Self::MkDir,
            5 => Self::MkNod,
            6 => Self::MkFifo,
            7 => Self::MkSock,
            8 => Self::Symlink,
            9 => Self::Rename,
            10 => Self::Link,
            11 => Self::Unlink,
            12 => Self::RmDir,
            13 => Self::SetXattr,
            14 => Self::RemoveXattr,
            15 => Self::Write,
            16 => Self::Clone,
            17 => Self::Truncate,
            18 => Self::Chmod,
            19 => Self::Chown,
            20 => Self::Utimes,
            21 => Self::End,
            22 => Self::UpdateExtent,
            23 => Self::Fallocate,
            24 => Self::FileAttr,
            25 => Self::EncodedWrite,
            other => Self::Unknown(other),
        }
    }

    /// Whether this command brings a path into existence that was not in the parent snapshot.
    fn creates(self) -> bool {
        matches!(
            self,
            Self::MkFile
                | Self::MkDir
                | Self::MkNod
                | Self::MkFifo
                | Self::MkSock
                | Self::Symlink
                | Self::Link
        )
    }
}

/// Why a stream could not be trusted. Every variant means the same thing to a caller: do not
/// apply anything, fall back to a reconcile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeltaError {
    /// Not a `btrfs send` stream at all.
    NotAStream,
    /// A stream version this decoder does not implement.
    UnsupportedVersion(u32),
    /// The stream ended in the middle of a structure. This is what an interrupted `btrfs send`
    /// looks like, and it is why a delta is all-or-nothing.
    Truncated { at: usize },
    /// A command's CRC32C did not match — the stream is corrupt.
    CorruptCommand { at: usize },
    /// The stream parsed but never reached `BTRFS_SEND_C_END`.
    Unterminated,
    /// A command that needs a path did not carry one.
    MissingPath(SendCmd),
    /// The stream could not be read from disk.
    Io(String),
}

impl std::fmt::Display for DeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAStream => write!(f, "not a btrfs send stream"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported send stream version {v}"),
            Self::Truncated { at } => {
                write!(f, "send stream truncated at byte {at} (interrupted send?)")
            }
            Self::CorruptCommand { at } => write!(f, "crc32c mismatch on the command at byte {at}"),
            Self::Unterminated => write!(f, "send stream has no BTRFS_SEND_C_END command"),
            Self::MissingPath(cmd) => write!(f, "{cmd:?} carried no path attribute"),
            Self::Io(message) => write!(f, "reading the send stream: {message}"),
        }
    }
}

impl std::error::Error for DeltaError {}

/// One decoded command: its kind and its attributes, kept as raw bytes so an attribute this
/// decoder does not interpret is still visible to a caller that later needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendCommand {
    pub cmd: SendCmd,
    pub attrs: Vec<(u16, Vec<u8>)>,
}

impl SendCommand {
    fn attr(&self, key: u16) -> Option<&[u8]> {
        self.attrs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_slice())
    }

    /// The subvolume-relative path this command is about.
    #[must_use]
    pub fn path(&self) -> Option<&[u8]> {
        self.attr(attr::PATH)
    }

    /// A rename's destination.
    #[must_use]
    pub fn path_to(&self) -> Option<&[u8]> {
        self.attr(attr::PATH_TO)
    }

    /// A hard link's existing name (`BTRFS_SEND_A_PATH_LINK`).
    #[must_use]
    pub fn path_link(&self) -> Option<&[u8]> {
        self.attr(attr::PATH_LINK)
    }

    /// The inode number the stream assigned, where it carried one. Diagnostic: the index keys on
    /// `(dev, ino)` read from the live tree, never on what a stream claimed.
    #[must_use]
    pub fn ino(&self) -> Option<u64> {
        self.u64_attr(attr::INO)
    }

    /// A `truncate`'s target size.
    #[must_use]
    pub fn size(&self) -> Option<u64> {
        self.u64_attr(attr::SIZE)
    }

    /// A `chmod`'s mode.
    #[must_use]
    pub fn mode(&self) -> Option<u64> {
        self.u64_attr(attr::MODE)
    }

    fn u64_attr(&self, key: u16) -> Option<u64> {
        let bytes = self.attr(key)?;
        match bytes.len() {
            8 => Some(u64::from_le_bytes(bytes.try_into().ok()?)),
            4 => Some(u64::from(u32::from_le_bytes(bytes.try_into().ok()?))),
            2 => Some(u64::from(u16::from_le_bytes(bytes.try_into().ok()?))),
            _ => None,
        }
    }
}

/// A whole decoded stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendStream {
    pub version: u32,
    pub commands: Vec<SendCommand>,
    /// Bytes the stream occupied, for the cost report.
    pub bytes: usize,
}

impl SendStream {
    /// The subvolume this stream describes, and the parent it was taken against — enough to check
    /// that a stored checkpoint really is the parent of the snapshot being applied.
    #[must_use]
    pub fn lineage(&self) -> Option<Lineage> {
        let header = self
            .commands
            .iter()
            .find(|c| matches!(c.cmd, SendCmd::Snapshot | SendCmd::Subvol))?;
        Some(Lineage {
            name: header.path().unwrap_or_default().to_vec(),
            uuid: header.attr(attr::UUID).unwrap_or_default().to_vec(),
            ctransid: header.u64_attr(attr::CTRANSID).unwrap_or_default(),
            parent_uuid: header.attr(attr::CLONE_UUID).map(<[u8]>::to_vec),
            parent_ctransid: header.u64_attr(attr::CLONE_CTRANSID),
        })
    }

    /// Whether this stream is incremental. A full send (no `-p`) describes a whole subvolume and
    /// is not a delta: applying one as if it were would read as "everything is new".
    #[must_use]
    pub fn is_incremental(&self) -> bool {
        self.lineage()
            .is_some_and(|lineage| lineage.parent_uuid.is_some())
    }
}

/// Who this stream came from and what it was taken against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lineage {
    pub name: Vec<u8>,
    pub uuid: Vec<u8>,
    pub ctransid: u64,
    pub parent_uuid: Option<Vec<u8>>,
    pub parent_ctransid: Option<u64>,
}

/// Decode a send stream from a file.
pub fn decode_file(path: &Path) -> Result<SendStream, DeltaError> {
    let bytes = std::fs::read(path).map_err(|error| DeltaError::Io(error.to_string()))?;
    decode(&bytes)
}

/// Decode a send stream.
///
/// Strict on purpose: the CRC of every command is checked, and a stream that does not end in
/// `BTRFS_SEND_C_END` is rejected rather than returned as a shorter delta.
pub fn decode(bytes: &[u8]) -> Result<SendStream, DeltaError> {
    if bytes.len() < HEADER_LEN || &bytes[..MAGIC.len()] != MAGIC {
        return Err(DeltaError::NotAStream);
    }
    let version = u32::from_le_bytes(
        bytes[13..17]
            .try_into()
            .map_err(|_| DeltaError::NotAStream)?,
    );
    if version == 0 || version > 2 {
        return Err(DeltaError::UnsupportedVersion(version));
    }

    let mut commands = Vec::new();
    let mut offset = HEADER_LEN;
    let mut terminated = false;
    while offset < bytes.len() {
        let start = offset;
        if bytes.len() - offset < CMD_HEADER_LEN {
            return Err(DeltaError::Truncated { at: start });
        }
        let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let cmd = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into().unwrap());
        let crc = u32::from_le_bytes(bytes[offset + 6..offset + 10].try_into().unwrap());
        let end = offset
            .checked_add(CMD_HEADER_LEN)
            .and_then(|value| value.checked_add(length))
            .ok_or(DeltaError::Truncated { at: start })?;
        if end > bytes.len() {
            return Err(DeltaError::Truncated { at: start });
        }
        verify_crc(&bytes[start..end], crc, start)?;

        let cmd = SendCmd::from_u16(cmd);
        let attrs = parse_attrs(&bytes[start + CMD_HEADER_LEN..end], version, start)?;
        offset = end;
        if cmd == SendCmd::End {
            terminated = true;
            commands.push(SendCommand { cmd, attrs });
            break;
        }
        commands.push(SendCommand { cmd, attrs });
    }
    if !terminated {
        return Err(DeltaError::Unterminated);
    }
    Ok(SendStream {
        version,
        commands,
        bytes: offset,
    })
}

/// The kernel writes `crc32c(0, header_with_zeroed_crc || body)` — seed 0, no final inversion,
/// which is `crc32c()` from libcrc32c rather than the inverted CRC-32 most crates hand out.
fn verify_crc(command: &[u8], expected: u32, at: usize) -> Result<(), DeltaError> {
    let mut zeroed = command.to_vec();
    zeroed[6..10].fill(0);
    if crc32c(&zeroed) == expected {
        Ok(())
    } else {
        Err(DeltaError::CorruptCommand { at })
    }
}

fn parse_attrs(body: &[u8], version: u32, at: usize) -> Result<Vec<(u16, Vec<u8>)>, DeltaError> {
    let mut attrs = Vec::new();
    let mut offset = 0usize;
    while offset < body.len() {
        if body.len() - offset < TLV_HEADER_LEN {
            return Err(DeltaError::Truncated { at: at + offset });
        }
        let kind = u16::from_le_bytes(body[offset..offset + 2].try_into().unwrap());
        let length = u16::from_le_bytes(body[offset + 2..offset + 4].try_into().unwrap()) as usize;
        let start = offset + TLV_HEADER_LEN;
        let end = start + length;
        if end > body.len() {
            // Stream version 2 lets a trailing `BTRFS_SEND_A_DATA` run to the end of the command
            // without repeating its length. Nothing else may overrun.
            if version >= 2 && kind == attr::DATA {
                attrs.push((kind, body[start..].to_vec()));
                return Ok(attrs);
            }
            return Err(DeltaError::Truncated { at: at + offset });
        }
        attrs.push((kind, body[start..end].to_vec()));
        offset = end;
    }
    Ok(attrs)
}

/// CRC-32C (Castagnoli, reflected polynomial 0x82F6_3B78), seed 0, no output inversion.
fn crc32c(bytes: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    (value >> 1) ^ 0x82F6_3B78
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        table
    });
    let mut crc = 0u32;
    for byte in bytes {
        crc = table[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

// --- the namespace replay ----------------------------------------------------------------------

/// A subvolume-relative path, exactly as the stream spells it: bytes, no leading separator.
type Rel = Vec<u8>;

/// Where a node in the replayed namespace came from.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Origin {
    /// Not present in the parent snapshot: the delta brought it into existence.
    Created,
    /// Present in the parent snapshot at this path — which is where the index believes its row is.
    Existing(Rel),
}

#[derive(Clone, Debug)]
struct Node {
    origin: Origin,
    /// Whether anything about the entry itself changed (as opposed to only its path).
    dirty: bool,
}

/// The net effect of a stream, in subvolume-relative paths, with every temporary name resolved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeltaPlan {
    /// `(where the index has it, where it ended up)`, deepest source first — a child that left a
    /// directory must move before the directory does, or its row is dragged along with it.
    pub moves: Vec<(Rel, Rel)>,
    /// Paths whose rows must go, addressed where the index will have them *after* the moves.
    pub removes: Vec<Rel>,
    /// Paths the delta created, shallowest first so a parent row exists before its children.
    pub creates: Vec<Rel>,
    /// Paths that stayed put but changed.
    pub touches: Vec<Rel>,
}

impl DeltaPlan {
    /// How many paths this delta talks about — the number that must scale with the change rather
    /// than with the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.moves.len() + self.removes.len() + self.creates.len() + self.touches.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Replays a decoded stream into its net effect.
struct Replay {
    nodes: BTreeMap<Rel, Node>,
    /// Baseline paths whose rows must be removed. Recorded where the *index* has them, then
    /// re-addressed through the accepted moves at emission time.
    removed: Vec<Rel>,
}

impl Replay {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            removed: Vec::new(),
        }
    }

    /// The path at which the index believes `key`'s row lives, given the renames replayed so far.
    ///
    /// The deepest node that is `key` or one of its ancestors decides: if that node came from the
    /// parent snapshot, the row is under *its* original path; if the delta created it, everything
    /// beneath it is new too.
    fn baseline_of(&self, key: &[u8]) -> Origin {
        let upto = (std::ops::Bound::Unbounded, std::ops::Bound::Included(key));
        for (candidate, node) in self.nodes.range::<[u8], _>(upto).rev() {
            if !is_prefix_path(candidate, key) {
                continue;
            }
            return match &node.origin {
                Origin::Created => Origin::Created,
                Origin::Existing(original) => {
                    let mut rebuilt = original.clone();
                    rebuilt.extend_from_slice(&key[candidate.len()..]);
                    Origin::Existing(rebuilt)
                }
            };
        }
        Origin::Existing(key.to_vec())
    }

    /// Take a node out of the map along with everything under it, returning the node itself and
    /// the `(suffix, node)` pairs beneath it.
    fn detach(&mut self, key: &[u8]) -> (Option<Node>, Vec<(Rel, Node)>) {
        let node = self.nodes.remove(key);
        let mut prefix = key.to_vec();
        prefix.push(b'/');
        let descendants: Vec<Rel> = self
            .nodes
            .range::<[u8], _>((
                std::ops::Bound::Included(prefix.as_slice()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|(candidate, _)| candidate.starts_with(&prefix))
            .map(|(candidate, _)| candidate.clone())
            .collect();
        let moved = descendants
            .into_iter()
            .map(|candidate| {
                let suffix = candidate[key.len()..].to_vec();
                let node = self.nodes.remove(&candidate).expect("just enumerated");
                (suffix, node)
            })
            .collect();
        (node, moved)
    }

    fn create(&mut self, key: &[u8]) {
        // A create over an existing path replaces it: whatever was there is gone, and if it came
        // from the parent snapshot the index still has a row for it.
        let (existing, descendants) = self.detach(key);
        self.record_removals(existing, descendants);
        self.nodes.insert(
            key.to_vec(),
            Node {
                origin: Origin::Created,
                dirty: true,
            },
        );
    }

    fn touch(&mut self, key: &[u8]) {
        if let Some(node) = self.nodes.get_mut(key) {
            node.dirty = true;
            return;
        }
        let origin = self.baseline_of(key);
        self.nodes.insert(
            key.to_vec(),
            Node {
                origin,
                dirty: true,
            },
        );
    }

    fn rename(&mut self, from: &[u8], to: &[u8]) {
        let (node, descendants) = self.detach(from);
        let origin = node.map_or_else(|| self.baseline_of(from), |node| node.origin);
        let (victim, victim_descendants) = self.detach(to);
        self.record_removals(victim, victim_descendants);
        self.nodes.insert(
            to.to_vec(),
            Node {
                origin,
                dirty: true,
            },
        );
        for (suffix, node) in descendants {
            let mut key = to.to_vec();
            key.extend_from_slice(&suffix);
            self.nodes.insert(key, node);
        }
    }

    fn unlink(&mut self, key: &[u8]) {
        let (node, descendants) = self.detach(key);
        if node.is_none()
            && let Origin::Existing(original) = self.baseline_of(key)
        {
            // Never mentioned before, so the parent snapshot is the only place it can have been.
            self.removed.push(original);
        }
        self.record_removals(node, descendants);
    }

    /// A node (and anything under it) has left the namespace. Only what the parent snapshot
    /// actually held becomes a removal — something the delta both created and destroyed is a net
    /// nothing, and telling the index about it would be an invented change.
    fn record_removals(&mut self, node: Option<Node>, descendants: Vec<(Rel, Node)>) {
        for node in node
            .into_iter()
            .chain(descendants.into_iter().map(|(_, n)| n))
        {
            if let Origin::Existing(original) = node.origin {
                self.removed.push(original);
            }
        }
    }

    fn finish(self) -> DeltaPlan {
        let mut moves = Vec::new();
        let mut creates = Vec::new();
        let mut touches = Vec::new();
        for (final_path, node) in &self.nodes {
            match &node.origin {
                Origin::Created => creates.push(final_path.clone()),
                Origin::Existing(original) if original != final_path => {
                    moves.push((original.clone(), final_path.clone()));
                }
                Origin::Existing(_) if node.dirty => touches.push(final_path.clone()),
                Origin::Existing(_) => {}
            }
        }
        // A directory move already carries its subtree (`Mutation::Move` rewrites every descendant
        // row), so a child whose move is exactly its parent's move is not a second mutation.
        let all = moves.clone();
        moves.retain(|(from, to)| !implied_by_ancestor(&all, from, to));
        // Deepest source first: a child leaving a directory must be re-pathed before the directory
        // itself moves, or the directory's subtree rewrite takes the child with it.
        moves.sort_by(|(a, _), (b, _)| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));
        creates.sort_by_key(|path| (depth(path), path.clone()));
        let removes = self
            .removed
            .iter()
            .map(|original| readdress(&moves, original))
            .collect();
        DeltaPlan {
            moves,
            removes,
            creates,
            touches,
        }
    }
}

/// Whether `(from, to)` is exactly what some other move in `moves` already does to this path.
fn implied_by_ancestor(moves: &[(Rel, Rel)], from: &[u8], to: &[u8]) -> bool {
    moves.iter().any(|(ancestor_from, ancestor_to)| {
        if ancestor_from.len() >= from.len() || !is_prefix_path(ancestor_from, from) {
            return false;
        }
        let suffix = &from[ancestor_from.len()..];
        let mut expected = ancestor_to.clone();
        expected.extend_from_slice(suffix);
        expected == to
    })
}

/// Where a baseline path ends up once the accepted moves have been applied — the address a
/// removal has to use, because the row will have been re-pathed by then.
fn readdress(moves: &[(Rel, Rel)], original: &[u8]) -> Rel {
    let mut best: Option<&(Rel, Rel)> = None;
    for candidate in moves {
        if is_prefix_path(&candidate.0, original)
            && best.is_none_or(|current| candidate.0.len() > current.0.len())
        {
            best = Some(candidate);
        }
    }
    match best {
        Some((from, to)) => {
            let mut rebuilt = to.clone();
            rebuilt.extend_from_slice(&original[from.len()..]);
            rebuilt
        }
        None => original.to_vec(),
    }
}

/// Component-boundary prefix test, so `a/bc` is not under `a/b`.
fn is_prefix_path(prefix: &[u8], path: &[u8]) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !path.starts_with(prefix) {
        return false;
    }
    matches!(path.get(prefix.len()), None | Some(b'/'))
}

// The workspace carries neither `bytecount` nor `memchr`, and these are short path strings
// counted once per planned move; a new dependency through cargo-deny would buy nothing.
#[allow(clippy::naive_bytecount)]
fn depth(path: &[u8]) -> usize {
    path.iter().filter(|byte| **byte == b'/').count()
}

/// Replay a decoded stream into its net effect on the namespace.
///
/// Commands that touch only file contents or attributes (`write`, `update_extent`, `truncate`,
/// `chmod`, `chown`, `utimes`, `fallocate`, xattrs) mark their path changed; commands that move
/// the namespace are replayed in order.
pub fn plan(stream: &SendStream) -> Result<DeltaPlan, DeltaError> {
    let mut replay = Replay::new();
    for command in &stream.commands {
        match command.cmd {
            SendCmd::Subvol | SendCmd::Snapshot | SendCmd::End | SendCmd::Unknown(_) => {}
            SendCmd::Rename => {
                let from = command.path().ok_or(DeltaError::MissingPath(command.cmd))?;
                let to = command
                    .path_to()
                    .ok_or(DeltaError::MissingPath(command.cmd))?;
                let (from, to) = (from.to_vec(), to.to_vec());
                replay.rename(&from, &to);
            }
            SendCmd::Unlink | SendCmd::RmDir => {
                let path = command
                    .path()
                    .ok_or(DeltaError::MissingPath(command.cmd))?
                    .to_vec();
                replay.unlink(&path);
            }
            other => {
                let path = command
                    .path()
                    .ok_or(DeltaError::MissingPath(command.cmd))?
                    .to_vec();
                if other.creates() {
                    replay.create(&path);
                } else {
                    replay.touch(&path);
                }
            }
        }
    }
    Ok(replay.finish())
}

// --- turning a plan into mutations -------------------------------------------------------------

/// Where metadata for a changed path comes from.
///
/// A trait rather than a bare `lstat` for two reasons: the decoder's tests must not need a
/// filesystem, and the measurement harness needs to compare stat-in-the-live-tree against
/// stat-in-the-snapshot (they differ — a snapshot has its own `st_dev`, so metadata taken from one
/// would not compare equal to a scan of the live tree).
///
/// It is handed the **absolute** path, already resolved against
/// [`DeltaTarget::subvolume_path`], and that is deliberate: the path an implementation stats and
/// the path the resulting mutation carries are then the same bytes by construction, so no
/// implementation can join a stream-relative path onto a root of its own choosing.
pub trait MetaLookup: std::fmt::Debug {
    /// The metadata of an absolute path in the live tree, or `None` if it is no longer there.
    fn lookup(&self, path: &NativePath) -> Option<EntryMeta>;
}

/// `lstat`, exactly as a scan would. It holds no root of its own — [`DeltaTarget`] resolved the
/// path, and must have resolved it against the *live* subvolume rather than the snapshot: `st_dev`
/// differs between a btrfs subvolume and its snapshot, and the index stores `(dev, ino)`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsMetadata;

impl MetaLookup for FsMetadata {
    fn lookup(&self, path: &NativePath) -> Option<EntryMeta> {
        let path = path.to_path();
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        Some(EntryMeta::from_symlink_metadata(&path, &metadata))
    }
}

/// What a delta is being applied to.
///
/// The two paths are not the same thing and are routinely different. `root_path` must be
/// `subvolume_path` or a directory under it; a target that breaks that describes no path at all
/// and every planned path lands in [`DeltaReport::outside_root`].
#[derive(Clone, Debug)]
pub struct DeltaTarget {
    pub root: RootId,
    /// Where the sent subvolume is mounted in the **live** tree — never the read-only snapshot,
    /// whose `st_dev` is its own. Every path in the stream is relative to this.
    pub subvolume_path: NativePath,
    /// The indexed root's path in the live tree — the prefix every emitted mutation carries, and
    /// the bound outside which a stream path is another root's business.
    pub root_path: NativePath,
    /// The same policy the scanner and the watcher use, so a delta cannot introduce a path a scan
    /// would have skipped.
    pub policy: RootPolicy,
}

/// What a delta actually did, as numbers rather than adjectives.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeltaReport {
    pub stream_bytes: usize,
    pub commands: usize,
    /// Paths the plan named, before policy filtering.
    pub planned_paths: usize,
    pub mutations: usize,
    pub creates: usize,
    pub removes: usize,
    pub moves: usize,
    pub touches: usize,
    /// Paths dropped because [`RootPolicy`] excludes them — a delta must index exactly what a scan
    /// would, no more.
    pub excluded: usize,
    /// Paths the stream named that lie outside the indexed root. A subvolume is larger than one
    /// root: a delta of `@home` names every user's home, and only one of them is this root's.
    /// Kept apart from `vanished` on purpose — this is "not ours", that is "ours and gone", and a
    /// target whose two roots were confused shows up here as the whole delta.
    pub outside_root: usize,
    /// Paths that were gone by the time their metadata was read. A delta describes a snapshot; the
    /// live tree has moved on. These are dropped, and the count is the honest measure of the race.
    pub vanished: usize,
    /// Commands whose kind this decoder does not know.
    pub unknown_commands: usize,
}

/// What a stream-relative path turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Resolved {
    /// Under the indexed root, and the policy would have indexed it: this is a row.
    Indexed(NativePath),
    /// Under the indexed root, but the policy excludes it — a scan would have written no row.
    Excluded,
    /// Not under the indexed root at all. Some other root's business, or nobody's.
    Outside,
}

/// Turn a plan into the batch the index applies.
///
/// Two filters, and they are not the same one. A path is first *placed*: resolved against the
/// subvolume root the stream speaks in, and kept only if it lands inside this root. What survives
/// is then asked of the policy — which happens here rather than in the replay because a move can
/// straddle the policy boundary: a file dragged out of `node_modules/` is a *creation* as far as
/// the index is concerned, and one dragged into it is a *deletion*. The same is true of the root
/// boundary, and for the same reason.
///
/// One thing this cannot do on its own: a *directory* that arrives from outside the indexed
/// region — dragged out of `node_modules/`, or in from another root's tree — is a single
/// `Upsert`, because the stream names the directory and not the children that came with it. The
/// rows underneath it have to come from a targeted scan of that path; a delta has no walker.
#[must_use]
pub fn to_mutations(
    plan: &DeltaPlan,
    target: &DeltaTarget,
    meta: &dyn MetaLookup,
) -> (MutationBatch, DeltaReport) {
    debug_assert!(
        target.root_path.starts_with(&target.subvolume_path),
        "the indexed root {} is not inside the sent subvolume {}: no stream path can resolve \
         into it",
        target.root_path.display(),
        target.subvolume_path.display(),
    );
    let mut report = DeltaReport {
        planned_paths: plan.len(),
        ..DeltaReport::default()
    };
    let mut mutations = Vec::new();

    for (from, to) in &plan.moves {
        match (resolve(target, from), resolve(target, to)) {
            (Resolved::Indexed(from), Resolved::Indexed(to)) => {
                let Some(entry) = meta.lookup(&to) else {
                    report.vanished += 1;
                    continue;
                };
                report.moves += 1;
                mutations.push(Mutation::Move {
                    root: target.root,
                    from,
                    to,
                    meta: entry,
                });
            }
            // It was ours and now it is not: the row goes, addressed where the index has it.
            (Resolved::Indexed(from), _) => {
                report.removes += 1;
                mutations.push(Mutation::Remove {
                    root: target.root,
                    path: from,
                });
            }
            // It was not ours and now it is: as new as anything the delta created.
            (_, Resolved::Indexed(to)) => {
                push_upsert(&mut mutations, &mut report, target, meta, to, false);
            }
            // Neither end is a row of ours. Which counter says why, and "not this root's tree at
            // all" wins only when that is true of both ends.
            (Resolved::Outside, Resolved::Outside) => report.outside_root += 1,
            _ => report.excluded += 1,
        }
    }

    for path in &plan.removes {
        match resolve(target, path) {
            Resolved::Indexed(path) => {
                report.removes += 1;
                mutations.push(Mutation::Remove {
                    root: target.root,
                    path,
                });
            }
            Resolved::Excluded => report.excluded += 1,
            Resolved::Outside => report.outside_root += 1,
        }
    }

    for path in &plan.creates {
        if let Some(path) = counted(target, &mut report, path) {
            push_upsert(&mut mutations, &mut report, target, meta, path, false);
        }
    }
    for path in &plan.touches {
        if let Some(path) = counted(target, &mut report, path) {
            push_upsert(&mut mutations, &mut report, target, meta, path, true);
        }
    }

    report.mutations = mutations.len();
    (
        MutationBatch::new(MutationSource::Reconcile, mutations),
        report,
    )
}

/// Resolve one path, counting the reason it is not a row of ours.
fn counted(target: &DeltaTarget, report: &mut DeltaReport, relative: &[u8]) -> Option<NativePath> {
    match resolve(target, relative) {
        Resolved::Indexed(path) => Some(path),
        Resolved::Excluded => {
            report.excluded += 1;
            None
        }
        Resolved::Outside => {
            report.outside_root += 1;
            None
        }
    }
}

fn push_upsert(
    mutations: &mut Vec<Mutation>,
    report: &mut DeltaReport,
    target: &DeltaTarget,
    meta: &dyn MetaLookup,
    path: NativePath,
    metadata_only: bool,
) {
    let Some(entry) = meta.lookup(&path) else {
        report.vanished += 1;
        return;
    };
    if metadata_only {
        report.touches += 1;
        mutations.push(Mutation::Touch {
            root: target.root,
            path,
            meta: entry,
        });
    } else {
        report.creates += 1;
        mutations.push(Mutation::Upsert {
            root: target.root,
            path,
            meta: entry,
        });
    }
}

/// Place a stream-relative path in the live tree, and say whether it is a row of this root's.
///
/// The containment test is [`NativePath::starts_with`], which is component-bounded: `/home/dkx`
/// is not under `/home/dk`. The root itself resolves as `Indexed` — a delta that touched the root
/// directory changes the root's own row, exactly as a scan would record it.
fn resolve(target: &DeltaTarget, relative: &[u8]) -> Resolved {
    let path = target.subvolume_path.join(relative);
    if !path.starts_with(&target.root_path) {
        return Resolved::Outside;
    }
    if included(target, &path) {
        Resolved::Indexed(path)
    } else {
        Resolved::Excluded
    }
}

/// Whether the root's policy would have indexed this absolute path.
///
/// The base-bounded form, which is what the scanner uses and what [`crate::Index::apply`] will
/// re-check the emitted mutation against: components *above* the root are never classified, so a
/// root the user registered inside a build directory is indexed normally. Asking with the
/// subvolume-relative path instead would silently never match a subtree rule, which is written in
/// absolute paths.
fn included(target: &DeltaTarget, path: &NativePath) -> bool {
    target
        .policy
        .stored_coverage_within(&target.root_path, path, crate::EntryKind::Directory)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;

    /// A minimal encoder, for tests only.
    ///
    /// The lane's rule is that nothing here generates a stream for real: a production encoder is
    /// btrfs's job and duplicating it would be a second implementation to keep correct. But the
    /// decoder has to be exercised before a Btrfs filesystem exists on this machine, and the
    /// replay's hard cases (rename cycles, temporary names, straddling moves) are far easier to
    /// write down than to provoke out of a real kernel. So: a builder that writes the same bytes
    /// the kernel does, confined to `#[cfg(test)]`, and checked against a real stream by the
    /// fixture harness.
    #[derive(Debug, Default)]
    struct StreamBuilder {
        bytes: Vec<u8>,
        version: u32,
    }

    impl StreamBuilder {
        fn new() -> Self {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(MAGIC);
            bytes.extend_from_slice(&1u32.to_le_bytes());
            Self { bytes, version: 1 }
        }

        fn command(mut self, cmd: u16, attrs: &[(u16, &[u8])]) -> Self {
            let mut body = Vec::new();
            for (kind, value) in attrs {
                body.extend_from_slice(&kind.to_le_bytes());
                body.extend_from_slice(&(value.len() as u16).to_le_bytes());
                body.extend_from_slice(value);
            }
            let mut command = Vec::new();
            command.extend_from_slice(&(body.len() as u32).to_le_bytes());
            command.extend_from_slice(&cmd.to_le_bytes());
            command.extend_from_slice(&0u32.to_le_bytes());
            command.extend_from_slice(&body);
            let crc = crc32c(&command);
            command[6..10].copy_from_slice(&crc.to_le_bytes());
            self.bytes.extend_from_slice(&command);
            self
        }

        fn snapshot(self) -> Self {
            self.command(
                2,
                &[
                    (attr::PATH, b"snap-b"),
                    (attr::UUID, &[7u8; 16]),
                    (attr::CTRANSID, &9u64.to_le_bytes()),
                    (attr::CLONE_UUID, &[3u8; 16]),
                    (attr::CLONE_CTRANSID, &4u64.to_le_bytes()),
                ],
            )
        }

        fn mkfile(self, path: &[u8]) -> Self {
            self.command(3, &[(attr::PATH, path), (attr::INO, &257u64.to_le_bytes())])
        }

        fn mkdir(self, path: &[u8]) -> Self {
            self.command(4, &[(attr::PATH, path), (attr::INO, &258u64.to_le_bytes())])
        }

        fn symlink(self, path: &[u8], target: &[u8]) -> Self {
            self.command(8, &[(attr::PATH, path), (attr::PATH_LINK, target)])
        }

        fn link(self, path: &[u8], existing: &[u8]) -> Self {
            self.command(10, &[(attr::PATH, path), (attr::PATH_LINK, existing)])
        }

        fn rename(self, from: &[u8], to: &[u8]) -> Self {
            self.command(9, &[(attr::PATH, from), (attr::PATH_TO, to)])
        }

        fn unlink(self, path: &[u8]) -> Self {
            self.command(11, &[(attr::PATH, path)])
        }

        fn rmdir(self, path: &[u8]) -> Self {
            self.command(12, &[(attr::PATH, path)])
        }

        fn truncate(self, path: &[u8], size: u64) -> Self {
            self.command(17, &[(attr::PATH, path), (attr::SIZE, &size.to_le_bytes())])
        }

        fn chmod(self, path: &[u8], mode: u64) -> Self {
            self.command(18, &[(attr::PATH, path), (attr::MODE, &mode.to_le_bytes())])
        }

        fn utimes(self, path: &[u8]) -> Self {
            self.command(20, &[(attr::PATH, path)])
        }

        fn update_extent(self, path: &[u8]) -> Self {
            self.command(22, &[(attr::PATH, path)])
        }

        fn end(mut self) -> Vec<u8> {
            let _ = self.version;
            self = self.command(21, &[]);
            self.bytes
        }
    }

    fn planned(bytes: &[u8]) -> DeltaPlan {
        plan(&decode(bytes).expect("decodes")).expect("plans")
    }

    /// Metadata for whatever the test says exists, keyed by the **absolute** path — which is what
    /// the lookup is handed, so a test that names the wrong one finds nothing rather than quietly
    /// agreeing with a wrong join.
    #[derive(Debug, Default)]
    struct FakeMeta {
        present: std::collections::HashSet<Vec<u8>>,
    }

    impl FakeMeta {
        fn with(paths: &[&[u8]]) -> Self {
            Self {
                present: paths.iter().map(|path| path.to_vec()).collect(),
            }
        }
    }

    impl MetaLookup for FakeMeta {
        fn lookup(&self, path: &NativePath) -> Option<EntryMeta> {
            self.present
                .contains(path.as_bytes())
                .then(EntryMeta::default)
        }
    }

    /// The production shape: `@home` is the subvolume and mounts at `/home`, so the stream speaks
    /// in `dk/...` and the indexed root is one directory down. Every `to_mutations` test uses it,
    /// because a target whose two roots are the same is exactly the blind spot that let a delta
    /// join stream paths onto the wrong directory and count the results as vanished.
    fn target() -> DeltaTarget {
        DeltaTarget {
            root: crate::RootId(1),
            subvolume_path: NativePath::from_bytes(b"/home".to_vec()),
            root_path: NativePath::from_bytes(b"/home/dk".to_vec()),
            policy: RootPolicy::default_home(),
        }
    }

    /// The degenerate shape, where the indexed root *is* the subvolume root.
    fn flat_target() -> DeltaTarget {
        DeltaTarget {
            root: crate::RootId(1),
            subvolume_path: NativePath::from_bytes(b"/srv/data".to_vec()),
            root_path: NativePath::from_bytes(b"/srv/data".to_vec()),
            policy: RootPolicy::default_home(),
        }
    }

    fn paths_of(batch: &MutationBatch) -> Vec<String> {
        let mut paths: Vec<String> = batch
            .mutations
            .iter()
            .map(|mutation| match mutation {
                Mutation::Upsert { path, .. } | Mutation::Touch { path, .. } => {
                    format!("upsert {}", path.display())
                }
                Mutation::Remove { path, .. } => format!("remove {}", path.display()),
                Mutation::Move { from, to, .. } => {
                    format!("move {} -> {}", from.display(), to.display())
                }
                other @ Mutation::Invalidate { .. } => format!("{other:?}"),
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn a_stream_header_is_recognised_and_a_foreign_one_is_not() {
        let bytes = StreamBuilder::new().snapshot().end();
        let stream = decode(&bytes).expect("decodes");
        assert_eq!(stream.version, 1);
        assert!(stream.is_incremental());
        assert_eq!(decode(b"not a stream at all"), Err(DeltaError::NotAStream));
    }

    #[test]
    fn an_interrupted_stream_is_refused_rather_than_half_applied() {
        // This is the whole safety argument for the backend: a delta that stopped early describes
        // a state the filesystem was never in, so there is no correct way to apply part of it.
        let bytes = StreamBuilder::new().snapshot().mkfile(b"a").end();
        for cut in [HEADER_LEN + 3, bytes.len() - 4, bytes.len() - 1] {
            let error = decode(&bytes[..cut]).expect_err("must refuse");
            assert!(
                matches!(
                    error,
                    DeltaError::Truncated { .. } | DeltaError::Unterminated
                ),
                "{error:?} at cut {cut}"
            );
        }
    }

    #[test]
    fn a_stream_without_an_end_command_is_not_a_delta() {
        let mut builder = StreamBuilder::new().snapshot().mkfile(b"a");
        builder.bytes.truncate(builder.bytes.len());
        assert_eq!(decode(&builder.bytes), Err(DeltaError::Unterminated));
    }

    #[test]
    fn a_flipped_bit_fails_the_crc_instead_of_becoming_a_mutation() {
        let mut bytes = StreamBuilder::new().snapshot().mkfile(b"a").end();
        let last = bytes.len() - 6;
        bytes[last] ^= 0x01;
        assert!(matches!(
            decode(&bytes),
            Err(DeltaError::CorruptCommand { .. })
        ));
    }

    #[test]
    fn creation_deletion_and_metadata_change_reduce_to_the_obvious_plan() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkdir(b"new")
                .mkfile(b"new/file")
                .unlink(b"gone")
                .chmod(b"kept", 0o644)
                .end(),
        );
        assert_eq!(plan.creates, vec![b"new".to_vec(), b"new/file".to_vec()]);
        assert_eq!(plan.removes, vec![b"gone".to_vec()]);
        assert_eq!(plan.touches, vec![b"kept".to_vec()]);
        assert!(plan.moves.is_empty());
    }

    #[test]
    fn a_temporary_name_never_reaches_the_index() {
        // `btrfs send` renames through `o<ino>-<gen>-<seq>` whenever a destination is not yet
        // free. A decoder that emitted one mutation per command would put that name in the index.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"a", b"o257-9-0")
                .rename(b"b", b"a")
                .rename(b"o257-9-0", b"b")
                .end(),
        );
        assert_eq!(
            plan.moves,
            vec![
                (b"a".to_vec(), b"b".to_vec()),
                (b"b".to_vec(), b"a".to_vec())
            ]
        );
        assert!(plan.creates.is_empty() && plan.removes.is_empty());
    }

    #[test]
    fn a_file_created_and_then_deleted_inside_the_window_is_not_a_change() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"scratch")
                .truncate(b"scratch", 10)
                .unlink(b"scratch")
                .end(),
        );
        assert!(plan.is_empty(), "{plan:?}");
    }

    #[test]
    fn a_directory_move_is_one_mutation_and_not_one_per_child() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"docs", b"papers")
                .utimes(b"papers/one")
                .end(),
        );
        assert_eq!(plan.moves, vec![(b"docs".to_vec(), b"papers".to_vec())]);
        // The child was mentioned, but its move is exactly its parent's move.
        assert!(plan.moves.len() == 1 && plan.touches.is_empty(), "{plan:?}");
    }

    #[test]
    fn a_child_leaving_a_moved_directory_is_re_pathed_before_the_directory_moves() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"a/x", b"b/x")
                .rename(b"a", b"c")
                .end(),
        );
        assert_eq!(
            plan.moves,
            vec![
                (b"a/x".to_vec(), b"b/x".to_vec()),
                (b"a".to_vec(), b"c".to_vec()),
            ],
            "the deeper source has to move first, or the directory rewrite drags it along"
        );
    }

    #[test]
    fn a_deletion_under_a_directory_that_then_moves_is_addressed_where_the_row_ends_up() {
        // Applied in order, `Move(a -> b)` re-paths `a/f` to `b/f`. A removal addressed at `a/f`
        // would then find nothing and leave a row for a file that is gone.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .unlink(b"a/f")
                .rename(b"a", b"b")
                .end(),
        );
        assert_eq!(plan.moves, vec![(b"a".to_vec(), b"b".to_vec())]);
        assert_eq!(plan.removes, vec![b"b/f".to_vec()]);
    }

    #[test]
    fn a_deletion_after_a_directory_move_resolves_through_the_baseline() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"a", b"b")
                .unlink(b"b/old")
                .end(),
        );
        assert_eq!(plan.moves, vec![(b"a".to_vec(), b"b".to_vec())]);
        assert_eq!(plan.removes, vec![b"b/old".to_vec()]);
    }

    #[test]
    fn a_swap_through_an_intermediate_directory_name_resolves_to_the_right_row() {
        // `a` becomes `b`, `x` becomes `a`, and then something under the *new* `a` is deleted.
        // Reading the stream literally would remove a row that belongs to the old `a`.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"a", b"b")
                .rename(b"x", b"a")
                .unlink(b"a/old")
                .end(),
        );
        assert_eq!(plan.removes, vec![b"a/old".to_vec()]);
    }

    #[test]
    fn replacing_a_path_removes_the_old_row_before_the_new_one_is_written() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .unlink(b"conf")
                .mkfile(b"conf")
                .end(),
        );
        assert_eq!(plan.removes, vec![b"conf".to_vec()]);
        assert_eq!(plan.creates, vec![b"conf".to_vec()]);
    }

    #[test]
    fn a_directory_recreated_after_removal_keeps_the_removal_first() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rmdir(b"build")
                .mkdir(b"build")
                .mkfile(b"build/out")
                .end(),
        );
        assert_eq!(plan.removes, vec![b"build".to_vec()]);
        assert_eq!(
            plan.creates,
            vec![b"build".to_vec(), b"build/out".to_vec()],
            "parents before children, so a row's parent_id can resolve"
        );
    }

    #[test]
    fn hard_links_and_symlinks_are_separate_paths_of_their_own() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .link(b"second-name", b"first-name")
                .symlink(b"pointer", b"first-name")
                .end(),
        );
        assert_eq!(
            plan.creates,
            vec![b"pointer".to_vec(), b"second-name".to_vec()]
        );
    }

    #[test]
    fn a_rewrite_in_place_is_a_metadata_touch() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .update_extent(b"notes.md")
                .utimes(b"notes.md")
                .end(),
        );
        assert_eq!(plan.touches, vec![b"notes.md".to_vec()]);
    }

    /// Coverage tiers changed what "the same paths a scan would" means: a build directory is
    /// `Opaque`, so the directory itself is stored — known and sized — while nothing inside it
    /// is. The delta follows the scanner, which is the point of asking the policy at all.
    #[test]
    fn the_root_policy_excludes_the_same_paths_a_scan_would() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"dk/project/target/debug/thing.o")
                .mkfile(b"dk/project/src/main.rs")
                .mkdir(b"dk/project/node_modules")
                .end(),
        );
        let meta = FakeMeta::with(&[
            b"/home/dk/project/src/main.rs",
            b"/home/dk/project/node_modules",
        ]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(
            report.excluded, 1,
            "only the file inside the build directory has no row"
        );
        let paths: Vec<&[u8]> = batch
            .mutations
            .iter()
            .filter_map(|mutation| match mutation {
                Mutation::Upsert { path, .. } => Some(path.as_bytes()),
                _ => None,
            })
            .collect();
        let mut paths = paths;
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                &b"/home/dk/project/node_modules"[..],
                &b"/home/dk/project/src/main.rs"[..]
            ]
        );
    }

    #[test]
    fn a_move_across_the_policy_boundary_becomes_a_creation_or_a_deletion() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"dk/target/thing", b"dk/src/thing")
                .rename(b"dk/src/other", b"dk/target/other")
                .end(),
        );
        let meta = FakeMeta::with(&[b"/home/dk/src/thing"]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(
            report.creates, 1,
            "out of an excluded directory: a creation"
        );
        assert_eq!(report.removes, 1, "into an excluded directory: a deletion");
        assert_eq!(batch.mutations.len(), 2);
    }

    #[test]
    fn a_path_that_vanished_before_its_metadata_was_read_is_counted_not_invented() {
        let plan = planned(&StreamBuilder::new().snapshot().mkfile(b"dk/ghost").end());
        let (batch, report) = to_mutations(&plan, &target(), &FakeMeta::default());
        assert!(batch.mutations.is_empty());
        assert_eq!(report.vanished, 1);
    }

    #[test]
    fn a_whole_directory_rename_costs_one_mutation() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"dk/Photos", b"dk/Pictures")
                .end(),
        );
        let meta = FakeMeta::with(&[b"/home/dk/Pictures"]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(report.moves, 1);
        assert_eq!(batch.mutations.len(), 1);
    }

    #[test]
    fn component_prefix_matching_does_not_confuse_siblings() {
        assert!(is_prefix_path(b"a/b", b"a/b/c"));
        assert!(is_prefix_path(b"a/b", b"a/b"));
        assert!(!is_prefix_path(b"a/b", b"a/bc"));
        assert!(!is_prefix_path(b"a/b", b"a"));
    }

    #[test]
    fn the_policy_test_looks_at_every_component_not_only_the_leaf() {
        let target = target();
        let path = |bytes: &[u8]| NativePath::from_bytes(bytes.to_vec());
        assert!(included(&target, &path(b"/home/dk/src/main.rs")));
        assert!(!included(&target, &path(b"/home/dk/a/.git/config")));
        // An `Opaque` directory is itself stored; only what is inside it is not.
        assert!(included(&target, &path(b"/home/dk/node_modules")));
        assert!(!included(
            &target,
            &path(b"/home/dk/node_modules/left-pad/index.js")
        ));
        assert!(included(&target, &path(b"/home/dk/targeted/file")));
        // Components *above* the root are the user's choice of root, not a rule to apply: a home
        // called `.git` would still be indexed.
        assert!(included(&target, &path(b"/home/dk/file")));
    }

    #[test]
    fn an_unknown_command_does_not_stop_the_decode() {
        let bytes = StreamBuilder::new()
            .snapshot()
            .command(200, &[(attr::PATH, b"whatever")])
            .mkfile(b"a")
            .end();
        let stream = decode(&bytes).expect("decodes");
        assert!(
            stream
                .commands
                .iter()
                .any(|c| c.cmd == SendCmd::Unknown(200))
        );
        let plan = plan(&stream).expect("plans");
        assert_eq!(plan.creates, vec![b"a".to_vec()]);
    }

    #[test]
    fn a_full_send_is_not_mistaken_for_a_delta() {
        // Without `-p`, the first command is SUBVOL and there is no parent. Applying that as a
        // delta would read as "the whole subvolume is new".
        let bytes = StreamBuilder::new()
            .command(
                1,
                &[
                    (attr::PATH, b"snap"),
                    (attr::UUID, &[1u8; 16]),
                    (attr::CTRANSID, &1u64.to_le_bytes()),
                ],
            )
            .mkfile(b"a")
            .end();
        let stream = decode(&bytes).expect("decodes");
        assert!(!stream.is_incremental());
    }

    #[derive(Debug)]
    struct AlwaysRegular;

    impl MetaLookup for AlwaysRegular {
        fn lookup(&self, _path: &NativePath) -> Option<EntryMeta> {
            Some(EntryMeta {
                kind: EntryKind::Regular,
                ..EntryMeta::default()
            })
        }
    }

    #[test]
    fn stream_paths_resolve_against_the_subvolume_and_not_the_indexed_root() {
        // The shape every real deployment has: `@home` is the subvolume, `/home/dk` is the root.
        // Joining `dk/inbox/new.txt` onto the *root* would produce `/home/dk/dk/inbox/new.txt`,
        // which exists nowhere — so every path would read as vanished and the delta would apply
        // almost nothing while reporting success.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"dk/inbox/new.txt")
                .unlink(b"dk/gone.txt")
                .rename(b"dk/a.txt", b"dk/b.txt")
                .rename(b"dk/docs", b"dk/papers")
                .end(),
        );
        let meta = FakeMeta::with(&[
            b"/home/dk/inbox/new.txt",
            b"/home/dk/b.txt",
            b"/home/dk/papers",
        ]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(
            paths_of(&batch),
            vec![
                "move /home/dk/a.txt -> /home/dk/b.txt".to_owned(),
                "move /home/dk/docs -> /home/dk/papers".to_owned(),
                "remove /home/dk/gone.txt".to_owned(),
                "upsert /home/dk/inbox/new.txt".to_owned(),
            ]
        );
        assert_eq!(
            report.vanished, 0,
            "every path was found where it really is"
        );
        assert_eq!(report.outside_root, 0);
        assert_eq!(report.planned_paths, 4);
    }

    #[test]
    fn a_path_belonging_to_another_home_is_skipped_and_counted_not_applied() {
        // A delta of `@home` names every user's home. Applying one of those to this root would
        // write another user's tree into this user's index — and counting it as `vanished` would
        // hide that the delta was pointed at the wrong subtree.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"other/x")
                .mkfile(b"dk/mine")
                .unlink(b"other/old")
                .end(),
        );
        // Both really exist: this is a placement decision, not a stat failing.
        let meta = FakeMeta::with(&[b"/home/other/x", b"/home/dk/mine", b"/home/other/old"]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(paths_of(&batch), vec!["upsert /home/dk/mine".to_owned()]);
        assert_eq!(
            report.outside_root, 2,
            "one create and one remove, not ours"
        );
        assert_eq!(report.vanished, 0, "they were not gone, they were not ours");
        assert_eq!(
            report.excluded, 0,
            "the policy had nothing to say about them"
        );
    }

    #[test]
    fn the_root_boundary_is_component_bounded_so_a_sibling_home_is_not_swallowed() {
        // `/home/dkx` shares a byte prefix with `/home/dk` and shares nothing else.
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"dkx/file")
                .mkfile(b"dk/file")
                .end(),
        );
        let meta = FakeMeta::with(&[b"/home/dkx/file", b"/home/dk/file"]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(paths_of(&batch), vec!["upsert /home/dk/file".to_owned()]);
        assert_eq!(report.outside_root, 1);
        assert_eq!(
            resolve(&target(), b"dkx"),
            Resolved::Outside,
            "a longer name is not a child"
        );
        assert_eq!(
            resolve(&target(), b"dk"),
            Resolved::Indexed(NativePath::from_bytes(b"/home/dk".to_vec())),
            "the root itself is a row of ours: a delta that touched it changes its own entry"
        );
    }

    #[test]
    fn a_move_across_the_root_boundary_is_a_creation_or_a_deletion() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .rename(b"other/in", b"dk/in")
                .rename(b"dk/out", b"other/out")
                .rename(b"other/a", b"other/b")
                .end(),
        );
        let meta = FakeMeta::with(&[b"/home/dk/in", b"/home/other/out", b"/home/other/b"]);
        let (batch, report) = to_mutations(&plan, &target(), &meta);
        assert_eq!(
            paths_of(&batch),
            vec![
                "remove /home/dk/out".to_owned(),
                "upsert /home/dk/in".to_owned(),
            ]
        );
        assert_eq!(report.outside_root, 1, "the move that never touched us");
        assert_eq!(report.creates, 1);
        assert_eq!(report.removes, 1);
    }

    #[test]
    fn a_root_that_is_the_whole_subvolume_still_resolves() {
        let plan = planned(
            &StreamBuilder::new()
                .snapshot()
                .mkfile(b"notes.md")
                .unlink(b"stale")
                .end(),
        );
        let meta = FakeMeta::with(&[b"/srv/data/notes.md"]);
        let (batch, report) = to_mutations(&plan, &flat_target(), &meta);
        assert_eq!(
            paths_of(&batch),
            vec![
                "remove /srv/data/stale".to_owned(),
                "upsert /srv/data/notes.md".to_owned(),
            ]
        );
        assert_eq!(report.outside_root, 0);
    }

    #[test]
    fn entry_kind_is_not_guessed_from_the_stream() {
        // The stream says `mkdir`, but the index's kind comes from the stat, so a directory that
        // has since been replaced by a file cannot be recorded as a directory.
        let plan = planned(&StreamBuilder::new().snapshot().mkdir(b"dk/d").end());
        let (batch, _) = to_mutations(&plan, &target(), &AlwaysRegular);
        assert!(matches!(
            &batch.mutations[0],
            Mutation::Upsert { meta, .. } if meta.kind == EntryKind::Regular
        ));
    }
}
