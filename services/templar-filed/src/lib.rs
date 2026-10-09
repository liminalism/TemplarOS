//! `filed`: the file service behind the file manager, the Home Surface and the file-chooser
//! portal (planning doc 01 §2).
//!
//! The file manager stops being a monolithic GUI that owns its own state. Mount discovery,
//! search, transactional copy/move/rename/delete/trash with durable journals and undo, progress,
//! conflicts, recents, last-used directories and storage information all live here, and the
//! GUIs become clients. Two of the user-visible consequences fall straight out of that: a copy
//! survives the crash of whatever started it, and the file manager, the Home Surface and the
//! file chooser cannot disagree about which folder you were last in — because there is only one
//! answer to ask for.

// Not `forbid(unsafe_code)`: `statvfs`, `localtime_r` and `getuid` have no safe equivalent in
// std, and each call site carries a `SAFETY:` note. Everything else in the crate is safe.

pub mod actions;
pub mod bookmarks;
pub mod browse;
pub mod dbus;
pub mod delta_catchup;
pub mod execute;
pub mod index_host;
pub mod index_lanes;
pub mod index_metrics;
pub mod index_slot;
pub mod journal;
pub mod lastdir;
pub mod mounts;
pub mod native_fs;
#[cfg(test)]
mod native_path;
pub mod observer;
pub mod operations;
pub mod pressure;
pub mod preview;
pub mod recents;
pub mod search;
pub mod storage;
pub mod transfer_undo;
pub mod trash;
pub mod watcher;

pub use browse::{BrowseEntry, DirectoryListing, EntryKind, list_directory};
pub use execute::{Cancel, ExecutorConfig};
pub use journal::{Journal, MemoryJournal, SqliteJournal};
pub use lastdir::{LastDirectories, OperationType, Suggestion};
pub use operations::{
    ConflictPolicy, Operation, OperationId, OperationKind, OperationState, Options, Step, Survey,
    Verification, plan, survey, undo_of,
};
pub use storage::{ReserveVerdict, VolumeInfo, check_reserve};
