//! Recent files — the Home Surface's Recent drawer, and the memory that makes "open with the
//! thing I used last time" possible.
//!
//! Kept here rather than in each application's own history for the same reason the last-used
//! directory is: three clients with three private lists is exactly the fragmentation the OS
//! exists to remove. Entries are keyed by canonical `AppId` as well as path, so both
//! "recent anywhere" and "recent in this application" are answerable from one table.

use anyhow::Result;
use lege_state::Store;
use lege_types::AppId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What the user did with the file. Opening and saving are worth telling apart: a file chooser
/// offering "recently saved" when you meant "recently opened" is a small but constant annoyance.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecentAction {
    Opened,
    Saved,
    Created,
    Modified,
}

impl RecentAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Saved => "saved",
            Self::Created => "created",
            Self::Modified => "modified",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Opened, Self::Saved, Self::Created, Self::Modified]
            .into_iter()
            .find(|action| action.as_str() == value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEntry {
    pub path: PathBuf,
    pub app_id: Option<AppId>,
    pub at_ms: i64,
    pub mime: Option<String>,
    pub action: RecentAction,
}

/// The recent list, in the one per-user state store.
#[derive(Clone, Debug)]
pub struct Recents {
    store: Store,
    /// Entries older than this are pruned on write. Zero keeps everything.
    pub retain: usize,
}

impl Recents {
    pub fn new(store: Store) -> Self {
        Self { store, retain: 500 }
    }

    /// Record a use. Re-recording the same (path, app, action) moves it to the top rather than
    /// adding a duplicate.
    pub fn record(
        &self,
        path: &Path,
        app_id: Option<&AppId>,
        action: RecentAction,
        mime: Option<&str>,
    ) -> Result<()> {
        let now = lege_state::now_ms();
        self.store.lock().execute(
            "INSERT INTO recents (path, app_id, at_ms, mime, action) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (path, app_id, action) DO UPDATE SET
                at_ms = excluded.at_ms, mime = COALESCE(excluded.mime, recents.mime)",
            rusqlite::params![
                path.to_string_lossy(),
                app_id.map(ToString::to_string),
                now,
                mime,
                action.as_str(),
            ],
        )?;
        if self.retain > 0 {
            self.store.lock().execute(
                "DELETE FROM recents WHERE rowid NOT IN
                   (SELECT rowid FROM recents ORDER BY at_ms DESC LIMIT ?1)",
                [self.retain as i64],
            )?;
        }
        Ok(())
    }

    /// Most recent first, optionally restricted to one application.
    pub fn list(&self, app_id: Option<&AppId>, limit: usize) -> Result<Vec<RecentEntry>> {
        let store = self.store.lock();
        let (sql, bind_app) = match app_id {
            Some(_) => (
                "SELECT path, app_id, at_ms, mime, action FROM recents
                  WHERE app_id = ?2 ORDER BY at_ms DESC LIMIT ?1",
                true,
            ),
            None => (
                "SELECT path, app_id, at_ms, mime, action FROM recents
                  ORDER BY at_ms DESC LIMIT ?1",
                false,
            ),
        };
        let mut statement = store.prepare(sql)?;
        let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<RecentEntry> {
            Ok(RecentEntry {
                path: PathBuf::from(row.get::<_, String>(0)?),
                app_id: row
                    .get::<_, Option<String>>(1)?
                    .and_then(|id| id.parse().ok()),
                at_ms: row.get(2)?,
                mime: row.get(3)?,
                action: RecentAction::parse(&row.get::<_, String>(4)?)
                    .unwrap_or(RecentAction::Opened),
            })
        };
        let rows = if bind_app {
            statement.query_map(
                rusqlite::params![limit as i64, app_id.map(ToString::to_string)],
                map,
            )?
        } else {
            statement.query_map(rusqlite::params![limit as i64], map)?
        };
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Drop entries whose file no longer exists — a stale Recent drawer is worse than a short
    /// one. Returns how many went.
    pub fn prune_missing(&self) -> Result<usize> {
        let paths: Vec<String> = {
            let store = self.store.lock();
            let mut statement = store.prepare("SELECT DISTINCT path FROM recents")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.filter_map(Result::ok).collect()
        };
        let gone: Vec<String> = paths
            .into_iter()
            .filter(|path| Path::new(path).symlink_metadata().is_err())
            .collect();
        let store = self.store.lock();
        for path in &gone {
            store.execute("DELETE FROM recents WHERE path = ?1", [path])?;
        }
        Ok(gone.len())
    }

    /// Forget one path entirely (the user's "remove from recent").
    pub fn forget(&self, path: &Path) -> Result<()> {
        self.store.lock().execute(
            "DELETE FROM recents WHERE path = ?1",
            [path.to_string_lossy()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recents() -> Recents {
        Recents::new(Store::in_memory().unwrap())
    }

    #[test]
    fn recording_the_same_file_twice_moves_it_up_rather_than_duplicating() {
        let recents = recents();
        let viewer: AppId = "org.legeos.Viewer".parse().unwrap();
        recents
            .record(
                Path::new("/a/one.pdf"),
                Some(&viewer),
                RecentAction::Opened,
                Some("application/pdf"),
            )
            .unwrap();
        recents
            .record(
                Path::new("/a/two.pdf"),
                Some(&viewer),
                RecentAction::Opened,
                None,
            )
            .unwrap();
        recents
            .record(
                Path::new("/a/one.pdf"),
                Some(&viewer),
                RecentAction::Opened,
                None,
            )
            .unwrap();

        let listed = recents.list(None, 10).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].path, PathBuf::from("/a/one.pdf"));
        assert_eq!(
            listed[0].mime.as_deref(),
            Some("application/pdf"),
            "a later record without a MIME type keeps the one we knew"
        );
    }

    #[test]
    fn opening_and_saving_are_separate_entries_and_filterable_by_app() {
        let recents = recents();
        let viewer: AppId = "org.legeos.Viewer".parse().unwrap();
        let editor: AppId = "org.legeos.Editor".parse().unwrap();
        recents
            .record(
                Path::new("/a/x.txt"),
                Some(&viewer),
                RecentAction::Opened,
                None,
            )
            .unwrap();
        recents
            .record(
                Path::new("/a/x.txt"),
                Some(&viewer),
                RecentAction::Saved,
                None,
            )
            .unwrap();
        recents
            .record(
                Path::new("/a/y.txt"),
                Some(&editor),
                RecentAction::Opened,
                None,
            )
            .unwrap();

        assert_eq!(recents.list(None, 10).unwrap().len(), 3);
        assert_eq!(recents.list(Some(&viewer), 10).unwrap().len(), 2);
        assert_eq!(recents.list(Some(&editor), 10).unwrap().len(), 1);
    }

    #[test]
    fn missing_files_are_pruned_and_a_path_can_be_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("here.txt");
        std::fs::write(&present, b"x").unwrap();
        let recents = recents();
        recents
            .record(&present, None, RecentAction::Opened, None)
            .unwrap();
        recents
            .record(
                Path::new("/gone/away.txt"),
                None,
                RecentAction::Opened,
                None,
            )
            .unwrap();

        assert_eq!(recents.prune_missing().unwrap(), 1);
        let listed = recents.list(None, 10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, present);

        recents.forget(&present).unwrap();
        assert!(recents.list(None, 10).unwrap().is_empty());
    }

    #[test]
    fn the_list_is_bounded() {
        let mut recents = recents();
        recents.retain = 3;
        for index in 0..10 {
            recents
                .record(
                    &PathBuf::from(format!("/a/{index}.txt")),
                    None,
                    RecentAction::Opened,
                    None,
                )
                .unwrap();
        }
        assert_eq!(recents.list(None, 100).unwrap().len(), 3);
    }

    #[test]
    fn an_action_name_round_trips() {
        for action in [
            RecentAction::Opened,
            RecentAction::Saved,
            RecentAction::Created,
            RecentAction::Modified,
        ] {
            assert_eq!(RecentAction::parse(action.as_str()), Some(action));
        }
        assert_eq!(RecentAction::parse("burned"), None);
    }
}
