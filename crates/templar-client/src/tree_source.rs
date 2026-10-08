//! Store-free treemap providers. A visible tray consumes Index1 snapshots without scanning;
//! direct bounded scanning is only for an absent/incompatible service, never a stale index.
use crate::proxies::files::FilesIndexProxy;
use std::path::Path;
use templar_file_index::{
    Coverage, EntryKind, NativePath,
    query::FieldMask,
    snapshot::{HEADER_LEN, SnapshotDetail, SnapshotManifest, SnapshotReader, SnapshotRequest, SnapshotRoot},
};
use templar_treemap::RawEntry;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeLoad {
    pub entries: Vec<RawEntry>,
    /// Both namespace and measured-size truth, not merely "a frame decoded".
    pub complete: bool,
    pub generation: Option<u64>,
    pub snapshot_seq: Option<u64>,
    pub roots: Vec<SnapshotRoot>,
    pub reason: Option<String>,
}

#[derive(Debug)]
pub enum TreeSourceError {
    NoService,
    Incompatible(String),
    Call(String),
    Snapshot(String),
}

impl TreeSourceError {
    #[must_use]
    pub fn allows_fallback(&self) -> bool { matches!(self, Self::NoService | Self::Incompatible(_)) }
}

impl std::fmt::Display for TreeSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoService => write!(formatter, "no file index service is running"),
            Self::Incompatible(detail) => write!(formatter, "index snapshot is incompatible: {detail}"),
            Self::Call(detail) => write!(formatter, "index call failed: {detail}"),
            Self::Snapshot(detail) => write!(formatter, "snapshot did not parse: {detail}"),
        }
    }
}
impl std::error::Error for TreeSourceError {}

fn call_error(error: &zbus::Error) -> TreeSourceError {
    if let zbus::Error::MethodError(name, _, _) = error {
        match name.as_str() {
            "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner" => return TreeSourceError::NoService,
            "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownInterface" => return TreeSourceError::Incompatible(error.to_string()),
            _ => {}
        }
    }
    TreeSourceError::Call(error.to_string())
}

pub trait TreeSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DirectScanSource { pub limits: templar_treemap::ScanLimits }

impl TreeSource for DirectScanSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError> {
        let outcome = templar_treemap::scan(root, self.limits);
        let reason = if outcome.complete { None } else { Some(format!(
            "direct scan incomplete: entries limit={}, depth limit={}, missing={}, unreadable={}{}",
            outcome.entry_limit, outcome.depth_limit, outcome.missing_entries, outcome.unreadable_entries,
            outcome.last_error.as_ref().map(|error| format!(" ({error})")).unwrap_or_default(),
        )) };
        Ok(TreeLoad { entries: outcome.entries, complete: outcome.complete, reason, ..TreeLoad::default() })
    }
}

const TREE_FIELDS: FieldMask = FieldMask(FieldMask::PATH.0 | FieldMask::KIND.0 | FieldMask::SIZE.0 | FieldMask::COVERAGE.0 | FieldMask::AGGREGATE.0);
/// A transport/memory bound, not a claim that a partial frame represents a whole tree.
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

pub async fn load_snapshot_tree(connection: &zbus::Connection, root: &Path) -> Result<TreeLoad, TreeSourceError> {
    let proxy = FilesIndexProxy::new(connection).await.map_err(|error| call_error(&error))?;
    let request = SnapshotRequest {
        fields: TREE_FIELDS,
        detail: SnapshotDetail::Materialized,
        subtree: Some(NativePath::from_path(root)),
        include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
        ..SnapshotRequest::default()
    };
    let request_json = serde_json::to_string(&request).map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let (fd, manifest_json) = proxy.open_snapshot(&request_json).await.map_err(|error| call_error(&error))?;
    let manifest: SnapshotManifest = serde_json::from_str(&manifest_json)
        .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
    let bytes = read_fd(fd.into(), manifest.payload_len)
        .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
    decode_snapshot_tree(&bytes, &manifest, root)
}

/// The exact production decoder, usable by a filed-owned integration fixture without linking
/// a SQLite writer into client or shell. Header/manifest identity and native scope are checked.
pub fn decode_snapshot_tree(bytes: &[u8], manifest: &SnapshotManifest, root: &Path) -> Result<TreeLoad, TreeSourceError> {
    let reader = SnapshotReader::new(bytes).map_err(TreeSourceError::Snapshot)?;
    if !reader.field_mask.contains(TREE_FIELDS) {
        return Err(TreeSourceError::Incompatible("PATH/KIND/SIZE/COVERAGE/AGGREGATE fields are required".to_owned()));
    }
    if reader.format != manifest.format || reader.generation != manifest.generation
        || reader.field_mask.0 != manifest.field_mask || reader.entry_count != manifest.entry_count
        || bytes.len().checked_sub(HEADER_LEN as usize) != usize::try_from(manifest.payload_len).ok()
        || u32::from_le_bytes(bytes[56..60].try_into().unwrap()) != manifest.payload_crc32c
    { return Err(TreeSourceError::Snapshot("manifest does not identify this frame".to_owned())); }
    let roots = reader.roots().map_err(TreeSourceError::Snapshot)?;
    if roots.len() != manifest.roots.len() { return Err(TreeSourceError::Snapshot("manifest root count differs".to_owned())); }
    for ((id, path), status) in roots.iter().zip(&manifest.roots) {
        if *id != status.root || templar_file_index::query::base64(path.as_bytes()) != status.path_b64
            || status.generation > manifest.generation
        { return Err(TreeSourceError::Snapshot("manifest root identity differs".to_owned())); }
    }
    let wanted = NativePath::from_path(root);
    let relevant: Vec<_> = manifest.roots.iter().zip(&roots).filter(|(_, (_, path))|
        wanted.starts_with(path) || path.starts_with(&wanted)).collect();
    let mut complete = !relevant.is_empty() && relevant.iter().all(|(status, _)|
        status.state == "current" && status.complete && status.metadata == "verified");
    let mut reason = if relevant.is_empty() { Some("requested subtree has no indexed coverage".to_owned()) }
        else if complete { None }
        else { Some(relevant.iter().map(|(status, _)| format!("{}: {}, metadata {}{}",
            status.display, status.state, status.metadata,
            status.reason.as_ref().map(|reason| format!(" ({reason})")).unwrap_or_default(),
        )).collect::<Vec<_>>().join("; ")) };
    // Overlapping registered roots may carry the same path. Its most specific root owns it;
    // keys become RawEntry paths without another owned-path copy.
    let mut entries = std::collections::HashMap::<std::path::PathBuf, (usize, u64, bool)>::new();
    for entry in reader.entries().map_err(TreeSourceError::Snapshot)? {
        let path = entry.path.ok_or_else(|| TreeSourceError::Snapshot("missing native path".to_owned()))?;
        let Some((_, root_path)) = roots.iter().find(|(id, _)| *id == entry.root) else {
            return Err(TreeSourceError::Snapshot("entry names an unknown root".to_owned()));
        };
        if !path.starts_with(&wanted) || !path.starts_with(root_path) || path.as_bytes().contains(&0) {
            return Err(TreeSourceError::Snapshot("entry escapes the requested native scope".to_owned()));
        }
        let kind = entry.kind.ok_or_else(|| TreeSourceError::Snapshot("missing entry kind".to_owned()))?;
        let coverage = entry.coverage.ok_or_else(|| TreeSourceError::Snapshot("missing entry coverage".to_owned()))?;
        let is_dir = kind == EntryKind::Directory;
        let size = if is_dir {
            if coverage == Coverage::Opaque {
                if let Some(aggregate) = entry.aggregate {
                    if aggregate.stale || !aggregate.readable {
                        complete = false;
                        reason = Some("cached opaque-directory sizes are stale or unreadable".to_owned());
                    }
                    aggregate.recursive_bytes
                } else {
                    complete = false;
                    reason = Some("opaque-directory sizes have not been measured".to_owned());
                    0
                }
            } else { 0 }
        } else { entry.size_bytes.ok_or_else(|| TreeSourceError::Snapshot("missing file size".to_owned()))? };
        let specificity = root_path.as_bytes().len();
        match entries.entry(path.into_path()) {
            std::collections::hash_map::Entry::Occupied(mut old) if old.get().0 < specificity => { old.insert((specificity, size, is_dir)); }
            std::collections::hash_map::Entry::Vacant(slot) => { slot.insert((specificity, size, is_dir)); }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    Ok(TreeLoad {
        entries: entries.into_iter().map(|(path, (_, size, is_dir))| RawEntry { path, size, is_dir }).collect(),
        complete, generation: Some(manifest.generation.0), snapshot_seq: Some(manifest.snapshot_seq),
        roots: relevant.into_iter().map(|(status, _)| status.clone()).collect(), reason,
    })
}

fn read_fd(fd: std::os::fd::OwnedFd, payload_len: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let expected = payload_len.checked_add(u64::from(HEADER_LEN)).filter(|bytes| *bytes <= MAX_SNAPSHOT_BYTES)
        .ok_or_else(|| std::io::Error::other("snapshot exceeds the 512 MiB client transport bound"))?;
    let mut file = std::fs::File::from(fd);
    if file.metadata()?.len() != expected { return Err(std::io::Error::other("snapshot descriptor length differs from manifest")); }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(usize::try_from(expected).map_err(std::io::Error::other)?)
        .map_err(std::io::Error::other)?;
    file.take(expected + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected { return Err(std::io::Error::other("snapshot descriptor changed length")); }
    Ok(bytes)
}

/// For a blocking task holding a runtime handle, never the reactor thread itself.
#[derive(Clone, Debug)]
pub struct SyncSnapshotSource { handle: tokio::runtime::Handle, connection: zbus::Connection }
impl SyncSnapshotSource {
    #[must_use]
    pub fn new(handle: tokio::runtime::Handle, connection: zbus::Connection) -> Self { Self { handle, connection } }
}
impl TreeSource for SyncSnapshotSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError> {
        self.handle.block_on(load_snapshot_tree(&self.connection, root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_scan_reports_traversal_limits_and_a_missing_root_without_invented_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub/deep")).unwrap();
        std::fs::write(dir.path().join("sub/deep/two"), b"there").unwrap();
        let full = DirectScanSource::default().load_tree(dir.path()).unwrap();
        assert!(full.complete && full.generation.is_none());
        let limited = DirectScanSource { limits: templar_treemap::ScanLimits { max_entries: 2, max_depth: 6 } }
            .load_tree(dir.path()).unwrap();
        assert_eq!(limited.entries.len(), 2);
        assert!(!limited.complete && limited.reason.unwrap().contains("entries limit=true"));
        let shallow = DirectScanSource { limits: templar_treemap::ScanLimits { max_entries: 100, max_depth: 1 } }
            .load_tree(dir.path()).unwrap();
        assert!(!shallow.complete && shallow.reason.unwrap().contains("depth limit=true"));
        let missing = DirectScanSource::default().load_tree(&dir.path().join("missing")).unwrap();
        assert!(!missing.complete && missing.entries.is_empty());
    }

    #[test]
    fn service_failures_do_not_silently_trigger_a_second_tree_walk() {
        assert!(TreeSourceError::NoService.allows_fallback());
        assert!(TreeSourceError::Incompatible("old interface".to_owned()).allows_fallback());
        assert!(!TreeSourceError::Call("permission denied".to_owned()).allows_fallback());
        assert!(!TreeSourceError::Snapshot("checksum mismatch".to_owned()).allows_fallback());
    }

    #[test]
    fn descriptor_reads_are_bounded_and_require_the_manifest_length() {
        let file = tempfile::tempfile().unwrap();
        assert!(read_fd(file.into(), MAX_SNAPSHOT_BYTES).is_err());
        let file = tempfile::tempfile().unwrap();
        assert!(read_fd(file.into(), 0).is_err());
    }
}
