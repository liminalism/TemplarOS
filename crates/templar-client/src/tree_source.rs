//! `TreeSource`: where a treemap gets its rows from
//! (`docs/planning/shared-incremental-file-index.md` §4.3).
//!
//! `templar-treemap` itself stays dependency-free (`ARCHITECTURE.md` — "no dependencies at all"),
//! so the seam that lets a treemap draw from a live index instead of walking the disk lives here
//! instead: a thin adapter between `org.templaros.Files.Index1.OpenSnapshot` and
//! `templar_treemap::build_tree`'s `RawEntry` rows.
//!
//! [`TreeSource`] itself is the synchronous contract the plan of record specifies, and
//! [`DirectScanSource`] is its always-available implementation — the same bounded
//! `templar_treemap::scan` the shell has always used, kept as an explicit fallback rather than
//! replaced. The index-backed path, [`load_snapshot_tree`], is a plain `async fn` rather than a
//! second `impl TreeSource`: fitting a D-Bus round trip into a synchronous trait method would
//! need either `async-trait` (a new dependency, and a heap allocation per call) or a hand-rolled
//! boxed future, for two implementations a caller picks between with one `if`. A caller on an
//! async runtime awaits it directly; [`SyncSnapshotSource`] is there for the one caller that is
//! not — a blocking task holding a `tokio::runtime::Handle`.

use crate::proxies::files::FilesIndexProxy;
use std::path::Path;
use templar_file_index::{
    model::{Coverage, NativePath},
    query::FieldMask,
    snapshot::{SnapshotDetail, SnapshotReader, SnapshotRequest},
};
use templar_treemap::RawEntry;

/// What a treemap needs to draw one tree: its rows, and whether the pass that produced them saw
/// the whole subtree. Drawing a partial tree as though it were complete would misreport how much
/// of the directory was actually measured.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeLoad {
    pub entries: Vec<RawEntry>,
    pub complete: bool,
}

/// Why a tree could not be loaded from a source. Never fatal to the caller: every implementation
/// this crate ships is meant to be tried in sequence, ending in [`DirectScanSource`], which
/// cannot fail this way.
#[derive(Debug)]
pub enum TreeSourceError {
    /// No `org.templaros.Files` answered the session bus.
    NoService,
    /// The service answered but the D-Bus call itself failed.
    Call(String),
    /// The snapshot bytes did not parse.
    Snapshot(String),
}

impl std::fmt::Display for TreeSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoService => write!(formatter, "no file index service is running"),
            Self::Call(detail) => write!(formatter, "index call failed: {detail}"),
            Self::Snapshot(detail) => write!(formatter, "snapshot did not parse: {detail}"),
        }
    }
}

impl std::error::Error for TreeSourceError {}

/// `docs/planning/shared-incremental-file-index.md` §4.3's tree-provider abstraction: load one
/// directory's tree, from wherever this implementation gets it.
pub trait TreeSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError>;
}

/// The direct fallback: `templar_treemap::scan`, bounded exactly as it always was. Always available,
/// and the only implementation that never asks a daemon anything — what a caller falls back to
/// when no service answers, or when it answered with something this build could not use.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectScanSource {
    pub limits: templar_treemap::ScanLimits,
}

impl TreeSource for DirectScanSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad, TreeSourceError> {
        let entries = templar_treemap::scan(root, self.limits);
        // `scan` does not report whether its own bound stopped it early; entries.len() reaching
        // the cap is the one case this can still tell honestly. A depth cutoff with entries to
        // spare is reported complete, which matches what the direct scanner has always implied.
        let complete = entries.len() < self.limits.max_entries;
        Ok(TreeLoad { entries, complete })
    }
}

/// Loads one directory's tree from a running `templar-filed`'s index, via
/// `Index1.OpenSnapshot` rebased to `root` (`SnapshotRequest::subtree`) in `Materialized` detail
/// — a file tray wants the files in a folder, not a directory-only summary; `Summary` detail is
/// for a drive-wide visualiser's first frame, which nothing in this repository draws yet.
///
/// `include_coverage` asks for every tier so an unexpanded `Opaque` build directory still shows
/// up as a single sized box rather than silently vanishing from the tile.
pub async fn load_snapshot_tree(
    connection: &zbus::Connection,
    root: &Path,
) -> Result<TreeLoad, TreeSourceError> {
    let proxy = FilesIndexProxy::new(connection)
        .await
        .map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let request = SnapshotRequest {
        fields: FieldMask::PATH.with(FieldMask::KIND).with(FieldMask::SIZE),
        detail: SnapshotDetail::Materialized,
        subtree: Some(NativePath::from_path(root)),
        include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
        ..SnapshotRequest::default()
    };
    let request_json = serde_json::to_string(&request)
        .map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let (fd, manifest_json) = proxy
        .open_snapshot(&request_json)
        .await
        .map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let complete = manifest_complete(&manifest_json, root);

    let std_fd: std::os::fd::OwnedFd = fd.into();
    let bytes = read_fd(std_fd).map_err(|error| TreeSourceError::Call(error.to_string()))?;
    let entries = raw_entries_from_snapshot(&bytes)?;
    Ok(TreeLoad { entries, complete })
}

/// The pure half of [`load_snapshot_tree`]: `TEMPLARIDX1` bytes to `RawEntry` rows, with no D-Bus or
/// filesystem I/O of its own. Split out so a test can prove the conversion produces the same tree
/// shape `templar_treemap::scan` would, from an `Index` built in-process — without a running
/// `templar-filed` to talk to.
fn raw_entries_from_snapshot(bytes: &[u8]) -> Result<Vec<RawEntry>, TreeSourceError> {
    let reader = SnapshotReader::new(bytes).map_err(TreeSourceError::Snapshot)?;
    Ok(reader
        .entries()
        .map_err(TreeSourceError::Snapshot)?
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path?.to_path();
            let is_dir = entry.kind == Some(templar_file_index::model::EntryKind::Directory);
            let size = if is_dir {
                entry
                    .aggregate
                    .map_or(0, |aggregate| aggregate.recursive_bytes)
            } else {
                entry.size_bytes.unwrap_or(0)
            };
            Some(RawEntry { path, size, is_dir })
        })
        .collect())
}

/// Reads the manifest's own root entries for `root`'s `complete` flag, defaulting to `false` on
/// anything unparsed: a caller must not draw a partial tree as a whole one because a field it
/// could not read happened to decode as the friendlier value.
fn manifest_complete(manifest_json: &str, root: &Path) -> bool {
    #[derive(serde::Deserialize)]
    struct Manifest {
        roots: Vec<ManifestRoot>,
    }
    #[derive(serde::Deserialize)]
    struct ManifestRoot {
        path_b64: String,
        complete: bool,
    }
    let Ok(manifest) = serde_json::from_str::<Manifest>(manifest_json) else {
        return false;
    };
    let wanted = NativePath::from_path(root);
    manifest.roots.iter().any(|entry| {
        templar_file_index::query::unbase64(&entry.path_b64)
            .is_some_and(|bytes| NativePath::from_bytes(bytes) == wanted)
            && entry.complete
    })
}

fn read_fd(fd: std::os::fd::OwnedFd) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::from(fd);
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// [`TreeSource`] wrapping [`load_snapshot_tree`] for a caller that holds a
/// `tokio::runtime::Handle` but is not itself running on that runtime's reactor — a blocking task
/// (`spawn_blocking`), where `block_on` is the documented way back into async code. Never call
/// this from the reactor thread itself: `block_on` on the thread that should be polling the
/// future it is waiting on deadlocks.
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
            .clone()
            .block_on(load_snapshot_tree(&self.connection, root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_file_index::{
        Index, IndexConfig, RootPolicy, ScanOptions, model::NativePath as IndexPath,
    };

    /// The adapter's whole reason to exist: it must produce the same tree `templar_treemap::scan`
    /// would from the same directory. Built in-process against an `Index` rather than a running
    /// `templar-filed`, so the comparison is hermetic and does not need a D-Bus session — only
    /// `raw_entries_from_snapshot`'s D-Bus-free half is exercised, which is exactly the
    /// conversion `load_snapshot_tree` also uses.
    #[test]
    fn the_snapshot_adapter_matches_a_direct_scan_for_a_fixture_where_both_are_valid() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        std::fs::write(home.join("report.pdf"), vec![0u8; 111]).unwrap();
        std::fs::write(home.join("notes/one.md"), vec![0u8; 22]).unwrap();
        std::fs::write(home.join("notes/two.md"), vec![0u8; 33]).unwrap();

        let index = Index::open(&dir.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
        let root = index
            .add_root(&IndexPath::from_path(&home), RootPolicy::default_home())
            .unwrap()
            .root;
        let outcome = index.scan_root(root, ScanOptions::default()).unwrap();
        assert!(outcome.complete);

        let request = SnapshotRequest {
            fields: FieldMask::PATH.with(FieldMask::KIND).with(FieldMask::SIZE),
            detail: SnapshotDetail::Materialized,
            subtree: Some(IndexPath::from_path(&home)),
            include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
            ..SnapshotRequest::default()
        };
        let mut frame = Vec::new();
        index.write_snapshot(&mut frame, &request).unwrap();
        let mut from_snapshot = raw_entries_from_snapshot(&frame).unwrap();

        let mut from_scan = templar_treemap::scan(&home, templar_treemap::ScanLimits::default());

        let sort_key = |entry: &RawEntry| entry.path.clone();
        from_snapshot.sort_by_key(sort_key);
        from_scan.sort_by_key(sort_key);
        assert_eq!(
            from_snapshot, from_scan,
            "the index-backed tree and the direct scan disagree on the same fixture"
        );
    }

    #[test]
    fn a_direct_scan_of_a_small_fixture_is_reported_complete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one.txt"), b"hi").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/two.txt"), b"there").unwrap();

        let source = DirectScanSource::default();
        let load = source.load_tree(dir.path()).unwrap();
        assert!(load.complete, "well under the entry cap");
        assert!(load.entries.iter().any(|entry| entry.path == dir.path()));
        assert!(
            load.entries
                .iter()
                .any(|entry| entry.path.ends_with("one.txt") && !entry.is_dir)
        );
    }

    #[test]
    fn a_direct_scan_at_the_entry_cap_is_reported_incomplete() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..5 {
            std::fs::write(dir.path().join(format!("f{n}.txt")), b"x").unwrap();
        }
        let source = DirectScanSource {
            limits: templar_treemap::ScanLimits {
                max_entries: 3,
                max_depth: 6,
            },
        };
        let load = source.load_tree(dir.path()).unwrap();
        assert_eq!(load.entries.len(), 3);
        assert!(!load.complete);
    }
}
