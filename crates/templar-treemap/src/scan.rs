//! A bounded, std-only directory walk: a tray shows one directory, and a tile's interior is a
//! few hundred pixels across, so a scan that stops at a few thousand entries or a few levels
//! loses nothing visible and keeps explicit fallback work bounded. Symlinks are not followed;
//! limits, disappearing entries and unreadable paths are reported, never silently called complete.

use std::path::{Path, PathBuf};

/// One scanned or indexed entry. Scanned directories carry size 0; an indexed directory may
/// carry cached bytes that are not represented by its visible children. [`crate::build_tree`]
/// adds those bytes to the visible children's sizes without inventing file rows.
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

/// The measured rows and what prevented a complete picture. A missing root has no invented row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub entries: Vec<RawEntry>,
    pub complete: bool,
    pub entry_limit: bool,
    pub depth_limit: bool,
    pub missing_entries: u64,
    pub unreadable_entries: u64,
    pub last_error: Option<String>,
}

impl ScanOutcome {
    fn note_error(&mut self, error: &std::io::Error) {
        self.complete = false;
        if error.kind() == std::io::ErrorKind::NotFound {
            self.missing_entries += 1;
        } else {
            self.unreadable_entries += 1;
        }
        if self.last_error.is_none() {
            self.last_error = Some(error.to_string());
        }
    }
}

/// Walk `root` breadth-first within `limits`, reporting completion from the traversal itself.
/// A depth-boundary directory is read only far enough to discover whether it has unvisited rows;
/// an empty boundary is complete. The queue keeps entry indices, not duplicate owned paths.
pub fn scan(root: &Path, limits: ScanLimits) -> ScanOutcome {
    let mut outcome = ScanOutcome {
        complete: true,
        ..ScanOutcome::default()
    };
    match root.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            outcome.note_error(&std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "treemap root is not a directory (symlinks are not followed)",
            ));
            return outcome;
        }
        Err(error) => {
            outcome.note_error(&error);
            return outcome;
        }
    }
    if limits.max_entries == 0 {
        outcome.entry_limit = true;
        outcome.complete = false;
        return outcome;
    }
    outcome.entries.push(RawEntry {
        path: root.to_path_buf(),
        size: 0,
        is_dir: true,
    });
    let mut queue = std::collections::VecDeque::from([(0usize, 0usize)]);
    while let Some((directory, depth)) = queue.pop_front() {
        let reader = match std::fs::read_dir(&outcome.entries[directory].path) {
            Ok(reader) => reader,
            Err(error) => {
                outcome.note_error(&error);
                continue;
            }
        };
        for entry in reader {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    outcome.note_error(&error);
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match path.symlink_metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    outcome.note_error(&error);
                    continue;
                }
            };
            if !metadata.is_dir() && !metadata.is_file() {
                continue;
            }
            if depth >= limits.max_depth {
                outcome.depth_limit = true;
                outcome.complete = false;
                break;
            }
            if outcome.entries.len() >= limits.max_entries {
                outcome.entry_limit = true;
                outcome.complete = false;
                return outcome;
            }
            let is_dir = metadata.is_dir();
            let index = outcome.entries.len();
            outcome.entries.push(RawEntry {
                path,
                size: if is_dir { 0 } else { metadata.len() },
                is_dir,
            });
            if is_dir {
                queue.push_back((index, depth + 1));
            }
        }
    }
    outcome
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
        assert!(all.complete);
        assert_eq!(all.entries.iter().filter(|e| !e.is_dir).count(), 3);
        assert_eq!(all.entries[0].path, dir);

        let shallow = scan(
            &dir,
            ScanLimits {
                max_entries: 1000,
                max_depth: 1,
            },
        );
        assert!(
            shallow
                .entries
                .iter()
                .all(|e| e.path.strip_prefix(&dir).unwrap().components().count() <= 1)
        );
        assert!(!shallow.complete && shallow.depth_limit);

        let capped = scan(
            &dir,
            ScanLimits {
                max_entries: 2,
                max_depth: 6,
            },
        );
        assert_eq!(capped.entries.len(), 2);
        assert!(!capped.complete && capped.entry_limit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_depth_boundary_and_exact_entry_cap_are_complete_but_missing_roots_are_not() {
        let dir =
            std::env::temp_dir().join(format!("templar-treemap-boundary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty = scan(
            &dir,
            ScanLimits {
                max_entries: 1,
                max_depth: 0,
            },
        );
        assert!(empty.complete);
        std::fs::write(dir.join("only"), b"x").unwrap();
        let exact = scan(
            &dir,
            ScanLimits {
                max_entries: 2,
                max_depth: 1,
            },
        );
        assert!(exact.complete);
        let shallow = scan(
            &dir,
            ScanLimits {
                max_entries: 100,
                max_depth: 0,
            },
        );
        assert!(!shallow.complete && shallow.depth_limit);
        let missing = scan(&dir.join("missing"), ScanLimits::default());
        assert!(!missing.complete && missing.entries.is_empty());
        assert_eq!(missing.missing_entries, 1);
        let not_directory = scan(&dir.join("only"), ScanLimits::default());
        assert!(!not_directory.complete && not_directory.unreadable_entries == 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn symlink_targets_are_neither_followed_nor_counted_as_files() {
        let dir =
            std::env::temp_dir().join(format!("templar-treemap-symlink-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("owned")).unwrap();
        std::fs::write(dir.join("owned/real"), b"x").unwrap();
        std::os::unix::fs::symlink("owned", dir.join("link")).unwrap();
        std::os::unix::fs::symlink("missing", dir.join("dangling")).unwrap();
        let measured = scan(&dir, ScanLimits::default());
        assert!(measured.complete);
        assert_eq!(measured.entries.len(), 3);
        assert!(
            !measured
                .entries
                .iter()
                .any(|entry| entry.path.ends_with("link") || entry.path.ends_with("dangling"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_directory_keeps_the_known_row_and_reports_incomplete_for_unprivileged_users() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            std::env::temp_dir().join(format!("templar-treemap-denied-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("private")).unwrap();
        std::fs::write(dir.join("private/secret"), b"x").unwrap();
        std::fs::set_permissions(dir.join("private"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let denied_here = std::fs::read_dir(dir.join("private")).is_err();
        let measured = scan(&dir, ScanLimits::default());
        std::fs::set_permissions(dir.join("private"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        if denied_here {
            assert!(!measured.complete && measured.unreadable_entries > 0);
            assert!(measured.last_error.is_some());
        } // A privileged test process cannot demonstrate DAC denial.
    }
}
