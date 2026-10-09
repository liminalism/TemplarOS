//! File operations as transactions. A long copy or cross-volume move survives a file-manager
//! crash because `filed` — not the GUI — owns the journal.
//!
//! Robust move: copy to a temporary destination → flush → verify → atomically expose the
//! destination → delete the source only after destination completion → record enough to explain
//! or reverse it. Deleting is the one operation that cannot be reversed, and the UI is told so
//! before it runs.
//!
//! Planning is split in two on purpose. [`survey`] is the only part that touches the
//! filesystem: it walks the sources, totals them, notices which destinations already exist and
//! asks whether source and destination share a device. [`plan`] is pure, takes that survey and
//! produces the step list — so every conflict rule, every same-device fast path and every
//! ordering guarantee is unit-testable without a filesystem.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub Uuid);

impl OperationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Parse the hyphenated or compact form.
    pub fn parse(text: &str) -> Option<Self> {
        Uuid::parse_str(text).ok().map(Self)
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

pub use templar_client::files::TransferMapping;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum OperationKind {
    Copy {
        #[serde(with = "templar_types::native_path::many")]
        sources: Vec<PathBuf>,
        #[serde(with = "templar_types::native_path")]
        destination_dir: PathBuf,
    },
    Move {
        #[serde(with = "templar_types::native_path::many")]
        sources: Vec<PathBuf>,
        #[serde(with = "templar_types::native_path")]
        destination_dir: PathBuf,
    },
    /// Exact move inverse, including original directories and KeepBoth filenames.
    RestoreMoveV1 { entries: Vec<TransferMapping> },
    /// Guarded inverse references the original persisted destination snapshots.
    UndoCopyV1 {
        original_operation: OperationId,
        #[serde(default, with = "templar_types::native_path::many")]
        roots: Vec<PathBuf>,
    },
    Rename {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
        new_name: String,
    },
    Trash {
        #[serde(with = "templar_types::native_path::many")]
        paths: Vec<PathBuf>,
    },
    /// Put trashed items back where they came from (the undo of [`OperationKind::Trash`]).
    RestoreFromTrash {
        #[serde(with = "templar_types::native_path::many")]
        paths: Vec<PathBuf>,
    },
    /// Irreversible; the UI must say so before running it.
    Delete {
        #[serde(with = "templar_types::native_path::many")]
        paths: Vec<PathBuf>,
    },
}

impl OperationKind {
    /// Whether the operation can be undone once committed.
    pub fn reversible(&self) -> bool {
        !matches!(self, Self::Delete { .. })
    }

    /// One line for the operations drawer and the history event.
    pub fn summary(&self) -> String {
        fn names(paths: &[PathBuf]) -> String {
            match paths {
                [] => "nothing".to_owned(),
                [one] => display_name(one),
                many => format!("{} items", many.len()),
            }
        }
        match self {
            Self::Copy {
                sources,
                destination_dir,
            } => format!("Copy {} to {}", names(sources), destination_dir.display()),
            Self::Move {
                sources,
                destination_dir,
            } => format!("Move {} to {}", names(sources), destination_dir.display()),
            Self::RestoreMoveV1 { entries } => format!(
                "Restore {} moved items to their original locations",
                entries.len()
            ),
            Self::UndoCopyV1 { .. } => "Undo copy (move created files to Trash)".to_owned(),
            Self::Rename { path, new_name } => {
                format!("Rename {} to {new_name}", display_name(path))
            }
            Self::Trash { paths } => format!("Move {} to Trash", names(paths)),
            Self::RestoreFromTrash { paths } => format!("Restore {} from Trash", names(paths)),
            Self::Delete { paths } => format!("Delete {} permanently", names(paths)),
        }
    }
}

/// Refuse malformed or dangerous requests before they enter the durable journal.
///
/// The service is the policy boundary: a correct GUI validates early for good feedback, but any
/// process on the session bus can call Operations1 directly. In particular, a rename is one
/// filename component, roots cannot be removed, and a directory cannot be copied into itself.
pub fn validate(kind: &OperationKind) -> Result<()> {
    match kind {
        OperationKind::Copy {
            sources,
            destination_dir,
        }
        | OperationKind::Move {
            sources,
            destination_dir,
        } => {
            validate_transfer(sources, destination_dir)?;
        }
        OperationKind::RestoreMoveV1 { entries } => {
            if entries.is_empty() {
                bail!("at least one restore mapping is required");
            }
            let mut sources = BTreeSet::new();
            let mut destinations = BTreeSet::new();
            for entry in entries {
                require_existing_removable(&entry.source, "restore source")?;
                require_removable_path(&entry.destination, "restore destination")?;
                if !sources.insert(&entry.source) || !destinations.insert(&entry.destination) {
                    bail!("duplicate restore mapping");
                }
                let parent = entry
                    .destination
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("restore destination has no parent"))?;
                if !parent.is_dir() {
                    bail!("restore destination parent is unavailable");
                }
                if entry.destination.symlink_metadata().is_ok() {
                    bail!(
                        "restore destination already exists; undo would overwrite it: {}",
                        entry.destination.display()
                    );
                }
                let source =
                    std::fs::canonicalize(&entry.source).unwrap_or_else(|_| entry.source.clone());
                let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
                if entry.source.symlink_metadata()?.is_dir() && parent.starts_with(&source) {
                    bail!("restore destination is inside its source");
                }
            }
        }
        OperationKind::UndoCopyV1 { .. } => {
            // FileService resolves and validates the original journal record, not caller roots.
        }
        OperationKind::Rename { path, new_name } => {
            require_existing_removable(path, "rename source")?;
            if !valid_name(new_name) {
                bail!("new name must be one filename component");
            }
            if path
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new(new_name))
            {
                bail!("new name is unchanged");
            }
        }
        OperationKind::Trash { paths } | OperationKind::Delete { paths } => {
            require_nonempty(paths, "path")?;
            let mut unique = BTreeSet::new();
            for path in paths {
                require_existing_removable(path, "path")?;
                if !unique.insert(path) {
                    bail!("duplicate path: {}", path.display());
                }
            }
        }
        OperationKind::RestoreFromTrash { paths } => {
            // These are the original paths recorded by the trash journal and are normally absent.
            require_nonempty(paths, "path")?;
            for path in paths {
                require_removable_path(path, "restore path")?;
            }
        }
    }
    Ok(())
}

fn validate_transfer(sources: &[PathBuf], destination_dir: &Path) -> Result<()> {
    require_nonempty(sources, "source")?;
    require_absolute(destination_dir, "destination directory")?;
    if !destination_dir.is_dir() {
        bail!(
            "destination is not a directory: {}",
            destination_dir.display()
        );
    }
    let destination =
        std::fs::canonicalize(destination_dir).unwrap_or_else(|_| destination_dir.to_path_buf());
    let mut unique = BTreeSet::new();
    for source in sources {
        require_existing_removable(source, "source")?;
        if !unique.insert(source) {
            bail!("duplicate source: {}", source.display());
        }
        if source.symlink_metadata()?.is_dir() {
            let source = std::fs::canonicalize(source).unwrap_or_else(|_| source.clone());
            if destination.starts_with(&source) {
                bail!(
                    "destination {} is inside source directory {}",
                    destination_dir.display(),
                    source.display()
                );
            }
        }
    }
    Ok(())
}

fn require_nonempty(paths: &[PathBuf], noun: &str) -> Result<()> {
    if paths.is_empty() {
        bail!("at least one {noun} is required");
    }
    Ok(())
}

fn require_absolute(path: &Path, noun: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{noun} must be absolute: {}", path.display());
    }
    Ok(())
}

fn require_removable_path(path: &Path, noun: &str) -> Result<()> {
    require_absolute(path, noun)?;
    if path.parent().is_none() {
        bail!("refusing to operate on filesystem root: {}", path.display());
    }
    Ok(())
}

fn require_existing_removable(path: &Path, noun: &str) -> Result<()> {
    require_removable_path(path, noun)?;
    path.symlink_metadata()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("{noun} {} is unavailable: {error}", path.display()))
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(component)) if !component.as_bytes().is_empty())
        && components.next().is_none()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationState {
    Planned,
    Running,
    /// Every destination written and verified; sources not yet removed (moves).
    Verified,
    Committed,
    Failed,
    Cancelled,
    Undone,
}

impl OperationState {
    /// Whether the operation has stopped for good.
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Failed | Self::Cancelled | Self::Undone
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Verified => "verified",
            Self::Committed => "committed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Undone => "undone",
        }
    }
}

/// What to do when a destination name is already taken.
///
/// The decision is made once, at plan time, so the journal records what was agreed rather than
/// re-asking after a crash.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Stop and report; the caller re-plans with a decision. The safe default.
    #[default]
    Fail,
    /// Leave the existing file alone and drop that source from the plan.
    Skip,
    /// Overwrite, atomically — the temporary file is renamed over the destination.
    Replace,
    /// Write alongside it as `name (2).ext`.
    KeepBoth,
}

/// How much a copy is checked before the source is allowed to disappear.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    /// Re-`stat` the destination and require the byte count to match. Cheap; catches a short
    /// write, a full disk and a truncated copy.
    #[default]
    Size,
    /// Additionally read the destination back and compare a CRC-32 of its contents with one
    /// computed while writing. Doubles read I/O; catches silent corruption in the write path,
    /// not deliberate tampering.
    Checksum,
}

/// Knobs a caller may set per operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Options {
    #[serde(default)]
    pub conflict: ConflictPolicy,
    #[serde(default)]
    pub verify: Verification,
    /// Consume the storage reserve anyway. Refused writes are the point of the reserve, so this
    /// is an explicit, recorded override rather than a default.
    #[serde(default)]
    pub override_reserve: bool,
}

/// One journalled step. Steps are idempotent: re-running a completed step is a no-op, which is
/// what makes resume-after-crash and undo safe.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "step")]
pub enum Step {
    CreateDirectory {
        #[serde(with = "templar_types::native_path")]
        destination: PathBuf,
    },
    /// A transfer root must not merge into an existing user directory on Fail/KeepBoth.
    CreateDirectoryExclusive {
        #[serde(with = "templar_types::native_path")]
        destination: PathBuf,
        #[serde(default)]
        created_identity: Option<(u64, u64)>,
    },
    VerifyCopyUndoV1 {
        original_operation: OperationId,
    },
    CopyToTemporary {
        #[serde(with = "templar_types::native_path")]
        source: PathBuf,
        #[serde(with = "templar_types::native_path")]
        temporary: PathBuf,
        bytes: u64,
    },
    CopySymlink {
        #[serde(with = "templar_types::native_path")]
        source: PathBuf,
        #[serde(with = "templar_types::native_path")]
        destination: PathBuf,
        #[serde(default)]
        created_identity: Option<(u64, u64)>,
    },
    FlushTemporary {
        #[serde(with = "templar_types::native_path")]
        temporary: PathBuf,
    },
    VerifyCopy {
        #[serde(with = "templar_types::native_path")]
        source: PathBuf,
        #[serde(with = "templar_types::native_path")]
        temporary: PathBuf,
        verify: Verification,
    },
    ExposeAtomically {
        #[serde(with = "templar_types::native_path")]
        temporary: PathBuf,
        #[serde(with = "templar_types::native_path")]
        destination: PathBuf,
    },
    /// Same-device move: one atomic `rename(2)`, no copy at all.
    RenameInPlace {
        #[serde(with = "templar_types::native_path")]
        from: PathBuf,
        #[serde(with = "templar_types::native_path")]
        to: PathBuf,
    },
    DeleteSource {
        #[serde(with = "templar_types::native_path")]
        source: PathBuf,
    },
    /// Only ever emitted for directories the operation itself emptied.
    RemoveDirectory {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
    },
    MoveToTrash {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
        /// Where the trash actually put it, filled in by the executor.
        ///
        /// The name is only known once the step runs: `trash::free_name` resolves collisions
        /// against the live directory, so the same path trashed twice lands under two different
        /// names. Undo reads this back rather than guessing, exactly as `Move`'s undo reads
        /// `RenameInPlace { to }`.
        #[serde(default, with = "templar_types::native_path::optional")]
        trashed: Option<PathBuf>,
    },
    RestoreFromTrash {
        #[serde(with = "templar_types::native_path")]
        trashed: PathBuf,
        /// Where it went back to, filled in by the executor, so undoing a restore re-trashes
        /// the file at its restored location rather than at the trash path it has left.
        #[serde(default, with = "templar_types::native_path::optional")]
        restored_to: Option<PathBuf>,
    },
    Unlink {
        #[serde(with = "templar_types::native_path")]
        path: PathBuf,
    },
}

impl Step {
    /// Bytes this step moves, for progress reporting.
    pub fn bytes(&self) -> u64 {
        match self {
            Self::CopyToTemporary { bytes, .. } => *bytes,
            _ => 0,
        }
    }

    /// Whether the step counts as one completed item in the progress readout.
    pub fn is_item_boundary(&self) -> bool {
        matches!(
            self,
            Self::ExposeAtomically { .. }
                | Self::RenameInPlace { .. }
                | Self::MoveToTrash { .. }
                | Self::RestoreFromTrash { .. }
                | Self::Unlink { .. }
                | Self::CopySymlink { .. }
        )
    }
}

/// Reports one committed step to the shared file index, the instant it commits.
///
/// `CopyToTemporary`/`FlushTemporary`/`VerifyCopy` report nothing: a temporary the operation has
/// not yet exposed is policy-skipped noise, never a row. `MoveToTrash` reports a removal — trash
/// is outside the indexed policy — and `RestoreFromTrash` reports the restored path as new only
/// when the executor filled it in (it always does once the rename itself has happened).
pub fn notify_index(sink: &dyn crate::execute::IndexSink, step: &Step) {
    match step {
        Step::CreateDirectory { destination }
        | Step::CreateDirectoryExclusive { destination, .. }
        | Step::ExposeAtomically { destination, .. }
        | Step::CopySymlink { destination, .. } => sink.created(destination),
        Step::RenameInPlace { from, to } => sink.moved(from, to),
        Step::DeleteSource { source } => sink.removed(source),
        Step::Unlink { path } | Step::RemoveDirectory { path } | Step::MoveToTrash { path, .. } => {
            sink.removed(path);
        }
        Step::RestoreFromTrash { restored_to, .. } => {
            if let Some(restored_to) = restored_to {
                sink.created(restored_to);
            }
        }
        Step::CopyToTemporary { .. }
        | Step::FlushTemporary { .. }
        | Step::VerifyCopy { .. }
        | Step::VerifyCopyUndoV1 { .. } => {}
    }
}

/// A journalled operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId,
    pub kind: OperationKind,
    pub state: OperationState,
    pub options: Options,
    pub steps: Vec<Step>,
    /// Index of the next step to run (the resume point after a crash).
    pub cursor: usize,
    /// Totals from the survey, for progress without re-walking the tree.
    pub total_bytes: u64,
    pub total_items: u64,
    pub done_bytes: u64,
    pub done_items: u64,
    /// Sources dropped by [`ConflictPolicy::Skip`], so the UI can say what it did not do.
    #[serde(default, with = "templar_types::native_path::many")]
    pub skipped: Vec<PathBuf>,
    /// Planned root mapping, journalled before execution rather than guessed from display names.
    #[serde(default)]
    pub transfers: Vec<TransferMapping>,
    /// Metadata keyed by existing destination step indexes; no duplicate tree path manifest.
    #[serde(default)]
    pub created: std::collections::BTreeMap<usize, crate::transfer_undo::CreatedMetadata>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Operation {
    pub fn progress(&self) -> f64 {
        if self.total_bytes > 0 {
            self.done_bytes as f64 / self.total_bytes as f64
        } else if self.total_items > 0 {
            self.done_items as f64 / self.total_items as f64
        } else {
            f64::from(u8::from(self.state.terminal()))
        }
    }
}

/// One thing the survey found under a source.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurveyEntry {
    /// Absolute path of the item.
    #[serde(with = "templar_types::native_path")]
    pub path: PathBuf,
    /// Path relative to the source root, so a destination can be derived without more I/O.
    #[serde(with = "templar_types::native_path")]
    pub relative: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub bytes: u64,
}

/// What one source expanded to.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SurveyedSource {
    #[serde(with = "templar_types::native_path")]
    pub root: PathBuf,
    /// Depth-first, parents before children, so directories exist before their contents.
    pub entries: Vec<SurveyEntry>,
    /// True when the destination directory is on the same device as this source, which makes a
    /// move an atomic rename rather than a copy-and-delete.
    pub same_device: bool,
    /// The destination name is already taken.
    pub destination_exists: bool,
}

impl SurveyedSource {
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.bytes).sum()
    }

    pub fn items(&self) -> u64 {
        self.entries.iter().filter(|entry| !entry.is_dir).count() as u64
    }
}

/// The filesystem facts [`plan`] needs, gathered once.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Survey {
    pub sources: Vec<SurveyedSource>,
    /// Names already present in the destination directory, so `KeepBoth` can pick a free one
    /// without racing every candidate against the filesystem.
    #[serde(default, with = "templar_types::native_path::names")]
    pub destination_names: BTreeSet<OsString>,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl Survey {
    pub fn total_bytes(&self) -> u64 {
        self.sources.iter().map(SurveyedSource::bytes).sum()
    }

    pub fn total_items(&self) -> u64 {
        self.sources.iter().map(SurveyedSource::items).sum()
    }
}

/// Walk `sources` and inspect `destination_dir`. The only part of planning that does I/O.
///
/// Symbolic links are recorded but never followed: copying a tree must not silently duplicate
/// whatever a link happens to point at, nor loop.
pub fn survey(sources: &[PathBuf], destination_dir: Option<&Path>) -> Survey {
    let destination_device = destination_dir.and_then(device_of);
    let destination_names = destination_dir
        .and_then(|dir| std::fs::read_dir(dir).ok())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name())
                .collect()
        })
        .unwrap_or_default();

    let mut survey = Survey {
        destination_names,
        ..Survey::default()
    };

    for root in sources {
        let mut entries = Vec::new();
        if let Err(error) = walk(root, Path::new(""), &mut entries) {
            survey.errors.push(format!("{}: {error}", root.display()));
            continue;
        }
        let same_device = match (
            root.symlink_metadata().ok().map(|metadata| {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            }),
            destination_device,
        ) {
            (Some(source), Some(destination)) => source == destination,
            _ => false,
        };
        let destination_exists = destination_dir.is_some_and(|dir| {
            root.file_name().is_some_and(|name| {
                survey.destination_names.contains(name) || dir.join(name).symlink_metadata().is_ok()
            })
        });
        survey.sources.push(SurveyedSource {
            root: root.clone(),
            entries,
            same_device,
            destination_exists,
        });
    }
    survey
}

fn walk(path: &Path, relative: &Path, out: &mut Vec<SurveyEntry>) -> std::io::Result<()> {
    let metadata = path.symlink_metadata()?;
    let is_symlink = metadata.file_type().is_symlink();
    let is_dir = !is_symlink && metadata.is_dir();
    out.push(SurveyEntry {
        path: path.to_path_buf(),
        relative: relative.to_path_buf(),
        is_dir,
        is_symlink,
        bytes: if is_dir || is_symlink {
            0
        } else {
            metadata.len()
        },
    });
    if !is_dir {
        return Ok(());
    }
    let mut children: Vec<PathBuf> = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    children.sort();
    for child in children {
        let name = child.file_name().unwrap_or_default().to_owned();
        walk(&child, &relative.join(name), out)?;
    }
    Ok(())
}

fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|meta| meta.dev())
}

/// Build the step list for `kind`. Pure: the [`Survey`] holds every filesystem fact it needs.
pub fn plan(id: OperationId, kind: OperationKind, survey: &Survey, options: Options) -> Operation {
    let mut steps = Vec::new();
    let mut skipped = Vec::new();
    let mut transfers = Vec::new();
    let mut taken = survey.destination_names.clone();

    match &kind {
        OperationKind::Copy {
            destination_dir, ..
        }
        | OperationKind::Move {
            destination_dir, ..
        } => plan_transfer(
            id,
            matches!(kind, OperationKind::Move { .. }),
            destination_dir,
            survey,
            options,
            &mut taken,
            &mut steps,
            &mut skipped,
            &mut transfers,
        ),
        OperationKind::RestoreMoveV1 { entries } => {
            for entry in entries {
                if let Some(source) = survey
                    .sources
                    .iter()
                    .find(|source| source.root == entry.source)
                {
                    transfers.push(entry.clone());
                    plan_source_transfer(id, true, &entry.destination, source, options, &mut steps);
                }
            }
        }
        OperationKind::UndoCopyV1 {
            original_operation,
            roots,
        } => {
            steps.push(Step::VerifyCopyUndoV1 {
                original_operation: *original_operation,
            });
            steps.extend(roots.iter().map(|path| Step::MoveToTrash {
                path: path.clone(),
                trashed: None,
            }));
        }
        OperationKind::Rename { path, new_name } => steps.push(Step::RenameInPlace {
            from: path.clone(),
            to: path.with_file_name(new_name),
        }),
        OperationKind::Trash { paths } => {
            steps.extend(paths.iter().map(|path| Step::MoveToTrash {
                path: path.clone(),
                trashed: None,
            }));
        }
        OperationKind::RestoreFromTrash { paths } => {
            steps.extend(paths.iter().map(|path| Step::RestoreFromTrash {
                trashed: path.clone(),
                restored_to: None,
            }));
        }
        OperationKind::Delete { paths } => {
            // Depth-first removal: contents before the directory that holds them.
            for source in &survey.sources {
                for entry in source.entries.iter().rev() {
                    if entry.is_dir {
                        steps.push(Step::RemoveDirectory {
                            path: entry.path.clone(),
                        });
                    } else {
                        steps.push(Step::Unlink {
                            path: entry.path.clone(),
                        });
                    }
                }
            }
            // A caller that did not survey (paths already known gone) still gets a plan.
            if survey.sources.is_empty() {
                steps.extend(paths.iter().map(|path| Step::Unlink { path: path.clone() }));
            }
        }
    }

    let total_items = steps.iter().filter(|step| step.is_item_boundary()).count() as u64;
    Operation {
        id,
        kind,
        state: OperationState::Planned,
        options,
        total_bytes: steps.iter().map(Step::bytes).sum(),
        total_items,
        done_bytes: 0,
        done_items: 0,
        steps,
        cursor: 0,
        skipped,
        transfers,
        created: std::collections::BTreeMap::default(),
        error: None,
    }
}

/// Steps for a copy or a move: the same walk, with source removal appended for a move.
#[allow(clippy::too_many_arguments)]
fn plan_transfer(
    id: OperationId,
    is_move: bool,
    destination_dir: &Path,
    survey: &Survey,
    options: Options,
    taken: &mut BTreeSet<OsString>,
    steps: &mut Vec<Step>,
    skipped: &mut Vec<PathBuf>,
    transfers: &mut Vec<TransferMapping>,
) {
    for source in &survey.sources {
        let Some(name) = resolve_name(source, taken, options.conflict) else {
            skipped.push(source.root.clone());
            continue;
        };
        let destination_root = destination_dir.join(&name);
        transfers.push(TransferMapping {
            source: source.root.clone(),
            destination: destination_root.clone(),
        });
        plan_source_transfer(id, is_move, &destination_root, source, options, steps);
    }
}

fn plan_source_transfer(
    id: OperationId,
    is_move: bool,
    destination_root: &Path,
    source: &SurveyedSource,
    options: Options,
    steps: &mut Vec<Step>,
) {
    // Same-device move of a whole tree: one rename, no traversal, no copy. That is the
    // difference between "instant" and "twenty minutes" on a large directory, and it is
    // atomic, so a crash cannot leave a half-move behind.
    if is_move && source.same_device && !source.destination_exists {
        steps.push(Step::RenameInPlace {
            from: source.root.clone(),
            to: destination_root.to_path_buf(),
        });
        return;
    }

    for entry in &source.entries {
        let destination = if entry.relative.as_os_str().is_empty() {
            destination_root.to_path_buf()
        } else {
            destination_root.join(&entry.relative)
        };
        if entry.is_dir {
            if options.conflict == ConflictPolicy::Replace {
                steps.push(Step::CreateDirectory { destination });
            } else {
                steps.push(Step::CreateDirectoryExclusive {
                    destination,
                    created_identity: None,
                });
            }
        } else if entry.is_symlink {
            steps.push(Step::CopySymlink {
                source: entry.path.clone(),
                destination,
                created_identity: None,
            });
        } else {
            let temporary = temporary_for(&destination, id);
            steps.push(Step::CopyToTemporary {
                source: entry.path.clone(),
                temporary: temporary.clone(),
                bytes: entry.bytes,
            });
            steps.push(Step::FlushTemporary {
                temporary: temporary.clone(),
            });
            steps.push(Step::VerifyCopy {
                source: entry.path.clone(),
                temporary: temporary.clone(),
                verify: options.verify,
            });
            steps.push(Step::ExposeAtomically {
                temporary,
                destination,
            });
        }
    }

    // Sources disappear only after every destination is exposed — children first, so a
    // directory is empty by the time it is removed.
    if is_move {
        for entry in source.entries.iter().rev() {
            if entry.is_dir {
                steps.push(Step::RemoveDirectory {
                    path: entry.path.clone(),
                });
            } else {
                steps.push(Step::DeleteSource {
                    source: entry.path.clone(),
                });
            }
        }
    }
}

/// The destination name to use, honouring the conflict policy. `None` means skip this source.
fn resolve_name(
    source: &SurveyedSource,
    taken: &mut BTreeSet<OsString>,
    policy: ConflictPolicy,
) -> Option<OsString> {
    let name = source.root.file_name()?.to_owned();
    if !source.destination_exists && !taken.contains(&name) {
        taken.insert(name.clone());
        return Some(name);
    }
    match policy {
        ConflictPolicy::Skip => None,
        // `Fail` still produces a plan; the executor refuses when the destination is there, so
        // the caller gets one clear error instead of a half-finished copy.
        ConflictPolicy::Fail | ConflictPolicy::Replace => Some(name),
        ConflictPolicy::KeepBoth => {
            let free = free_name(&name, taken);
            taken.insert(free.clone());
            Some(free)
        }
    }
}

/// `report.pdf` → `report (2).pdf`, `report (3).pdf`, … The suffix goes before the extension so
/// the file still opens with the right application.
fn free_name(name: &OsStr, taken: &BTreeSet<OsString>) -> OsString {
    let bytes = name.as_bytes();
    let split = bytes
        .iter()
        .rposition(|byte| *byte == b'.')
        .filter(|index| *index > 0);
    let (stem, extension) =
        split.map_or((bytes, &[][..]), |index| (&bytes[..index], &bytes[index..]));
    let candidate = |suffix: &str| {
        let mut name = stem.to_vec();
        name.extend_from_slice(suffix.as_bytes());
        name.extend_from_slice(extension);
        OsString::from_vec(name)
    };
    (2..1_000)
        .map(|index| candidate(&format!(" ({index})")))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| candidate(&format!(" ({})", Uuid::new_v4().simple())))
}

/// Hidden, operation-scoped, and in the destination directory so the final rename never crosses
/// a filesystem boundary (a cross-device `rename(2)` fails, and a copy-then-unlink is not
/// atomic).
fn temporary_for(destination: &Path, id: OperationId) -> PathBuf {
    let parent = destination.parent().unwrap_or(Path::new("."));
    // Native names can be both non-UTF-8 and NAME_MAX bytes long. The temporary must neither
    // replace bytes nor prepend enough text to make an otherwise valid filename impossible.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in destination.file_name().unwrap_or_default().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    parent.join(format!(".templaros-{}-{hash:016x}.part", id.0.simple()))
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "unnamed".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The operation that reverses `operation`, or `None` when it cannot be reversed.
///
/// Undo is planned from the *committed* operation rather than recomputed later, so it stays
/// correct even if the user has since created something with the same name.
pub fn undo_of(operation: &Operation) -> Option<OperationKind> {
    if operation.state != OperationState::Committed {
        return None;
    }
    match &operation.kind {
        OperationKind::Move { .. } | OperationKind::RestoreMoveV1 { .. } => {
            if operation.options.conflict == ConflictPolicy::Replace
                || operation.transfers.is_empty()
            {
                return None;
            }
            Some(OperationKind::RestoreMoveV1 {
                entries: operation
                    .transfers
                    .iter()
                    .map(|entry| TransferMapping {
                        source: entry.destination.clone(),
                        destination: entry.source.clone(),
                    })
                    .collect(),
            })
        }
        OperationKind::Rename { path, .. } => {
            if operation.options.conflict == ConflictPolicy::Replace {
                return None;
            }
            operation.steps.iter().find_map(|step| match step {
                Step::RenameInPlace { to, .. } => Some(OperationKind::RestoreMoveV1 {
                    entries: vec![TransferMapping {
                        source: to.clone(),
                        destination: path.clone(),
                    }],
                }),
                _ => None,
            })
        }
        // Read back where the trash actually put each item, never the paths the operation was
        // asked for: those name the place the file has just left, and a restore aimed at them
        // finds nothing there, decides it has already run, and commits without moving anything.
        OperationKind::Trash { .. } | OperationKind::UndoCopyV1 { .. } => {
            let trashed: Vec<PathBuf> = operation
                .steps
                .iter()
                .filter_map(|step| match step {
                    Step::MoveToTrash { trashed, .. } => trashed.clone(),
                    _ => None,
                })
                .collect();
            (!trashed.is_empty()).then_some(OperationKind::RestoreFromTrash { paths: trashed })
        }
        OperationKind::RestoreFromTrash { .. } => {
            let restored: Vec<PathBuf> = operation
                .steps
                .iter()
                .filter_map(|step| match step {
                    Step::RestoreFromTrash { restored_to, .. } => restored_to.clone(),
                    _ => None,
                })
                .collect();
            (!restored.is_empty()).then_some(OperationKind::Trash { paths: restored })
        }
        OperationKind::Copy { .. } => {
            if operation.options.conflict == ConflictPolicy::Replace
                || operation.transfers.is_empty()
                || operation.created.is_empty()
            {
                return None;
            }
            Some(OperationKind::UndoCopyV1 {
                original_operation: operation.id,
                roots: Vec::new(),
            })
        }
        OperationKind::Delete { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_survey(root: &str, bytes: u64, same_device: bool, exists: bool) -> SurveyedSource {
        SurveyedSource {
            root: PathBuf::from(root),
            entries: vec![SurveyEntry {
                path: PathBuf::from(root),
                relative: PathBuf::new(),
                is_dir: false,
                is_symlink: false,
                bytes,
            }],
            same_device,
            destination_exists: exists,
        }
    }

    fn names(operation: &Operation) -> Vec<&'static str> {
        operation
            .steps
            .iter()
            .map(|step| match step {
                Step::CreateDirectory { .. } | Step::CreateDirectoryExclusive { .. } => "mkdir",
                Step::CopyToTemporary { .. } => "copy",
                Step::CopySymlink { .. } => "symlink",
                Step::FlushTemporary { .. } => "flush",
                Step::VerifyCopy { .. } => "verify",
                Step::ExposeAtomically { .. } => "expose",
                Step::RenameInPlace { .. } => "rename",
                Step::DeleteSource { .. } => "delete-source",
                Step::RemoveDirectory { .. } => "rmdir",
                Step::MoveToTrash { .. } => "trash",
                Step::RestoreFromTrash { .. } => "restore",
                Step::Unlink { .. } => "unlink",
                Step::VerifyCopyUndoV1 { .. } => "verify-copy-undo",
            })
            .collect()
    }

    #[test]
    fn a_cross_device_move_deletes_the_source_only_after_the_destination_is_exposed() {
        let survey = Survey {
            sources: vec![file_survey("/a/report.pdf", 10, false, false)],
            ..Survey::default()
        };
        let operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec!["/a/report.pdf".into()],
                destination_dir: "/b".into(),
            },
            &survey,
            Options::default(),
        );
        assert_eq!(
            names(&operation),
            ["copy", "flush", "verify", "expose", "delete-source"]
        );
        assert_eq!(operation.total_bytes, 10);
        assert_eq!(operation.total_items, 1);
        assert!(operation.kind.reversible());
    }

    #[test]
    fn operation_validation_rejects_traversal_roots_and_self_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let child = source.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let file = source.join("one.txt");
        std::fs::write(&file, b"one").unwrap();

        let traversal = OperationKind::Rename {
            path: file.clone(),
            new_name: "../escape".into(),
        };
        assert!(
            validate(&traversal)
                .unwrap_err()
                .to_string()
                .contains("component")
        );

        let recursive = OperationKind::Copy {
            sources: vec![source],
            destination_dir: child,
        };
        assert!(
            validate(&recursive)
                .unwrap_err()
                .to_string()
                .contains("inside source")
        );

        let root = OperationKind::Delete {
            paths: vec![PathBuf::from("/")],
        };
        assert!(
            validate(&root)
                .unwrap_err()
                .to_string()
                .contains("filesystem root")
        );
    }

    #[test]
    fn operation_validation_accepts_normal_copy_rename_and_trash_requests() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("one.txt");
        let destination = temp.path().join("destination");
        std::fs::write(&source, b"one").unwrap();
        std::fs::create_dir(&destination).unwrap();
        assert!(
            validate(&OperationKind::Copy {
                sources: vec![source.clone()],
                destination_dir: destination,
            })
            .is_ok()
        );
        assert!(
            validate(&OperationKind::Rename {
                path: source.clone(),
                new_name: "two.txt".into(),
            })
            .is_ok()
        );
        assert!(
            validate(&OperationKind::Trash {
                paths: vec![source]
            })
            .is_ok()
        );
    }

    #[test]
    fn a_same_device_move_is_one_atomic_rename() {
        let survey = Survey {
            sources: vec![file_survey("/a/report.pdf", 10, true, false)],
            ..Survey::default()
        };
        let operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec!["/a/report.pdf".into()],
                destination_dir: "/a/sub".into(),
            },
            &survey,
            Options::default(),
        );
        assert_eq!(names(&operation), ["rename"]);
    }

    #[test]
    fn a_copied_tree_creates_parents_before_children_and_removes_them_last() {
        let survey = Survey {
            sources: vec![SurveyedSource {
                root: "/a/dir".into(),
                entries: vec![
                    SurveyEntry {
                        path: "/a/dir".into(),
                        relative: PathBuf::new(),
                        is_dir: true,
                        is_symlink: false,
                        bytes: 0,
                    },
                    SurveyEntry {
                        path: "/a/dir/f".into(),
                        relative: "f".into(),
                        is_dir: false,
                        is_symlink: false,
                        bytes: 4,
                    },
                ],
                same_device: false,
                destination_exists: false,
            }],
            ..Survey::default()
        };
        let operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec!["/a/dir".into()],
                destination_dir: "/b".into(),
            },
            &survey,
            Options::default(),
        );
        assert_eq!(
            names(&operation),
            [
                "mkdir",
                "copy",
                "flush",
                "verify",
                "expose",
                "delete-source",
                "rmdir"
            ]
        );
        assert!(matches!(
            &operation.steps[0],
            Step::CreateDirectoryExclusive { destination, .. } if destination == Path::new("/b/dir")
        ));
    }

    #[test]
    fn conflict_policies() {
        let sources = vec!["/a/report.pdf".into()];
        let survey = Survey {
            sources: vec![file_survey("/a/report.pdf", 1, false, true)],
            destination_names: [OsString::from("report.pdf")].into(),
            ..Survey::default()
        };
        let copy = |conflict| {
            plan(
                OperationId::new(),
                OperationKind::Copy {
                    sources: sources.clone(),
                    destination_dir: "/b".into(),
                },
                &survey,
                Options {
                    conflict,
                    ..Options::default()
                },
            )
        };

        let skipped = copy(ConflictPolicy::Skip);
        assert!(skipped.steps.is_empty());
        assert_eq!(skipped.skipped, vec![PathBuf::from("/a/report.pdf")]);

        let replaced = copy(ConflictPolicy::Replace);
        assert!(matches!(
            replaced.steps.last(),
            Some(Step::ExposeAtomically { destination, .. }) if destination == Path::new("/b/report.pdf")
        ));

        let both = copy(ConflictPolicy::KeepBoth);
        assert!(matches!(
            both.steps.last(),
            Some(Step::ExposeAtomically { destination, .. })
                if destination == Path::new("/b/report (2).pdf")
        ));
    }

    #[test]
    fn keep_both_skips_names_already_taken() {
        let mut taken: BTreeSet<OsString> = [
            OsString::from("report (2).pdf"),
            OsString::from("report (3).pdf"),
        ]
        .into();
        assert_eq!(
            free_name(OsStr::new("report.pdf"), &taken),
            OsString::from("report (4).pdf")
        );
        taken.clear();
        assert_eq!(
            free_name(OsStr::new("archive"), &taken),
            OsString::from("archive (2)")
        );
        assert_eq!(
            free_name(OsStr::new(".hidden"), &taken),
            OsString::from(".hidden (2)")
        );
    }

    #[test]
    fn delete_is_irreversible_and_rename_is_one_step() {
        assert!(!OperationKind::Delete { paths: vec![] }.reversible());
        let operation = plan(
            OperationId::new(),
            OperationKind::Rename {
                path: "/a/x.txt".into(),
                new_name: "y.txt".into(),
            },
            &Survey::default(),
            Options::default(),
        );
        assert_eq!(
            operation.steps,
            vec![Step::RenameInPlace {
                from: "/a/x.txt".into(),
                to: "/a/y.txt".into()
            }]
        );
    }

    #[test]
    fn undo_moves_back_and_refuses_a_delete() {
        let survey = Survey {
            sources: vec![file_survey("/a/report.pdf", 1, true, false)],
            ..Survey::default()
        };
        let mut operation = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec!["/a/report.pdf".into()],
                destination_dir: "/b".into(),
            },
            &survey,
            Options::default(),
        );
        assert_eq!(undo_of(&operation), None, "an uncommitted move has no undo");
        operation.state = OperationState::Committed;
        let inverse = undo_of(&operation).unwrap();
        let OperationKind::RestoreMoveV1 { entries } = inverse else {
            panic!("missing exact move inverse");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, Path::new("/b/report.pdf"));
        assert_eq!(entries[0].destination, Path::new("/a/report.pdf"));

        let mut deletion = plan(
            OperationId::new(),
            OperationKind::Delete {
                paths: vec!["/a/x".into()],
            },
            &Survey::default(),
            Options::default(),
        );
        deletion.state = OperationState::Committed;
        assert_eq!(undo_of(&deletion), None);
    }

    #[test]
    fn the_temporary_file_lives_in_the_destination_directory() {
        let temporary = temporary_for(Path::new("/b/sub/report.pdf"), OperationId::new());
        assert_eq!(temporary.parent(), Some(Path::new("/b/sub")));
        assert!(
            temporary
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".templaros-")
        );
    }

    #[test]
    fn surveying_a_real_tree_orders_parents_first_and_does_not_follow_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("from/tree");
        let destination = dir.path().join("to");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        std::fs::write(root.join("nested/b.txt"), b"worldly").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();

        let survey = survey(std::slice::from_ref(&root), Some(&destination));
        assert!(
            survey.destination_names.is_empty(),
            "an empty destination has nothing to collide with"
        );
        let source = &survey.sources[0];
        assert!(source.entries[0].is_dir);
        assert_eq!(source.entries[0].relative, PathBuf::new());
        assert_eq!(survey.total_bytes(), 12, "symlink contributes no bytes");
        assert_eq!(survey.total_items(), 3, "two files and one link");
        assert!(source.entries.iter().any(|entry| entry.is_symlink));
        assert!(source.same_device);
        assert!(!source.destination_exists);

        // Parents always precede their children.
        for (index, entry) in source.entries.iter().enumerate() {
            if let Some(parent) = entry.path.parent()
                && let Some(position) = source.entries.iter().position(|other| other.path == parent)
            {
                assert!(position < index);
            }
        }
    }

    #[test]
    fn surveying_a_missing_source_records_an_error_rather_than_panicking() {
        let survey = survey(&[PathBuf::from("/definitely/not/here")], None);
        assert!(survey.sources.is_empty());
        assert_eq!(survey.errors.len(), 1);
    }

    /// Removing a reference from a Home shortcut pane is a field-document edit
    /// (`templar_field::FieldOp::RemoveReference`) and never reaches this service: nothing in
    /// [`OperationKind`] models pane membership, so no code path here can touch a referenced
    /// file because its shortcut went away. Trashing the *target* is the separate, explicitly
    /// labelled operation — journalled, and answering with its own undo that restores exactly
    /// what the trash recorded.
    #[test]
    fn shortcut_pane_remove_is_not_a_file_operation_and_trash_is_separate_with_undo() {
        let kind = OperationKind::Trash {
            paths: vec!["/home/a/notes.md".into()],
        };
        assert_eq!(kind.summary(), "Move notes.md to Trash");

        let survey = Survey {
            sources: vec![file_survey("/home/a/notes.md", 1, true, false)],
            ..Survey::default()
        };
        let mut operation = plan(OperationId::new(), kind, &survey, Options::default());
        assert_eq!(
            undo_of(&operation),
            None,
            "an uncommitted trash has no undo"
        );
        for step in &mut operation.steps {
            if let Step::MoveToTrash { trashed, .. } = step {
                *trashed = Some("/home/a/.local/share/Trash/files/notes.md".into());
            }
        }
        operation.state = OperationState::Committed;
        assert_eq!(
            undo_of(&operation),
            Some(OperationKind::RestoreFromTrash {
                paths: vec!["/home/a/.local/share/Trash/files/notes.md".into()],
            }),
            "the undo restores exactly what the trash recorded"
        );
    }
}
