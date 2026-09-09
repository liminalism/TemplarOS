//! Per-user folder bookmarks, shared by every Files window. Mutations are atomic and never
//! change the referenced directory.
//!
//! filed is the authority, and the GTK bookmarks file is a one-way export of it. pcmanfm-qt's
//! Places sidebar (libfm-qt) reads `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, and that is the only
//! route it offers — there is no plugin API to teach it about `Files.Browse1` — so every mutation
//! rewrites that file. Nothing reads it back: importing would make two writers of one list and
//! there would be no answer to which one is right.
use anyhow::{Context, Result, ensure};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Bookmarks(pub templar_state::Store);

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
                rusqlite::params![text, label, templar_state::now_ms()],
            )?;
        } else {
            self.0
                .lock()
                .execute("DELETE FROM folder_bookmarks WHERE path=?1", [text])?;
        }
        // Best-effort: a read-only or absent home must not fail a bookmark that the store already
        // committed. The export is a convenience for another application, not part of the write.
        if let Some(file) = gtk_bookmarks_path()
            && let Err(error) = self.export_gtk_bookmarks(&file)
        {
            tracing::warn!(%error, path = %file.display(), "exporting GTK bookmarks");
        }
        Ok(())
    }

    /// Rewrite `file` as the GTK bookmarks list: one `file://` URI per line, with the label after
    /// a space when it differs from the folder's own name (the form GTK and libfm-qt both parse).
    pub fn export_gtk_bookmarks(&self, file: &Path) -> Result<()> {
        let mut text = String::new();
        for root in self.roots()? {
            let Some(path) = root.path.to_str() else {
                continue;
            };
            let _ = write!(text, "file://{}", uri_encode(path));
            if root
                .path
                .file_name()
                .is_some_and(|name| name != root.label.as_str())
            {
                text.push(' ');
                text.push_str(&root.label);
            }
            text.push('\n');
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))
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

/// `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, the file libfm-qt's Places sidebar reads. `None` when
/// this process has no home at all, which is a service under test, not a session.
fn gtk_bookmarks_path() -> Option<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(config.join("gtk-3.0/bookmarks"))
}

/// Percent-encode the bytes a `file://` URI may not carry literally. Deliberately minimal: a path
/// is already absolute and already UTF-8 here, so only the delimiters and the characters that end
/// a bookmarks line need escaping.
fn uri_encode(path: &str) -> String {
    path.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_export_is_the_file_libfm_qt_reads_and_survives_a_space_in_a_name() {
        let home = tempfile::tempdir().expect("a temporary home");
        let folder = home.path().join("My Notes");
        std::fs::create_dir_all(&folder).expect("a folder to bookmark");
        let bookmarks = Bookmarks(templar_state::Store::in_memory().expect("a store"));
        bookmarks.set(&folder, true).expect("bookmarking a folder");

        let file = home.path().join("config/gtk-3.0/bookmarks");
        bookmarks.export_gtk_bookmarks(&file).expect("exporting");
        let text = std::fs::read_to_string(&file).expect("the exported file");
        assert_eq!(
            text,
            format!("file://{}\n", uri_encode(&folder.to_string_lossy())),
            "one URI per line, no label because it is the folder's own name"
        );
        assert!(text.contains("My%20Notes"), "{text}");

        // Removing the bookmark rewrites the file rather than leaving the stale line: filed is
        // the authority, so the export always says exactly what filed holds.
        bookmarks.set(&folder, false).expect("unbookmarking");
        bookmarks.export_gtk_bookmarks(&file).expect("re-exporting");
        assert_eq!(std::fs::read_to_string(&file).expect("the file"), "");
    }
}
