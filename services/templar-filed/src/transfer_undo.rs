//! Ordinary-change protection for Copy undo. Metadata is attached to the already-journalled
//! destination step indexes, never an independently growing directory manifest. Undo moves
//! owned roots to the real Trash, not permanent deletion: stat and rename are not atomic, so
//! contents changed in that narrow race remain recoverable from the journalled Trash.
use std::{collections::{BTreeMap, BTreeSet}, path::Path, os::unix::fs::MetadataExt};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use crate::operations::{ConflictPolicy, Operation, OperationKind, OperationState, Step};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreatedMetadata {
    dev: u64, ino: u64, len: u64, mode: u32,
    mtime: i64, mtime_nsec: i64, ctime: i64, ctime_nsec: i64,
}
impl CreatedMetadata {
    fn at(path: &Path) -> Result<Self> {
        let m = path.symlink_metadata().with_context(|| format!("reading created entry {}", path.display()))?;
        Ok(Self { dev: m.dev(), ino: m.ino(), len: m.len(), mode: m.mode(),
            mtime: m.mtime(), mtime_nsec: m.mtime_nsec(), ctime: m.ctime(), ctime_nsec: m.ctime_nsec() })
    }
    fn is_dir(&self) -> bool { self.mode & 0o170000 == 0o040000 }
}
fn destination(step: &Step) -> Option<&Path> {
    match step {
        Step::CreateDirectory { destination } | Step::CopySymlink { destination, .. }
            | Step::CreateDirectoryExclusive { destination, .. }
            | Step::ExposeAtomically { destination, .. } => Some(destination),
        _ => None,
    }
}
pub fn capture(operation: &Operation) -> Result<BTreeMap<usize, CreatedMetadata>> {
    if !matches!(operation.kind, OperationKind::Copy { .. }) || operation.options.conflict == ConflictPolicy::Replace {
        return Ok(BTreeMap::new());
    }
    operation.steps.iter().enumerate().filter_map(|(index, step)| destination(step).map(|path| (index, path)))
        .map(|(index, path)| CreatedMetadata::at(path).map(|metadata| (index, metadata))).collect()
}
fn require_snapshot(operation: &Operation) -> Result<()> {
    if operation.state != OperationState::Committed || !matches!(operation.kind, OperationKind::Copy { .. })
        || operation.options.conflict == ConflictPolicy::Replace || operation.transfers.is_empty() {
        bail!("this journal record is not an undoable newly-created copy");
    }
    if operation.created.len() != operation.steps.iter().filter(|step| destination(step).is_some()).count() {
        bail!("the copy has no complete committed destination snapshot");
    }
    Ok(())
}
/// Complete preflight, before an inverse enters the journal.
pub fn validate(operation: &Operation) -> Result<()> {
    require_snapshot(operation)?;
    validate_entries(operation, None)
}
/// Recheck the particular root immediately before trashing it, including on inverse resume.
/// Previously trashed roots need not exist to validate the remaining roots.
pub fn validate_root(operation: &Operation, root: &Path) -> Result<()> {
    require_snapshot(operation)?;
    if !operation.transfers.iter().any(|entry| entry.destination == root) {
        bail!("copy undo root does not belong to the original journal record");
    }
    validate_entries(operation, Some(root))
}
fn validate_entries(operation: &Operation, root: Option<&Path>) -> Result<()> {
    let owned: BTreeSet<&Path> = operation.steps.iter().filter_map(destination)
        .filter(|path| root.is_none_or(|root| path.starts_with(root))).collect();
    for (&index, expected) in &operation.created {
        let path = operation.steps.get(index).and_then(destination).context("invalid copy snapshot step")?;
        if root.is_some_and(|root| !path.starts_with(root)) { continue; }
        if CreatedMetadata::at(path)? != *expected {
            bail!("copy destination changed since the operation; undo refused: {}", path.display());
        }
        if expected.is_dir() {
            for child in std::fs::read_dir(path).with_context(|| format!("reading created directory {}", path.display()))? {
                let child = child?.path();
                if !owned.contains(child.as_path()) {
                    bail!("copy destination contains an entry the operation did not create; undo refused: {}", child.display());
                }
            }
        }
    }
    Ok(())
}
