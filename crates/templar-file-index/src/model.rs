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

    /// Move the native bytes into an owned path without copying them.
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        PathBuf::from(std::ffi::OsString::from_vec(self.0))
    }

    /// Borrow the byte-exact Unix path for I/O that does not need another owned copy.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.0))
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
    #[cfg(feature = "store")]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    #[default]
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

/// Whether a root's *metadata* has been re-read, as opposed to whether its *membership* has.
///
/// [`RootState`] is the namespace axis and stays exactly what it was: `Current` means "this root's
/// set of paths was exhaustive as of a complete pass". It has never meant anything about sizes and
/// timestamps, and the index has been quietly relying on that: a `PolicyChanged` reconcile commits
/// `complete = true` while `take_on_trust` skips every child `stat`, so the root reads `current`
/// over metadata nobody verified, with no way to say so.
///
/// The two axes are orthogonal on purpose. A filename query should not pay for a per-child
/// `symlink_metadata`; `sort:size` and a drive visualiser should, and they are the ones that need
/// to know when it has not been paid.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataFreshness {
    /// Every row the last complete pass committed had its metadata read from the filesystem.
    #[default]
    Verified,
    /// At least one row's size, times or mode came from the index rather than from a `stat` — or
    /// the pass did not cover the whole root. Never a reason to refuse an answer, only to label
    /// one.
    Unverified,
}

impl MetadataFreshness {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Unverified => "unverified",
        }
    }

    /// Anything this build does not recognise reads as `Unverified`: the honest answer to "I do
    /// not know what that means" is "I cannot vouch for it".
    #[must_use]
    pub fn from_str_or_unverified(value: &str) -> Self {
        if value == "verified" {
            Self::Verified
        } else {
            Self::Unverified
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
    VolumeReturned,
    VolumeReturnedUnverified,
    PolicyChanged,
    BudgetExhausted {
        budget: BudgetKind,
    },
    PermissionDenied,
    /// A directory could not be read, and not because of permissions — an i/o error, or a tree
    /// that changed under the walk. Deliberately distinct from [`Self::PermissionDenied`], which
    /// a user can act on, and from [`Self::WatcherExhausted`], which is a budget rather than a
    /// failure to see.
    Unreadable {
        directories: u64,
    },
    /// The daemon was not running, so the gap cannot be reconstructed from events.
    Downtime,
    /// The daemon was running but had disarmed its watches: no consumer wanted them, so the tree
    /// was not being observed. Deliberately *not* `Downtime` — a report that says "the daemon was
    /// down" when it was up and chose not to look is a report that sends the reader to the wrong
    /// place (`templar doctor`).
    Unwatched,
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
            Self::VolumeReturned => "volume-returned".to_owned(),
            Self::VolumeReturnedUnverified => "volume-returned-unverified".to_owned(),
            Self::PolicyChanged => "policy-changed".to_owned(),
            Self::BudgetExhausted { budget } => format!("budget:{}", budget.as_str()),
            Self::PermissionDenied => "permission-denied".to_owned(),
            Self::Unreadable { directories } => format!("unreadable:directories={directories}"),
            Self::Downtime => "downtime".to_owned(),
            Self::Unwatched => "unwatched".to_owned(),
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
#[serde(default)]
pub struct VolumeIdentity {
    pub dev_id: Option<u64>,
    pub uuid: Option<String>,
    pub mount_point: Option<NativePath>,
    pub label: Option<String>,
    pub fs_type: Option<String>,
    pub fs_uuid: Option<String>,
    /// Opaque statfs fsid, a weak hint rather than persistent identity.
    pub fsid: Option<String>,
    pub mount_root: Option<NativePath>,
    pub mount_id: Option<u64>,
    pub confidence: VolumeConfidence,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeConfidence {
    Strong,
    Weak,
    #[default]
    None,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OfflineReason {
    VolumeAbsent,
    DifferentVolume { observed: VolumeIdentity },
    Unmounted,
    PresenceUnknown { detail: String },
}

/// Whether the index cache is open. An unavailable cache was left untouched; the reason says why
/// (`error::OpenFailure`), and every other file-service interface keeps serving meanwhile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Availability {
    /// `"available"` or `"unavailable"`.
    pub state: String,
    pub reason: Option<String>,
    /// When the current state began, in Unix milliseconds; `None` for a cache open since start.
    pub since_ms: Option<i64>,
}

impl Default for Availability {
    fn default() -> Self {
        Self {
            state: "available".to_owned(),
            reason: None,
            since_ms: None,
        }
    }
}

impl Availability {
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.state == "available"
    }
}

/// What the cache occupies on disk. `db_bytes` alone is the main file, which omits a write-ahead
/// log that can be most of the space during a long scan.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StorageBytes {
    pub main_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
}

impl StorageBytes {
    /// The three files' current sizes; an absent file counts as zero.
    #[must_use]
    pub fn of(path: &std::path::Path) -> Self {
        let size = |suffix: &str| {
            let mut name = path.as_os_str().to_os_string();
            name.push(suffix);
            std::fs::metadata(std::path::PathBuf::from(name)).map_or(0, |meta| meta.len())
        };
        Self {
            main_bytes: size(""),
            wal_bytes: size("-wal"),
            shm_bytes: size("-shm"),
        }
    }

    #[must_use]
    pub fn total(self) -> u64 {
        self.main_bytes + self.wal_bytes + self.shm_bytes
    }
}

/// The watcher's honest self-report, owned here so `templar files index status`, `Index1.Roots` and
/// a later `templar doctor` check all read one model instead of three.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WatchStatus {
    pub backend: String,
    /// Known required directories, or None when refusal tracking overflowed/old payload.
    #[serde(default)]
    pub watches_required: Option<u64>,
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

impl WatchStatus {
    pub fn required_display(&self) -> String {
        self.watches_required
            .map_or_else(|| "unknown".into(), |count| count.to_string())
    }

    /// Unknown required coverage also means refused is only a known lower bound.
    pub fn refused_display(&self) -> String {
        if self.watches_required.is_none() {
            format!(">={}", self.watches_refused)
        } else {
            self.watches_refused.to_string()
        }
    }
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
    #[serde(default)]
    pub offline_reason: Option<OfflineReason>,
    #[serde(default)]
    pub offline_since_ms: Option<i64>,
    /// Event-observed hint for explicit re-registration. Never an automatic root path rewrite.
    #[serde(default)]
    pub mounted_elsewhere: Option<NativePath>,
    /// The metadata axis, orthogonal to `state`. `state = current, metadata = unverified` is a
    /// representable and reachable combination, and it is the honest description of what a
    /// policy-change reconcile leaves behind.
    #[serde(default)]
    pub metadata: MetadataFreshness,
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
    /// The registered root whose path most closely contains this one. That container treats this
    /// root's path as a boundary: every path at or under it is owned here, so the two never hold
    /// the same path.
    #[serde(default)]
    pub nested_in: Option<RootId>,
    /// Registered roots strictly inside this one, each a boundary this root does not acquire.
    #[serde(default)]
    pub contains: Vec<RootId>,
}

/// How much of a subtree the index holds, and how much of it search is allowed to see.
///
/// One index serves two consumers with opposite needs. A filename search wants `target/` not to
/// exist; a drive visualiser needs to know it is 46 GB. The policy this replaces answered only the
/// first question, by dropping those directories entirely — so the OS did not know a 46 GB
/// `target/` was there, and the visualiser had to walk the disk itself. A tier answers both: the
/// directory is known and sized, and nothing inside it is searchable.
///
/// The order is the strength order, and it is load-bearing: a child is stored at the *minimum* of
/// its own tier and its ancestors', so a file with an ordinary name under an `Opaque` build
/// directory does not become searchable by being ordinary.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Coverage {
    /// Not in the index at all, and not descended into. The trash: rows for it are noise in every
    /// consumer, not just in search.
    Ignored,
    /// The entry is stored; its contents are not materialised. What a build or cache directory
    /// gets — known, sized, and holding no rows for a search to leak until the subtree is
    /// explicitly expanded.
    Opaque,
    /// Stored and descended into, and never searchable. What an expanded `Opaque` subtree becomes:
    /// a drive visualiser can account for every file in it, a filename search still cannot see one.
    Inventory,
    /// Stored, descended into, and searchable. Everything the user actually keeps.
    #[default]
    Detailed,
}

impl Coverage {
    #[must_use]
    pub fn as_i64(self) -> i64 {
        match self {
            Self::Ignored => 0,
            Self::Opaque => 1,
            Self::Inventory => 2,
            Self::Detailed => 3,
        }
    }

    /// Anything this build does not recognise reads as `Detailed`, which is what every row written
    /// before the column existed is: a database from a future build is rebuilt, never
    /// misinterpreted, but a single unexpected value must not silently hide a file from search.
    #[must_use]
    pub fn from_i64(value: i64) -> Self {
        match value {
            0 => Self::Ignored,
            1 => Self::Opaque,
            2 => Self::Inventory,
            _ => Self::Detailed,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Opaque => "opaque",
            Self::Inventory => "inventory",
            Self::Detailed => "detailed",
        }
    }

    /// Whether a directory at this tier has its contents materialised. `Opaque` is the tier that
    /// stops a walk.
    #[must_use]
    pub fn materialises_children(self) -> bool {
        matches!(self, Self::Inventory | Self::Detailed)
    }

    /// Whether rows at this tier reach a search. The one predicate `refresh_fts` and the `LIKE`
    /// fallback both answer to, because they are independent leaks.
    #[must_use]
    pub fn is_searchable(self) -> bool {
        matches!(self, Self::Detailed)
    }
}

/// What a [`CoverageRule`] matches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "match")]
pub enum CoverageMatch {
    /// This exact file or directory name, at any depth.
    Name { name: String },
    /// The trash in both of its shapes: `Trash`, and `.Trash-$uid` whose suffix differs on every
    /// machine and so cannot be written as a literal name.
    Trash,
    /// This path and everything beneath it. How a subtree is materialised on demand without
    /// rewriting the root's stored policy.
    Subtree { path: NativePath },
}

impl CoverageMatch {
    #[must_use]
    fn matches(&self, path: &NativePath, name: &[u8]) -> bool {
        match self {
            Self::Name { name: wanted } => wanted.as_bytes() == name,
            Self::Trash => is_trash_name(name),
            Self::Subtree { path: prefix } => path.starts_with(prefix),
        }
    }
}

/// One classification rule. Rules are ordered and the first match wins, so a `Subtree` rule placed
/// ahead of the name rules is how "expand this one build directory" is expressed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CoverageRule {
    pub matcher: CoverageMatch,
    pub coverage: Coverage,
}

impl CoverageRule {
    #[must_use]
    pub fn name(name: &str, coverage: Coverage) -> Self {
        Self {
            matcher: CoverageMatch::Name {
                name: name.to_owned(),
            },
            coverage,
        }
    }

    #[must_use]
    pub fn subtree(path: &NativePath, coverage: Coverage) -> Self {
        Self {
            matcher: CoverageMatch::Subtree { path: path.clone() },
            coverage,
        }
    }
}

/// What a root includes, and at what tier. One predicate, so the scanner and the watcher cannot
/// disagree about which files exist.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RootPolicy {
    /// Follow into other filesystems. Off for `/` and for a home root; on only where a user
    /// explicitly indexed a mounted volume.
    pub cross_mount: bool,
    /// Observe native external operations while consumers are attached. Disabling monitoring
    /// preserves searchable rows but leaves namespace freshness explicitly unwatched.
    #[serde(default = "default_monitoring")]
    pub monitoring: bool,
    /// Directory names never descended into. The pre-tier policy, kept because it is what every
    /// root registered before coverage rules existed has stored in its `policy_json`: a policy
    /// with no rules falls back to it and behaves exactly as it did.
    pub skip_names: Vec<String>,
    /// Skip `Trash` and `.Trash-*`, whose uid suffix cannot be written as a literal skip name.
    /// Part of the same fallback.
    pub skip_trash: bool,
    pub include_hidden: bool,
    /// Ordered classification rules; the first match wins. Empty means "use the fallback above".
    #[serde(default)]
    pub coverage_rules: Vec<CoverageRule>,
    /// What an entry no rule matched is stored at.
    #[serde(default)]
    pub default_coverage: Coverage,
}

const fn default_monitoring() -> bool {
    true
}

impl Default for RootPolicy {
    fn default() -> Self {
        Self::default_home()
    }
}

impl RootPolicy {
    /// Directory names that hold generated output rather than anything a person wrote: caches,
    /// version-control internals, build trees, virtualenvs.
    pub const BUILD_AND_CACHE_NAMES: &'static [&'static str] = &[
        ".git",
        ".hg",
        ".svn",
        ".cache",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
    ];

    /// The policy a home root gets.
    ///
    /// Caches, version-control internals and build output are noise in every search a person
    /// actually wants — but they are not noise to a drive visualiser, and this index serves both.
    /// So they are `Opaque` rather than dropped: the directory is known and its own row is stored,
    /// nothing inside it is materialised or searchable, and a consumer that wants the contents
    /// asks for the subtree to be expanded. The trash stays `Ignored`; nobody wants those rows.
    ///
    /// `add_root` is insert-if-absent, so changing this does not reach a root that already exists.
    /// Only an explicit `set_root_policy` does, and that already marks the root
    /// `Stale{PolicyChanged}` and re-derives coverage on the repair pass.
    #[must_use]
    pub fn default_home() -> Self {
        let mut coverage_rules = vec![CoverageRule {
            matcher: CoverageMatch::Trash,
            coverage: Coverage::Ignored,
        }];
        coverage_rules.extend(
            Self::BUILD_AND_CACHE_NAMES
                .iter()
                .map(|name| CoverageRule::name(name, Coverage::Opaque)),
        );
        Self {
            cross_mount: false,
            monitoring: true,
            // Kept empty: the rules above say the same thing with more resolution, and a
            // non-empty list here would only shadow them for a reader.
            skip_names: Vec::new(),
            skip_trash: true,
            include_hidden: true,
            coverage_rules,
            default_coverage: Coverage::Detailed,
        }
    }

    /// The pre-tier policy, for a caller that genuinely wants build output to not exist — and the
    /// shape every root registered before coverage rules existed still has stored.
    #[must_use]
    pub fn legacy_home() -> Self {
        Self {
            cross_mount: false,
            monitoring: true,
            skip_names: Self::BUILD_AND_CACHE_NAMES
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            skip_trash: true,
            include_hidden: true,
            coverage_rules: Vec::new(),
            default_coverage: Coverage::Detailed,
        }
    }

    /// The same policy with `subtree` and everything under it materialised at `coverage`,
    /// overriding whatever the name rules would have said. The rule goes in front, so it wins.
    #[must_use]
    pub fn materialising(&self, subtree: &NativePath, coverage: Coverage) -> Self {
        let mut policy = self.clone();
        policy
            .coverage_rules
            .insert(0, CoverageRule::subtree(subtree, coverage));
        policy
    }

    /// A stable digest of the policy. A change to it invalidates a root: the rows were produced
    /// under different rules and cannot be compared with rows produced under these.
    ///
    /// The tier fields are appended only when they are set, so a policy that predates them hashes
    /// to exactly the byte string it always did and no existing root is invalidated by an upgrade.
    #[must_use]
    pub fn hash(&self) -> String {
        let mut names = self.skip_names.clone();
        names.sort();
        let mut canonical = format!(
            "cross_mount={};skip_trash={};include_hidden={};skip={}",
            self.cross_mount,
            self.skip_trash,
            self.include_hidden,
            names.join(",")
        );
        if names.iter().any(|name| name.contains(',')) {
            canonical.push_str(";skip-native=");
            for name in &names {
                canonical.push_str(&crate::query::base64(name.as_bytes()));
                canonical.push(';');
            }
        }
        if !self.monitoring {
            canonical.push_str(";monitoring=false");
        }
        if !self.coverage_rules.is_empty() || self.default_coverage != Coverage::Detailed {
            canonical.push_str(";default=");
            canonical.push_str(self.default_coverage.as_str());
            for rule in &self.coverage_rules {
                canonical.push_str(";rule=");
                match &rule.matcher {
                    CoverageMatch::Name { name } => {
                        canonical.push_str("name:");
                        canonical.push_str(&crate::query::base64(name.as_bytes()));
                    }
                    CoverageMatch::Trash => canonical.push_str("trash"),
                    CoverageMatch::Subtree { path } => {
                        canonical.push_str("subtree:");
                        canonical.push_str(&crate::query::base64(path.as_bytes()));
                    }
                }
                canonical.push('=');
                canonical.push_str(rule.coverage.as_str());
            }
        }
        format!("{:016x}", fnv1a64(canonical.as_bytes()))
    }

    /// The tier one path's own name earns, ignoring what its ancestors earned.
    #[must_use]
    pub fn coverage_of(&self, path: &NativePath, _kind: EntryKind) -> Coverage {
        let name = path
            .file_name()
            .map(NativePath::into_bytes)
            .unwrap_or_default();
        for rule in &self.coverage_rules {
            if rule.matcher.matches(path, &name) {
                return rule.coverage;
            }
        }
        // No rule matched, so fall back to the pre-tier policy. This is what makes an upgrade
        // silent: a root whose stored `policy_json` has no rules classifies exactly as it did.
        if self.skip_trash && is_trash_name(&name) {
            return Coverage::Ignored;
        }
        if self
            .skip_names
            .iter()
            .any(|skip| skip.as_bytes() == name.as_slice())
        {
            return Coverage::Ignored;
        }
        self.default_coverage
    }

    /// The tier a row is actually stored at, or `None` when no row is written at all.
    ///
    /// A path is capped by its ancestors: nothing is stored under an `Ignored` or `Opaque`
    /// directory, and an entry under an `Inventory` one is `Inventory` however ordinary its own
    /// name is. `base` bounds the walk — components above it are never classified, so a root
    /// registered *inside* a build directory is still indexed normally.
    #[must_use]
    pub fn stored_coverage_within(
        &self,
        base: &NativePath,
        path: &NativePath,
        kind: EntryKind,
    ) -> Option<Coverage> {
        let mut inherited = Coverage::Detailed;
        for ancestor in ancestors_between(base, path) {
            inherited = inherited.min(self.coverage_of(&ancestor, EntryKind::Directory));
            if !inherited.materialises_children() {
                return None;
            }
        }
        match inherited.min(self.coverage_of(path, kind)) {
            Coverage::Ignored => None,
            coverage => Some(coverage),
        }
    }

    /// [`Self::stored_coverage_within`] with no base, classifying **every** component of the
    /// path. What a live watcher event and a filesystem delta get, where only the path is in hand.
    ///
    /// The difference matters in exactly one place: a root registered inside a directory the rules
    /// would classify — `~/code/target` as its own root — is `Opaque` to this and indexed normally
    /// by the base-bounded form the scanner uses. That is the right way round; the scanner knows
    /// what the user registered and a bare path does not.
    #[must_use]
    pub fn stored_coverage(&self, path: &NativePath, kind: EntryKind) -> Option<Coverage> {
        let mut inherited = Coverage::Detailed;
        for ancestor in ancestors_of(path) {
            inherited = inherited.min(self.coverage_of(&ancestor, EntryKind::Directory));
            if !inherited.materialises_children() {
                return None;
            }
        }
        match inherited.min(self.coverage_of(path, kind)) {
            Coverage::Ignored => None,
            coverage => Some(coverage),
        }
    }

    /// Whether this path is excluded — now "no row is written for it", which is `Ignored` or an
    /// unmaterialised descendant of an `Opaque` directory. The scanner and the watcher both call
    /// exactly this.
    #[must_use]
    pub fn is_skipped(&self, path: &NativePath, kind: EntryKind) -> bool {
        !self.should_index(path, kind)
    }

    /// The shallowest ancestor whose own tier does not materialise its children — the boundary
    /// that stopped a walk from ever reaching `path`. `None` when every ancestor materialises, which
    /// means `path` itself is directly `Ignored` (or nothing stopped it at all).
    ///
    /// What lets a live mutation under an unmaterialised subtree find the one row that could still
    /// say something about it: the `Opaque` directory's cached aggregate. A `Trash`-shaped
    /// ancestor never reaches here in practice — it is never armed, so it never generates an event
    /// — but the search does not assume that; it just returns whichever ancestor actually stopped
    /// materialisation, and the caller decides whether that tier has an aggregate to invalidate.
    #[must_use]
    pub fn unmaterialised_boundary(
        &self,
        base: Option<&NativePath>,
        path: &NativePath,
    ) -> Option<NativePath> {
        let ancestors = match base {
            Some(base) => ancestors_between(base, path),
            None => ancestors_of(path),
        };
        ancestors.into_iter().find(|ancestor| {
            !self
                .coverage_of(ancestor, EntryKind::Directory)
                .materialises_children()
        })
    }

    #[must_use]
    pub fn should_index(&self, path: &NativePath, kind: EntryKind) -> bool {
        self.stored_coverage(path, kind).is_some()
    }
}

/// Every strict ancestor of `path`, shallowest first. Works for a relative path too: a delta
/// stream names paths relative to a subvolume and still has to be classified component by
/// component.
fn ancestors_of(path: &NativePath) -> Vec<NativePath> {
    let mut chain = Vec::new();
    let mut current = path.clone();
    while let Some(parent) = current.parent() {
        // `/`'s parent is `/`; without this the walk never terminates.
        if parent.as_bytes() == current.as_bytes() {
            break;
        }
        chain.push(parent.clone());
        current = parent;
    }
    chain.reverse();
    chain
}

/// The same, bounded below by `base`. `base` itself is never classified: a registered root is
/// indexed because the user said so, whatever the directory happens to be called.
fn ancestors_between(base: &NativePath, path: &NativePath) -> Vec<NativePath> {
    ancestors_of(path)
        .into_iter()
        .filter(|ancestor| ancestor.starts_with(base) && !base.starts_with(ancestor))
        .collect()
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
    /// The tier this row is stored at. `Detailed` for every row written before the column
    /// existed, which is what the old skip list kept.
    #[serde(default)]
    pub coverage: Coverage,
    pub written_generation: Generation,
    pub last_seen_generation: Generation,
    pub deleted_generation: Option<Generation>,
}

/// Whether a repair covers a whole root or one subtree. Garbage collection is scoped to it, so a
/// subtree repair cannot delete the rest of the root.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScanScope {
    #[default]
    Root,
    Subtree(NativePath),
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
    fn native_rule_bytes_and_matcher_kinds_are_part_of_policy_identity() {
        let mut first = RootPolicy::legacy_home();
        first.coverage_rules = vec![CoverageRule {
            matcher: CoverageMatch::Subtree {
                path: NativePath::from_bytes(b"/tmp/\xff".to_vec()),
            },
            coverage: Coverage::Ignored,
        }];
        let mut second = first.clone();
        second.coverage_rules[0].matcher = CoverageMatch::Subtree {
            path: NativePath::from_bytes(b"/tmp/\xfe".to_vec()),
        };
        assert_ne!(first.hash(), second.hash());
        first.coverage_rules[0].matcher = CoverageMatch::Subtree {
            path: NativePath::from_bytes(b"/tmp/name".to_vec()),
        };
        second.coverage_rules[0].matcher = CoverageMatch::Name {
            name: "/tmp/name".to_owned(),
        };
        assert_ne!(first.hash(), second.hash());
        first.coverage_rules.clear();
        second.coverage_rules.clear();
        first.skip_names = vec!["a,b".to_owned()];
        second.skip_names = vec!["a".to_owned(), "b".to_owned()];
        assert_ne!(first.hash(), second.hash());
    }

    #[test]
    fn the_trash_is_ignored_in_both_of_its_shapes() {
        let policy = RootPolicy::default_home();
        for name in ["/home/u/Trash", "/media/x/.Trash-1000"] {
            let path = NativePath::from_bytes(name.as_bytes().to_vec());
            assert_eq!(
                policy.stored_coverage(&path, EntryKind::Directory),
                None,
                "{name} still has a row"
            );
        }
        assert!(!policy.is_skipped(
            &NativePath::from_bytes(b"/home/u/notes".to_vec()),
            EntryKind::Directory
        ));
    }

    /// The tier split, in the one case it exists for: build output is *known* and not searchable,
    /// where the policy it replaces made it not exist at all.
    #[test]
    fn build_output_is_opaque_rather_than_absent() {
        let policy = RootPolicy::default_home();
        let path = |bytes: &[u8]| NativePath::from_bytes(bytes.to_vec());

        assert_eq!(
            policy.stored_coverage(&path(b"/home/u/proj/target"), EntryKind::Directory),
            Some(Coverage::Opaque),
            "the OS has to know a 46 GB target/ is there"
        );
        assert_eq!(
            policy.stored_coverage(
                &path(b"/home/u/proj/target/debug/thing.o"),
                EntryKind::Regular
            ),
            None,
            "and hold no rows for what is inside it"
        );
        assert_eq!(
            policy.stored_coverage(&path(b"/home/u/proj/src/main.rs"), EntryKind::Regular),
            Some(Coverage::Detailed)
        );
        assert_eq!(
            policy.stored_coverage(&path(b"/home/u/targeted/file"), EntryKind::Regular),
            Some(Coverage::Detailed),
            "the rule is a whole component, not a prefix"
        );

        // Expanding one subtree materialises it as inventory without touching the stored policy.
        let expanded = policy.materialising(&path(b"/home/u/proj/target"), Coverage::Inventory);
        assert_eq!(
            expanded.stored_coverage(
                &path(b"/home/u/proj/target/debug/thing.o"),
                EntryKind::Regular
            ),
            Some(Coverage::Inventory)
        );
        assert_eq!(
            policy.stored_coverage(
                &path(b"/home/u/proj/target/debug/thing.o"),
                EntryKind::Regular
            ),
            None,
            "the original policy is unchanged"
        );
    }

    /// The boundary a mutation under an `Opaque` subtree resolves to: the `Opaque` directory
    /// itself, not the file inside it that triggered the event.
    #[test]
    fn the_unmaterialised_boundary_is_the_opaque_directory_not_the_file_inside_it() {
        let policy = RootPolicy::default_home();
        let path = |bytes: &[u8]| NativePath::from_bytes(bytes.to_vec());

        let boundary = policy
            .unmaterialised_boundary(None, &path(b"/home/u/proj/target/debug/thing.o"))
            .expect("target/ stops materialisation");
        assert_eq!(boundary, path(b"/home/u/proj/target"));

        assert_eq!(
            policy.unmaterialised_boundary(None, &path(b"/home/u/proj/src/main.rs")),
            None,
            "every ancestor of a Detailed file materialises"
        );

        // Bounded by `base`: a root registered *inside* `target/` treats it as ordinary, so there
        // is no boundary below the root itself.
        let inside = path(b"/home/u/proj/target");
        assert_eq!(
            policy.unmaterialised_boundary(Some(&inside), &path(b"/home/u/proj/target/thing.o")),
            None
        );
    }

    /// A root registered before coverage rules existed has none in its stored `policy_json`, and
    /// must keep classifying exactly as it did — including hashing to the same digest, or the next
    /// `set_root_policy` would invalidate every root on the machine for nothing.
    #[test]
    fn a_policy_without_rules_falls_back_to_the_skip_list_it_shipped_with() {
        let legacy = RootPolicy::legacy_home();
        let rehydrated: RootPolicy =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(rehydrated, legacy);

        let target = NativePath::from_bytes(b"/home/u/proj/target".to_vec());
        assert_eq!(legacy.stored_coverage(&target, EntryKind::Directory), None);
        assert_eq!(
            legacy.hash(),
            "e639ff8fd14417d8",
            "the pre-tier digest is frozen: a changed hash invalidates every existing root"
        );
        assert_ne!(legacy.hash(), RootPolicy::default_home().hash());
    }
}
