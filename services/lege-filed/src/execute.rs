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
    let outstanding = operation
        .total_bytes
        .saturating_sub(operation.done_bytes);
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

        let step = operation.steps[operation.cursor].clone();
        let conflict = operation.options.conflict;
        if let Err(error) = apply(&step, conflict, config) {
            operation.state = OperationState::Failed;
            operation.error = Some(format!("{error:#}"));
            on_progress(operation);
            return Err(error);
        }

        operation.done_bytes = operation.done_bytes.saturating_add(step.bytes());
        if step.is_item_boundary() {
            operation.done_items = operation.done_items.saturating_add(1);
        }
        operation.cursor += 1;

        // Everything is written and verified; only source removal is left. Recording this is
        // what lets a crash here resume by deleting sources rather than copying again.
        if operation.state == OperationState::Running && remaining_is_only_source_removal(operation)
        {
            operation.state = OperationState::Verified;
        }

        if step.is_item_boundary() || last_report.elapsed() >= config.progress_interval {
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
fn apply(step: &Step, conflict: ConflictPolicy, config: &ExecutorConfig) -> Result<()> {
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

        Step::MoveToTrash { path } => {
            if path.symlink_metadata().is_err() {
                return Ok(()); // already trashed
            }
            let directory = trash::trash_dir_for(path, &config.data_home)?;
            trash::trash(path, &directory, &trash::now_local())
                .map(|_| ())
                .with_context(|| format!("moving {} to the trash", path.display()))
        }

        Step::RestoreFromTrash { trashed } => {
            if trashed.symlink_metadata().is_err() {
                return Ok(()); // already restored
            }
            let directory = trash::trash_dir_for(trashed, &config.data_home)?;
            trash::restore(trashed, &directory)
                .map(|_| ())
                .with_context(|| format!("restoring {}", trashed.display()))
        }

        Step::Unlink { path } => unlink(path),
    }
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
        for step in &operation.steps {
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

        let undo = undo_of(&operation).expect("trashing is reversible");
        let trashed: Vec<PathBuf> =
            trash::list(&trash::trash_dir_for(&path, &dir.path().join("data")).unwrap())
                .unwrap()
                .into_iter()
                .map(|item| item.trashed_path)
                .collect();
        assert_eq!(trashed.len(), 1);

        let mut restoration = plan(
            OperationId::new(),
            match undo {
                OperationKind::RestoreFromTrash { .. } => {
                    OperationKind::RestoreFromTrash { paths: trashed }
                }
                other => other,
            },
            &Survey::default(),
            Options::default(),
        );
        execute(&mut restoration, dir.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"keep me");
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
