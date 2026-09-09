//! Trash drawer — what is actually in the trash, and the two things done with it from Home.
//!
//! This drawer used to render the journalled trash and restore operations from
//! `Files.Operations1.Recent`, and said so in its empty state, because no interface listed what
//! was *in* the trash: `templar-filed` owned the freedesktop trash directories but exposed no
//! enumeration, and a shell that scanned `~/.local/share/Trash` itself would be exactly the
//! duplicated, disagreeing state the file service exists to remove.
//!
//! `Files.Trash1` is that interface, so the drawer now shows the trash — every trash, the home
//! one and each mounted volume's, which the service sweeps so no client has to know there is more
//! than one. Each row carries the path it names, so clicking one restores that item; emptying is
//! the declared `trash.empty` action. Both go back through `Trash1`, which journals them like any
//! other file operation, so Home can never hold an opinion about the trash that the file manager
//! does not share.

use crate::drawers::DrawerFeed;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::path::PathBuf;

/// `org.templaros.Files.Trash1`, as much of it as Home uses.
#[async_trait]
pub trait TrashClient {
    async fn list(&self) -> Result<String>;
    async fn restore(&self, trashed_paths: Vec<String>) -> Result<String>;
    async fn empty(&self) -> Result<String>;
}

#[async_trait]
impl TrashClient for templar_client::proxies::FilesTrashProxy<'_> {
    async fn list(&self) -> Result<String> {
        self.list().await.context("Files.Trash1.List")
    }
    async fn restore(&self, trashed_paths: Vec<String>) -> Result<String> {
        self.restore(trashed_paths, "")
            .await
            .context("Files.Trash1.Restore")
    }
    async fn empty(&self) -> Result<String> {
        self.empty("").await.context("Files.Trash1.Empty")
    }
}

/// One item in the trash, as `Trash1.List` reports it. Unknown fields are ignored so the service
/// can add columns without this refusing to parse.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct TrashEntry {
    pub name: String,
    pub trashed_path: PathBuf,
    pub original_path: PathBuf,
    pub deletion_date: String,
    #[serde(default)]
    pub is_directory: bool,
    #[serde(default)]
    pub size_bytes: u64,
}

pub fn parse(entries_json: &str) -> Result<Vec<TrashEntry>> {
    serde_json::from_str(entries_json).context("parsing Files.Trash1.List JSON")
}

pub async fn render(client: &impl TrashClient, limit: u32) -> Result<String> {
    Ok(feed(client, limit).await?.to_text())
}

pub async fn feed(client: &impl TrashClient, limit: u32) -> Result<DrawerFeed> {
    Ok(feed_entries(&parse(&client.list().await?)?, limit))
}

/// The listing: how many items are in the trash, then one row each, newest deletion first.
///
/// The headline is a count of *contents*, which the operations journal could never honestly
/// give — it knew what had been trashed, not what was still there.
pub fn feed_entries(entries: &[TrashEntry], limit: u32) -> DrawerFeed {
    if entries.is_empty() {
        return DrawerFeed::vacant("empty");
    }
    let shown = entries.iter().take(limit as usize);
    let mut feed = DrawerFeed::new(
        DrawerFeed::count(entries.len(), "item", "items", "empty"),
        shown
            .clone()
            .map(|entry| {
                format!(
                    "{:<10} {:>8}  {}",
                    day_of(&entry.deletion_date),
                    if entry.is_directory {
                        "folder".to_owned()
                    } else {
                        crate::format::bytes(entry.size_bytes)
                    },
                    entry.name,
                )
            })
            .collect(),
    );
    // Each row names the item it shows, so clicking it restores *that* one. Kept beside the
    // lines rather than parsed back out of them: a file name may contain anything at all.
    feed.row_targets = shown
        .map(|entry| Some(entry.trashed_path.to_string_lossy().into_owned()))
        .collect();
    feed
}

/// The date half of a `YYYY-MM-DDThh:mm:ss` deletion stamp; the whole stamp is too wide for a
/// tile and the time is rarely what distinguishes two rows.
fn day_of(deletion_date: &str) -> &str {
    deletion_date.split('T').next().unwrap_or(deletion_date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeTrash {
        listing: &'static str,
        restored: Mutex<Vec<Vec<String>>>,
        emptied: Mutex<u32>,
    }

    impl FakeTrash {
        fn with(listing: &'static str) -> Self {
            Self {
                listing,
                ..Self::default()
            }
        }
    }

    #[async_trait]
    impl TrashClient for FakeTrash {
        async fn list(&self) -> Result<String> {
            Ok(self.listing.to_owned())
        }
        async fn restore(&self, trashed_paths: Vec<String>) -> Result<String> {
            self.restored.lock().unwrap().push(trashed_paths);
            Ok("op-1".to_owned())
        }
        async fn empty(&self) -> Result<String> {
            *self.emptied.lock().unwrap() += 1;
            Ok("op-2".to_owned())
        }
    }

    const FIXTURE: &str = r#"[
        {"name":"report.pdf",
         "trashed_path":"/home/u/.local/share/Trash/files/report.pdf",
         "info_path":"/home/u/.local/share/Trash/info/report.pdf.trashinfo",
         "original_path":"/home/u/work/report.pdf",
         "deletion_date":"2026-08-30T18:04:11",
         "trash_root":"/home/u/.local/share/Trash",
         "is_directory":false,"size_bytes":2048},
        {"name":"old project",
         "trashed_path":"/media/stick/.Trash-1000/files/old project",
         "info_path":"/media/stick/.Trash-1000/info/old project.trashinfo",
         "original_path":"/media/stick/work/old project",
         "deletion_date":"2026-08-01T09:00:00",
         "trash_root":"/media/stick/.Trash-1000",
         "is_directory":true,"size_bytes":4096}
    ]"#;

    #[tokio::test]
    async fn the_drawer_counts_what_is_in_the_trash_across_every_volume() {
        let feed = feed(&FakeTrash::with(FIXTURE), 20).await.unwrap();
        assert_eq!(feed.headline, "2 items");
        assert!(feed.lines[0].contains("2026-08-30"));
        assert!(feed.lines[0].contains("report.pdf"));
        // The item on the removable volume is in the same listing as the home trash's.
        assert!(feed.lines[1].contains("old project"));
        assert!(feed.lines[1].contains("folder"), "{:?}", feed.lines[1]);
    }

    #[tokio::test]
    async fn every_row_names_the_item_it_shows_so_a_click_restores_that_one() {
        let feed = feed(&FakeTrash::with(FIXTURE), 20).await.unwrap();
        assert_eq!(feed.row_targets.len(), feed.lines.len());
        assert_eq!(
            feed.row_targets[1].as_deref(),
            Some("/media/stick/.Trash-1000/files/old project"),
            "a name with a space must survive as a target, not be re-parsed out of a row"
        );
    }

    #[tokio::test]
    async fn an_empty_trash_says_empty_rather_than_explaining_a_missing_interface() {
        let feed = feed(&FakeTrash::with("[]"), 20).await.unwrap();
        assert_eq!(feed.headline, "empty");
        assert!(feed.lines.is_empty());
        assert!(feed.available);
    }

    #[tokio::test]
    async fn the_listing_is_capped_but_the_headline_still_counts_everything() {
        let feed = feed(&FakeTrash::with(FIXTURE), 1).await.unwrap();
        assert_eq!(feed.headline, "2 items");
        assert_eq!(feed.lines.len(), 1);
        assert_eq!(feed.row_targets.len(), 1);
    }

    #[tokio::test]
    async fn restoring_and_emptying_go_back_through_the_service() {
        let client = FakeTrash::with(FIXTURE);
        client
            .restore(vec![
                "/home/u/.local/share/Trash/files/report.pdf".to_owned(),
            ])
            .await
            .unwrap();
        client.empty().await.unwrap();
        assert_eq!(
            *client.restored.lock().unwrap(),
            vec![vec![
                "/home/u/.local/share/Trash/files/report.pdf".to_owned()
            ]]
        );
        assert_eq!(*client.emptied.lock().unwrap(), 1);
    }
}
