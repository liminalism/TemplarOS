//! File operations as transactions. A long copy or cross-volume move survives a file-manager
//! crash because `filed` — not the GUI — owns the journal.
//!
//! Robust move: copy to a temporary destination → flush → verify size (and optionally checksum)
//! → atomically expose the destination → delete the source only after destination completion →
//! record enough to explain or reverse.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub Uuid);

impl OperationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for OperationId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum OperationKind {
    Copy {
        sources: Vec<PathBuf>,
        destination_dir: PathBuf,
    },
    Move {
        sources: Vec<PathBuf>,
        destination_dir: PathBuf,
    },
    Rename {
        path: PathBuf,
        new_name: String,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    /// Irreversible; the UI must say so before running it.
    Delete {
        paths: Vec<PathBuf>,
    },
}

impl OperationKind {
    /// Whether the operation can be undone once committed.
    pub fn reversible(&self) -> bool {
        !matches!(self, Self::Delete { .. })
    }
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

/// One journaled step. Steps are idempotent so a crashed operation can be resumed or reversed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "step")]
pub enum Step {
    CopyToTemporary {
        source: PathBuf,
        temporary: PathBuf,
    },
    FlushTemporary {
        temporary: PathBuf,
    },
    VerifySize {
        source: PathBuf,
        temporary: PathBuf,
    },
    ExposeAtomically {
        temporary: PathBuf,
        destination: PathBuf,
    },
    DeleteSource {
        source: PathBuf,
    },
    RenameInPlace {
        from: PathBuf,
        to: PathBuf,
    },
    MoveToTrash {
        path: PathBuf,
        trash_info: PathBuf,
    },
    Unlink {
        path: PathBuf,
    },
}

/// A journaled operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: OperationId,
    pub kind: OperationKind,
    pub state: OperationState,
    pub steps: Vec<Step>,
    /// Index of the next step to run (resume point after a crash).
    pub cursor: usize,
}

/// Build the step list for `kind`. Pure: nothing touches the filesystem here.
pub fn plan(id: OperationId, kind: OperationKind) -> Operation {
    let steps = match &kind {
        OperationKind::Copy {
            sources,
            destination_dir,
        } => sources
            .iter()
            .flat_map(|source| copy_steps(source, destination_dir, id))
            .collect(),
        OperationKind::Move {
            sources,
            destination_dir,
        } => sources
            .iter()
            .flat_map(|source| {
                let mut steps = copy_steps(source, destination_dir, id);
                steps.push(Step::DeleteSource {
                    source: source.clone(),
                });
                steps
            })
            .collect(),
        OperationKind::Rename { path, new_name } => {
            vec![Step::RenameInPlace {
                from: path.clone(),
                to: path.with_file_name(new_name),
            }]
        }
        OperationKind::Trash { paths } => paths
            .iter()
            .map(|path| Step::MoveToTrash {
                path: path.clone(),
                trash_info: PathBuf::from(format!("{}.trashinfo", file_name(path))),
            })
            .collect(),
        OperationKind::Delete { paths } => paths
            .iter()
            .map(|path| Step::Unlink { path: path.clone() })
            .collect(),
    };
    Operation {
        id,
        kind,
        state: OperationState::Planned,
        steps,
        cursor: 0,
    }
}

fn copy_steps(source: &Path, destination_dir: &Path, id: OperationId) -> Vec<Step> {
    let destination = destination_dir.join(file_name(source));
    let temporary = destination_dir.join(format!(
        ".legeos-{}-{}.part",
        id.0.simple(),
        file_name(source)
    ));
    vec![
        Step::CopyToTemporary {
            source: source.to_path_buf(),
            temporary: temporary.clone(),
        },
        Step::FlushTemporary {
            temporary: temporary.clone(),
        },
        Step::VerifySize {
            source: source.to_path_buf(),
            temporary: temporary.clone(),
        },
        Step::ExposeAtomically {
            temporary,
            destination,
        },
    ]
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "unnamed".into(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_move_deletes_the_source_only_after_the_destination_is_exposed() {
        let op = plan(
            OperationId::new(),
            OperationKind::Move {
                sources: vec!["/a/report.pdf".into()],
                destination_dir: "/b".into(),
            },
        );
        let names: Vec<&str> = op
            .steps
            .iter()
            .map(|s| match s {
                Step::CopyToTemporary { .. } => "copy",
                Step::FlushTemporary { .. } => "flush",
                Step::VerifySize { .. } => "verify",
                Step::ExposeAtomically { .. } => "expose",
                Step::DeleteSource { .. } => "delete-source",
                _ => "other",
            })
            .collect();
        assert_eq!(
            names,
            ["copy", "flush", "verify", "expose", "delete-source"]
        );
        assert!(op.kind.reversible());
        assert_eq!(op.state, OperationState::Planned);
    }

    #[test]
    fn delete_is_flagged_irreversible_and_rename_is_one_step() {
        assert!(!OperationKind::Delete { paths: vec![] }.reversible());
        let op = plan(
            OperationId::new(),
            OperationKind::Rename {
                path: "/a/x.txt".into(),
                new_name: "y.txt".into(),
            },
        );
        assert_eq!(
            op.steps,
            vec![Step::RenameInPlace {
                from: "/a/x.txt".into(),
                to: "/a/y.txt".into()
            }]
        );
    }
}
