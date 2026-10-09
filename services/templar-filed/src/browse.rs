//! Bounded, structured directory listings for file-manager, Home and portal clients.
//!
//! A shell surface must not walk directories and invent its own ordering: that would make every
//! client disagree about which entries exist and where they appear. `templar-filed` therefore owns
//! the small, synchronous filesystem read and returns a wire-friendly snapshot. This is not the
//! recursive search index or the Silvaview-derived size treemap: a person browsing a folder gets
//! directories first and natural filename order (`file2` before `file10`).

use crate::storage::{self, VolumeInfo};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// No client can make one D-Bus reply carry an unbounded directory.
pub const MAX_ENTRIES: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

/// One immediate child of a directory. Symlinks retain their identity while `kind` describes
/// the target when it can be inspected, so a symlink to a directory is still navigable.
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
    /// More visible entries existed than the bounded response could carry.
    pub truncated: bool,
    /// The server-applied limit (the requested value clamped to [`MAX_ENTRIES`]).
    pub limit: usize,
}

/// A stable place or service-approved mounted drive for file clients to render.
///
/// XDG user-directory discovery belongs here rather than in every GUI. `volume` is populated
/// only for drives, and carries the same reserve-aware numbers as `Files.Volumes1`.
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

/// Recursive contents of a directory for an explicitly requested properties view.
///
/// Symlinks are never followed. A subtree that cannot be read contributes to
/// `unreadable_directories` and makes `complete` false, so the UI never presents a partial total
/// as exact.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FolderSummary {
    pub file_count: u64,
    pub directory_count: u64,
    pub total_size_bytes: u64,
    pub total_allocated_bytes: u64,
    pub unreadable_directories: u64,
    pub complete: bool,
}

/// Metadata for the explicitly requested properties pane. Inspecting a regular file is one stat;
/// inspecting a directory additionally walks its descendants off the async executor so the
/// properties button can report a real file count and total size.
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

/// Read one directory without recursion. Hidden entries are identified in every response even
/// when the caller chooses not to include them, and filesystem errors keep the path in their
/// message so a UI can explain the failed location rather than merely saying "unavailable".
pub fn list_directory(
    path: &Path,
    show_hidden: bool,
    requested_limit: usize,
) -> Result<DirectoryListing> {
    if !path.is_absolute() {
        bail!("directory path must be absolute: {}", path.display());
    }
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspecting directory {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("not a directory: {}", path.display());
    }

    let limit = requested_limit.clamp(1, MAX_ENTRIES);
    let reader =
        std::fs::read_dir(path).with_context(|| format!("reading directory {}", path.display()))?;
    let mut entries = Vec::new();
    for child in reader {
        let child = child.with_context(|| format!("reading an entry in {}", path.display()))?;
        let name = child.file_name().to_string_lossy().into_owned();
        let hidden = name.starts_with('.');
        if hidden && !show_hidden {
            continue;
        }
        entries.push(entry_from(child.path(), name, hidden));
    }
    entries.sort_by(entry_cmp);
    let truncated = entries.len() > limit;
    entries.truncate(limit);

    Ok(DirectoryListing {
        directory: path.to_path_buf(),
        parent: path.parent().map(Path::to_path_buf),
        entries,
        truncated,
        limit,
    })
}

/// Resolve XDG places and mounted drives once for every file client.
pub fn sources(reserve_percent: u64) -> Result<BrowserSources> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let user_dirs = home.as_ref().and_then(|home| {
        let config =
            std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
        std::fs::read_to_string(config.join("user-dirs.dirs")).ok()
    });
    let volumes = storage::volumes(reserve_percent)?;
    Ok(sources_from(home.as_deref(), user_dirs.as_deref(), volumes))
}

/// Pure half of [`sources`], kept public for deterministic policy tests.
pub fn sources_from(
    home: Option<&Path>,
    user_dirs_text: Option<&str>,
    volumes: Vec<VolumeInfo>,
) -> BrowserSources {
    const PLACES: &[(&str, &str, &str)] = &[
        ("desktop", "Desktop", "XDG_DESKTOP_DIR"),
        ("documents", "Documents", "XDG_DOCUMENTS_DIR"),
        ("downloads", "Downloads", "XDG_DOWNLOAD_DIR"),
        ("music", "Music", "XDG_MUSIC_DIR"),
        ("pictures", "Pictures", "XDG_PICTURES_DIR"),
        ("videos", "Videos", "XDG_VIDEOS_DIR"),
    ];

    let configured = home
        .map(|home| parse_user_dirs(user_dirs_text.unwrap_or_default(), home))
        .unwrap_or_default();
    let mut roots = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(home) = home {
        seen.insert(home.to_path_buf());
        roots.push(place("home", "Home", home.to_path_buf()));
        for (key, label, xdg_key) in PLACES {
            let path = configured
                .iter()
                .find(|(name, _)| name == xdg_key)
                .map_or_else(|| home.join(label), |(_, path)| path.clone());
            if seen.insert(path.clone()) {
                roots.push(place(key, label, path));
            }
        }
    }

    for volume in volumes {
        let path = volume.mount_point.clone();
        let label = if path == Path::new("/") {
            "System".to_owned()
        } else {
            path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        };
        roots.push(BrowserRoot {
            key: format!("drive:{}", path.display()),
            label,
            available: path.is_dir(),
            path,
            kind: RootKind::Drive,
            volume: Some(volume),
        });
    }
    BrowserSources { roots }
}

fn place(key: &str, label: &str, path: PathBuf) -> BrowserRoot {
    BrowserRoot {
        key: key.to_owned(),
        label: label.to_owned(),
        available: path.is_dir(),
        path,
        kind: RootKind::Place,
        volume: None,
    }
}

fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if !key.starts_with("XDG_") {
                return None;
            }
            let raw = value.trim();
            let expand_home = raw.strip_prefix('"').unwrap_or(raw).starts_with("$HOME");
            let value = unquote_user_dir(raw)?;
            let path = if expand_home && value.starts_with("$HOME/") {
                home.join(&value["$HOME/".len()..])
            } else if expand_home && value == "$HOME" {
                home.to_path_buf()
            } else if value.starts_with('/') {
                PathBuf::from(value)
            } else {
                return None;
            };
            Some((key.to_owned(), path))
        })
        .collect()
}

/// Read the literal double-quoted form generated by xdg-user-dirs, never a shell evaluation.
/// Backslashes escape only the characters double quotes escape; spaces and '#' stay filenames.
fn unquote_user_dir(value: &str) -> Option<String> {
    let Some(inner) = value.strip_prefix('"') else {
        return Some(value.to_owned());
    };
    let mut out = String::new();
    let mut chars = inner.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '"' => {
                let suffix = inner[index + 1..].trim();
                return (suffix.is_empty() || suffix.starts_with('#')).then_some(out);
            }
            '\\' => {
                let (_, next) = chars.next()?;
                if !matches!(next, '"' | '\\' | '$' | '`') {
                    out.push('\\');
                }
                out.push(next);
            }
            ch => out.push(ch),
        }
    }
    None
}

/// Make a directory under `parent`, returning the name it actually got.
///
/// The service picks the free name rather than failing on a collision, because the alternative is
/// every surface inventing its own numbering and two file managers on one machine disagreeing
/// about what the next untitled folder is called. Bounded rather than open-ended: after a
/// thousand it stops, which is long past the point where the name was the problem.
pub fn create_directory(parent: &Path, preferred_name: &str) -> Result<String> {
    if !valid_component(preferred_name) {
        bail!("a folder name must be one ordinary filename component: {preferred_name:?}");
    }
    if !parent.is_dir() {
        bail!("{} is not a directory", parent.display());
    }
    let (stem, extension) = match preferred_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (preferred_name, String::new()),
    };
    for index in 1..1_000 {
        let name = if index == 1 {
            preferred_name.to_owned()
        } else {
            format!("{stem} {index}{extension}")
        };
        let candidate = parent.join(&name);
        // `create_dir` fails if the path exists, which is the check and the creation in one step
        // — a separate `exists()` first would be a race with anything else making the same name.
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", candidate.display()));
            }
        }
    }
    bail!(
        "no free name for {preferred_name:?} in {}",
        parent.display()
    )
}

/// One ordinary filename component: never a traversal, never a path, never a device.
fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.as_bytes().contains(&0)
}

/// Inspect one path and, for a real directory, summarize its descendants without following links.
pub fn inspect(path: &Path) -> Result<FileInfo> {
    if !path.is_absolute() {
        bail!("file path must be absolute: {}", path.display());
    }
    let link_metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    let is_symlink = link_metadata.file_type().is_symlink();
    let target_metadata = if is_symlink {
        std::fs::metadata(path).ok()
    } else {
        Some(link_metadata.clone())
    };
    let metadata = target_metadata.as_ref().unwrap_or(&link_metadata);
    let kind = if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let folder = (kind == EntryKind::Directory && !is_symlink)
        .then(|| summarize_folder(path))
        .transpose()?;
    Ok(FileInfo {
        path: path.to_path_buf(),
        canonical_path: std::fs::canonicalize(path).ok(),
        name: path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
        kind,
        is_symlink,
        symlink_target: is_symlink.then(|| std::fs::read_link(path).ok()).flatten(),
        size_bytes: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
        allocated_bytes: metadata.blocks().saturating_mul(512),
        modified_ms: unix_ms(metadata.mtime(), metadata.mtime_nsec()),
        accessed_ms: unix_ms(metadata.atime(), metadata.atime_nsec()),
        changed_ms: unix_ms(metadata.ctime(), metadata.ctime_nsec()),
        mode: metadata.mode(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        read_only: metadata.permissions().readonly(),
        folder,
    })
}

/// Walk a directory without following symlinks. Failure to read the selected directory itself is
/// an error; unreadable descendants are counted and skipped so callers still receive useful,
/// explicitly incomplete totals.
fn summarize_folder(root: &Path) -> Result<FolderSummary> {
    let mut summary = FolderSummary {
        complete: true,
        ..FolderSummary::default()
    };
    let mut pending = vec![root.to_path_buf()];
    let mut root_pending = true;
    while let Some(directory) = pending.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if root_pending => {
                return Err(error).with_context(|| format!("reading {}", root.display()));
            }
            Err(_) => {
                summary.unreadable_directories = summary.unreadable_directories.saturating_add(1);
                summary.complete = false;
                root_pending = false;
                continue;
            }
        };
        root_pending = false;
        for entry in entries {
            let Ok(entry) = entry else {
                summary.complete = false;
                continue;
            };
            let Ok(metadata) = entry.path().symlink_metadata() else {
                summary.complete = false;
                continue;
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                summary.directory_count = summary.directory_count.saturating_add(1);
                pending.push(entry.path());
            } else if file_type.is_file() {
                summary.file_count = summary.file_count.saturating_add(1);
                summary.total_size_bytes = summary.total_size_bytes.saturating_add(metadata.len());
                summary.total_allocated_bytes = summary
                    .total_allocated_bytes
                    .saturating_add(metadata.blocks().saturating_mul(512));
            }
        }
    }
    Ok(summary)
}

fn unix_ms(seconds: i64, nanoseconds: i64) -> i64 {
    seconds
        .saturating_mul(1_000)
        .saturating_add(nanoseconds / 1_000_000)
}

fn entry_from(path: PathBuf, name: String, hidden: bool) -> BrowseEntry {
    let link_metadata = std::fs::symlink_metadata(&path).ok();
    let is_symlink = link_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink());
    // Follow a symlink for its navigable kind and useful file size, while retaining the flag
    // above. A broken or unreadable target remains an `other` entry instead of disappearing.
    let metadata = std::fs::metadata(&path).ok().or(link_metadata);
    let kind = metadata.as_ref().map_or(EntryKind::Other, |metadata| {
        if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        }
    });
    let size_bytes = metadata
        .as_ref()
        .filter(|metadata| metadata.is_file())
        .map_or(0, std::fs::Metadata::len);
    let mtime_ms = metadata
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| {
            duration.as_millis().min(i64::MAX as u128) as i64
        });
    BrowseEntry {
        path,
        name,
        kind,
        is_symlink,
        hidden,
        size_bytes,
        mtime_ms,
    }
}

/// The one shared ordering (`templar_types::file_sort`): the service's default listing order is
/// the contract's default descriptor, so a client re-sorting with the same descriptor can
/// never disagree with what the service served.
fn entry_cmp(left: &BrowseEntry, right: &BrowseEntry) -> Ordering {
    templar_types::file_sort::compare(
        &sort_item(left),
        &sort_item(right),
        templar_types::file_sort::SortDescriptor::default(),
    )
}

/// `BrowseEntry` as the shared comparator reads it. The kind label is the lowered extension —
/// the same label the Files app shows in its TYPE column.
pub fn sort_item(entry: &BrowseEntry) -> templar_types::file_sort::SortItem<'_> {
    templar_types::file_sort::SortItem {
        name: &entry.name,
        path: &entry.path,
        is_dir: entry.is_dir(),
        hidden: entry.hidden,
        kind_label: entry
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or(""),
        size_bytes: entry.size_bytes,
        mtime_ms: entry.mtime_ms,
        manual_index: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_actions_a_new_folder_gets_the_next_free_name_and_the_service_says_which() {
        let dir = tempfile::tempdir().unwrap();
        // The first is the name asked for; the service, not the surface, picks the ones after —
        // otherwise two file managers on one machine number them differently.
        assert_eq!(
            create_directory(dir.path(), "New folder").unwrap(),
            "New folder"
        );
        assert_eq!(
            create_directory(dir.path(), "New folder").unwrap(),
            "New folder 2"
        );
        assert_eq!(
            create_directory(dir.path(), "New folder").unwrap(),
            "New folder 3"
        );
        assert!(dir.path().join("New folder 3").is_dir());
        // A name taken by a *file* is still taken.
        std::fs::write(dir.path().join("Notes"), b"x").unwrap();
        assert_eq!(create_directory(dir.path(), "Notes").unwrap(), "Notes 2");
    }

    #[test]
    fn file_actions_a_new_folder_name_is_one_component_and_never_a_traversal() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["", ".", "..", "../escape", "sub/deeper", "a\0b"] {
            assert!(
                create_directory(dir.path(), name).is_err(),
                "{name:?} was accepted as a folder name"
            );
        }
        assert!(
            !dir.path().parent().unwrap().join("escape").exists(),
            "a traversal must not have created anything outside the parent"
        );
        // And a parent that is not a directory is refused rather than guessed at.
        let file = dir.path().join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(create_directory(&file, "New folder").is_err());
    }

    #[test]
    fn browse_is_folder_first_natural_and_reports_real_metadata() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("folder10")).unwrap();
        std::fs::create_dir(temp.path().join("folder2")).unwrap();
        std::fs::write(temp.path().join("file10.txt"), b"ten").unwrap();
        std::fs::write(temp.path().join("file2.txt"), b"two").unwrap();
        std::fs::write(temp.path().join(".secret"), b"hidden").unwrap();

        let listing = list_directory(temp.path(), false, 100).unwrap();
        let names: Vec<&str> = listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(names, ["folder2", "folder10", "file2.txt", "file10.txt"]);
        assert!(listing.entries[0].is_dir());
        assert_eq!(listing.entries[2].size_bytes, 3);
        assert!(listing.entries[2].mtime_ms > 0);
        assert!(!listing.truncated);
        assert_eq!(listing.parent, temp.path().parent().map(Path::to_path_buf));
    }

    #[test]
    fn browse_hidden_entries_are_opt_in_and_identified() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible"), b"v").unwrap();
        std::fs::write(temp.path().join(".hidden"), b"h").unwrap();
        assert_eq!(
            list_directory(temp.path(), false, 10)
                .unwrap()
                .entries
                .len(),
            1
        );
        let listing = list_directory(temp.path(), true, 10).unwrap();
        assert_eq!(listing.entries.len(), 2);
        assert!(
            listing
                .entries
                .iter()
                .find(|entry| entry.name == ".hidden")
                .unwrap()
                .hidden
        );
    }

    #[test]
    fn browse_response_is_capped_and_says_when_truncated() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..6 {
            std::fs::write(temp.path().join(format!("item{index}")), b"x").unwrap();
        }
        let listing = list_directory(temp.path(), false, 3).unwrap();
        assert_eq!(listing.entries.len(), 3);
        assert_eq!(listing.limit, 3);
        assert!(listing.truncated);
    }

    #[test]
    fn browse_rejects_relative_paths_and_regular_files_clearly() {
        let relative = list_directory(Path::new("relative"), false, 10).unwrap_err();
        assert!(relative.to_string().contains("must be absolute"));

        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("one.txt");
        std::fs::write(&file, b"one").unwrap();
        let error = list_directory(&file, false, 10).unwrap_err();
        assert!(error.to_string().contains("not a directory"));
        assert!(error.to_string().contains("one.txt"));
    }

    #[test]
    fn configured_desktop_keeps_quoted_native_home_and_literal_filename_characters() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let home = PathBuf::from(OsString::from_vec(b"/home/name-\xff".to_vec()));
        let config = r#"XDG_DESKTOP_DIR="$HOME/Desk\\top \"work\" \$notes" # configured"#;
        let sources = sources_from(Some(&home), Some(config), Vec::new());
        let desktop = sources
            .roots
            .iter()
            .find(|root| root.key == "desktop")
            .unwrap();
        assert_eq!(desktop.path, home.join(r#"Desk\top "work" $notes"#));
        let disabled = sources_from(Some(&home), Some(r#"XDG_DESKTOP_DIR="$HOME""#), Vec::new());
        assert!(!disabled.roots.iter().any(|root| root.key == "desktop"));
    }

    #[test]
    fn browser_sources_honour_xdg_places_and_include_capacity_bearing_drives() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let custom = home.join("Writing");
        std::fs::create_dir(&custom).unwrap();
        let volume = VolumeInfo {
            mount_point: PathBuf::from("/"),
            fs_type: "btrfs".into(),
            total_bytes: 1_000,
            free_bytes: 400,
            available_bytes: 350,
            reserved_bytes: 100,
            read_only: false,
        };
        let sources = sources_from(
            Some(home),
            Some("XDG_DOCUMENTS_DIR=\"$HOME/Writing\""),
            vec![volume.clone()],
        );
        assert_eq!(sources.roots[0].key, "home");
        let documents = sources
            .roots
            .iter()
            .find(|root| root.key == "documents")
            .unwrap();
        assert_eq!(documents.path, custom);
        assert!(documents.available);
        let drive = sources
            .roots
            .iter()
            .find(|root| root.kind == RootKind::Drive)
            .unwrap();
        assert_eq!(drive.volume.as_ref(), Some(&volume));
    }

    #[test]
    fn file_properties_are_bounded_and_report_real_unix_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("report.txt");
        std::fs::write(&file, b"hello").unwrap();
        let info = inspect(&file).unwrap();
        assert_eq!(info.name, "report.txt");
        assert_eq!(info.kind, EntryKind::File);
        assert_eq!(info.size_bytes, 5);
        assert!(info.allocated_bytes >= info.size_bytes);
        assert!(info.modified_ms > 0);
        assert!(info.mode > 0);
        assert_eq!(info.canonical_path.as_deref(), Some(file.as_path()));
        assert!(info.folder.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn folder_properties_count_recursive_files_and_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("folder");
        std::fs::create_dir_all(folder.join("nested/deeper")).unwrap();
        std::fs::write(folder.join("one.bin"), [1_u8; 3]).unwrap();
        std::fs::write(folder.join("nested/two.bin"), [2_u8; 7]).unwrap();
        std::fs::write(folder.join("nested/deeper/three.bin"), [3_u8; 11]).unwrap();
        symlink(temp.path(), folder.join("loop-to-parent")).unwrap();
        symlink(folder.join("one.bin"), folder.join("linked-file")).unwrap();

        let info = inspect(&folder).unwrap();
        let summary = info.folder.unwrap();
        assert_eq!(summary.file_count, 3);
        assert_eq!(summary.directory_count, 2);
        assert_eq!(summary.total_size_bytes, 21);
        assert!(summary.total_allocated_bytes >= summary.total_size_bytes);
        assert_eq!(summary.unreadable_directories, 0);
        assert!(summary.complete);

        let linked_folder = temp.path().join("linked-folder");
        symlink(&folder, &linked_folder).unwrap();
        let linked_info = inspect(&linked_folder).unwrap();
        assert_eq!(linked_info.kind, EntryKind::Directory);
        assert!(linked_info.is_symlink);
        assert!(linked_info.folder.is_none());
    }

    #[test]
    fn properties_reject_relative_paths() {
        assert!(
            inspect(Path::new("relative"))
                .unwrap_err()
                .to_string()
                .contains("absolute")
        );
    }

    #[cfg(unix)]
    #[test]
    fn browse_symlink_keeps_identity_and_target_kind() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("target")).unwrap();
        symlink(temp.path().join("target"), temp.path().join("linked")).unwrap();
        let listing = list_directory(temp.path(), false, 10).unwrap();
        let linked = listing
            .entries
            .iter()
            .find(|entry| entry.name == "linked")
            .unwrap();
        assert!(linked.is_symlink);
        assert_eq!(linked.kind, EntryKind::Directory);
    }

    /// The service's ordering IS the shared contract (`templar_types::file_sort`): every field is
    /// a deterministic total order with stable tie breakers, direction flips only the primary
    /// key, and folder-first is an explicit policy — so a client re-sorting with the same
    /// descriptor can never disagree with a listing this service served.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn sort_contract_is_shared_total_and_stable_across_fields_and_directions() {
        use templar_types::file_sort::{SortDescriptor, SortField, compare};

        fn entry(name: &str, dir: bool, hidden: bool, size: u64, mtime: i64) -> BrowseEntry {
            BrowseEntry {
                path: PathBuf::from(format!("/fixture/{name}")),
                name: name.to_owned(),
                kind: if dir {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                is_symlink: false,
                hidden,
                size_bytes: size,
                mtime_ms: mtime,
            }
        }

        let entries = vec![
            entry("file10.txt", false, false, 10, 5),
            entry("file2.txt", false, false, 200, 5),
            entry("Alpha", true, false, 0, 9),
            entry("beta", true, false, 0, 1),
            entry(".hidden.txt", false, true, 3, 2),
            entry("same.txt", false, false, 200, 5),
            entry("archive.zip", false, false, 50, 7),
        ];

        // The service's own default order equals the shared default descriptor's.
        let mut by_service = entries.clone();
        by_service.sort_by(entry_cmp);
        let mut by_contract = entries.clone();
        by_contract
            .sort_by(|a, b| compare(&sort_item(a), &sort_item(b), SortDescriptor::default()));
        assert_eq!(by_service, by_contract, "one contract, both sides");
        let names: Vec<&str> = by_service.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Alpha",
                "beta",
                "archive.zip",
                "file2.txt",
                "file10.txt",
                "same.txt",
                ".hidden.txt"
            ],
            "folders first, natural order, hidden last"
        );

        for field in [
            SortField::NaturalName,
            SortField::Kind,
            SortField::Size,
            SortField::Modified,
            SortField::Manual,
        ] {
            for ascending in [true, false] {
                for folders_first in [true, false] {
                    let descriptor = SortDescriptor {
                        field,
                        ascending,
                        folders_first,
                    };
                    let mut once = entries.clone();
                    once.sort_by(|a, b| compare(&sort_item(a), &sort_item(b), descriptor));
                    let mut twice = once.clone();
                    twice.reverse();
                    twice.sort_by(|a, b| compare(&sort_item(a), &sort_item(b), descriptor));
                    assert_eq!(once, twice, "{descriptor:?} is deterministic in its inputs");
                    // Totality: no two distinct entries compare equal (the path tie breaker).
                    for (index, left) in once.iter().enumerate() {
                        for right in &once[index + 1..] {
                            assert_ne!(
                                compare(&sort_item(left), &sort_item(right), descriptor),
                                std::cmp::Ordering::Equal,
                                "{descriptor:?}: {left:?} vs {right:?}"
                            );
                        }
                    }
                    if folders_first {
                        let last_dir = once.iter().rposition(BrowseEntry::is_dir);
                        let first_file = once.iter().position(|e| !e.is_dir());
                        if let (Some(last_dir), Some(first_file)) = (last_dir, first_file) {
                            assert!(last_dir < first_file, "{descriptor:?}: folders lead");
                        }
                    }
                }
            }
        }

        // Direction flips only the primary key: two size-equal files keep their natural-name
        // tie order in both directions.
        let descriptor = SortDescriptor {
            field: SortField::Size,
            ascending: true,
            folders_first: true,
        };
        let position = |sorted: &[BrowseEntry], name: &str| {
            sorted.iter().position(|e| e.name == name).unwrap()
        };
        let mut ascending = entries.clone();
        ascending.sort_by(|a, b| compare(&sort_item(a), &sort_item(b), descriptor));
        let mut descending = entries.clone();
        descending.sort_by(|a, b| {
            compare(
                &sort_item(a),
                &sort_item(b),
                SortDescriptor {
                    ascending: false,
                    ..descriptor
                },
            )
        });
        assert!(position(&ascending, "file2.txt") < position(&ascending, "same.txt"));
        assert!(
            position(&descending, "file2.txt") < position(&descending, "same.txt"),
            "equal sizes keep their stable tie order in both directions"
        );
    }
}
