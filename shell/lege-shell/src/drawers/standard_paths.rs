//! Downloads, Documents and Media drawers: standard XDG user directories, plus the saved queries
//! that make a drawer *be* a query rather than only a directory.
//!
//! Planning doc 01 §1: "A drawer could represent a real directory, a mount, a saved query, a tag,
//! or a project. The underlying files should remain normal POSIX files at visible paths." So this
//! resolves the real directories from `user-dirs.dirs` (the file `xdg-user-dirs` writes, which
//! every other desktop tool already honours) and shows them as paths — no proprietary object
//! layer, nothing a terminal cannot also see.
//!
//! Saved queries come from `Files.Search1.SavedQueries`. They are listed beside the directories
//! rather than merged into them: a query is not a folder, and pretending otherwise is how a file
//! manager ends up with "smart folders" nobody can find on disk.

use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_client::proxies::FilesSearchProxy;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The three standard-path drawers, and which `XDG_*_DIR` keys each shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StandardDrawer {
    Downloads,
    Documents,
    Media,
}

impl StandardDrawer {
    /// `user-dirs.dirs` keys, in display order. Media is three directories because "media" is
    /// what a person calls pictures, music and video collectively; XDG has no single key for it.
    pub fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Downloads => &["XDG_DOWNLOAD_DIR"],
            Self::Documents => &["XDG_DOCUMENTS_DIR"],
            Self::Media => &["XDG_PICTURES_DIR", "XDG_MUSIC_DIR", "XDG_VIDEOS_DIR"],
        }
    }

    /// The name shown at the head of the drawer.
    pub fn title(self) -> &'static str {
        match self {
            Self::Downloads => "Downloads",
            Self::Documents => "Documents",
            Self::Media => "Media",
        }
    }

    /// Fallback directory name for a key, used when `user-dirs.dirs` is absent or silent about
    /// it — the same English defaults `xdg-user-dirs` itself falls back to.
    fn default_name(key: &str) -> &'static str {
        match key {
            "XDG_DOWNLOAD_DIR" => "Downloads",
            "XDG_DOCUMENTS_DIR" => "Documents",
            "XDG_PICTURES_DIR" => "Pictures",
            "XDG_MUSIC_DIR" => "Music",
            "XDG_VIDEOS_DIR" => "Videos",
            _ => "",
        }
    }
}

/// Parse `user-dirs.dirs`: shell-style `KEY="$HOME/Name"` assignments, `#` comments.
///
/// Only `$HOME`-relative and absolute values are honoured; anything else is ignored rather than
/// guessed at, so a hand-edited file with an unexpected expansion degrades to the default name
/// instead of producing a path that does not exist.
pub fn parse_user_dirs(text: &str, home: &Path) -> Vec<(String, PathBuf)> {
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
            let value = value.trim().trim_matches('"');
            let path = if let Some(rest) = value.strip_prefix("$HOME/") {
                home.join(rest)
            } else if value == "$HOME" {
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

/// Resolve one drawer's directories against `user_dirs` (as parsed above), falling back to the
/// conventional English names under `home`.
pub fn directories_for(
    drawer: StandardDrawer,
    user_dirs: &[(String, PathBuf)],
    home: &Path,
) -> Vec<PathBuf> {
    drawer
        .keys()
        .iter()
        .map(|key| {
            user_dirs.iter().find(|(name, _)| name == key).map_or_else(
                || home.join(StandardDrawer::default_name(key)),
                |(_, path)| path.clone(),
            )
        })
        .collect()
}

/// Mirrors `services/lege-filed/src/search.rs::SavedQuery`'s wire JSON.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SavedQuery {
    pub name: String,
    pub query: String,
    pub roots: Vec<PathBuf>,
}

/// What the standard-path drawers need from `Files.Search1`, behind a trait so tests never touch
/// a session bus.
#[async_trait]
pub trait SearchClient: Send + Sync {
    async fn saved_queries(&self) -> Result<String>;
}

#[async_trait]
impl SearchClient for FilesSearchProxy<'_> {
    async fn saved_queries(&self) -> Result<String> {
        self.saved_queries()
            .await
            .context("Files.Search1.SavedQueries")
    }
}

/// Saved queries whose roots fall under one of `directories` — the ones that belong in *this*
/// drawer. A query with no roots at all searches everything indexed, so it belongs to no
/// particular drawer and is left out here rather than repeated in all three.
pub fn queries_under<'a>(
    queries: &'a [SavedQuery],
    directories: &[PathBuf],
) -> Vec<&'a SavedQuery> {
    queries
        .iter()
        .filter(|query| {
            !query.roots.is_empty()
                && query.roots.iter().any(|root| {
                    directories
                        .iter()
                        .any(|directory| root.starts_with(directory))
                })
        })
        .collect()
}

pub async fn render(
    client: &impl SearchClient,
    drawer: StandardDrawer,
    home: &Path,
    user_dirs_text: Option<&str>,
) -> Result<String> {
    let queries: Vec<SavedQuery> = serde_json::from_str(&client.saved_queries().await?)
        .context("parsing Files.Search1.SavedQueries queries_json")?;
    let user_dirs = user_dirs_text
        .map(|text| parse_user_dirs(text, home))
        .unwrap_or_default();
    Ok(render_parts(
        drawer,
        &directories_for(drawer, &user_dirs, home),
        &queries,
    ))
}

fn render_parts(drawer: StandardDrawer, directories: &[PathBuf], queries: &[SavedQuery]) -> String {
    let mut lines: Vec<String> = directories
        .iter()
        .map(|directory| {
            format!(
                "{:<40} {}",
                directory.display(),
                if directory.is_dir() {
                    "directory"
                } else {
                    "directory (not present)"
                }
            )
        })
        .collect();
    for query in queries_under(queries, directories) {
        lines.push(format!("{:<40} saved query: {}", query.name, query.query));
    }
    if lines.is_empty() {
        return format!("{} — nothing", drawer.title());
    }
    lines.join("\n")
}

/// The user's home directory, for resolving `user-dirs.dirs`. `None` when `$HOME` is unset,
/// which is a real state in a systemd unit with a scrubbed environment.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read `$XDG_CONFIG_HOME/user-dirs.dirs` (or `~/.config/user-dirs.dirs`). Absent is normal —
/// the caller falls back to the conventional names.
pub fn read_user_dirs(home: &Path) -> Option<String> {
    let config_home =
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home.join(".config"), PathBuf::from);
    std::fs::read_to_string(config_home.join("user-dirs.dirs")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSearch(&'static str);

    #[async_trait]
    impl SearchClient for FakeSearch {
        async fn saved_queries(&self) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    const USER_DIRS: &str = r#"
# This file is written by xdg-user-dirs-update
XDG_DOWNLOAD_DIR="$HOME/Hämtningar"
XDG_DOCUMENTS_DIR="$HOME/Dokument"
XDG_PICTURES_DIR="/mnt/photos"
XDG_DESKTOP_DIR="$HOME/Desktop"
"#;

    #[test]
    fn user_dirs_parses_home_relative_and_absolute_values() {
        let home = Path::new("/home/u");
        let dirs = parse_user_dirs(USER_DIRS, home);
        assert_eq!(
            dirs.iter()
                .find(|(k, _)| k == "XDG_DOWNLOAD_DIR")
                .unwrap()
                .1,
            PathBuf::from("/home/u/Hämtningar"),
            "a localised directory name must survive verbatim"
        );
        assert_eq!(
            dirs.iter()
                .find(|(k, _)| k == "XDG_PICTURES_DIR")
                .unwrap()
                .1,
            PathBuf::from("/mnt/photos")
        );
        assert!(
            !dirs.iter().any(|(k, _)| k.starts_with('#')),
            "a comment was parsed as a key"
        );
    }

    #[test]
    fn a_missing_key_falls_back_to_the_conventional_name() {
        let home = Path::new("/home/u");
        let dirs = parse_user_dirs(USER_DIRS, home);
        // Music and Videos are absent from the fixture.
        let media = directories_for(StandardDrawer::Media, &dirs, home);
        assert_eq!(
            media,
            vec![
                PathBuf::from("/mnt/photos"),
                PathBuf::from("/home/u/Music"),
                PathBuf::from("/home/u/Videos"),
            ]
        );
    }

    #[test]
    fn with_no_user_dirs_file_at_all_every_drawer_still_resolves() {
        let home = Path::new("/home/u");
        assert_eq!(
            directories_for(StandardDrawer::Downloads, &[], home),
            vec![PathBuf::from("/home/u/Downloads")]
        );
        assert_eq!(
            directories_for(StandardDrawer::Documents, &[], home),
            vec![PathBuf::from("/home/u/Documents")]
        );
    }

    #[test]
    fn only_queries_rooted_in_this_drawer_appear_in_it() {
        let queries = vec![
            SavedQuery {
                name: "Invoices".into(),
                query: "invoice".into(),
                roots: vec![PathBuf::from("/home/u/Documents/finance")],
            },
            SavedQuery {
                name: "Holiday".into(),
                query: "2026".into(),
                roots: vec![PathBuf::from("/mnt/photos/2026")],
            },
            SavedQuery {
                name: "Everything".into(),
                query: "report".into(),
                roots: vec![],
            },
        ];
        let documents = [PathBuf::from("/home/u/Documents")];
        let matched = queries_under(&queries, &documents);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "Invoices");

        // A rootless query searches everything, so it belongs to no one drawer.
        let photos = [PathBuf::from("/mnt/photos")];
        assert_eq!(queries_under(&queries, &photos).len(), 1);
    }

    #[tokio::test]
    async fn render_lists_directories_and_their_saved_queries() {
        let json =
            r#"[{"name":"Invoices","query":"invoice","roots":["/home/u/Documents/finance"]}]"#;
        let text = render(
            &FakeSearch(json),
            StandardDrawer::Documents,
            Path::new("/home/u"),
            Some(USER_DIRS),
        )
        .await
        .unwrap();
        assert!(text.contains("/home/u/Dokument"));
        // The query's root is under the *default* Documents path, not the localised one, so it
        // is correctly excluded — the drawer shows the directory the user actually configured.
        assert!(!text.contains("Invoices"));

        let text = render(
            &FakeSearch(json),
            StandardDrawer::Documents,
            Path::new("/home/u"),
            None,
        )
        .await
        .unwrap();
        assert!(text.contains("/home/u/Documents"));
        assert!(text.contains("saved query: invoice"));
    }

    #[tokio::test]
    async fn a_directory_that_does_not_exist_says_so_rather_than_vanishing() {
        let text = render(
            &FakeSearch("[]"),
            StandardDrawer::Downloads,
            Path::new("/definitely/not/here"),
            None,
        )
        .await
        .unwrap();
        assert!(text.contains("not present"));
    }

    #[tokio::test]
    async fn malformed_saved_query_json_is_a_clear_error_not_a_panic() {
        let error = render(
            &FakeSearch("not json"),
            StandardDrawer::Media,
            Path::new("/home/u"),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("SavedQueries"));
    }
}
