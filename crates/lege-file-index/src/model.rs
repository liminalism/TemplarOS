//! The vocabulary: lossless paths, filesystem identity, root freshness and index policy.
//!
//! Everything here is data. The rule the whole crate turns on lives in [`NativePath`]: a path is
//! bytes, and the moment a path becomes a `String` it has stopped being able to name the file it
//! came from. The display form exists so a UI can draw something; it never goes back to the
//! filesystem.

use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

/// A path as the kernel has it: bytes, not text.
///
/// Unix paths are arbitrary non-NUL byte sequences. `to_string_lossy` replaces what it cannot
/// decode with U+FFFD, and a path that has been through that no longer opens the file it names —
/// which is how a file manager deletes the wrong thing. So the index stores bytes, and
/// [`NativePath::display`] is presentation only.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NativePath(Vec<u8>);

impl NativePath {
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        Self(path.as_os_str().as_bytes().to_vec())
    }

    #[must_use]
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// Back to a `PathBuf`, exactly — this is what a file action must use.
    #[must_use]
    pub fn to_path(&self) -> PathBuf {
        PathBuf::from(std::ffi::OsString::from_vec(self.0.clone()))
    }

    /// For humans and for the search accelerator. Never fed back to the filesystem.
    #[must_use]
    pub fn display(&self) -> String {
        OsStr::from_bytes(&self.0).to_string_lossy().into_owned()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The last component, or `None` for `/` and the empty path.
    #[must_use]
    pub fn file_name(&self) -> Option<Self> {
        let trimmed = trim_trailing_separators(&self.0);
        if trimmed.is_empty() {
            return None;
        }
        let start = trimmed
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(0, |index| index + 1);
        Some(Self(trimmed[start..].to_vec()))
    }

    /// The containing directory, or `None` at the top.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let trimmed = trim_trailing_separators(&self.0);
        let index = trimmed.iter().rposition(|byte| *byte == b'/')?;
        if index == 0 {
            return Some(Self(vec![b'/']));
        }
        Some(Self(trimmed[..index].to_vec()))
    }

    /// `child` appended, with exactly one separator.
    #[must_use]
    pub fn join(&self, child: &[u8]) -> Self {
        let mut bytes = trim_trailing_separators(&self.0).to_vec();
        if !bytes.is_empty() || self.0.first() == Some(&b'/') {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(child);
        Self(bytes)
    }

    /// Prefix test on **component** boundaries: `/home/ab` does not start with `/home/a`.
    #[must_use]
    pub fn starts_with(&self, prefix: &Self) -> bool {
        let prefix_bytes = trim_trailing_separators(&prefix.0);
        if prefix_bytes.is_empty() {
            return true;
        }
        if !self.0.starts_with(prefix_bytes) {
            return false;
        }
        match self.0.get(prefix_bytes.len()) {
            None => true,
            Some(byte) => *byte == b'/' || prefix_bytes == b"/",
        }
    }

    /// The `LIKE`/`GLOB`-free way to ask SQLite for "everything under this directory": the
    /// half-open byte range `[prefix/, prefix0)`.
    #[must_use]
    pub(crate) fn subtree_bounds(&self) -> (Vec<u8>, Vec<u8>) {
        let mut low = trim_trailing_separators(&self.0).to_vec();
        low.push(b'/');
        let mut high = low.clone();
        let last = high.len() - 1;
        high[last] = b'0'; // '/' + 1
        (low, high)
    }
}

fn trim_trailing_separators(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 1 && bytes[end - 1] == b'/' {
        end -= 1;
    }
    &bytes[..end]
}

/// What an entry is. Symlinks are their own kind and are never followed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

impl EntryKind {
    #[must_use]
    pub fn as_i64(self) -> i64 {
        match self {
            Self::Regular => 0,
            Self::Directory => 1,
            Self::Symlink => 2,
            Self::Other => 3,
        }
    }

    #[must_use]
    pub fn from_i64(value: i64) -> Self {
        match value {
            1 => Self::Directory,
            2 => Self::Symlink,
            3 => Self::Other,
            _ => Self::Regular,
        }
    }

    #[must_use]
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            Self::Symlink
        } else if file_type.is_dir() {
            Self::Directory
        } else if file_type.is_file() {
            Self::Regular
        } else {
            Self::Other
        }
    }
}

/// Best-effort filesystem object identity: device plus inode.
///
/// Advisory, deliberately. Inode numbers are reused, bind mounts show the same object twice and
/// network filesystems invent them. It is good enough to notice a rename and to recognise a hard
/// link, and it is not a durable document id.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ObjectKey {
    pub dev: u64,
    pub ino: u64,
}

/// A root's id in **this** database file. A rebuild invalidates it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RootId(pub i64);

/// An entry's id in **this** database file, at **this** generation lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub i64);

/// A database-wide monotone counter. Every answer the index gives names the one it was produced
/// at, so no consumer can render a result without being able to say how old it is.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Generation(pub u64);

impl std::fmt::Display for Generation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// How much of the truth a root's committed generation actually holds.
///
/// The point of the enum is that only one variant means "believed complete and up to date", and
/// nothing reaches it except a complete scan. Everything else carries a reason, so a UI can say
/// "results may be out of date" instead of quietly lying.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootState {
    /// Registered, never scanned.
    Empty,
    /// A scan is in flight. A previous generation may still be served.
    Building,
    /// Complete, and no known outstanding change. The only honest "up to date".
    Current,
    /// The committed snapshot is good; a queued event backlog is being applied.
    CatchingUp,
    /// Something is known to be missed: overflow, ambiguity, a budget, downtime, a policy change.
    Stale,
    /// The volume is gone. The last committed generation stays searchable, marked offline.
    Offline,
    /// Complete except where the user cannot read. Never promoted to `Current`.
    PermissionLimited,
}

impl RootState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Building => "building",
            Self::Current => "current",
            Self::CatchingUp => "catching-up",
            Self::Stale => "stale",
            Self::Offline => "offline",
            Self::PermissionLimited => "permission-limited",
        }
    }

    #[must_use]
    pub fn from_str_or_stale(value: &str) -> Self {
        match value {
            "empty" => Self::Empty,
            "building" => Self::Building,
            "current" => Self::Current,
            "catching-up" => Self::CatchingUp,
            "offline" => Self::Offline,
            "permission-limited" => Self::PermissionLimited,
            _ => Self::Stale,
        }
    }

    /// The transition table. An unlisted pair is [`crate::IndexError::IllegalTransition`], because
    /// a state machine that silently accepts anything is a state machine that eventually claims
    /// `current` for a root nobody scanned.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }
        match self {
            Self::Empty => matches!(next, Self::Building | Self::Offline),
            Self::Building => matches!(
                next,
                Self::Current | Self::PermissionLimited | Self::Stale | Self::Empty | Self::Offline
            ),
            Self::Current | Self::PermissionLimited => matches!(
                next,
                Self::CatchingUp | Self::Stale | Self::Offline | Self::Building
            ),
            Self::CatchingUp => matches!(
                next,
                Self::Current | Self::PermissionLimited | Self::Stale | Self::Offline
            ),
            Self::Stale => matches!(
                next,
                Self::Building | Self::CatchingUp | Self::Offline | Self::Current
            ),
            // Coming back is never "current": a volume that reappeared has to be re-enumerated.
            Self::Offline => matches!(next, Self::Building | Self::Stale),
        }
    }
}

/// Why a root stopped being trustworthy. Carried into `RootStatus::reason` as a stable string.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum StaleReason {
    /// The kernel queue overflowed; events between then and now are unknown.
    WatcherOverflow,
    /// The watch budget ran out: some directories are unwatched.
    WatcherExhausted {
        armed: u64,
        refused: u64,
    },
    WatcherFailed {
        detail: String,
    },
    /// A rename could not be paired with its partner.
    RenameAmbiguous,
    MountDisappeared,
    PolicyChanged,
    BudgetExhausted {
        budget: BudgetKind,
    },
    PermissionDenied,
    /// The daemon was not running, so the gap cannot be reconstructed from events.
    Downtime,
    Inconsistent {
        detail: String,
    },
    /// A background repair scan, in progress. Paired with `RootState::Building`.
    Reconcile,
}

impl StaleReason {
    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Self::WatcherOverflow => "watcher-overflow".to_owned(),
            Self::WatcherExhausted { armed, refused } => {
                format!("watcher-exhausted:armed={armed},refused={refused}")
            }
            Self::WatcherFailed { detail } => format!("watcher-failed:{detail}"),
            Self::RenameAmbiguous => "rename-ambiguous".to_owned(),
            Self::MountDisappeared => "mount-disappeared".to_owned(),
            Self::PolicyChanged => "policy-changed".to_owned(),
            Self::BudgetExhausted { budget } => format!("budget:{}", budget.as_str()),
            Self::PermissionDenied => "permission-denied".to_owned(),
            Self::Downtime => "downtime".to_owned(),
            Self::Inconsistent { detail } => format!("inconsistent:{detail}"),
            Self::Reconcile => "reconcile".to_owned(),
        }
    }
}

/// Which budget stopped a scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetKind {
    Entries,
    Bytes,
    Duration,
    Depth,
}

impl BudgetKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Bytes => "bytes",
            Self::Duration => "duration",
            Self::Depth => "depth",
        }
    }
}

/// Enough identity to notice that a removable root went away and came back as the same volume.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VolumeIdentity {
    pub dev_id: Option<u64>,
    pub uuid: Option<String>,
    pub mount_point: Option<NativePath>,
    pub label: Option<String>,
}

/// The watcher's honest self-report, owned here so `lege files index status`, `Index1.Roots` and
/// a later `lege doctor` check all read one model instead of three.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchStatus {
    pub backend: String,
    pub watches_armed: u64,
    pub watches_refused: u64,
    pub watch_limit: Option<u64>,
    pub queue_limit: Option<u64>,
    pub last_event_ms: Option<i64>,
    pub last_overflow_ms: Option<i64>,
    pub overflow_count: u64,
    pub pending_events: u64,
    pub last_error: Option<String>,
}

/// Everything a consumer needs to decide whether to trust a root's rows.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RootStatus {
    pub root: RootId,
    pub path: NativePath,
    pub display: String,
    pub volume: VolumeIdentity,
    pub state: RootState,
    pub reason: Option<String>,
    pub generation: Generation,
    /// Whether the committed generation covered the whole root.
    pub complete: bool,
    pub entry_count: u64,
    pub policy_hash: String,
    pub last_scan_ms: Option<i64>,
    pub last_reconcile_started_ms: Option<i64>,
    pub last_reconcile_finished_ms: Option<i64>,
    pub last_error: Option<String>,
    pub watch: Option<WatchStatus>,
}

/// What a root includes. One predicate, so the scanner and the watcher cannot disagree about
/// which files exist.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RootPolicy {
    /// Follow into other filesystems. Off for `/` and for a home root; on only where a user
    /// explicitly indexed a mounted volume.
    pub cross_mount: bool,
    /// Directory names never descended into.
    pub skip_names: Vec<String>,
    /// Skip `Trash` and `.Trash-*`, whose uid suffix cannot be written as a literal skip name.
    pub skip_trash: bool,
    pub include_hidden: bool,
}

impl Default for RootPolicy {
    fn default() -> Self {
        Self::default_home()
    }
}

impl RootPolicy {
    /// The policy the file service has always had: caches, version-control internals, build
    /// output and the trash are noise in every search a person actually wants.
    #[must_use]
    pub fn default_home() -> Self {
        Self {
            cross_mount: false,
            skip_names: [
                ".git",
                ".hg",
                ".svn",
                ".cache",
                "node_modules",
                "target",
                "__pycache__",
                ".venv",
            ]
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
            skip_trash: true,
            include_hidden: true,
        }
    }

    /// A stable digest of the policy. A change to it invalidates a root: the rows were produced
    /// under different rules and cannot be compared with rows produced under these.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut names = self.skip_names.clone();
        names.sort();
        let canonical = format!(
            "cross_mount={};skip_trash={};include_hidden={};skip={}",
            self.cross_mount,
            self.skip_trash,
            self.include_hidden,
            names.join(",")
        );
        format!("{:016x}", fnv1a64(canonical.as_bytes()))
    }

    /// Whether this path is excluded. The scanner and the watcher both call exactly this.
    #[must_use]
    pub fn is_skipped(&self, path: &NativePath, _kind: EntryKind) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        let bytes = name.into_bytes();
        if self.skip_trash && is_trash_name(&bytes) {
            return true;
        }
        self.skip_names
            .iter()
            .any(|skip| skip.as_bytes() == bytes.as_slice())
    }

    #[must_use]
    pub fn should_index(&self, path: &NativePath, kind: EntryKind) -> bool {
        !self.is_skipped(path, kind)
    }
}

/// The two shapes the trash specification allows: a home `Trash` with no leading dot, and a
/// volume `.Trash-$uid` whose suffix differs on every machine.
fn is_trash_name(name: &[u8]) -> bool {
    name == b"Trash" || name.starts_with(b".Trash")
}

/// A small, stable, dependency-free digest. Not cryptographic and does not need to be: it names a
/// policy, it does not authenticate one.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The metadata one directory entry contributes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryMeta {
    pub kind: EntryKind,
    pub size_bytes: u64,
    pub alloc_bytes: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub mode: u32,
    pub hidden: bool,
    pub object_key: Option<ObjectKey>,
    pub nlink: u32,
    pub mime: Option<String>,
}

impl Default for EntryKind {
    fn default() -> Self {
        Self::Regular
    }
}

impl EntryMeta {
    /// Built from `symlink_metadata`, never `metadata`: the index describes the link, not its
    /// target, and following links turns a walk into a graph traversal with cycles.
    #[must_use]
    pub fn from_symlink_metadata(path: &Path, metadata: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let kind = EntryKind::of(metadata);
        let hidden = Path::new(path)
            .file_name()
            .is_some_and(|name| name.as_bytes().first() == Some(&b'.'));
        Self {
            kind,
            size_bytes: if kind == EntryKind::Directory {
                0
            } else {
                metadata.len()
            },
            alloc_bytes: metadata.blocks() * 512,
            mtime_ns: metadata.mtime() * 1_000_000_000 + i64::from(metadata.mtime_nsec() as i32),
            ctime_ns: metadata.ctime() * 1_000_000_000 + i64::from(metadata.ctime_nsec() as i32),
            mode: metadata.mode(),
            hidden,
            object_key: Some(ObjectKey {
                dev: metadata.dev(),
                ino: metadata.ino(),
            }),
            nlink: metadata.nlink() as u32,
            mime: None,
        }
    }

    /// Whether a stored row already says exactly this. What lets the maintenance lane apply a
    /// file operation immediately and then drop the watcher's echo of it.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.size_bytes == other.size_bytes
            && self.mtime_ns == other.mtime_ns
            && self.ctime_ns == other.ctime_ns
            && self.object_key == other.object_key
    }
}

/// One stored row, as the maintenance lane reads it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRow {
    pub entry_id: EntryId,
    pub root: RootId,
    pub parent: Option<EntryId>,
    pub path: NativePath,
    pub name: NativePath,
    pub display: String,
    pub meta: EntryMeta,
    pub written_generation: Generation,
    pub last_seen_generation: Generation,
    pub deleted_generation: Option<Generation>,
}

/// Whether a repair covers a whole root or one subtree. Garbage collection is scoped to it, so a
/// subtree repair cannot delete the rest of the root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScanScope {
    Root,
    Subtree(NativePath),
}

impl Default for ScanScope {
    fn default() -> Self {
        Self::Root
    }
}

/// Milliseconds since the Unix epoch, the unit every `*_ms` column uses.
#[must_use]
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_native_path_survives_bytes_no_string_can_hold() {
        let raw = b"/tmp/caf\xff\xfe/report.pdf";
        let path = NativePath::from_bytes(raw.to_vec());
        assert_eq!(path.as_bytes(), raw);
        assert_eq!(NativePath::from_path(&path.to_path()), path);
        assert_eq!(path.file_name().unwrap().as_bytes(), b"report.pdf");
        assert_eq!(path.parent().unwrap().as_bytes(), b"/tmp/caf\xff\xfe");
        assert!(
            path.display().contains('\u{fffd}'),
            "display is the lossy one"
        );
    }

    #[test]
    fn prefix_matching_respects_component_boundaries() {
        let child = NativePath::from_bytes(b"/home/ab/x".to_vec());
        assert!(child.starts_with(&NativePath::from_bytes(b"/home".to_vec())));
        assert!(child.starts_with(&NativePath::from_bytes(b"/home/".to_vec())));
        assert!(!child.starts_with(&NativePath::from_bytes(b"/home/a".to_vec())));
    }

    #[test]
    fn nothing_but_a_complete_scan_reaches_current() {
        assert!(RootState::Building.can_transition_to(RootState::Current));
        assert!(RootState::CatchingUp.can_transition_to(RootState::Current));
        assert!(!RootState::Offline.can_transition_to(RootState::Current));
        assert!(!RootState::Empty.can_transition_to(RootState::Current));
    }

    #[test]
    fn a_policy_change_changes_the_hash() {
        let policy = RootPolicy::default_home();
        let mut other = policy.clone();
        other.include_hidden = false;
        assert_ne!(policy.hash(), other.hash());
        // Order of the skip list is not part of the policy's identity.
        let mut reordered = policy.clone();
        reordered.skip_names.reverse();
        assert_eq!(policy.hash(), reordered.hash());
    }

    #[test]
    fn the_trash_is_skipped_in_both_of_its_shapes() {
        let policy = RootPolicy::default_home();
        for name in ["/home/u/Trash", "/media/x/.Trash-1000", "/home/u/.git"] {
            assert!(
                policy.is_skipped(
                    &NativePath::from_bytes(name.as_bytes().to_vec()),
                    EntryKind::Directory
                ),
                "{name} was not skipped"
            );
        }
        assert!(!policy.is_skipped(
            &NativePath::from_bytes(b"/home/u/notes".to_vec()),
            EntryKind::Directory
        ));
    }
}
