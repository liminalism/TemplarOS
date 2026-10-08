//! Supported GTK settings, also consumed by the Qt 5/6 `gtk3` platform-theme plugins.
//! Configuration is for supported future launches; writing files does not prove live toolkit reload.
//! A journal owns individual keys, never unrelated preferences or entire pre-existing files.
use serde::{Deserialize, Serialize};
use std::{fs, io::{Read, Write}, path::{Path, PathBuf}, sync::{Mutex, atomic::{AtomicU64, Ordering}}};
use templar_types::appearance::{Appearance, DARK};

const TARGETS: [&str; 2] = ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"];
const KEYS: [&str; 5] = ["gtk-theme-name", "gtk-application-prefer-dark-theme", "gtk-font-name", "gtk-icon-theme-name", "gtk-enable-animations"];
const JOURNAL: &str = "templaros/appearance-owned.json";
static SERIAL: Mutex<()> = Mutex::new(());
static TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
#[error("Toolkit appearance configuration incomplete after {applied:?}: {cause}")]
pub struct AppearanceConfigError {
    pub applied: Vec<PathBuf>,
    pub cause: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Journal { files: Vec<OwnedFile> }
#[derive(Serialize, Deserialize)]
struct OwnedFile { target: String, keys: Vec<OwnedKey> }
#[derive(Clone, Serialize, Deserialize)]
struct OwnedKey {
    name: String,
    original: Option<String>,
    written: Option<String>,
    // A saved intent may precede an interrupted file write. Both sides are owned until settled.
    previous: Vec<Option<String>>,
}
struct Change { target: String, before: Option<FileVersion>, after: String }

pub fn apply(config_home: &Path, appearance: Appearance) -> Result<Vec<PathBuf>, AppearanceConfigError> {
    configure(config_home, Some(appearance))
}
pub fn restore(config_home: &Path) -> Result<Vec<PathBuf>, AppearanceConfigError> {
    configure(config_home, None)
}
fn configure(home: &Path, appearance: Option<Appearance>) -> Result<Vec<PathBuf>, AppearanceConfigError> {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut applied = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        let journal_path = home.join(JOURNAL);
        let saved = FileVersion::read(&journal_path)?;
        let saved_text = saved.as_ref().map_or("", |version| version.text.as_str());
        if saved.is_none() && appearance.is_none() { return Ok(()); }
        let mut journal: Journal = if saved.is_none() { Journal::default() } else { serde_json::from_str(saved_text)? };
        // Journal data cannot expand the supported write surface.
        for file in &journal.files {
            anyhow::ensure!(TARGETS.contains(&file.target.as_str()), "unknown owned target");
            anyhow::ensure!(file.keys.len() == KEYS.len(), "incomplete ownership journal");
            for (key, expected) in file.keys.iter().zip(KEYS) { anyhow::ensure!(key.name == expected, "unknown owned key"); }
        }
        anyhow::ensure!(journal.files.len() <= TARGETS.len(), "duplicate owned files");
        for (index, file) in journal.files.iter().enumerate() {
            anyhow::ensure!(!journal.files[..index].iter().any(|previous| previous.target == file.target), "duplicate owned target");
        }
        let values = appearance.map(|appearance| {
            let dark = appearance.palette.ground == DARK.ground;
            [if dark { "Adwaita-dark" } else { "Adwaita" }.to_owned(), dark.to_string(),
                "DejaVu Sans 10".to_owned(), "Adwaita".to_owned(), (!appearance.reduced_motion).to_string()]
        });
        let mut changes = Vec::new();
        for target in TARGETS {
            let before = FileVersion::read(&home.join(target))?;
            let document = before.as_ref().map_or("", |version| version.text.as_str());
            let index = journal.files.iter().position(|file| file.target == target);
            let index = match index {
                Some(index) => index,
                None if appearance.is_some() => {
                    let keys = KEYS.iter().map(|key| Ok(OwnedKey { name: (*key).into(),
                        original: value(document, key)?, written: value(document, key)?, previous: Vec::new() }))
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    journal.files.push(OwnedFile { target: target.into(), keys });
                    journal.files.len() - 1
                }
                None => continue,
            };
            let mut after = document.to_owned();
            for (i, owned) in journal.files[index].keys.iter_mut().enumerate() {
                let current = value(document, &owned.name)?;
                anyhow::ensure!(current == owned.written || owned.previous.contains(&current),
                    "{}: {} changed outside Templar; external edit preserved", target, owned.name);
                let wanted = values.as_ref().map(|values| values[i].clone()).or_else(|| owned.original.clone());
                set_value(&mut after, &owned.name, wanted.as_deref())?;
                if !owned.previous.contains(&current) { owned.previous.push(current); }
                owned.written = wanted;
            }
            changes.push(Change { target: target.into(), before, after });
        }
        // Persist the original keys and write intent before any configuration mutation.
        let saved_intent = atomic_write(&journal_path, &serde_json::to_string_pretty(&journal)?, saved.as_ref())?;
        for change in changes {
            let path = home.join(&change.target);
            if change.before.as_ref().map_or("", |version| version.text.as_str()) == change.after {
                anyhow::ensure!(FileVersion::read(&path)? == change.before, "{} changed during application; external edit preserved", change.target);
            } else {
                atomic_write(&path, &change.after, change.before.as_ref())?;
                applied.push(path);
            }
        }
        if appearance.is_none() {
            remove_owned(&journal_path, &saved_intent)?;
        } else {
            for file in &mut journal.files { for key in &mut file.keys { key.previous.clear(); } }
            atomic_write(&journal_path, &serde_json::to_string_pretty(&journal)?, Some(&saved_intent))?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(applied),
        Err(error) => Err(AppearanceConfigError { applied, cause: format!("{error:#}") }),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileVersion {
    text: String,
    dev: u64,
    inode: u64,
    modified_ns: (i64, i64),
    changed_ns: (i64, i64),
}
impl FileVersion {
    fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        if let Some(parent) = path.parent() && let Ok(meta) = fs::symlink_metadata(parent) {
            anyhow::ensure!(!meta.file_type().is_symlink(), "{} is a symlinked configuration directory", parent.display());
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink(), "{} is not a regular owned target", path.display());
        let mut file = fs::OpenOptions::new().read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32).open(path)?;
        let opened = file.metadata()?;
        anyhow::ensure!(opened.dev() == metadata.dev() && opened.ino() == metadata.ino(),
            "{} changed identity while opening", path.display());
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        let after = file.metadata()?;
        anyhow::ensure!(opened.mtime() == after.mtime() && opened.mtime_nsec() == after.mtime_nsec()
            && opened.ctime() == after.ctime() && opened.ctime_nsec() == after.ctime_nsec(),
            "{} changed while reading", path.display());
        Ok(Some(Self { text, dev: opened.dev(), inode: opened.ino(),
            modified_ns: (opened.mtime(), opened.mtime_nsec()),
            changed_ns: (opened.ctime(), opened.ctime_nsec()) }))
    }
    fn same_after_move(&self, other: &Self) -> bool {
        self.dev == other.dev && self.inode == other.inode
            && self.modified_ns == other.modified_ns && self.text == other.text
    }
}
#[cfg(test)]
fn load(path: &Path) -> anyhow::Result<String> {
    Ok(FileVersion::read(path)?.map_or_else(String::new, |version| version.text))
}
fn temp_path(parent: &Path) -> PathBuf {
    parent.join(format!(".templar-appearance-{}-{}.tmp", std::process::id(), TEMP.fetch_add(1, Ordering::Relaxed)))
}
fn exchange(left: &Path, right: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(rustix::fs::CWD, left, rustix::fs::CWD, right, rustix::fs::RenameFlags::EXCHANGE)
        .map_err(std::io::Error::from)
}
fn move_noreplace(left: &Path, right: &Path) -> std::io::Result<()> {
    rustix::fs::renameat_with(rustix::fs::CWD, left, rustix::fs::CWD, right, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(std::io::Error::from)
}
fn atomic_write(path: &Path, contents: &str, expected: Option<&FileVersion>) -> anyhow::Result<FileVersion> {
    publish_with(path, contents, expected, || {})
}
// Inspect the inode actually displaced by Linux EXCHANGE. An independent replacement cannot
// disappear in the last-check-to-rename window; failed repairs retain a named recovery file.
fn publish_with(path: &Path, contents: &str, expected: Option<&FileVersion>, before_exchange: impl FnOnce()) -> anyhow::Result<FileVersion> {
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("configuration parent missing"))?;
    fs::create_dir_all(parent)?;
    let temp = temp_path(parent);
    let mut created = false;
    let mut preserve_recovery = false;
    let result = (|| -> anyhow::Result<FileVersion> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
        created = true;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        let prepared = FileVersion::read(&temp)?.ok_or_else(|| anyhow::anyhow!("prepared configuration disappeared"))?;
        anyhow::ensure!(FileVersion::read(path)?.as_ref() == expected, "{} changed before publication; external edit preserved", path.display());
        before_exchange();
        if let Some(expected) = expected {
            exchange(&temp, path)?;
            preserve_recovery = true;
            let displaced = FileVersion::read(&temp).ok().flatten();
            let current = FileVersion::read(path).ok().flatten();
            if displaced.as_ref().is_some_and(|version| version.same_after_move(expected))
                && current.as_ref().is_some_and(|version| version.same_after_move(&prepared)) {
                fs::remove_file(&temp)?;
                preserve_recovery = false;
                fs::File::open(parent)?.sync_all()?;
                return Ok(current.expect("published version checked"));
            }
            // If another writer races the repair, exchange keeps its inode at `temp`.
            let rollback = if current.as_ref().is_some_and(|version| version.same_after_move(&prepared)) {
                exchange(&temp, path).err()
            } else { None };
            fs::File::open(parent)?.sync_all()?;
            anyhow::bail!("{}: configuration conflict; intervening changes preserved, recovery: {}{}",
                path.display(), temp.display(), rollback.map_or_else(String::new, |error| format!("; repair failed: {error}")));
        }
        move_noreplace(&temp, path)?;
        created = false;
        fs::File::open(parent)?.sync_all()?;
        let current = FileVersion::read(path)?.ok_or_else(|| anyhow::anyhow!("published configuration disappeared"))?;
        anyhow::ensure!(current.same_after_move(&prepared), "{} changed after publication; external edit preserved", path.display());
        Ok(current)
    })();
    if result.is_err() && created && !preserve_recovery { let _ = fs::remove_file(&temp); }
    if preserve_recovery {
        result.map_err(|error| anyhow::anyhow!("{error:#}; retained recovery file: {}", temp.display()))
    } else { result }
}
fn remove_owned(path: &Path, expected: &FileVersion) -> anyhow::Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("configuration parent missing"))?;
    let recovery = temp_path(parent);
    anyhow::ensure!(FileVersion::read(path)?.as_ref() == Some(expected), "ownership journal changed before removal");
    move_noreplace(path, &recovery)?;
    if FileVersion::read(&recovery).ok().flatten().as_ref().is_some_and(|version| version.same_after_move(expected)) {
        fs::remove_file(&recovery)?;
        fs::File::open(parent)?.sync_all()?;
        return Ok(());
    }
    // NOREPLACE cannot overwrite a second writer that recreated the original pathname.
    let rollback = move_noreplace(&recovery, path).err();
    fs::File::open(parent)?.sync_all()?;
    anyhow::bail!("ownership journal removal conflict; external edit preserved{}",
        rollback.map_or_else(String::new, |error| format!("; recovery {}: {error}", recovery.display())));
}
fn value(document: &str, key: &str) -> anyhow::Result<Option<String>> {
    let mut settings = false;
    let mut sections = 0;
    let mut found = None;
    for line in document.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            settings = line == "[Settings]";
            if settings { sections += 1; }
        } else if settings && !line.starts_with(['#', ';'])
            && let Some((name, value)) = line.split_once('=') && name.trim() == key {
            anyhow::ensure!(found.is_none(), "duplicate GTK setting {key}");
            found = Some(value.trim().to_owned());
        }
    }
    anyhow::ensure!(sections <= 1, "duplicate GTK Settings section");
    Ok(found)
}
fn set_value(document: &mut String, key: &str, wanted: Option<&str>) -> anyhow::Result<()> {
    let _ = value(document, key)?;
    let mut lines: Vec<String> = document.lines().map(str::to_owned).collect();
    let section = lines.iter().position(|line| line.trim() == "[Settings]");
    let start = match section {
        Some(start) => start,
        None if wanted.is_some() => { lines.push("[Settings]".into()); lines.len() - 1 },
        None => return Ok(()),
    };
    let end = (start + 1..lines.len()).find(|index| lines[*index].trim().starts_with('[')).unwrap_or(lines.len());
    let index = (start + 1..end).find(|index| lines[*index].split_once('=').is_some_and(|(name, _)| name.trim() == key));
    match (index, wanted) {
        (Some(index), Some(value)) => lines[index] = format!("{key}={value}"),
        (Some(index), None) => { lines.remove(index); },
        (None, Some(value)) => lines.insert(end, format!("{key}={value}")),
        (None, None) => return Ok(()),
    }
    *document = lines.join("\n");
    document.push('\n');
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_edit_between_final_check_and_exchange_is_restored_and_retained() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("settings.ini");
        fs::write(&destination, "[Settings]\nother=original\n").unwrap();
        let expected = FileVersion::read(&destination).unwrap().unwrap();
        let wanted = "[Settings]\nother=original\ngtk-font-name=DejaVu Sans 10\n";
        let external = "[Settings]\nother=native-editor\n";
        let error = publish_with(&destination, wanted, Some(&expected), || {
            fs::write(&destination, external).unwrap();
        }).unwrap_err();
        assert!(error.to_string().contains("conflict"));
        assert_eq!(fs::read_to_string(&destination).unwrap(), external);
        let recovery: Vec<_> = fs::read_dir(home.path()).unwrap()
            .map(|entry| entry.unwrap().path()).filter(|path| path != &destination).collect();
        assert_eq!(recovery.len(), 1);
        assert_eq!(fs::read_to_string(&recovery[0]).unwrap(), wanted);
    }

    #[test]
    fn native_creation_between_final_check_and_publish_is_never_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("settings.ini");
        let error = publish_with(&destination, "[Settings]\ngtk-font-name=DejaVu Sans 10\n", None, || {
            fs::write(&destination, "[Settings]\nother=native-new-file\n").unwrap();
        }).unwrap_err();
        assert!(!error.to_string().is_empty());
        assert_eq!(fs::read_to_string(destination).unwrap(), "[Settings]\nother=native-new-file\n");
    }
    #[test]
    fn symlink_substitution_at_exchange_does_not_touch_the_external_target() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("settings.ini");
        let external = home.path().join("outside");
        fs::write(&destination, "[Settings]\nother=original\n").unwrap();
        fs::write(&external, "never replace this").unwrap();
        let expected = FileVersion::read(&destination).unwrap().unwrap();
        assert!(publish_with(&destination, "[Settings]\nother=owned\n", Some(&expected), || {
            fs::remove_file(&destination).unwrap();
            std::os::unix::fs::symlink(&external, &destination).unwrap();
        }).is_err());
        assert!(fs::symlink_metadata(destination).unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(external).unwrap(), "never replace this");
    }
    #[test]
    fn toggling_restores_owned_keys_but_keeps_unrelated_changes() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(TARGETS[0]);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[Settings]\ngtk-font-name=Serif 11\nother=keep\n").unwrap();
        apply(home.path(), Appearance::from_settings("dark", "blue", true)).unwrap();
        assert_eq!(value(&load(&path).unwrap(), "gtk-enable-animations").unwrap().as_deref(), Some("false"));
        let mut changed = load(&path).unwrap();
        changed.push_str("outside=new\n");
        fs::write(&path, changed).unwrap();
        restore(home.path()).unwrap();
        let restored = load(&path).unwrap();
        assert_eq!(value(&restored, "gtk-font-name").unwrap().as_deref(), Some("Serif 11"));
        assert!(restored.contains("other=keep\n"));
        assert!(restored.contains("outside=new\n"));
        assert!(value(&restored, "gtk-theme-name").unwrap().is_none());
    }
    #[test]
    fn external_edits_are_not_overwritten_or_hidden_as_success() {
        let home = tempfile::tempdir().unwrap();
        apply(home.path(), Appearance::default()).unwrap();
        let path = home.path().join(TARGETS[0]);
        let changed = load(&path).unwrap().replace("gtk-font-name=DejaVu Sans 10", "gtk-font-name=Serif 16");
        fs::write(&path, &changed).unwrap();
        assert!(restore(home.path()).is_err());
        assert_eq!(load(&path).unwrap(), changed);
        assert!(apply(home.path(), Appearance::from_settings("dark", "default", false)).is_err());
    }
    #[test]
    fn interrupted_write_intent_can_finish_and_restore_original_preferences() {
        let home = tempfile::tempdir().unwrap();
        apply(home.path(), Appearance::default()).unwrap();
        let path = home.path().join(TARGETS[0]);
        let mut journal: Journal = serde_json::from_str(&load(&home.path().join(JOURNAL)).unwrap()).unwrap();
        let owned = &mut journal.files[0].keys[0];
        owned.previous.push(owned.written.clone());
        owned.written = Some("Adwaita-dark".to_owned());
        fs::write(home.path().join(JOURNAL), serde_json::to_string(&journal).unwrap()).unwrap();
        // The journal was saved, but the first toolkit file still has the old value.
        assert_eq!(value(&load(&path).unwrap(), "gtk-theme-name").unwrap().as_deref(), Some("Adwaita"));
        apply(home.path(), Appearance::from_settings("dark", "default", false)).unwrap();
        assert_eq!(value(&load(&path).unwrap(), "gtk-theme-name").unwrap().as_deref(), Some("Adwaita-dark"));
        restore(home.path()).unwrap();
        assert!(value(&load(&path).unwrap(), "gtk-theme-name").unwrap().is_none());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_targets_never_mutate_external_files() {
        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        let path = home.path().join(TARGETS[0]);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(apply(home.path(), Appearance::default()).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    }
}
