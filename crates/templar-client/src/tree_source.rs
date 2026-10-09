//! Store-free treemap providers. A visible tray consumes Index1 snapshots without scanning;
//! direct bounded scanning is only for an absent/incompatible service, never a stale index.
use crate::proxies::files::FilesIndexProxy;
use std::{os::unix::ffi::OsStrExt, path::Path};
use templar_file_index::{
    Coverage, EntryKind, NativePath,
    query::FieldMask,
    snapshot::{
        HEADER_LEN, SnapshotDetail, SnapshotManifest, SnapshotReader, SnapshotRequest, SnapshotRoot,
    },
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
    pub fn allows_fallback(&self) -> bool {
        matches!(self, Self::NoService | Self::Incompatible(_))
    }
}

impl std::fmt::Display for TreeSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoService => write!(formatter, "no file index service is running"),
            Self::Incompatible(detail) => {
                write!(formatter, "index snapshot is incompatible: {detail}")
            }
            Self::Call(detail) => write!(formatter, "index call failed: {detail}"),
            Self::Snapshot(detail) => write!(formatter, "snapshot did not parse: {detail}"),
        }
    }
}
impl std::error::Error for TreeSourceError {}

fn call_error(error: &zbus::Error) -> TreeSourceError {
    if let zbus::Error::MethodError(name, _, _) = error {
        match name.as_str() {
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner" => return TreeSourceError::NoService,
            "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownInterface" => {
                return TreeSourceError::Incompatible(error.to_string());
            }
            _ => {}
        }
    }
    TreeSourceError::Call(error.to_string())
}

pub trait TreeSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DirectScanSource {
    pub limits: templar_treemap::ScanLimits,
}

impl TreeSource for DirectScanSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError> {
        let outcome = templar_treemap::scan(root, self.limits);
        let reason = if outcome.complete {
            None
        } else {
            Some(format!(
                "direct scan incomplete: entries limit={}, depth limit={}, missing={}, unreadable={}{}",
                outcome.entry_limit,
                outcome.depth_limit,
                outcome.missing_entries,
                outcome.unreadable_entries,
                outcome
                    .last_error
                    .as_ref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default(),
            ))
        };
        Ok(TreeLoad {
            entries: outcome.entries,
            complete: outcome.complete,
            reason,
            ..TreeLoad::default()
        })
    }
}

const TREE_FIELDS: FieldMask = FieldMask(
    FieldMask::PATH.0
        | FieldMask::KIND.0
        | FieldMask::SIZE.0
        | FieldMask::COVERAGE.0
        | FieldMask::AGGREGATE.0,
);
/// A transport/memory bound, not a claim that a partial frame represents a whole tree.
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;

pub async fn load_snapshot_tree(
    connection: &zbus::Connection,
    root: &Path,
) -> Result<TreeLoad, TreeSourceError> {
    load_snapshot_tree_view(connection, root, SnapshotDetail::Materialized, None).await
}

/// A bounded directory view. Summary is used first; expanding a directory asks for materialized
/// rows for that native subtree. Missing aggregates remain explicit partial size truth.
pub async fn load_snapshot_tree_view(
    connection: &zbus::Connection,
    root: &Path,
    detail: SnapshotDetail,
    max_depth: Option<u32>,
) -> Result<TreeLoad, TreeSourceError> {
    let proxy = FilesIndexProxy::new(connection)
        .await
        .map_err(|error| call_error(&error))?;
    let request = SnapshotRequest {
        fields: TREE_FIELDS,
        detail,
        max_depth,
        subtree: Some(NativePath::from_path(root)),
        include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
        ..SnapshotRequest::default()
    };
    let request_json = serde_json::to_string(&request)
        .map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let (fd, manifest_json) = proxy
        .open_snapshot(&request_json)
        .await
        .map_err(|error| call_error(&error))?;
    let manifest: SnapshotManifest = serde_json::from_str(&manifest_json)
        .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let map = SnapshotMap::new(fd.into(), &manifest)?;
        let mut load = decode_snapshot_tree_view(map.as_bytes(), &manifest, &root, detail)?;
        if max_depth.is_some() && detail == SnapshotDetail::Materialized {
            load.complete = false;
            load.reason
                .get_or_insert_with(|| "showing a bounded part of the directory".to_owned());
        }
        Ok(load)
    })
    .await
    .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?
}

/// The exact production decoder, usable by a filed-owned integration fixture without linking
/// a SQLite writer into client or shell. Header/manifest identity and native scope are checked.
pub fn decode_snapshot_tree(
    bytes: &[u8],
    manifest: &SnapshotManifest,
    root: &Path,
) -> Result<TreeLoad, TreeSourceError> {
    decode_snapshot_tree_view(bytes, manifest, root, SnapshotDetail::Materialized)
}

pub fn decode_snapshot_tree_view(
    bytes: &[u8],
    manifest: &SnapshotManifest,
    root: &Path,
    detail: SnapshotDetail,
) -> Result<TreeLoad, TreeSourceError> {
    let ValidatedSnapshot { reader, roots } = validate_snapshot_frame(bytes, manifest)?;
    let wanted = NativePath::from_path(root);
    let relevant: Vec<_> = manifest
        .roots
        .iter()
        .zip(&roots)
        .filter(|(_, (_, path))| wanted.starts_with(path) || path.starts_with(&wanted))
        .collect();
    let mut complete = !relevant.is_empty()
        && relevant.iter().all(|(status, _)| {
            status.state == "current" && status.complete && status.metadata == "verified"
        });
    let mut reason = if relevant.is_empty() {
        Some("requested subtree has no indexed coverage".to_owned())
    } else if complete {
        None
    } else {
        Some(
            relevant
                .iter()
                .map(|(status, _)| {
                    format!(
                        "{}: {}, metadata {}{}",
                        status.display,
                        status.state,
                        status.metadata,
                        status
                            .reason
                            .as_ref()
                            .map(|reason| format!(" ({reason})"))
                            .unwrap_or_default(),
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    };
    // Overlapping registered roots may carry the same path. Its most specific root owns it;
    // keys become RawEntry paths without another owned-path copy.
    let nested = roots.iter().enumerate().any(|(i, (_, path))| {
        roots
            .iter()
            .enumerate()
            .any(|(j, (_, other))| i != j && path.starts_with(other))
    });
    let mut duplicate_paths = std::collections::HashMap::<&[u8], (usize, u64, bool)>::new();
    let iterator = reader.entries_iter().map_err(TreeSourceError::Snapshot)?;
    let mut entries = Vec::new();
    if !nested {
        reserve_snapshot_entries(&mut entries, reader.entry_count)?;
    }
    for entry in iterator {
        let entry = entry.map_err(TreeSourceError::Snapshot)?;
        let DecodedTreeEntry {
            path,
            specificity,
            size,
            is_dir,
        } = decode_tree_entry(&entry, &roots, &wanted, detail, &mut complete, &mut reason)?;
        if !nested {
            entries.push(RawEntry {
                path: std::ffi::OsStr::from_bytes(path).into(),
                size,
                is_dir,
            });
            continue;
        }
        match duplicate_paths.entry(path) {
            std::collections::hash_map::Entry::Occupied(mut old) if old.get().0 < specificity => {
                old.insert((specificity, size, is_dir));
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert((specificity, size, is_dir));
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    entries.extend(
        duplicate_paths
            .into_iter()
            .map(|(path, (_, size, is_dir))| RawEntry {
                path: std::ffi::OsStr::from_bytes(path).into(),
                size,
                is_dir,
            }),
    );
    if detail == SnapshotDetail::Summary {
        subtract_represented_children(&mut entries, &mut complete, &mut reason);
    }
    Ok(TreeLoad {
        entries,
        complete,
        generation: Some(manifest.generation.0),
        snapshot_seq: Some(manifest.snapshot_seq),
        roots: relevant
            .into_iter()
            .map(|(status, _)| status.clone())
            .collect(),
        reason,
    })
}

fn reserve_snapshot_entries(
    entries: &mut Vec<RawEntry>,
    count: u64,
) -> Result<(), TreeSourceError> {
    let count =
        usize::try_from(count).map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
    entries
        .try_reserve_exact(count)
        .map_err(|error| TreeSourceError::Snapshot(error.to_string()))
}

struct ValidatedSnapshot<'a> {
    reader: SnapshotReader<'a>,
    roots: Vec<(templar_file_index::RootId, NativePath)>,
}

fn validate_snapshot_frame<'a>(
    bytes: &'a [u8],
    manifest: &SnapshotManifest,
) -> Result<ValidatedSnapshot<'a>, TreeSourceError> {
    let reader = SnapshotReader::new(bytes).map_err(TreeSourceError::Snapshot)?;
    if !reader.field_mask.contains(TREE_FIELDS) {
        return Err(TreeSourceError::Incompatible(
            "PATH/KIND/SIZE/COVERAGE/AGGREGATE fields are required".to_owned(),
        ));
    }
    if reader.format != manifest.format
        || reader.generation != manifest.generation
        || reader.field_mask.0 != manifest.field_mask
        || reader.entry_count != manifest.entry_count
        || bytes.len().checked_sub(HEADER_LEN as usize)
            != usize::try_from(manifest.payload_len).ok()
        || u32::from_le_bytes(bytes[56..60].try_into().unwrap()) != manifest.payload_crc32c
    {
        return Err(TreeSourceError::Snapshot(
            "manifest does not identify this frame".to_owned(),
        ));
    }
    let roots = reader.roots().map_err(TreeSourceError::Snapshot)?;
    if roots.len() != manifest.roots.len() {
        return Err(TreeSourceError::Snapshot(
            "manifest root count differs".to_owned(),
        ));
    }
    for ((id, path), status) in roots.iter().zip(&manifest.roots) {
        if *id != status.root
            || templar_file_index::query::base64(path.as_bytes()) != status.path_b64
            || status.generation > manifest.generation
        {
            return Err(TreeSourceError::Snapshot(
                "manifest root identity differs".to_owned(),
            ));
        }
    }
    Ok(ValidatedSnapshot { reader, roots })
}

struct DecodedTreeEntry<'a> {
    path: &'a [u8],
    specificity: usize,
    size: u64,
    is_dir: bool,
}

fn decode_tree_entry<'a>(
    entry: &templar_file_index::snapshot::SnapshotEntryRef<'a>,
    roots: &[(templar_file_index::RootId, NativePath)],
    wanted: &NativePath,
    detail: SnapshotDetail,
    complete: &mut bool,
    reason: &mut Option<String>,
) -> Result<DecodedTreeEntry<'a>, TreeSourceError> {
    let path = entry
        .path
        .ok_or_else(|| TreeSourceError::Snapshot("missing native path".to_owned()))?;
    let Some((_, root_path)) = roots.iter().find(|(id, _)| *id == entry.root) else {
        return Err(TreeSourceError::Snapshot(
            "entry names an unknown root".to_owned(),
        ));
    };
    let within = |base: &[u8]| {
        path == base
            || (path.starts_with(base) && (base == b"/" || path.get(base.len()) == Some(&b'/')))
    };
    if !within(wanted.as_bytes()) || !within(root_path.as_bytes()) || path.contains(&0) {
        return Err(TreeSourceError::Snapshot(
            "entry escapes the requested native scope".to_owned(),
        ));
    }
    let kind = entry
        .kind
        .ok_or_else(|| TreeSourceError::Snapshot("missing entry kind".to_owned()))?;
    let coverage = entry
        .coverage
        .ok_or_else(|| TreeSourceError::Snapshot("missing entry coverage".to_owned()))?;
    let is_dir = kind == EntryKind::Directory;
    let size = if is_dir {
        if coverage == Coverage::Opaque || detail == SnapshotDetail::Summary {
            if let Some(aggregate) = entry.aggregate {
                if aggregate.stale || !aggregate.readable {
                    *complete = false;
                    *reason = Some("cached directory sizes are stale or unreadable".to_owned());
                }
                aggregate.recursive_bytes
            } else {
                *complete = false;
                *reason = Some("directory sizes have not been measured".to_owned());
                0
            }
        } else {
            0
        }
    } else {
        entry
            .size_bytes
            .ok_or_else(|| TreeSourceError::Snapshot("missing file size".to_owned()))?
    };
    let specificity = root_path.as_bytes().len();
    Ok(DecodedTreeEntry {
        path,
        specificity,
        size,
        is_dir,
    })
}

fn subtract_represented_children(
    entries: &mut [RawEntry],
    complete: &mut bool,
    reason: &mut Option<String>,
) {
    // RawEntry directory sizes are bytes represented by that node alone. Cached aggregates
    // are recursive, so subtract represented child totals before bottom-up tree aggregation.
    let paths: std::collections::HashMap<_, _> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.path.as_path(), index))
        .collect();
    let mut children = vec![0u64; entries.len()];
    for entry in entries.iter() {
        if let Some(parent) = entry.path.parent().and_then(|path| paths.get(path)) {
            children[*parent] = children[*parent].saturating_add(entry.size);
        }
    }
    drop(paths);
    for (entry, children) in entries.iter_mut().zip(children) {
        if let Some(remainder) = entry.size.checked_sub(children) {
            entry.size = remainder;
        } else {
            entry.size = 0;
            *complete = false;
            *reason = Some("directory totals do not yet cover the visible children".to_owned());
        }
    }
}

/// A sealed, immutable frame mapped without a second payload allocation. The descriptor may
/// close after mmap: the mapping retains its backing, and all four seals prohibit replacement,
/// resize and writes for its lifetime.
#[derive(Debug)]
pub struct SnapshotMap {
    address: std::ptr::NonNull<std::ffi::c_void>,
    length: usize,
}
#[allow(unsafe_code)] // Audited mmap boundary: four seals and exact fstat length precede all access.
impl SnapshotMap {
    pub fn new(
        fd: std::os::fd::OwnedFd,
        manifest: &SnapshotManifest,
    ) -> Result<Self, TreeSourceError> {
        let file = std::fs::File::from(fd);
        let fail = |message: &str| TreeSourceError::Snapshot(message.to_owned());
        let expected = manifest
            .payload_len
            .checked_add(u64::from(HEADER_LEN))
            .filter(|length| *length <= MAX_SNAPSHOT_BYTES)
            .ok_or_else(|| fail("snapshot exceeds the 512 MiB client transport bound"))?;
        if !manifest.sealed {
            return Err(fail("snapshot manifest is not sealed"));
        }
        let seals = rustix::fs::fcntl_get_seals(&file).map_err(|error| fail(&error.to_string()))?;
        let required = rustix::fs::SealFlags::WRITE
            | rustix::fs::SealFlags::GROW
            | rustix::fs::SealFlags::SHRINK
            | rustix::fs::SealFlags::SEAL;
        if !seals.contains(required) {
            return Err(fail("snapshot descriptor lacks immutable seals"));
        }
        let stat = rustix::fs::fstat(&file).map_err(|error| fail(&error.to_string()))?;
        if u64::try_from(stat.st_size).ok() != Some(expected) {
            return Err(fail("snapshot descriptor length differs from manifest"));
        }
        let length = usize::try_from(expected).map_err(|error| fail(&error.to_string()))?;
        // SAFETY: the size was checked against this sealed descriptor; read-only shared pages
        // cannot be mutated or truncated. This type owns and unmaps the returned region.
        let address = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                &file,
                0,
            )
        }
        .map_err(|error| fail(&error.to_string()))?;
        let address =
            std::ptr::NonNull::new(address).ok_or_else(|| fail("null snapshot mapping"))?;
        Ok(Self { address, length })
    }
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: the sealed, readable mapping spans length bytes and outlives the borrowed slice.
        unsafe { std::slice::from_raw_parts(self.address.as_ptr().cast(), self.length) }
    }
}
#[allow(unsafe_code)] // Matches the single mapping owned above.
impl Drop for SnapshotMap {
    fn drop(&mut self) {
        // SAFETY: this is the region returned by mmap and this owner unmaps it exactly once.
        let _ = unsafe { rustix::mm::munmap(self.address.as_ptr(), self.length) };
    }
}

/// For a blocking task holding a runtime handle, never the reactor thread itself.
#[derive(Clone, Debug)]
pub struct SyncSnapshotSource {
    handle: tokio::runtime::Handle,
    connection: zbus::Connection,
}
impl SyncSnapshotSource {
    #[must_use]
    pub fn new(handle: tokio::runtime::Handle, connection: zbus::Connection) -> Self {
        Self { handle, connection }
    }
}
impl TreeSource for SyncSnapshotSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError> {
        self.handle
            .block_on(load_snapshot_tree(&self.connection, root))
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
        let limited = DirectScanSource {
            limits: templar_treemap::ScanLimits {
                max_entries: 2,
                max_depth: 6,
            },
        }
        .load_tree(dir.path())
        .unwrap();
        assert_eq!(limited.entries.len(), 2);
        assert!(!limited.complete && limited.reason.unwrap().contains("entries limit=true"));
        let shallow = DirectScanSource {
            limits: templar_treemap::ScanLimits {
                max_entries: 100,
                max_depth: 1,
            },
        }
        .load_tree(dir.path())
        .unwrap();
        assert!(!shallow.complete && shallow.reason.unwrap().contains("depth limit=true"));
        let missing = DirectScanSource::default()
            .load_tree(&dir.path().join("missing"))
            .unwrap();
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
    fn the_client_refuses_an_unsealed_descriptor() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(HEADER_LEN.into()).unwrap();
        let manifest = SnapshotManifest {
            format: 1,
            generation: templar_file_index::Generation(0),
            field_mask: 0,
            entry_count: 0,
            payload_len: 0,
            payload_crc32c: 0,
            roots: Vec::new(),
            sealed: true,
            snapshot_seq: 0,
        };
        assert!(SnapshotMap::new(file.into(), &manifest).is_err());
    }

    #[test]
    fn a_sealed_mapping_survives_descriptor_close_and_checks_the_manifest_length() {
        use std::io::Write as _;
        let descriptor = rustix::fs::memfd_create(
            "owned-map-test",
            rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
        )
        .unwrap();
        let mut file = std::fs::File::from(descriptor);
        file.write_all(&[42; HEADER_LEN as usize]).unwrap();
        rustix::fs::fcntl_add_seals(
            &file,
            rustix::fs::SealFlags::WRITE
                | rustix::fs::SealFlags::GROW
                | rustix::fs::SealFlags::SHRINK
                | rustix::fs::SealFlags::SEAL,
        )
        .unwrap();
        let mut manifest = SnapshotManifest {
            format: 1,
            generation: templar_file_index::Generation(0),
            field_mask: 0,
            entry_count: 0,
            payload_len: 1,
            payload_crc32c: 0,
            roots: Vec::new(),
            sealed: true,
            snapshot_seq: 0,
        };
        assert!(SnapshotMap::new(file.try_clone().unwrap().into(), &manifest).is_err());
        manifest.payload_len = 0;
        let map = SnapshotMap::new(file.into(), &manifest).unwrap();
        assert_eq!(map.as_bytes(), &[42; HEADER_LEN as usize]);
    }
}
