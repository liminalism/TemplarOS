//! Trash drawer — and an honest note about where its contents come from.
//!
//! The plan asks this drawer for "contents, size and restore actions". No interface in
//! `interfaces/dbus/` lists what is *currently in* the trash: `lege-filed` owns the freedesktop
//! trash directories (`services/lege-filed/src/trash.rs`) but exposes no enumeration method, and
//! a shell that scanned `~/.local/share/Trash` itself would be exactly the duplicated,
//! disagreeing state the file service exists to remove — the shell would then have its own idea
//! of the trash, drifting from the file manager's.
//!
//! So this drawer renders what the services *do* report: the journalled trash and restore
//! operations from `Files.Operations1.Recent` — what was trashed, when, and what was put back.
//! That is a real, useful view (it is the "restore what I just deleted" case), and it is
//! deliberately labelled as recent trash activity rather than dressed up as a directory listing
//! it cannot honestly produce. A `Files.Trash1` interface listing entries with their sizes and
//! original paths is the follow-up this drawer wants; it is not this track's file to add.

use super::operations::{Operation, OperationsClient, parse};
use crate::drawers::DrawerFeed;
use anyhow::Result;

/// Keep only the operations that moved something into, or out of, the trash.
pub fn filter_trash(operations: &[Operation]) -> Vec<&Operation> {
    operations
        .iter()
        .filter(|operation| operation.kind.touches_trash())
        .collect()
}

pub async fn render(client: &impl OperationsClient, limit: u32) -> Result<String> {
    Ok(feed(client, limit).await?.to_text())
}

pub async fn feed(client: &impl OperationsClient, limit: u32) -> Result<DrawerFeed> {
    Ok(feed_operations(&parse(&client.recent(limit).await?)?))
}

pub fn render_operations(operations: &[Operation]) -> String {
    feed_operations(operations).to_text()
}

pub fn feed_operations(operations: &[Operation]) -> DrawerFeed {
    let trash = filter_trash(operations);
    if trash.is_empty() {
        return DrawerFeed::new(
            "no recent trash activity",
            vec![
                "(the file service journals trash and restore operations; it does not yet expose \
                 the trash's current contents)"
                    .to_owned(),
            ],
        );
    }
    // Deliberately "recent activity", not a count of what is in the trash: the service journals
    // operations and cannot yet answer what the trash currently holds, and a headline reading
    // "4 items" would be claiming otherwise.
    // "records", not "items": these are journalled operations, and the caveat below says so in
    // full for any tile tall enough to show it. The service cannot yet answer what the trash
    // currently holds, so no headline here may imply a count of its contents.
    let headline = DrawerFeed::count(trash.len(), "recent record", "recent records", "nothing");
    let mut lines = vec!["(recent activity, not a full listing)".to_owned()];
    lines.extend(
        trash
            .iter()
            .map(|operation| format!("{:<10} {}", operation.state, operation.kind.summary())),
    );
    DrawerFeed::new(headline, lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct FakeOperations(&'static str);

    #[async_trait]
    impl OperationsClient for FakeOperations {
        async fn recent(&self, _limit: u32) -> Result<String> {
            Ok(self.0.to_owned())
        }
    }

    const FIXTURE: &str = r#"[
        {"id":"11111111-1111-4111-8111-111111111111",
         "kind":{"kind":"copy","sources":["/home/u/a.iso"],"destination_dir":"/media/stick"},
         "state":"running","total_bytes":1048576,"done_bytes":524288,"total_items":1,"done_items":0,
         "error":null},
        {"id":"22222222-2222-4222-8222-222222222222",
         "kind":{"kind":"trash","paths":["/home/u/old.txt","/home/u/older.txt"]},
         "state":"committed","total_bytes":0,"done_bytes":0,"total_items":2,"done_items":2,
         "error":null},
        {"id":"44444444-4444-4444-8444-444444444444",
         "kind":{"kind":"restore-from-trash","paths":["/home/u/back.txt"]},
         "state":"committed","total_bytes":0,"done_bytes":0,"total_items":1,"done_items":1,
         "error":null}
    ]"#;

    #[test]
    fn only_trash_and_restore_operations_survive_the_filter() {
        let operations = parse(FIXTURE).unwrap();
        let trash = filter_trash(&operations);
        assert_eq!(
            trash.len(),
            2,
            "the copy must not appear in the Trash drawer"
        );
        assert!(trash[0].kind.summary().contains("to Trash"));
        assert!(trash[1].kind.summary().contains("from Trash"));
    }

    #[tokio::test]
    async fn render_labels_itself_as_activity_rather_than_a_listing() {
        let text = render(&FakeOperations(FIXTURE), 20).await.unwrap();
        assert!(text.contains("not a full listing"));
        assert!(text.contains("Move 2 items to Trash"));
        assert!(text.contains("Restore back.txt from Trash"));
        assert!(
            !text.contains("a.iso"),
            "an unrelated copy leaked into the Trash drawer"
        );
    }

    #[tokio::test]
    async fn an_empty_trash_explains_why_it_can_only_show_activity() {
        let text = render(&FakeOperations("[]"), 20).await.unwrap();
        assert!(text.contains("no recent trash activity"));
        assert!(text.contains("does not yet expose"));
    }
}
