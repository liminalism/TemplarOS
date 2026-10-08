//! Per-user folder bookmarks. The database is authoritative only for Templar's own places.
//! GTK/libfm-qt bookmarks have independent writers: export is explicit, additive and atomic,
//! preserves unmanaged bytes, backs up the previous file, and detects intervening edits.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io::Write as _, path::{Path, PathBuf}};

#[derive(Clone, Debug)]
pub struct Bookmarks(pub templar_state::Store);

/// The result of an explicit export, including the recovery copy when the file changed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportResult {
    pub path: PathBuf,
    pub added: usize,
    pub already_present: usize,
    pub backup: Option<PathBuf>,
    pub ownership: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OwnedEntries {
    /// Exact lines appended by Templar. An existing URI is never adopted as owned.
    lines: Vec<String>,
}

// Serialize filed's exports. Other applications do not honor this lock, so each export also
// compares the original bytes and identity immediately before publishing either file.
static EXPORT_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

impl Bookmarks {
    pub fn set(&self, path: &Path, bookmarked: bool) -> Result<()> {
        ensure!(path.is_absolute(), "A bookmark needs an absolute folder path");
        let text = path.to_str().context("Path cannot be represented on this interface")?;
        if bookmarked {
            ensure!(path.is_dir(), "Only folders can be bookmarked");
            let label = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
            self.0.lock().execute(
                "INSERT INTO folder_bookmarks(path,label,at_ms) VALUES (?1,?2,?3) ON CONFLICT(path) DO NOTHING",
                rusqlite::params![text, label, templar_state::now_ms()],
            )?;
        } else {
            self.0.lock().execute("DELETE FROM folder_bookmarks WHERE path=?1", [text])?;
        }
        // Changing Home places never writes another application's bookmarks implicitly.
        Ok(())
    }

    pub fn export(&self) -> Result<ExportResult> {
        let path = gtk_bookmarks_path().context("No HOME/XDG_CONFIG_HOME for bookmark export")?;
        self.export_gtk_bookmarks(&path)
    }

    /// Add missing Home places only. Unbookmarking in Templar never removes a GTK entry, even
    /// one we previously exported: this is additive export, not two-way synchronization/import.
    pub fn export_gtk_bookmarks(&self, file: &Path) -> Result<ExportResult> {
        self.export_with(file, || {})
    }

    fn export_with(&self, file: &Path, before_publish: impl FnOnce()) -> Result<ExportResult> {
        let _lock = EXPORT_LOCK.lock();
        let parent = file.parent().context("Bookmark file has no parent")?;
        std::fs::create_dir_all(parent)?;
        let ownership = file.with_file_name("bookmarks.templaros-owned.json");
        let original = FileVersion::read(file)?;
        let owned_version = FileVersion::read(&ownership)?;
        let mut owned: OwnedEntries = match &owned_version {
            Some(version) => serde_json::from_slice(&version.bytes)
                .context("Invalid bookmark ownership sidecar; refusing to replace it")?,
            None => OwnedEntries::default(),
        };
        let mut bytes = original.as_ref().map_or_else(Vec::new, |version| version.bytes.clone());
        let mut uris: HashSet<Vec<u8>> = bytes.split(|byte| *byte == b'\n')
            .filter_map(bookmark_uri).collect();
        // Retain evidence only for owned lines still present verbatim. Changed/removed native
        // entries are not reclaimed, and a matching pre-existing URI is never made ours.
        let lines: HashSet<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
        let previous_owned = owned.lines.len();
        owned.lines.retain(|line| lines.contains(line.as_bytes()));
        let mut added = 0;
        let mut already_present = 0;
        for root in self.roots()? {
            let path = root.path.to_str().context("Bookmark path is not UTF-8")?;
            let uri = format!("file://{}", uri_encode(path));
            let key = bookmark_uri(uri.as_bytes()).context("Invalid exported bookmark URI")?;
            if !uris.insert(key) {
                already_present += 1;
                continue;
            }
            let mut line = uri;
            if root.path.file_name().is_some_and(|name| name != root.label.as_str()) {
                line.push(' ');
                line.push_str(&root.label.replace(['\r', '\n'], " "));
            }
            if !bytes.is_empty() && !bytes.ends_with(b"\n") { bytes.push(b'\n'); }
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
            owned.lines.push(line);
            added += 1;
        }
        let mut result = ExportResult {
            path: file.to_path_buf(), added, already_present, backup: None, ownership,
        };
        if added == 0 {
            if owned.lines.len() != previous_owned {
                let tracking = prepared(parent, &serde_json::to_vec(&owned)?)?;
                before_publish();
                ensure!(FileVersion::read(file)? == original, "Bookmark export conflict: native bookmarks changed");
                if let Some(expected) = &owned_version {
                    publish_checked(tracking, &result.ownership, expected, || {})?;
                } else {
                    tracking.persist_noclobber(&result.ownership).map_err(|error| error.error)?;
                }
                std::fs::File::open(parent)?.sync_all()?;
            }
            return Ok(result);
        }
        let replacement = prepared(parent, &bytes)?;
        let tracking = prepared(parent, &serde_json::to_vec(&owned)?)?;
        // Backups are unique and never overwrite an earlier recovery copy.
        if let Some(version) = &original {
            let backup = prepared(parent, &version.bytes)?;
            let name = parent.join(format!("bookmarks.backup-{}", uuid::Uuid::new_v4()));
            backup.persist_noclobber(&name).map_err(|error| error.error)?;
            result.backup = Some(name);
            std::fs::File::open(parent)?.sync_all()?;
        }
        before_publish();
        ensure!(FileVersion::read(file)? == original && FileVersion::read(&result.ownership)? == owned_version,
            "Bookmark export conflict: the bookmarks or ownership file changed; retry from a fresh read");
        if let Some(expected) = &original {
            publish_checked(replacement, file, expected, || {})?;
        } else {
            replacement.persist_noclobber(file).map_err(|error| error.error)?;
        }
        // Record only a confirmed publication. A crash can lose our ownership evidence but
        // cannot falsely adopt a line another application added after an aborted export.
        if let Some(expected) = &owned_version {
            publish_checked(tracking, &result.ownership, expected, || {})?;
        } else {
            tracking.persist_noclobber(&result.ownership).map_err(|error| error.error)?;
        }
        std::fs::File::open(parent)?.sync_all()?;
        Ok(result)
    }

    pub fn roots(&self) -> Result<Vec<crate::browse::BrowserRoot>> {
        let db = self.0.lock();
        let mut query = db.prepare("SELECT path,label FROM folder_bookmarks ORDER BY at_ms,path")?;
        let rows = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (path, label) = row?;
            Ok(crate::browse::BrowserRoot {
                key: format!("bookmark:{path}"), label, available: Path::new(&path).is_dir(),
                path: path.into(), kind: crate::browse::RootKind::Place, volume: None,
            })
        }).collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileVersion {
    bytes: Vec<u8>,
    dev: u64,
    inode: u64,
    modified_ns: (i64, i64),
    changed_ns: (i64, i64),
}

impl FileVersion {
    fn read(path: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        ensure!(metadata.is_file() && !metadata.file_type().is_symlink(),
            "Refusing to replace a non-regular bookmark file: {}", path.display());
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::io::Read as _;
        let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
        let opened = file.metadata()?;
        ensure!(opened.dev() == metadata.dev() && opened.ino() == metadata.ino(),
            "Bookmark export conflict: the file identity changed while opening");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        ensure!(opened.mtime() == after.mtime() && opened.mtime_nsec() == after.mtime_nsec()
            && opened.ctime() == after.ctime() && opened.ctime_nsec() == after.ctime_nsec(),
            "Bookmark export conflict: the file changed while reading");
        Ok(Some(Self {
            bytes, dev: opened.dev(), inode: opened.ino(),
            modified_ns: (opened.mtime(), opened.mtime_nsec()),
            changed_ns: (opened.ctime(), opened.ctime_nsec()),
        }))
    }

    // Moving the inode can change ctime; its identity, data and mtime must still match.
    fn same_after_move(&self, other: &Self) -> bool {
        self.dev == other.dev && self.inode == other.inode
            && self.modified_ns == other.modified_ns && self.bytes == other.bytes
    }
}

fn prepared(parent: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    Ok(file)
}

/// Linux exchange lets us inspect the displaced inode instead of blindly replacing a pathname
/// after an optimistic check. A conflicting write is restored when possible and always retained
/// in a recovery file, including a second writer racing the rollback.
fn publish_checked(
    replacement: tempfile::NamedTempFile, destination: &Path, expected: &FileVersion,
    before_exchange: impl FnOnce(),
) -> Result<()> {
    let published = FileVersion::read(replacement.path())?.context("Prepared bookmark export disappeared")?;
    ensure!(FileVersion::read(destination)?.as_ref() == Some(expected), "Bookmark export conflict: native bookmarks changed");
    before_exchange();
    exchange(replacement.path(), destination)?;
    if FileVersion::read(replacement.path()).ok().flatten().is_some_and(|displaced| displaced.same_after_move(expected)) {
        return Ok(());
    }
    // Never knowingly roll back over an independently replaced destination. If a writer races
    // this check, exchange retains that writer's file at the recovery path rather than deleting it.
    let rollback = if FileVersion::read(destination).ok().flatten().is_some_and(|current| current.same_after_move(&published)) {
        exchange(replacement.path(), destination).err()
    } else { None };
    let (_file, recovery) = replacement.keep().map_err(|error| error.error)?;
    if let Some(parent) = destination.parent() { std::fs::File::open(parent)?.sync_all()?; }
    anyhow::bail!("Bookmark export conflict: intervening changes preserved; recovery file: {}{}",
        recovery.display(), rollback.map_or_else(String::new, |error| format!("; rollback failed: {error}")))
}

fn exchange(left: &Path, right: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(rustix::fs::CWD, left, rustix::fs::CWD, right, rustix::fs::RenameFlags::EXCHANGE)
        .map_err(std::io::Error::from)
}

fn gtk_bookmarks_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("gtk-3.0/bookmarks"))
}

fn bookmark_uri(line: &[u8]) -> Option<Vec<u8>> {
    let uri = line.split(|byte| byte.is_ascii_whitespace()).next()?;
    if uri.is_empty() { return None; }
    let mut decoded = Vec::with_capacity(uri.len());
    let mut offset = 0;
    while offset < uri.len() {
        if uri[offset] == b'%' && offset + 2 < uri.len() {
            let hex = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
            if let (Some(high), Some(low)) = (hex(uri[offset + 1]), hex(uri[offset + 2])) {
                decoded.push(high * 16 + low);
                offset += 3;
                continue;
            }
        }
        decoded.push(uri[offset]);
        offset += 1;
    }
    if decoded.starts_with(b"file://localhost/") {
        decoded.drain(7..16);
    }
    Some(decoded)
}

fn uri_encode(path: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => encoded.push(byte as char),
            _ => { let _ = write!(encoded, "%{byte:02X}"); }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Bookmarks, PathBuf, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("My Notes é ; $(not-a-command)");
        std::fs::create_dir_all(&folder).unwrap();
        let bookmarks = Bookmarks(templar_state::Store::in_memory().unwrap());
        bookmarks.set(&folder, true).unwrap();
        let file = home.path().join("config/gtk-3.0/bookmarks");
        (home, bookmarks, folder, file)
    }

    #[test]
    fn bookmark_export_preserves_native_entries_backup_and_ownership_across_relogin() {
        let (_home, bookmarks, folder, file) = fixture();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let native = b"smb://server/share Native place\nfile:///old-place Old\n#opaque\xff";
        std::fs::write(&file, native).unwrap();
        let result = bookmarks.export_gtk_bookmarks(&file).unwrap();
        assert_eq!(result.added, 1);
        assert_eq!(std::fs::read(result.backup.unwrap()).unwrap(), native);
        let bytes = std::fs::read(&file).unwrap();
        assert!(bytes.starts_with(native));
        assert!(String::from_utf8_lossy(&bytes).contains("My%20Notes%20%C3%A9%20%3B%20%24%28not-a-command%29"));
        let tracking: OwnedEntries = serde_json::from_slice(&std::fs::read(&result.ownership).unwrap()).unwrap();
        assert_eq!(tracking.lines.len(), 1);
        assert!(!tracking.lines[0].contains("old-place"));
        // A new service handle does not rewrite native edits, and unbookmarking is not deletion.
        bookmarks.set(&folder, false).unwrap();
        let native_edit = [bytes.as_slice(), b"file:///later Added in PCManFM\n"].concat();
        std::fs::write(&file, &native_edit).unwrap();
        let restarted = Bookmarks(bookmarks.0.clone());
        assert_eq!(restarted.export_gtk_bookmarks(&file).unwrap().added, 0);
        assert_eq!(std::fs::read(&file).unwrap(), native_edit);
    }

    #[test]
    fn bookmark_export_deduplicates_native_uri_without_claiming_ownership() {
        let (_home, bookmarks, folder, file) = fixture();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let native = format!("file://localhost{} Native label\n", uri_encode(folder.to_str().unwrap()).replace("%3B", "%3b"));
        std::fs::write(&file, &native).unwrap();
        let result = bookmarks.export_gtk_bookmarks(&file).unwrap();
        assert_eq!(result.added, 0);
        assert_eq!(result.already_present, 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), native);
        assert!(!result.ownership.exists());
    }

    #[test]
    fn bookmark_export_drops_ownership_of_edited_native_labels_without_rewriting_gtk() {
        let (_home, bookmarks, folder, file) = fixture();
        let exported = bookmarks.export_gtk_bookmarks(&file).unwrap();
        let native = format!("file://{} Edited native label\n", uri_encode(folder.to_str().unwrap()));
        std::fs::write(&file, &native).unwrap();
        let result = bookmarks.export_gtk_bookmarks(&file).unwrap();
        assert_eq!((result.added, result.already_present), (0, 1));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), native);
        let tracking: OwnedEntries = serde_json::from_slice(&std::fs::read(exported.ownership).unwrap()).unwrap();
        assert!(tracking.lines.is_empty(), "edited native entry was falsely adopted");
    }

    #[test]
    fn bookmark_export_detects_intervening_native_edits_and_does_not_publish_tracking() {
        let (_home, bookmarks, _folder, file) = fixture();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"file:///before\n").unwrap();
        let error = bookmarks.export_with(&file, || std::fs::write(&file, b"file:///intervening\n").unwrap()).unwrap_err();
        assert!(error.to_string().contains("conflict"));
        assert_eq!(std::fs::read(&file).unwrap(), b"file:///intervening\n");
        assert!(!file.with_file_name("bookmarks.templaros-owned.json").exists());
    }

    #[test]
    fn bookmark_export_detects_the_last_check_to_rename_race_and_preserves_both_versions() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("bookmarks");
        std::fs::write(&destination, b"file:///original\n").unwrap();
        let expected = FileVersion::read(&destination).unwrap().unwrap();
        let replacement = prepared(home.path(), b"file:///original\nfile:///templar\n").unwrap();
        let recovery = replacement.path().to_path_buf();
        let error = publish_checked(replacement, &destination, &expected, || {
            std::fs::write(&destination, b"file:///native-intervening\n").unwrap();
        }).unwrap_err();
        assert!(error.to_string().contains("conflict"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"file:///native-intervening\n");
        assert_eq!(std::fs::read(&recovery).unwrap(), b"file:///original\nfile:///templar\n");
    }

    #[test]
    fn bookmark_export_refuses_symlinks_and_corrupt_ownership_without_data_loss() {
        let (home, bookmarks, _folder, file) = fixture();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let target = home.path().join("unmanaged");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(bookmarks.export_gtk_bookmarks(&file).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_file(&file).unwrap();
        std::fs::write(&file, b"file:///native\n").unwrap();
        std::fs::write(file.with_file_name("bookmarks.templaros-owned.json"), b"not json").unwrap();
        assert!(bookmarks.export_gtk_bookmarks(&file).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"file:///native\n");
    }
}
