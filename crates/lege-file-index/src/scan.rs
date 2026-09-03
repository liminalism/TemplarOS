//! The walker: uncapped by default, bounded by budget, honest when it stops early.
//!
//! What changed from the index this replaces is not the traversal — it is what happens at the
//! limit. The old walk stopped at fifty thousand entries and returned; the caller logged a warning
//! and served the truncated root as though it were the whole thing. Here a limit is recorded on
//! the session, the commit is marked incomplete, collection is suppressed and the root lands in a
//! state that says why. The tree is not smaller than it was; the answer just admits it.

use crate::{
    Result,
    generation::{ScanSession, ScannedEntry},
    model::{BudgetKind, EntryKind, EntryMeta, NativePath},
};
use std::path::Path;

/// Walks `root` into an open generation.
pub fn walk_into(session: &mut ScanSession, root: &NativePath) -> Result<()> {
    let start = root.to_path();
    let root_device = std::fs::symlink_metadata(&start).ok().map(|metadata| {
        use std::os::unix::fs::MetadataExt;
        metadata.dev()
    });
    walk(session, &start, None, 0, root_device)
}

fn walk(
    session: &mut ScanSession,
    path: &Path,
    parent: Option<&NativePath>,
    depth: u32,
    root_device: Option<u64>,
) -> Result<()> {
    if session.stopped() {
        return Ok(());
    }
    if let Some(max) = session.options().budget.max_depth
        && depth > max
    {
        // Depth is a budget like any other: it makes the pass incomplete rather than quietly
        // trimming the tree.
        session.note_depth_limit();
        return Ok(());
    }

    let native = NativePath::from_path(path);
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            session.note_unreadable(error.kind() == std::io::ErrorKind::PermissionDenied);
            return Ok(());
        }
    };

    let meta = EntryMeta::from_symlink_metadata(path, &metadata);
    let is_directory = meta.kind == EntryKind::Directory;
    session.observe(ScannedEntry {
        path: native.clone(),
        parent: parent.cloned(),
        meta,
    })?;

    if !is_directory || session.stopped() {
        return Ok(());
    }
    // A root is not allowed to wander onto another filesystem unless the user said so: the whole
    // machine is not what "index my home directory" means.
    if !session.options().policy.cross_mount
        && let Some(root_device) = root_device
        && root_device != device_of(&metadata)
        && depth > 0
    {
        return Ok(());
    }

    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            session.note_unreadable(error.kind() == std::io::ErrorKind::PermissionDenied);
            return Ok(());
        }
    };

    let mut children: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => children.push(entry.path()),
            Err(error) => {
                session.note_unreadable(error.kind() == std::io::ErrorKind::PermissionDenied);
            }
        }
    }
    children.sort();

    for child in children {
        if session.stopped() {
            break;
        }
        let child_native = NativePath::from_path(&child);
        if session
            .options()
            .policy
            .is_skipped(&child_native, EntryKind::Other)
        {
            continue;
        }
        walk(session, &child, Some(&native), depth + 1, root_device)?;
    }
    Ok(())
}

fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

trait DepthLimit {
    fn note_depth_limit(&mut self);
}

impl DepthLimit for ScanSession {
    /// A depth budget bounds this branch, and makes the pass incomplete — but the rest of the
    /// tree is still worth indexing, so it does not end the walk.
    fn note_depth_limit(&mut self) {
        self.set_limited(BudgetKind::Depth);
    }
}
