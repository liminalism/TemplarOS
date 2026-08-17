//! `filed`: the file service behind the file manager, the Home Surface and the file-chooser
//! portal (planning doc 01 §2). Mount discovery, indexing/search, transactional copy/move/
//! rename/delete/trash with durable journals and undo, progress, conflicts, recents, last-used
//! directories, previews, and association lookup through `appd`.

pub mod lastdir;
pub mod operations;
pub mod storage;

pub use operations::{Operation, OperationId, OperationKind, OperationState, Step, plan};
pub use storage::{ReserveVerdict, VolumeInfo, check_reserve};
