//! Wire models shared by graphical clients of `org.templaros.Files`.
//!
//! The daemon remains an independent process and clients do not link its implementation crate.
//! These serde models are the versioned JSON contract carried by the typed D-Bus proxies.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BrowseEntry {
    pub path: PathBuf,
    pub name: String,
    pub kind: EntryKind,
    pub is_symlink: bool,
    pub hidden: bool,
    pub size_bytes: u64,
    pub mtime_ms: i64,
}

impl BrowseEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DirectoryListing {
    pub directory: PathBuf,
    pub parent: Option<PathBuf>,
    pub entries: Vec<BrowseEntry>,
    pub truncated: bool,
    pub limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub reserved_bytes: u64,
    pub read_only: bool,
}

impl VolumeInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    pub fn usable_bytes(&self) -> u64 {
        self.available_bytes.saturating_sub(self.reserved_bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootKind {
    Place,
    Drive,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BrowserRoot {
    pub key: String,
    pub label: String,
    pub path: PathBuf,
    pub kind: RootKind,
    pub available: bool,
    pub volume: Option<VolumeInfo>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BrowserSources {
    pub roots: Vec<BrowserRoot>,
}

impl BrowserSources {
    pub fn home(&self) -> Option<&Path> {
        self.roots
            .iter()
            .find(|root| root.key == "home" && root.available)
            .map(|root| root.path.as_path())
    }

    pub fn first_available(&self) -> Option<&Path> {
        self.roots
            .iter()
            .find(|root| root.available)
            .map(|root| root.path.as_path())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub file_count: u64,
    pub directory_count: u64,
    pub total_size_bytes: u64,
    pub total_allocated_bytes: u64,
    pub unreadable_directories: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub name: String,
    pub kind: EntryKind,
    pub is_symlink: bool,
    pub symlink_target: Option<PathBuf>,
    pub size_bytes: u64,
    pub allocated_bytes: u64,
    pub modified_ms: i64,
    pub accessed_ms: i64,
    pub changed_ms: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub read_only: bool,
    #[serde(default)]
    pub folder: Option<FolderSummary>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewKind {
    Bpg,
    Heic,
    Pdf,
    Raster,
    Video,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreviewMetadata {
    pub renderer: String,
    pub kind: PreviewKind,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    #[default]
    Fail,
    Skip,
    Replace,
    KeepBoth,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    #[default]
    Size,
    Checksum,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationOptions {
    #[serde(default)]
    pub conflict: ConflictPolicy,
    #[serde(default)]
    pub verify: Verification,
    #[serde(default)]
    pub override_reserve: bool,
}

/// Exact native root mapping retained by a committed transfer for journal undo.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TransferMapping {
    #[serde(with = "templar_types::native_path")]
    pub source: PathBuf,
    #[serde(with = "templar_types::native_path")]
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum OperationRequest {
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
    RestoreMoveV1 {
        entries: Vec<TransferMapping>,
    },
    UndoCopyV1 {
        original_operation: String,
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
    RestoreFromTrash {
        #[serde(with = "templar_types::native_path::many")]
        paths: Vec<PathBuf>,
    },
    Delete {
        #[serde(with = "templar_types::native_path::many")]
        paths: Vec<PathBuf>,
    },
}

impl OperationRequest {
    pub fn summary(&self) -> String {
        fn names(paths: &[PathBuf]) -> String {
            match paths {
                [] => "nothing".to_owned(),
                [one] => one.file_name().map_or_else(
                    || one.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                ),
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
                format!("Rename {} to {new_name}", names(std::slice::from_ref(path)))
            }
            Self::Trash { paths } => format!("Move {} to Trash", names(paths)),
            Self::RestoreFromTrash { paths } => {
                format!("Restore {} from Trash", names(paths))
            }
            Self::Delete { paths } => format!("Delete {} permanently", names(paths)),
        }
    }
}

/// One item in one of this user's trashes, as `Files.Trash1.List` reports it.
///
/// A trash listing is not a directory listing and does not pretend to be one: the columns that
/// matter are when it went and where it came from, and `original_path` is the only thing that
/// makes "restore" mean anything to the person reading it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrashEntry {
    pub name: String,
    #[serde(with = "templar_types::native_path")]
    pub trashed_path: PathBuf,
    #[serde(with = "templar_types::native_path")]
    pub info_path: PathBuf,
    #[serde(with = "templar_types::native_path")]
    pub original_path: PathBuf,
    /// Local time, `YYYY-MM-DDThh:mm:ss`.
    pub deletion_date: String,
    /// The trash holding it: the home trash, or a volume's.
    #[serde(with = "templar_types::native_path")]
    pub trash_root: PathBuf,
    pub is_directory: bool,
    /// The item's own size, not a directory's recursive total.
    pub size_bytes: u64,
}

impl TrashEntry {
    /// Where the item was, as a person would say it — the whole point of the trash listing.
    pub fn original_location(&self) -> String {
        self.original_path.parent().map_or_else(
            || self.original_path.display().to_string(),
            |parent| parent.display().to_string(),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationState {
    Planned,
    Running,
    Verified,
    Committed,
    Failed,
    Cancelled,
    Undone,
}

impl OperationState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Failed | Self::Cancelled | Self::Undone
        )
    }
}

/// The subset of the durable journal record a UI needs. Unknown implementation fields (steps,
/// cursor and options) are intentionally ignored when deserializing.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub kind: OperationRequest,
    pub state: OperationState,
    pub total_bytes: u64,
    pub total_items: u64,
    pub done_bytes: u64,
    pub done_items: u64,
    #[serde(default, with = "templar_types::native_path::many")]
    pub skipped: Vec<PathBuf>,
    #[serde(default)]
    pub error: Option<String>,
}

impl OperationRecord {
    pub fn progress_percent(&self) -> u8 {
        let fraction = if self.total_bytes > 0 {
            self.done_bytes as f64 / self.total_bytes as f64
        } else if self.total_items > 0 {
            self.done_items as f64 / self.total_items as f64
        } else {
            f64::from(u8::from(self.state.terminal()))
        };
        (fraction.clamp(0.0, 1.0) * 100.0).round() as u8
    }
}

/// A rename is exactly one ordinary filename component, never a path traversal.
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.as_bytes().contains(&0)
        && Path::new(name)
            .parent()
            .is_some_and(|parent| parent.as_os_str().is_empty())
        && Path::new(name).file_name().is_some_and(|file| file == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_wire_contract_matches_operations1_json() {
        let request = OperationRequest::Move {
            sources: vec![PathBuf::from("/home/u/report.txt")],
            destination_dir: PathBuf::from("/home/u/Documents"),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "kind": "move",
                "sources": ["/home/u/report.txt"],
                "destination_dir": "/home/u/Documents"
            })
        );
        assert_eq!(
            serde_json::to_value(OperationOptions::default()).unwrap(),
            serde_json::json!({
                "conflict": "fail",
                "verify": "size",
                "override_reserve": false
            })
        );
    }

    #[test]
    fn operation_progress_prefers_bytes_then_items() {
        let json = serde_json::json!({
            "id": "one",
            "kind": {"kind":"trash", "paths":["/tmp/a", "/tmp/b"]},
            "state": "running",
            "total_bytes": 0,
            "total_items": 2,
            "done_bytes": 0,
            "done_items": 1,
            "steps": [],
            "cursor": 1,
            "options": {"conflict":"fail", "verify":"size", "override_reserve":false}
        });
        let record: OperationRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.progress_percent(), 50);
    }

    #[test]
    fn operation_rename_names_are_one_safe_component() {
        assert!(valid_file_name("report 2.txt"));
        for unsafe_name in ["", ".", "..", "../escape", "sub/name", "/absolute"] {
            assert!(!valid_file_name(unsafe_name), "{unsafe_name:?}");
        }
    }

    #[test]
    fn native_operation_and_additive_inverse_wire_models_do_not_use_display_names() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let source = PathBuf::from(OsString::from_vec(b"/from/name-\xff.pdf".to_vec()));
        let destination = PathBuf::from(OsString::from_vec(b"/Desktop/name-\xff (2).pdf".to_vec()));
        for request in [
            OperationRequest::Copy {
                sources: vec![source.clone()],
                destination_dir: "/Desktop".into(),
            },
            OperationRequest::RestoreMoveV1 {
                entries: vec![TransferMapping {
                    source: destination,
                    destination: source,
                }],
            },
            OperationRequest::UndoCopyV1 {
                original_operation: "11111111-1111-4111-8111-111111111111".into(),
            },
        ] {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(
                serde_json::from_str::<OperationRequest>(&json).unwrap(),
                request
            );
        }
    }
}
