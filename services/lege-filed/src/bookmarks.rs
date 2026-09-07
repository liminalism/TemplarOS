//! Per-user folder bookmarks, shared by every Files window. Mutations are atomic and never
//! change the referenced directory.
use anyhow::{Result, ensure};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Bookmarks(pub lege_state::Store);

impl Bookmarks {
    pub fn set(&self, path: &Path, bookmarked: bool) -> Result<()> {
        ensure!(
            path.is_absolute(),
            "A bookmark needs an absolute folder path"
        );
        let text = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Path cannot be represented on this interface"))?;
        if bookmarked {
            ensure!(path.is_dir(), "Only folders can be bookmarked");
            let label = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy();
            self.0.lock().execute(
                "INSERT INTO folder_bookmarks(path,label,at_ms) VALUES (?1,?2,?3) ON CONFLICT(path) DO NOTHING",
                rusqlite::params![text, label, lege_state::now_ms()],
            )?;
        } else {
            self.0
                .lock()
                .execute("DELETE FROM folder_bookmarks WHERE path=?1", [text])?;
        }
        Ok(())
    }

    pub fn roots(&self) -> Result<Vec<crate::browse::BrowserRoot>> {
        let db = self.0.lock();
        let mut query =
            db.prepare("SELECT path,label FROM folder_bookmarks ORDER BY at_ms,path")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (path, label) = row?;
            Ok(crate::browse::BrowserRoot {
                key: format!("bookmark:{path}"),
                label,
                available: Path::new(&path).is_dir(),
                path: path.into(),
                kind: crate::browse::RootKind::Place,
                volume: None,
            })
        })
        .collect()
    }
}
