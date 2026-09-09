//! A bounded, std-only directory walk: a tray shows one directory, and a tile's interior is a
//! few hundred pixels across, so a scan that stops at a few thousand entries or a few levels
//! loses nothing visible and keeps the Home Field's refresh loop cheap. Symlinks are not
//! followed; unreadable entries are skipped.

use std::path::{Path, PathBuf};

/// One scanned entry. Directories carry size 0; [`crate::build_tree`] aggregates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEntry {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

/// Where the walk stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanLimits {
    pub max_entries: usize,
    pub max_depth: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_depth: 6,
        }
    }
}

/// Walk `root` breadth-first within `limits`. The root itself is the first entry.
pub fn scan(root: &Path, limits: ScanLimits) -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0usize));
    entries.push(RawEntry {
        path: root.to_path_buf(),
        size: 0,
        is_dir: true,
    });
    while let Some((dir, depth)) = queue.pop_front() {
        if depth >= limits.max_depth || entries.len() >= limits.max_entries {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            if entries.len() >= limits.max_entries {
                break;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let path = entry.path();
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                entries.push(RawEntry {
                    path: path.clone(),
                    size: 0,
                    is_dir: true,
                });
                queue.push_back((path, depth + 1));
            } else if metadata.is_file() {
                entries.push(RawEntry {
                    path,
                    size: metadata.len(),
                    is_dir: false,
                });
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_temporary_tree_within_limits() {
        let dir = std::env::temp_dir().join(format!("templar-treemap-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("one.txt"), b"hello").unwrap();
        std::fs::write(dir.join("a/two.png"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("a/b/c/three.rs"), b"fn main() {}").unwrap();

        let all = scan(&dir, ScanLimits::default());
        assert_eq!(all.iter().filter(|e| !e.is_dir).count(), 3);
        assert_eq!(all[0].path, dir);

        let shallow = scan(
            &dir,
            ScanLimits {
                max_entries: 1000,
                max_depth: 1,
            },
        );
        assert!(
            shallow
                .iter()
                .all(|e| e.path.strip_prefix(&dir).unwrap().components().count() <= 1)
        );

        let capped = scan(
            &dir,
            ScanLimits {
                max_entries: 2,
                max_depth: 6,
            },
        );
        assert_eq!(capped.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
