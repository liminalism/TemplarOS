//! The walker: uncapped by default, bounded by budget, honest when it stops early.
//!
//! What changed from the index this replaces is not the traversal — it is what happens at the
//! limit. The old walk stopped at fifty thousand entries and returned; the caller logged a warning
//! and served the truncated root as though it were the whole thing. Here a limit is recorded on
//! the session, the commit is marked incomplete, collection is suppressed and the root lands in a
//! state that says why. The tree is not smaller than it was; the answer just admits it.

use crate::{
    Result,
    generation::{ScanSession, ScannedEntry, is_permission_denied},
    model::{BudgetKind, Coverage, EntryKind, EntryMeta, NativePath},
};
use std::path::Path;

/// Entries collected to sort a directory. Past this the walker visits the sorted prefix and then
/// streams the rest in readdir order, so a giant directory cannot pin the whole listing in RAM.
const SORT_CHILDREN_LIMIT: usize = 4096;

/// Walks `root` into an open generation. `root` is where the walk starts, not necessarily the
/// registered root: a subtree pass starts inside one.
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
        session.set_limited(BudgetKind::Depth);
        return Ok(());
    }

    let native = NativePath::from_path(path);
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // Gone between the `read_dir` that named it and the `stat` that would describe it. That
        // is a deletion, not a blind spot: the row is simply not re-observed and collection
        // tombstones it. Counting it as unreadable would make every pass over a live tree
        // incomplete, which is how a completeness test stops meaning anything. At the top it is
        // the opposite — a root whose own path has vanished proves nothing about its contents.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && depth > 0 => return Ok(()),
        Err(error) => {
            session.note_unreadable(is_permission_denied(&error));
            return Ok(());
        }
    };

    let meta = EntryMeta::from_symlink_metadata(path, &metadata);
    let is_directory = meta.kind == EntryKind::Directory;
    let coverage = session.coverage_for(&native, meta.kind);
    session.observe(ScannedEntry {
        path: native.clone(),
        parent: parent.cloned(),
        meta,
    })?;

    if !is_directory || session.stopped() {
        return Ok(());
    }
    // `Opaque` is where the walk stops: the directory is known and sized, and nothing inside it is
    // materialised until a consumer asks for the subtree by name. `Ignored` never got a row at
    // all. Either way, descending would write rows the policy says do not exist.
    if !coverage.is_some_and(Coverage::materialises_children) {
        // An `Opaque` directory still gets its size: a raw walk that sums bytes without writing
        // any of the rows the policy just refused to store. This is the one place that cost is
        // paid — an ordinary pass that visits the directory anyway — never on a live event.
        if coverage == Some(Coverage::Opaque) {
            let totals = crate::aggregate::sum_subtree(path, &session.options().policy);
            session.stage_aggregate(native, totals);
        }
        return Ok(());
    }
    // A root is not allowed to wander onto another filesystem unless the user said so: the whole
    // machine is not what "index my home directory" means.
    if !session.options().policy.cross_mount
        && ((depth > 0 && root_device.is_some_and(|device| device != device_of(&metadata)))
            || session.mount_boundary(&native)?)
    {
        return Ok(());
    }

    let mut entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && depth > 0 => return Ok(()),
        Err(error) => {
            session.note_unreadable(is_permission_denied(&error));
            return Ok(());
        }
    };

    // Sorted below the cap, streamed past it: collecting a million-entry directory to sort it
    // would pin ~100MB to order a walk whose result is order-free. The sorted prefix keeps small
    // directories deterministic; the spilled tail still visits everything, in readdir order.
    let mut children: Vec<std::path::PathBuf> = Vec::new();
    let mut spilled = false;
    for entry in entries.by_ref() {
        match entry {
            Ok(entry) => {
                children.push(entry.path());
                if children.len() > SORT_CHILDREN_LIMIT {
                    spilled = true;
                    break;
                }
            }
            Err(error) => {
                session.note_unreadable(is_permission_denied(&error));
            }
        }
    }
    if spilled {
        let tail = children.pop();
        children.sort();
        for child in &children {
            if session.stopped() {
                break;
            }
            walk(session, child, Some(&native), depth + 1, root_device)?;
        }
        return walk_spill(session, &native, tail, entries, depth, root_device);
    }
    children.sort();

    for child in children {
        if session.stopped() {
            break;
        }
        walk(session, &child, Some(&native), depth + 1, root_device)?;
    }
    Ok(())
}

/// Walk the spilled tail of an over-cap directory in readdir order: the entry that tripped the
/// cap first, then the rest of the still-open listing. Every child is visited; only the order
/// differs from the sorted prefix.
fn walk_spill(
    session: &mut ScanSession,
    dir: &NativePath,
    tail: Option<std::path::PathBuf>,
    entries: std::fs::ReadDir,
    depth: u32,
    root_device: Option<u64>,
) -> Result<()> {
    if let Some(child) = tail
        && !session.stopped()
    {
        walk(session, &child, Some(dir), depth + 1, root_device)?;
    }
    for entry in entries {
        if session.stopped() {
            break;
        }
        match entry {
            Ok(entry) => {
                let child = entry.path();
                walk(session, &child, Some(dir), depth + 1, root_device)?;
            }
            Err(error) => session.note_unreadable(is_permission_denied(&error)),
        }
    }
    Ok(())
}

fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}
