//! The freedesktop.org trash, implemented once so the file manager, the Home Surface, the
//! portal and `lege files` all agree about what "delete" means.
//!
//! Trashing is the reversible half of removal, which is why it is the default and why
//! [`crate::operations::OperationKind::Delete`] has to announce that it is not. The layout is
//! the freedesktop Trash specification: a home trash at `$XDG_DATA_HOME/Trash` with `files/`
//! and `info/` beside each other, and a per-volume trash for anything that would otherwise have
//! to cross a device boundary — because moving a file to the trash must never turn into a
//! long copy, and must never fail on a full disk.

use anyhow::{Context, Result, bail};
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// A trash directory: `files/` holds the items, `info/` the `.trashinfo` records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashDir {
    root: PathBuf,
    /// For a volume trash, paths in `.trashinfo` are relative to this; for the home trash they
    /// are absolute.
    top_dir: Option<PathBuf>,
}

impl TrashDir {
    pub fn files(&self) -> PathBuf {
        self.root.join("files")
    }

    pub fn info(&self) -> PathBuf {
        self.root.join("info")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.files())
            .with_context(|| format!("creating {}", self.files().display()))?;
        fs::create_dir_all(self.info())
            .with_context(|| format!("creating {}", self.info().display()))?;
        Ok(())
    }
}

/// What was recorded when an item was trashed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrashedItem {
    /// Where the item now lives, under `files/`.
    pub trashed_path: PathBuf,
    /// Where it came from.
    pub original_path: PathBuf,
    /// Local time, `YYYY-MM-DDThh:mm:ss`, as the specification requires.
    pub deletion_date: String,
    pub info_path: PathBuf,
}

/// Where trashed items go, given a home directory and the volume a path lives on.
///
/// `data_home` is `$XDG_DATA_HOME`. A path on the same device as the home trash uses it; on any
/// other device the volume's own trash is used, so trashing is always a rename.
pub fn trash_dir_for(path: &Path, data_home: &Path) -> Result<TrashDir> {
    let home_trash = TrashDir {
        root: data_home.join("Trash"),
        top_dir: None,
    };
    let (Some(path_device), Some(home_device)) = (device_of(path), device_of(data_home)) else {
        return Ok(home_trash);
    };
    if path_device == home_device {
        return Ok(home_trash);
    }

    let top = top_dir_of(path).with_context(|| {
        format!(
            "finding the mount point that holds {} for its volume trash",
            path.display()
        )
    })?;

    // The specification's `$topdir/.Trash/$uid` requires an administrator-created, sticky,
    // non-symlink `.Trash`. Anything else, and the fallback `$topdir/.Trash-$uid` is correct —
    // and is what nearly every removable volume actually has.
    let uid = current_uid();
    let admin = top.join(".Trash");
    if is_sticky_directory(&admin) {
        return Ok(TrashDir {
            root: admin.join(uid.to_string()),
            top_dir: Some(top),
        });
    }
    Ok(TrashDir {
        root: top.join(format!(".Trash-{uid}")),
        top_dir: Some(top),
    })
}

/// Move `path` into `trash`, writing its `.trashinfo` first so a crash between the two leaves a
/// stale record rather than an unexplainable file.
pub fn trash(path: &Path, trash: &TrashDir, now: &str) -> Result<TrashedItem> {
    trash.ensure()?;
    let absolute = absolute(path);
    if absolute.symlink_metadata().is_err() {
        bail!("{} does not exist", absolute.display());
    }

    let name = free_name(&trash.files(), &trash.info(), &file_name(&absolute));
    let trashed_path = trash.files().join(&name);
    let info_path = trash.info().join(format!("{name}.trashinfo"));

    let recorded = match &trash.top_dir {
        Some(top) => absolute
            .strip_prefix(top)
            .unwrap_or(&absolute)
            .to_path_buf(),
        None => absolute.clone(),
    };
    let info = format!(
        "[Trash Info]\nPath={}\nDeletionDate={now}\n",
        percent_encode(&recorded.to_string_lossy())
    );
    fs::write(&info_path, info).with_context(|| format!("writing {}", info_path.display()))?;

    if let Err(error) = fs::rename(&absolute, &trashed_path) {
        let _ = fs::remove_file(&info_path);
        return Err(error).with_context(|| {
            format!(
                "moving {} into {}",
                absolute.display(),
                trash.files().display()
            )
        });
    }

    Ok(TrashedItem {
        trashed_path,
        original_path: absolute,
        deletion_date: now.to_owned(),
        info_path,
    })
}

/// Read the `.trashinfo` beside a trashed item.
pub fn read_info(trashed_path: &Path, trash: &TrashDir) -> Result<TrashedItem> {
    let name = file_name(trashed_path);
    let info_path = trash.info().join(format!("{name}.trashinfo"));
    let text = fs::read_to_string(&info_path)
        .with_context(|| format!("reading {}", info_path.display()))?;
    let mut original = None;
    let mut deletion_date = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Path=") {
            original = Some(PathBuf::from(percent_decode(value)));
        } else if let Some(value) = line.strip_prefix("DeletionDate=") {
            value.clone_into(&mut deletion_date);
        }
    }
    let original = original.with_context(|| format!("{} has no Path=", info_path.display()))?;
    let original_path = match &trash.top_dir {
        Some(top) if original.is_relative() => top.join(original),
        _ => original,
    };
    Ok(TrashedItem {
        trashed_path: trashed_path.to_path_buf(),
        original_path,
        deletion_date,
        info_path,
    })
}

/// Put a trashed item back where it came from, recreating any parent directory that has since
/// been removed. Refuses rather than overwriting something that now occupies the old name.
pub fn restore(trashed_path: &Path, trash: &TrashDir) -> Result<PathBuf> {
    let item = read_info(trashed_path, trash)?;
    if item.original_path.symlink_metadata().is_ok() {
        bail!(
            "{} already exists; restoring would overwrite it",
            item.original_path.display()
        );
    }
    if let Some(parent) = item.original_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("recreating {}", parent.display()))?;
    }
    fs::rename(&item.trashed_path, &item.original_path).with_context(|| {
        format!(
            "restoring {} to {}",
            item.trashed_path.display(),
            item.original_path.display()
        )
    })?;
    let _ = fs::remove_file(&item.info_path);
    Ok(item.original_path)
}

/// Everything currently in a trash directory, newest record first.
pub fn list(trash: &TrashDir) -> Result<Vec<TrashedItem>> {
    let Ok(entries) = fs::read_dir(trash.files()) else {
        return Ok(Vec::new());
    };
    let mut items: Vec<TrashedItem> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| read_info(&entry.path(), trash).ok())
        .collect();
    items.sort_by(|left, right| right.deletion_date.cmp(&left.deletion_date));
    Ok(items)
}

/// Local time in the specification's `YYYY-MM-DDThh:mm:ss` form.
pub fn now_local() -> String {
    // SAFETY: `time(NULL)` reads the clock and cannot fail in a way we must handle;
    // `localtime_r` writes into the zeroed `tm` we own and takes a pointer to a live `time_t`.
    #[allow(unsafe_code)]
    let tm = unsafe {
        let seconds = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const seconds, &raw mut tm).is_null() {
            return "1970-01-01T00:00:00".to_owned();
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn free_name(files: &Path, info: &Path, name: &str) -> String {
    if files.join(name).symlink_metadata().is_err()
        && !info.join(format!("{name}.trashinfo")).exists()
    {
        return name.to_owned();
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    // Bounded rather than open-ended: after a thousand collisions on one name, an unguessable
    // suffix is both faster and no less correct than counting further.
    (2..1_000)
        .map(|index| format!("{stem}.{index}{extension}"))
        .find(|candidate| {
            files.join(candidate).symlink_metadata().is_err()
                && !info.join(format!("{candidate}.trashinfo")).exists()
        })
        .unwrap_or_else(|| format!("{stem}.{}{extension}", uuid::Uuid::new_v4().simple()))
}

/// The mount point that holds `path`: walk up until the device number changes.
pub fn top_dir_of(path: &Path) -> Option<PathBuf> {
    let absolute = absolute(path);
    let device = device_of(&absolute)?;
    let mut current = absolute.as_path();
    let mut best = current.to_path_buf();
    while let Some(parent) = current.parent() {
        match device_of(parent) {
            Some(parent_device) if parent_device == device => {
                best = parent.to_path_buf();
                current = parent;
            }
            _ => break,
        }
    }
    Some(best)
}

fn is_sticky_directory(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = path.symlink_metadata() else {
        return false;
    };
    metadata.is_dir()
        && !metadata.file_type().is_symlink()
        && metadata.permissions().mode() & 0o1000 != 0
}

fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    #[allow(unsafe_code)]
    unsafe {
        libc::getuid()
    }
}

fn device_of(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "unnamed".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Percent-encode everything outside the specification's unreserved set, keeping `/` so a path
/// stays readable.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_trash(root: &Path) -> TrashDir {
        TrashDir {
            root: root.join("Trash"),
            top_dir: None,
        }
    }

    #[test]
    fn trashing_records_the_original_path_and_restoring_puts_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        let source = dir.path().join("work/report.pdf");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"contents").unwrap();

        let trash_dir = home_trash(&data_home);
        let item = trash(&source, &trash_dir, "2026-08-21T14:32:00").unwrap();
        assert!(!source.exists(), "the source is gone");
        assert!(item.trashed_path.exists());
        assert_eq!(item.original_path, source);

        let info = fs::read_to_string(&item.info_path).unwrap();
        assert!(info.starts_with("[Trash Info]\n"));
        assert!(info.contains("DeletionDate=2026-08-21T14:32:00"));

        let restored = restore(&item.trashed_path, &trash_dir).unwrap();
        assert_eq!(restored, source);
        assert_eq!(fs::read(&source).unwrap(), b"contents");
        assert!(!item.info_path.exists());
    }

    #[test]
    fn restoring_refuses_to_overwrite_and_recreates_a_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        let trash_dir = home_trash(&data_home);
        let source = dir.path().join("nested/deep/file.txt");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"x").unwrap();

        let item = trash(&source, &trash_dir, "2026-08-21T14:32:00").unwrap();
        fs::remove_dir_all(dir.path().join("nested")).unwrap();
        assert_eq!(restore(&item.trashed_path, &trash_dir).unwrap(), source);

        let again = trash(&source, &trash_dir, "2026-08-21T14:33:00").unwrap();
        fs::write(&source, b"something new").unwrap();
        let error = restore(&again.trashed_path, &trash_dir).unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn colliding_names_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let trash_dir = home_trash(&dir.path().join("data"));
        let mut trashed = Vec::new();
        for index in 0..3 {
            let source = dir.path().join(format!("copy{index}/note.txt"));
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, format!("body {index}")).unwrap();
            trashed.push(trash(&source, &trash_dir, "2026-08-21T14:32:00").unwrap());
        }
        let names: Vec<String> = trashed
            .iter()
            .map(|item| file_name(&item.trashed_path))
            .collect();
        assert_eq!(names, ["note.txt", "note.2.txt", "note.3.txt"]);
        for (index, item) in trashed.iter().enumerate() {
            let recovered = read_info(&item.trashed_path, &trash_dir).unwrap();
            assert_eq!(recovered.original_path, item.original_path);
            assert_eq!(
                fs::read_to_string(&item.trashed_path).unwrap(),
                format!("body {index}")
            );
        }
        assert_eq!(list(&trash_dir).unwrap().len(), 3);
    }

    #[test]
    fn awkward_paths_survive_the_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let trash_dir = home_trash(&dir.path().join("data"));
        let source = dir.path().join("a b#c%d\ne.txt");
        fs::write(&source, b"x").unwrap();
        let item = trash(&source, &trash_dir, "2026-08-21T14:32:00").unwrap();
        let info = fs::read_to_string(&item.info_path).unwrap();
        assert!(!info.lines().nth(1).unwrap().contains(' '));
        assert_eq!(
            read_info(&item.trashed_path, &trash_dir)
                .unwrap()
                .original_path,
            source
        );
    }

    #[test]
    fn percent_coding_round_trips() {
        for value in ["/a/b c", "/a/#anchor", "/a/100%", "/plain/path", "/ü/né"] {
            assert_eq!(percent_decode(&percent_encode(value)), value);
        }
        assert_eq!(percent_encode("/a b"), "/a%20b");
    }

    #[test]
    fn a_same_device_path_uses_the_home_trash() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        fs::create_dir_all(&data_home).unwrap();
        let path = dir.path().join("file.txt");
        fs::write(&path, b"x").unwrap();
        let chosen = trash_dir_for(&path, &data_home).unwrap();
        assert_eq!(chosen.root, data_home.join("Trash"));
        assert_eq!(chosen.top_dir, None);
    }

    #[test]
    fn the_top_directory_of_a_path_is_a_mount_point() {
        let top = top_dir_of(Path::new("/")).unwrap();
        assert_eq!(top, Path::new("/"));
        assert!(top_dir_of(Path::new("/definitely/not/here")).is_none());
    }

    #[test]
    fn the_local_timestamp_has_the_specified_shape() {
        let now = now_local();
        assert_eq!(now.len(), 19, "{now}");
        assert_eq!(now.as_bytes()[10], b'T');
        assert!(now[..4].parse::<u32>().unwrap() >= 1970);
    }
}
