//! Structured file-browser data rendered by Home.
//!
//! These types mirror `org.templaros.Files.Browse1`'s JSON contract. The shell deliberately does
//! not depend on the daemon crate: it is a restartable client process and reaches file policy
//! only through the versioned D-Bus surface.

use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::files_surface::PinnedLocation;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
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

#[derive(Clone, Debug, Eq, PartialEq, Deserialize)]
pub struct DirectoryListing {
    pub directory: PathBuf,
    pub parent: Option<PathBuf>,
    pub entries: Vec<BrowseEntry>,
    pub truncated: bool,
    pub limit: usize,
}

/// Places, mounted volumes and the machine's own file feeds, in the order the pinned list
/// offers them.
///
/// This used to be a list of roots plus a map from each file drawer to the path it opened at.
/// There are no file drawers now — one Files object goes everywhere — so what is left is the
/// list itself, and every entry is a `FilesLocation` rather than a path, which is what lets
/// Recent and Trash sit in the same list as a folder.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrowserSources {
    pub pinned: Vec<PinnedLocation>,
}

impl BrowserSources {
    /// The first directory in the list — where the Files object opens when it has no memory of
    /// anywhere better.
    pub fn first_directory(&self) -> Option<&Path> {
        self.pinned
            .iter()
            .find_map(|pinned| pinned.location.directory())
    }

    /// The pinned entry whose label matches, case-insensitively. How the compact Files tile
    /// turns a name it shows into somewhere to go.
    pub fn by_label(&self, label: &str) -> Option<&PinnedLocation> {
        self.pinned
            .iter()
            .find(|pinned| pinned.label.eq_ignore_ascii_case(label))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_surface::{FilesLocation, PinnedKind};

    fn sources() -> BrowserSources {
        BrowserSources {
            pinned: vec![
                PinnedLocation {
                    label: "Recent".into(),
                    detail: "Lately used".into(),
                    location: FilesLocation::Recent,
                    kind: PinnedKind::Feed,
                },
                PinnedLocation {
                    label: "Documents".into(),
                    detail: "Standard folder".into(),
                    location: FilesLocation::Directory("/home/a/Documents".into()),
                    kind: PinnedKind::Place,
                },
            ],
        }
    }

    /// The first *directory*, not the first row: Recent is a place to be but not a folder to
    /// open at, and a Files object with no memory has to land somewhere it can list.
    #[test]
    fn the_opening_location_is_the_first_real_directory() {
        assert_eq!(
            sources().first_directory(),
            Some(Path::new("/home/a/Documents"))
        );
        assert_eq!(BrowserSources::default().first_directory(), None);
    }

    #[test]
    fn a_pinned_row_is_found_by_the_label_it_shows() {
        assert_eq!(
            sources().by_label("recent").map(|pinned| &pinned.location),
            Some(&FilesLocation::Recent)
        );
        assert!(sources().by_label("Nowhere").is_none());
    }
}
