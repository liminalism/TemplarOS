//! Structured file-browser data rendered by Home.
//!
//! These types mirror `org.legeos.Files.Browse1`'s JSON contract. The shell deliberately does
//! not depend on the daemon crate: it is a restartable client process and reaches file policy
//! only through the versioned D-Bus surface.

use serde::Deserialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use crate::home_surface::Drawer;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootKind {
    Place,
    Drive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrowserRoot {
    pub label: String,
    pub path: PathBuf,
    pub detail: String,
    pub kind: RootKind,
}

/// Places and mounted volumes, plus the first location each file drawer should open.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BrowserSources {
    pub roots: Vec<BrowserRoot>,
    pub starts: HashMap<Drawer, PathBuf>,
}

impl BrowserSources {
    pub fn start(&self, drawer: Drawer) -> Option<&Path> {
        self.starts.get(&drawer).map(PathBuf::as_path)
    }
}

pub fn is_file_drawer(drawer: Drawer) -> bool {
    matches!(
        drawer,
        Drawer::Downloads | Drawer::Documents | Drawer::Media | Drawer::Drives
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_browser_drawers_are_the_real_path_and_drive_surfaces() {
        for drawer in [
            Drawer::Downloads,
            Drawer::Documents,
            Drawer::Media,
            Drawer::Drives,
        ] {
            assert!(is_file_drawer(drawer));
        }
        assert!(!is_file_drawer(Drawer::Recent));
        assert!(!is_file_drawer(Drawer::Projects));
    }
}
