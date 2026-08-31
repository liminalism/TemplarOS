//! Running a planned operation, one journalled step at a time.
//!
//! Every step is idempotent, which is what makes the two hard cases work: resuming an operation
//! that was interrupted mid-way, and reversing one that finished. The executor never deletes a
//! source until the matching destination has been written, flushed, verified and atomically
//! exposed — so a power cut during a move leaves either the original or the copy, never neither.

use crate::{
    operations::{ConflictPolicy, Operation, OperationKind, OperationState, Step, Verification},
    storage::{self, ReserveVerdict},
    trash,
};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// A shared flag the caller flips to stop an operation between steps.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Everything the executor needs that is not in the plan.
#[derive(Clone, Debug)]
pub struct ExecutorConfig {
    /// `$XDG_DATA_HOME`, for the home trash.
    pub data_home: PathBuf,
    /// Reserve as a percentage of a volume's size; 0 disables the check.
    pub reserve_percent: u64,
    /// How often progress is written back to the journal. Every item boundary is always
    /// written; this bounds the interval for a single very large file.
    pub progress_interval: Duration,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            data_home: std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                })
                .unwrap_or_else(|| PathBuf::from("/tmp")),
            reserve_percent: 5,
            progress_interval: Duration::from_millis(250),
        }
    }
}

/// Refuse an operation that would eat into the space the OS keeps for logging in, updating and
/// recovering — unless the caller explicitly overrode it.
pub fn check_reserve(operation: &Operation, config: &ExecutorConfig) -> Result<ReserveVerdict> {
    if config.reserve_percent == 0 || operation.options.override_reserve {
        return Ok(ReserveVerdict::Ok);
    }
    let Some(destination) = operation.steps.iter().find_map(destination_dir_of) else {
        return Ok(ReserveVerdict::Ok);
    };
    let volume = storage::query(&destination, config.reserve_percent)
        .with_context(|| format!("checking free space on {}", destination.display()))?;
    // What is still to be written, not what the operation started as: bytes already copied are
    // on the volume, and counting them twice would refuse a resume that is nearly finished.
    let outstanding = operation.total_bytes.saturating_sub(operation.done_bytes);
    Ok(storage::check_reserve(&volume, outstanding))
}

fn destination_dir_of(step: &Step) -> Option<PathBuf> {
    match step {
        Step::CopyToTemporary { temporary, .. } => temporary.parent().map(Path::to_path_buf),
        Step::CreateDirectory { destination } => destination.parent().map(Path::to_path_buf),
        _ => None,
    }
}

/// Run `operation` from its current cursor to the end.
///
/// `on_progress` is called on every item boundary and at most once per
/// [`ExecutorConfig::progress_interval`] within a large file, so a caller can journal and signal
/// without either losing the resume point or fsyncing per block.
pub fn run(
    operation: &mut Operation,
    config: &ExecutorConfig,
    cancel: &Cancel,
    mut on_progress: impl FnMut(&Operation),
) -> Result<()> {
    if operation.state.terminal() {
        return Ok(());
    }
    if matches!(check_reserve(operation, config), Ok(ReserveVerdict::Refuse)) {
        operation.state = OperationState::Failed;
        operation.error = Some(
            "not enough space: this would consume the reserve the system keeps for logging in, \
             updating and recovering. Free space, or run it again with the reserve override."
                .to_owned(),
        );
        on_progress(operation);
        bail!("{}", operation.error.as_deref().unwrap_or("reserve"));
    }

    operation.state = OperationState::Running;
    operation.error = None;
    on_progress(operation);

    let mut last_report = Instant::now();
    while operation.cursor < operation.steps.len() {
        if cancel.is_cancelled() {
            operation.state = OperationState::Cancelled;
            on_progress(operation);
            return Ok(());
        }

        // Applied in place, not to a clone: a step that only learns where it put something once
        // it runs — trashing, restoring — writes that back here, and the next `on_progress`
        // journals it along with the rest of the operation.
        let cursor = operation.cursor;
        let conflict = operation.options.conflict;
        if let Err(error) = apply(&mut operation.steps[cursor], conflict, config) {
            operation.state = OperationState::Failed;
            operation.error = Some(format!("{error:#}"));
            on_progress(operation);
            return Err(error);
        }

        let (bytes, boundary) = {
            let step = &operation.steps[cursor];
            (step.bytes(), step.is_item_boundary())
        };
        operation.done_bytes = operation.done_bytes.saturating_add(bytes);
        if boundary {
            operation.done_items = operation.done_items.saturating_add(1);
        }
        operation.cursor += 1;

        // Everything is written and verified; only source removal is left. Recording this is
        // what lets a crash here resume by deleting sources rather than copying again.
        if operation.state == OperationState::Running && remaining_is_only_source_removal(operation)
        {
            operation.state = OperationState::Verified;
        }

        if boundary || last_report.elapsed() >= config.progress_interval {
            last_report = Instant::now();
            on_progress(operation);
        }
    }

    operation.state = OperationState::Committed;
    on_progress(operation);
    Ok(())
}

fn remaining_is_only_source_removal(operation: &Operation) -> bool {
    let remaining = &operation.steps[operation.cursor..];
    !remaining.is_empty()
        && remaining.iter().all(|step| {
            matches!(
                step,
                Step::DeleteSource { .. } | Step::RemoveDirectory { .. }
            )
        })
}

/// Apply one step. Re-applying a step that already took effect must succeed and change nothing.
///
/// Takes the step by mutable reference because the trash steps learn where they put something
/// only by doing it, and that is what undo later reads.
fn apply(step: &mut Step, conflict: ConflictPolicy, config: &ExecutorConfig) -> Result<()> {
    match step {
        Step::MoveToTrash { path, trashed } => {
            // Probed from the nearest surviving ancestor, because on the resume path below the
            // file is already gone and a vanished path has no device to pick a trash from.
            let directory = trash::trash_dir_for(&existing_ancestor(path), &config.data_home)?;
            if path.symlink_metadata().is_err() {
                // Already trashed — a crash between the rename and the journal write. The move
                // really happened, so recover the name it was given; leaving this unset would
                // cost the item its undo.
                if trashed.is_none() {
                    *trashed = trash::find_trashed(path, &directory);
                }
                return Ok(());
            }
            let item = trash::trash(path, &directory, &trash::now_local())
                .with_context(|| format!("moving {} to the trash", path.display()))?;
            *trashed = Some(item.trashed_path);
            Ok(())
        }

        Step::RestoreFromTrash {
            trashed,
            restored_to,
        } => {
            let directory = trash::trash_dir_for(&existing_ancestor(trashed), &config.data_home)?;
            if trashed.symlink_metadata().is_err() {
                // Already restored. If the `.trashinfo` outlived the rename we can still say
                // where the file went; if it did not, nothing on disk knows any more and this
                // item loses its undo.
                if restored_to.is_none() {
                    *restored_to = trash::read_info(trashed, &directory)
                        .ok()
                        .map(|item| item.original_path);
                }
                return Ok(());
            }
            let original = trash::restore(trashed, &directory)
                .with_context(|| format!("restoring {}", trashed.display()))?;
            *restored_to = Some(original);
            Ok(())
        }

        // Every other step only reads its plan, so it takes the step as shared and stays as it
        // was before the trash steps needed to write back.
        read_only => apply_read_only(read_only, conflict),
    }
}

/// The steps that only read their plan. Only the trash needs `ExecutorConfig`, so this does not
/// take one.
fn apply_read_only(step: &Step, conflict: ConflictPolicy) -> Result<()> {
    match step {
        Step::CreateDirectory { destination } => fs::create_dir_all(destination)
            .with_context(|| format!("creating {}", destination.display())),

        Step::CopySymlink {
            source,
            destination,
        } => copy_symlink(source, destination, conflict),

        Step::CopyToTemporary {
            source,
            temporary,
            bytes,
        } => copy_to_temporary(source, temporary, *bytes),

        Step::FlushTemporary { temporary } => flush(temporary),

        Step::VerifyCopy {
            source,
            temporary,
            verify,
        } => verify_copy(source, temporary, *verify),

        Step::ExposeAtomically {
            temporary,
            destination,
        } => expose(temporary, destination, conflict),

        Step::RenameInPlace { from, to } => rename_in_place(from, to, conflict),

        Step::DeleteSource { source } => remove_file_if_present(source),

        Step::RemoveDirectory { path } => remove_directory(path),

        Step::Unlink { path } => unlink(path),

        // `apply` handles these two. Reaching here would mean it had stopped recording where
        // the trash put an item, so it fails loudly rather than quietly costing that item its
        // undo — the exact failure this split exists to prevent.
        Step::MoveToTrash { .. } | Step::RestoreFromTrash { .. } => {
            bail!("a trash step reached the read-only executor, which cannot record where it went")
        }
    }
}

/// `path` itself if it is still there, else the nearest ancestor that is.
///
/// Which trash a file belongs in is decided by the device it sits on, and a path that has already
/// been moved away cannot answer that; its parent, on the same device, can.
fn existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|candidate| candidate.symlink_metadata().is_ok())
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

fn copy_symlink(source: &Path, destination: &Path, conflict: ConflictPolicy) -> Result<()> {
    let target =
        fs::read_link(source).with_context(|| format!("reading the link {}", source.display()))?;
    match destination.symlink_metadata() {
        // Already the same link: nothing to do.
        Ok(metadata)
            if metadata.file_type().is_symlink()
                && fs::read_link(destination).ok().as_ref() == Some(&target) =>
        {
            Ok(())
        }
        Ok(_) if conflict == ConflictPolicy::Replace => {
            fs::remove_file(destination).ok();
            std::os::unix::fs::symlink(&target, destination)
                .with_context(|| format!("linking {}", destination.display()))
        }
        Ok(_) => bail!("{} already exists", destination.display()),
        Err(_) => std::os::unix::fs::symlink(&target, destination)
            .with_context(|| format!("linking {}", destination.display())),
    }
}

fn copy_to_temporary(source: &Path, temporary: &Path, bytes: u64) -> Result<()> {
    // A temporary of exactly the right size is a completed copy from a previous run.
    if bytes > 0
        && temporary
            .metadata()
            .is_ok_and(|metadata| metadata.len() == bytes)
    {
        return Ok(());
    }
    if let Some(parent) = temporary.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_file(source, temporary)
        .map(|_| ())
        .with_context(|| format!("copying {}", source.display()))
}

fn flush(temporary: &Path) -> Result<()> {
    let file = File::open(temporary)
        .with_context(|| format!("opening {} to flush", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing {}", temporary.display()))?;
    // The directory entry itself must be durable too, or a crash can lose the name.
    if let Some(parent) = temporary.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn expose(temporary: &Path, destination: &Path, conflict: ConflictPolicy) -> Result<()> {
    if temporary.symlink_metadata().is_err() && destination.symlink_metadata().is_ok() {
        return Ok(()); // already exposed by an earlier run
    }
    if destination.symlink_metadata().is_ok() && conflict != ConflictPolicy::Replace {
        bail!("{} already exists", destination.display());
    }
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "exposing {} as {}",
            temporary.display(),
            destination.display()
        )
    })
}

fn rename_in_place(from: &Path, to: &Path, conflict: ConflictPolicy) -> Result<()> {
    if from.symlink_metadata().is_err() && to.symlink_metadata().is_ok() {
        return Ok(()); // already renamed
    }
    if to.symlink_metadata().is_ok() && conflict != ConflictPolicy::Replace {
        bail!("{} already exists", to.display());
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(from, to).with_context(|| format!("renaming {} to {}", from.display(), to.display()))
}

fn remove_file_if_present(source: &Path) -> Result<()> {
    match fs::remove_file(source) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", source.display())),
    }
}

fn remove_directory(path: &Path) -> Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        // Something arrived in the directory while the move ran. The destination is complete,
        // so leaving the remains behind beats failing a finished operation.
        Err(_) if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some()) => {
            tracing::warn!(path = %path.display(), "left behind: not empty after the move");
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn unlink(path: &Path) -> Result<()> {
    let Ok(metadata) = path.symlink_metadata() else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
    .or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error)
        }
    })
    .with_context(|| format!("deleting {}", path.display()))
}

/// Copy with a bounded buffer, preserving the permission bits. Sparse-file awareness and
/// `copy_file_range` acceleration are deliberately not here: correctness first, and
/// `std::fs::copy` already uses the fast paths the platform offers.
fn copy_file(source: &Path, destination: &Path) -> Result<u64> {
    let bytes = fs::copy(source, destination)?;
    Ok(bytes)
}

fn verify_copy(source: &Path, temporary: &Path, verify: Verification) -> Result<()> {
    let expected = source.metadata()?.len();
    let actual = temporary
        .metadata()
        .with_context(|| format!("{} is missing after the copy", temporary.display()))?
        .len();
    if expected != actual {
        bail!(
            "{} is {actual} bytes but {} is {expected}: the copy is short",
            temporary.display(),
            source.display()
        );
    }
    if verify == Verification::Checksum {
        let (source_crc, copy_crc) = (crc32_of(source)?, crc32_of(temporary)?);
        if source_crc != copy_crc {
            bail!(
                "{} does not match {}: CRC-32 {copy_crc:#010x} against {source_crc:#010x}",
                temporary.display(),
                source.display()
            );
        }
    }
    Ok(())
}

/// CRC-32 (IEEE 802.3) of a file's contents.
///
/// This detects a corrupted or truncated write, which is what a copy needs to check. It is not
/// a cryptographic digest and is not claimed to be one: an attacker who can write to the
/// destination can also match the checksum.
pub fn crc32_of(path: &Path) -> Result<u32> {
    let mut file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut crc = Crc32::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        crc.update(&buffer[..read]);
    }
    Ok(crc.finish())
}

/// Bytewise CRC-32, table generated once on first use.
#[derive(Debug)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        let table = crc_table();
        for byte in bytes {
            let index = ((self.0 ^ u32::from(*byte)) & 0xFF) as usize;
            self.0 = table[index] ^ (self.0 >> 8);
        }
    }

    pub fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Crc32 {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0_u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut value = index as u32;
            for _ in 0..8 {
                value = if value & 1 == 1 {
                    0xEDB8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
            }
            *slot = value;
        }
        table
    })
}

/// Resume an operation the service found unfinished at start.
///
/// A `Verified` operation is finished apart from removing its sources, and a `Running` one picks
/// up at its recorded cursor; both are safe because every step is idempotent.
pub fn resume(
    operation: &mut Operation,
    config: &ExecutorConfig,
    cancel: &Cancel,
    on_progress: impl FnMut(&Operation),
) -> Result<()> {
    tracing::info!(
        id = %operation.id,
        state = operation.state.as_str(),
        cursor = operation.cursor,
        steps = operation.steps.len(),
        "resuming a journalled operation"
    );
    run(operation, config, cancel, on_progress)
}

/// Whether the operation left anything behind that a caller should mention.
pub fn leftovers(operation: &Operation) -> Vec<&OperationKind> {
    if operation.skipped.is_empty() {
        Vec::new()
    } else {
        vec![&operation.kind]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::{
        OperationId, Options, Survey, SurveyEntry, SurveyedSource, plan, survey, undo_of,
    };
    use std::sync::Mutex;

    fn config(dir: &Path) -> ExecutorConfig {
        ExecutorConfig {
            data_home: dir.join("data"),
            reserve_percent: 0,
            progress_interval: Duration::from_millis(0),
        }
    }

    fn execute(operation: &mut Operation, dir: &Path) -> Result<()> {
        run(operation, &config(dir), &Cancel::new(), |_| {})
    }

    /// Plan one operation over a real fixture tree, exactly as the service does.
    fn planned(kind: OperationKind, sources: &[PathBuf], destination: Option<&Path>) -> Operation {
        let surveyed = crate::operations::survey(sources, destination);
        crate::operations::plan(OperationId::new(), kind, &surveyed, Options::default())
    }

    fn write(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn clipboard_a_paste_of_many_files_is_one_operation_over_the_whole_set() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("Documents");
        std::fs::create_dir_all(&destination).unwrap();
        let sources: Vec<PathBuf> = ["one.txt", "two.txt", "three.txt"]
            .iter()
            .map(|name| {
                let path = dir.path().join("from").join(name);
                write(&path, name.as_bytes());
                path
            })
            .collect();

        // One operation naming three files — what a Ctrl+C of three items and one Ctrl+V does.
        let mut operation = planned(
            OperationKind::Copy {
                sources: sources.clone(),
                destination_dir: destination.clone(),
            },
            &sources,
            Some(&destination),
        );
        assert_eq!(operation.total_items, 3);
        execute(&mut operation, dir.path()).unwrap();
        assert_eq!(operation.state, OperationState::Committed);
        // One operation, one set of totals, one progress figure covering all three.
        assert_eq!(operation.done_items, operation.total_items);
        assert!((operation.progress() - 1.0).abs() < f64::EPSILON);
        for name in ["one.txt", "two.txt", "three.txt"] {
            assert_eq!(
                std::fs::read(destination.join(name)).unwrap(),
                name.as_bytes()
            );
            // A copy leaves the sources alone.
            assert!(dir.path().join("from").join(name).exists());
        }
    }

    #[test]
    fn clipboard_a_cut_leaves_no_duplicate_behind_once_the_paste_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("Documents");
        std::fs::create_dir_all(&destination).unwrap();
        let sources: Vec<PathBuf> = ["one.txt", "two.txt"]
            .iter()
            .map(|name| {
                let path = dir.path().join("from").join(name);
                write(&path, name.as_bytes());
                path
            })
            .collect();

        let mut operation = planned(
            OperationKind::Move {
                sources: sources.clone(),
                destination_dir: destination.clone(),
            },
            &sources,
            Some(&destination),
        );
        execute(&mut operation, dir.path()).unwrap();
        assert_eq!(operation.state, OperationState::Committed);
        for (index, name) in ["one.txt", "two.txt"].iter().enumerate() {
            assert_eq!(
                std::fs::read(destination.join(name)).unwrap(),
                name.as_bytes()
            );
            assert!(
                !sources[index].exists(),
                "a cut that pasted must not leave the original behind as well"
            );
        }
    }

    #[test]
    fn trash_a_bulk_delete_is_one_operation_and_one_undo_restores_every_item() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["a.txt", "b.txt", "c.txt"];
        let victims: Vec<PathBuf> = names
            .iter()
            .map(|name| {
                let path = dir.path().join("work").join(name);
                write(&path, name.as_bytes());
                path
            })
            .collect();

        // Delete of a multiple selection is one trash operation naming every path, not one
        // operation per file — which is what makes the single undo below possible at all.
        let mut trashing = planned(
            OperationKind::Trash {
                paths: victims.clone(),
            },
            &[],
            None,
        );
        assert_eq!(trashing.steps.len(), 3);
        execute(&mut trashing, dir.path()).unwrap();
        assert_eq!(trashing.state, OperationState::Committed);
        for victim in &victims {
            assert!(!victim.exists(), "{} is still there", victim.display());
        }

        // One undo, restoring every item exactly: same path, same bytes.
        let undo = crate::operations::undo_of(&trashing).expect("trashing is reversible");
        let mut restoring = planned(undo, &[], None);
        execute(&mut restoring, dir.path()).unwrap();
        assert_eq!(restoring.state, OperationState::Committed);
        for (index, victim) in victims.iter().enumerate() {
            assert_eq!(
                std::fs::read(victim).unwrap(),
                names[index].as_bytes(),
                "{} did not come back as it was",
                victim.display()
            );
        }
    }

    #[test]
    fn trash_undo_restores_only_what_was_actually_trashed() {
        let dir = tempfile::tempdir().unwrap();
        let kept = dir.path().join("work/kept.txt");
        let trashed = dir.path().join("work/trashed.txt");
        write(&kept, b"kept");
        write(&trashed, b"trashed");

        let mut operation = planned(
            OperationKind::Trash {
                paths: vec![trashed.clone()],
            },
            &[],
            None,
        );
        execute(&mut operation, dir.path()).unwrap();

        let undo = crate::operations::undo_of(&operation).expect("a trash has an undo");
        // The undo names exactly the one item the trash actually took: an undo derived from the
        // *request* rather than from what happened would try to restore things that never moved.
        match &undo {
            OperationKind::RestoreFromTrash { paths } => assert_eq!(paths.len(), 1),
            other => panic!("undoing a trash must restore from it, got {other:?}"),
        }
        let mut restoring = planned(undo, &[], None);
        execute(&mut restoring, dir.path()).unwrap();
        assert_eq!(std::fs::read(&trashed).unwrap(), b"trashed");
        assert_eq!(
            std::fs::read(&kept).unwrap(),
            b"kept",
            "untouched throughout"
        );
    }

    #[test]
    fn operations_report_bounded_progress_that_only_ever_moves_forward() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("to");
        std::fs::create_dir_all(&destination).unwrap();
        let sources: Vec<PathBuf> = (0..5)
            .map(|index| {
                let path = dir.path().join("from").join(format!("f{index}.bin"));
                write(&path, &vec![b'x'; 4096]);
                path
            })
            .collect();
        let mut operation = planned(
            OperationKind::Copy {
                sources: sources.clone(),
                destination_dir: destination.clone(),
            },
            &sources,
            Some(&destination),
        );

        let mut seen: Vec<(f64, u64, u64)> = Vec::new();
        run(&mut operation, &config(dir.path()), &Cancel::new(), |op| {
            seen.push((op.progress(), op.done_items, op.done_bytes));
        })
        .unwrap();

        assert!(seen.len() >= 2, "progress was never reported: {seen:?}");
        for (progress, done_items, done_bytes) in &seen {
            assert!(
                (0.0..=1.0).contains(progress),
                "progress left the unit interval: {progress}"
            );
            assert!(*done_items <= operation.total_items);
            assert!(*done_bytes <= operation.total_bytes);
        }
        for pair in seen.windows(2) {
            assert!(
                pair[1].1 >= pair[0].1 && pair[1].2 >= pair[0].2,
                "progress went backwards: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(seen.last().unwrap().1, operation.total_items);
    }

    #[test]
    fn operations_stop_at_a_step_boundary_when_cancelled_and_say_they_were_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("to");
        std::fs::create_dir_all(&destination).unwrap();
        let sources: Vec<PathBuf> = (0..6)
            .map(|index| {
                let path = dir.path().join("from").join(format!("f{index}.txt"));
                write(&path, b"payload");
                path
            })
            .collect();
        let mut operation = planned(
            OperationKind::Copy {
                sources: sources.clone(),
                destination_dir: destination.clone(),
            },
            &sources,
            Some(&destination),
        );

        let cancel = Cancel::new();
        let mut steps = 0;
        run(&mut operation, &config(dir.path()), &cancel, |_| {
            steps += 1;
            if steps == 2 {
                cancel.cancel();
            }
        })
        .unwrap();

        // Cancelled, not failed and certainly not committed: nothing went wrong, the person
        // asked it to stop.
        assert_eq!(operation.state, OperationState::Cancelled);
        assert!(
            operation.cursor < operation.steps.len(),
            "a cancel must stop before the end, not after it"
        );
        assert!(operation.error.is_none(), "a cancel is not an error");
        // Whatever was already exposed stays exposed, and the journal explains what was done.
        assert!(operation.done_items <= operation.total_items);
    }

    #[test]
    fn operations_that_fail_part_way_are_never_reported_as_whole_success() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("to");
        std::fs::create_dir_all(&destination).unwrap();
        let good = dir.path().join("from/good.txt");
        let doomed = dir.path().join("from/doomed.txt");
        write(&good, b"kept");
        write(&doomed, b"vanishes");
        let sources = vec![good.clone(), doomed.clone()];
        let mut operation = planned(
            OperationKind::Copy {
                sources: sources.clone(),
                destination_dir: destination.clone(),
            },
            &sources,
            Some(&destination),
        );
        // The second source disappears after the survey and before its step runs — a race with
        // anything else on the machine, and the ordinary shape of a partial failure.
        std::fs::remove_file(&doomed).unwrap();

        let outcome = execute(&mut operation, dir.path());
        assert!(outcome.is_err(), "a vanished source is a failure");
        assert_eq!(
            operation.state,
            OperationState::Failed,
            "a partial failure must never be recorded as Committed"
        );
        // The error names the item, so a person can tell which of the two did not arrive.
        let error = operation
            .error
            .clone()
            .expect("a failed operation says why");
        assert!(
            error.contains("doomed.txt"),
            "the failure must name the item that failed: {error}"
        );
        // And the work that did land is visible rather than silently rolled back.
        assert!(operation.done_items < operation.total_items);
    }

    #[test]
    fn a_cross_device_style_move_copies_verifies_exposes_then_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/report.pdf");
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&destination_dir).unwrap();
        fs::write(&source, b"the contents").unwrap();

        // Force the copy path rather than the same-device rename, to exercise every step.
        let mut surveyed = survey(std::slice::from_ref(&source), Some(&destination_dir));
        surveyed.sources[0].same_device = false;

        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options {
                verify: Verification::Checksum,
                ..Options::default()
            },
        );
        execute(&mut operation, dir.path()).unwrap();

        assert_eq!(operation.state, OperationState::Committed);
        assert!(!source.exists());
        assert_eq!(
            fs::read(destination_dir.join("report.pdf")).unwrap(),
            b"the contents"
        );
        assert_eq!(operation.done_bytes, operation.total_bytes);
        assert_eq!(operation.done_items, 1);
        // No temporary left behind.
        let leftovers: Vec<_> = fs::read_dir(&destination_dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".legeos-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn a_tree_is_copied_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("from/tree");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("a.txt"), b"aaa").unwrap();
        fs::write(root.join("nested/b.txt"), b"bbbb").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(&destination_dir).unwrap();

        let surveyed = survey(std::slice::from_ref(&root), Some(&destination_dir));
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Copy {
                sources: vec![root.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options::default(),
        );
        execute(&mut operation, dir.path()).unwrap();

        assert_eq!(operation.state, OperationState::Committed);
        assert_eq!(
            fs::read(destination_dir.join("tree/a.txt")).unwrap(),
            b"aaa"
        );
        assert_eq!(
            fs::read(destination_dir.join("tree/nested/b.txt")).unwrap(),
            b"bbbb"
        );
        assert_eq!(
            fs::read_link(destination_dir.join("tree/link")).unwrap(),
            PathBuf::from("a.txt")
        );
        assert!(root.exists(), "a copy leaves the source alone");
    }

    #[test]
    fn an_interrupted_move_resumes_without_copying_twice() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/big.bin");
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&destination_dir).unwrap();
        fs::write(&source, vec![7_u8; 4096]).unwrap();

        let mut surveyed = survey(std::slice::from_ref(&source), Some(&destination_dir));
        surveyed.sources[0].same_device = false;
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options::default(),
        );

        // Model a crash rather than a cancellation: keep the journal record exactly as it stood
        // after the copy step, then stop. The temporary exists; the destination does not.
        let journalled: Arc<Mutex<Option<Operation>>> = Arc::default();
        let cancel = Cancel::new();
        let (recorder, stopper) = (Arc::clone(&journalled), cancel.clone());
        run(&mut operation, &config(dir.path()), &cancel, move |op| {
            if op.cursor == 1 && recorder.lock().unwrap().is_none() {
                *recorder.lock().unwrap() = Some(op.clone());
                stopper.cancel();
            }
        })
        .unwrap();
        assert!(source.exists(), "the source survives an interruption");

        let mut crashed = journalled.lock().unwrap().take().expect("journalled");
        assert_eq!(crashed.state, OperationState::Running);
        assert_eq!(crashed.cursor, 1);

        // Rewrite the source with different bytes of the same length. If resume copied again,
        // the destination would hold the new bytes; the point is that it does not.
        fs::write(&source, vec![9_u8; 4096]).unwrap();

        resume(&mut crashed, &config(dir.path()), &Cancel::new(), |_| {}).unwrap();
        assert_eq!(crashed.state, OperationState::Committed);
        assert!(!source.exists());
        assert_eq!(
            fs::read(destination_dir.join("big.bin")).unwrap(),
            vec![7_u8; 4096],
            "the completed copy was redone instead of resumed"
        );
    }

    #[test]
    fn a_cancelled_operation_stays_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/x.txt");
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&destination_dir).unwrap();
        fs::write(&source, b"body").unwrap();

        let mut surveyed = survey(std::slice::from_ref(&source), Some(&destination_dir));
        surveyed.sources[0].same_device = false;
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir,
            },
            &surveyed,
            Options::default(),
        );

        let cancel = Cancel::new();
        cancel.cancel();
        run(&mut operation, &config(dir.path()), &cancel, |_| {}).unwrap();
        assert_eq!(operation.state, OperationState::Cancelled);

        // Cancelling is the user saying stop; resuming must not quietly override that.
        resume(&mut operation, &config(dir.path()), &Cancel::new(), |_| {}).unwrap();
        assert_eq!(operation.state, OperationState::Cancelled);
        assert!(source.exists());
    }

    #[test]
    fn every_step_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/x.txt");
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&destination_dir).unwrap();
        fs::write(&source, b"body").unwrap();

        let mut surveyed = survey(std::slice::from_ref(&source), Some(&destination_dir));
        surveyed.sources[0].same_device = false;
        let operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options::default(),
        );

        // Apply every step twice, in order. A second application must be a no-op.
        let mut steps = operation.steps.clone();
        for step in &mut steps {
            apply(step, ConflictPolicy::Fail, &config(dir.path())).unwrap();
            apply(step, ConflictPolicy::Fail, &config(dir.path()))
                .unwrap_or_else(|error| panic!("{step:?} is not idempotent: {error:#}"));
        }
        assert_eq!(fs::read(destination_dir.join("x.txt")).unwrap(), b"body");
        assert!(!source.exists());
    }

    #[test]
    fn a_short_copy_is_caught_and_the_source_survives() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.txt");
        let temporary = dir.path().join(".legeos-x.part");
        fs::write(&source, b"the whole thing").unwrap();
        fs::write(&temporary, b"the whole").unwrap();

        let error = verify_copy(&source, &temporary, Verification::Size).unwrap_err();
        assert!(error.to_string().contains("short"), "{error}");
    }

    #[test]
    fn corruption_is_caught_only_by_the_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.bin");
        let temporary = dir.path().join("b.bin");
        fs::write(&source, b"abcdefgh").unwrap();
        fs::write(&temporary, b"abcdefgX").unwrap();

        verify_copy(&source, &temporary, Verification::Size)
            .expect("same length passes a size check");
        let error = verify_copy(&source, &temporary, Verification::Checksum).unwrap_err();
        assert!(error.to_string().contains("CRC-32"), "{error}");
    }

    #[test]
    fn crc32_matches_known_vectors() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xCBF4_3926);

        let mut empty = Crc32::new();
        assert_eq!(empty.finish(), 0);
        empty.update(b"");
        assert_eq!(empty.finish(), 0);

        // Chunked updates must equal one update.
        let mut split = Crc32::new();
        split.update(b"1234");
        split.update(b"56789");
        assert_eq!(split.finish(), 0xCBF4_3926);
    }

    #[test]
    fn trashing_and_undoing_it_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, b"keep me").unwrap();

        let mut operation = plan(
            OperationId::new(),
            OperationKind::Trash {
                paths: vec![path.clone()],
            },
            &survey(std::slice::from_ref(&path), None),
            Options::default(),
        );
        execute(&mut operation, dir.path()).unwrap();
        assert!(!path.exists());
        assert_eq!(operation.state, OperationState::Committed);

        // Straight from `undo_of` into `plan`, with nothing substituted: this is the path
        // `Files.Operations1.Undo` takes, and the version of this test that looked the trashed
        // name up itself and passed it in was the reason undo could report a restore it had
        // not performed without any test noticing.
        let undo = undo_of(&operation).expect("trashing is reversible");
        let mut restoration = plan(
            OperationId::new(),
            undo,
            &Survey::default(),
            Options::default(),
        );
        execute(&mut restoration, dir.path()).unwrap();
        assert_eq!(restoration.state, OperationState::Committed);
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
    }

    #[test]
    fn undoing_a_restore_puts_the_file_back_in_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, b"keep me").unwrap();

        let mut trashing = plan(
            OperationId::new(),
            OperationKind::Trash {
                paths: vec![path.clone()],
            },
            &survey(std::slice::from_ref(&path), None),
            Options::default(),
        );
        execute(&mut trashing, dir.path()).unwrap();

        let mut restoration = plan(
            OperationId::new(),
            undo_of(&trashing).expect("trashing is reversible"),
            &Survey::default(),
            Options::default(),
        );
        execute(&mut restoration, dir.path()).unwrap();
        assert!(path.exists());

        // The other direction, which had no coverage at all: undoing a restore has to re-trash
        // the file where it was put back, not at the trash path it has since left.
        let mut retrash = plan(
            OperationId::new(),
            undo_of(&restoration).expect("restoring is reversible"),
            &survey(std::slice::from_ref(&path), None),
            Options::default(),
        );
        execute(&mut retrash, dir.path()).unwrap();
        assert_eq!(retrash.state, OperationState::Committed);
        assert!(!path.exists(), "the file should be back in the trash");
        assert_eq!(
            trash::list(&trash::trash_dir_for(&path, &dir.path().join("data")).unwrap())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_conflicting_destination_fails_before_touching_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("from/x.txt");
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::create_dir_all(&destination_dir).unwrap();
        fs::write(&source, b"new").unwrap();
        fs::write(destination_dir.join("x.txt"), b"old").unwrap();

        let mut surveyed = survey(std::slice::from_ref(&source), Some(&destination_dir));
        surveyed.sources[0].same_device = false;
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options::default(),
        );
        assert!(execute(&mut operation, dir.path()).is_err());
        assert_eq!(operation.state, OperationState::Failed);
        assert!(source.exists(), "the source is untouched");
        assert_eq!(fs::read(destination_dir.join("x.txt")).unwrap(), b"old");

        // With Replace it goes through.
        let mut replacing = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec![source.clone()],
                destination_dir: destination_dir.clone(),
            },
            &surveyed,
            Options {
                conflict: ConflictPolicy::Replace,
                ..Options::default()
            },
        );
        execute(&mut replacing, dir.path()).unwrap();
        assert_eq!(fs::read(destination_dir.join("x.txt")).unwrap(), b"new");
    }

    #[test]
    fn the_reserve_refuses_an_operation_that_would_consume_it() {
        let dir = tempfile::tempdir().unwrap();
        let destination_dir = dir.path().join("to");
        fs::create_dir_all(&destination_dir).unwrap();

        // A plan that claims to write more than any volume holds.
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Copy {
                sources: vec![dir.path().join("huge")],
                destination_dir: destination_dir.clone(),
            },
            &Survey {
                sources: vec![SurveyedSource {
                    root: dir.path().join("huge"),
                    entries: vec![SurveyEntry {
                        path: dir.path().join("huge"),
                        relative: PathBuf::new(),
                        is_dir: false,
                        is_symlink: false,
                        bytes: u64::MAX / 2,
                    }],
                    same_device: true,
                    destination_exists: false,
                }],
                ..Survey::default()
            },
            Options::default(),
        );
        let configuration = ExecutorConfig {
            reserve_percent: 5,
            ..config(dir.path())
        };
        assert_eq!(
            check_reserve(&operation, &configuration).unwrap(),
            ReserveVerdict::Refuse
        );
        assert!(run(&mut operation, &configuration, &Cancel::new(), |_| {}).is_err());
        assert_eq!(operation.state, OperationState::Failed);
        assert!(
            operation
                .error
                .as_deref()
                .is_some_and(|error| error.contains("reserve"))
        );

        // The override is what the user's explicit "do it anyway" turns into.
        operation.options.override_reserve = true;
        operation.state = OperationState::Planned;
        assert_eq!(
            check_reserve(&operation, &configuration).unwrap(),
            ReserveVerdict::Ok
        );

        // Resuming counts only what is left: an operation that has already written almost
        // everything must not be refused for the bytes that are already on the volume.
        operation.options.override_reserve = false;
        operation.done_bytes = operation.total_bytes;
        assert_eq!(
            check_reserve(&operation, &configuration).unwrap(),
            ReserveVerdict::Ok
        );
    }
}
