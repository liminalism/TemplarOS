//! The freedesktop.org trash, implemented once so the file manager, the Home Surface, the
//! portal and `templar files` all agree about what "delete" means.
//!
//! Trashing is the reversible half of removal, which is why it is the default and why
//! [`crate::operations::OperationKind::Delete`] has to announce that it is not. The layout is
//! the freedesktop Trash specification: a home trash at `$XDG_DATA_HOME/Trash` with `files/`
//! and `info/` beside each other, and a per-volume trash for anything that would otherwise have
//! to cross a device boundary — because moving a file to the trash must never turn into a
//! long copy, and must never fail on a full disk.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Write as _,
    fs,
    ffi::{OsStr, OsString},
    io::Write as _,
    os::unix::ffi::{OsStrExt, OsStringExt},
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

/// The home trash: `$XDG_DATA_HOME/Trash`, where paths in `.trashinfo` are absolute.
pub fn home_trash(data_home: &Path) -> TrashDir {
    TrashDir {
        root: data_home.join("Trash"),
        top_dir: None,
    }
}

/// The trash on the volume mounted at `top_dir`, whether or not it exists yet.
///
/// The specification's `$topdir/.Trash/$uid` requires an administrator-created, sticky,
/// non-symlink `.Trash`. Anything else, and the fallback `$topdir/.Trash-$uid` is correct — and
/// is what nearly every removable volume actually has.
pub fn volume_trash(top_dir: &Path) -> TrashDir {
    let uid = current_uid();
    let admin = top_dir.join(".Trash");
    let root = if is_sticky_directory(&admin) {
        admin.join(uid.to_string())
    } else {
        top_dir.join(format!(".Trash-{uid}"))
    };
    TrashDir {
        root,
        top_dir: Some(top_dir.to_path_buf()),
    }
}

/// Every trash this user has: the home trash, plus the trash of every mount point in
/// `mount_points` that already has one.
///
/// A volume with no trash directory is left out rather than conjured into existence — listing
/// the trash must not create `.Trash-$uid` on every mounted volume, and emptying must not either.
/// The mount points are passed in rather than read here so the sweep is testable without a
/// mount table.
pub fn all_trash_dirs(data_home: &Path, mount_points: &[PathBuf]) -> Vec<TrashDir> {
    let mut dirs = vec![home_trash(data_home)];
    for mount in mount_points {
        let candidate = volume_trash(mount);
        if candidate.root == dirs[0].root || !candidate.files().is_dir() {
            continue;
        }
        if !dirs.iter().any(|existing| existing.root == candidate.root) {
            dirs.push(candidate);
        }
    }
    dirs
}

/// Where trashed items go, given a home directory and the volume a path lives on.
///
/// `data_home` is `$XDG_DATA_HOME`. A path on the same device as the home trash uses it; on any
/// other device the volume's own trash is used, so trashing is always a rename.
pub fn trash_dir_for(path: &Path, data_home: &Path) -> Result<TrashDir> {
    use std::os::unix::fs::MetadataExt;
    let home_trash = home_trash(data_home);
    let (Some(path_device), Some(home_device)) = (path.symlink_metadata().ok().map(|m| m.dev()), device_of(data_home)) else {
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

    Ok(volume_trash(&top))
}

/// Move `path` into `trash`, writing its `.trashinfo` first so a crash between the two leaves a
/// stale record rather than an unexplainable file.
pub fn trash(path: &Path, trash: &TrashDir, now: &str) -> Result<TrashedItem> {
    trash.ensure()?;
    let absolute = absolute(path);
    if absolute.symlink_metadata().is_err() {
        bail!("{} does not exist", absolute.display());
    }

    // Trash storage names are not file identity. Invalid-UTF-8 and long original names use
    // an opaque bounded key; the lossless original name is retained in the .trashinfo Path.
    let original_name = absolute.file_name().unwrap_or_else(|| OsStr::new("unnamed"));
    let storage_name = original_name.to_str().filter(|name| name.len() <= 220)
        .map(std::borrow::Cow::Borrowed)
        .unwrap_or_else(|| std::borrow::Cow::Owned(format!("native-{}", uuid::Uuid::new_v4())));
    let name = free_name(&trash.files(), &trash.info(), &storage_name);
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
        percent_encode(recorded.as_os_str())
    );
    let mut record = fs::OpenOptions::new().write(true).create_new(true).open(&info_path)
        .with_context(|| format!("creating {}", info_path.display()))?;
    record.write_all(info.as_bytes()).with_context(|| format!("writing {}", info_path.display()))?;
    record.sync_all().with_context(|| format!("flushing {}", info_path.display()))?;

    if let Err(error) = crate::native_fs::rename_no_replace(&absolute, &trashed_path) {
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
            original = Some(PathBuf::from(percent_decode(value)?));
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
    crate::native_fs::rename_no_replace(&item.trashed_path, &item.original_path).with_context(|| {
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

/// One trash entry as a client sees it: what it was called, where it came from, when it went,
/// and which trash it is sitting in.
///
/// The wire form of `Files.Trash1.List`. `size_bytes` is the item's *own* size and not a
/// directory's recursive total — a listing must not have to walk every trashed folder to be
/// drawn, and `is_directory` is there so a UI can say "folder" instead of a misleading 4 kB.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrashEntry {
    pub name: String,
    #[serde(with = "templar_types::native_path")]
    pub trashed_path: PathBuf,
    #[serde(with = "templar_types::native_path")]
    pub info_path: PathBuf,
    #[serde(with = "templar_types::native_path")]
    pub original_path: PathBuf,
    /// Local time, `YYYY-MM-DDThh:mm:ss`, as the specification requires.
    pub deletion_date: String,
    /// The trash directory holding this entry: the home trash, or a volume's.
    #[serde(with = "templar_types::native_path")]
    pub trash_root: PathBuf,
    pub is_directory: bool,
    pub size_bytes: u64,
}

impl TrashEntry {
    fn of(item: TrashedItem, trash: &TrashDir) -> Self {
        let metadata = item.trashed_path.symlink_metadata().ok();
        Self {
            name: file_name(&item.trashed_path),
            is_directory: metadata.as_ref().is_some_and(std::fs::Metadata::is_dir),
            size_bytes: metadata.as_ref().map_or(0, std::fs::Metadata::len),
            trash_root: trash.root.clone(),
            trashed_path: item.trashed_path,
            info_path: item.info_path,
            original_path: item.original_path,
            deletion_date: item.deletion_date,
        }
    }
}

/// Everything in every trash in `dirs`, newest deletion first.
///
/// A trash that cannot be read contributes nothing rather than failing the whole listing: an
/// unreadable removable volume must not be able to hide the home trash.
pub fn list_all(dirs: &[TrashDir]) -> Vec<TrashEntry> {
    let mut entries: Vec<TrashEntry> = dirs
        .iter()
        .flat_map(|trash| {
            list(trash)
                .unwrap_or_default()
                .into_iter()
                .map(move |item| TrashEntry::of(item, trash))
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .deletion_date
            .cmp(&left.deletion_date)
            .then_with(|| left.trashed_path.cmp(&right.trashed_path))
    });
    entries
}

/// The trash that holds `path` as one of its own top-level items, if any.
///
/// This is the whole of `Trash1`'s authority check: a caller may name anything, and only a direct
/// child of some `files/` directory is a trash entry. A path that merely *starts* with one — or
/// climbs out of it with `..` — is not, so neither restoring nor deleting can be aimed at a file
/// that was never in the trash.
pub fn owning_trash<'a>(path: &Path, dirs: &'a [TrashDir]) -> Option<&'a TrashDir> {
    let path = absolute(path);
    path.file_name()?;
    dirs.iter()
        .find(|trash| path.parent() == Some(trash.files().as_path()))
}

/// The `.trashinfo` record beside a trashed item, if it still exists.
///
/// A permanent delete removes it alongside the item; an item whose record has already been lost
/// is deleted on its own rather than refused.
pub fn info_path_of(trashed_path: &Path, trash: &TrashDir) -> Option<PathBuf> {
    let path = trash
        .info()
        .join(format!("{}.trashinfo", file_name(trashed_path)));
    path.symlink_metadata().is_ok().then_some(path)
}

/// Every path a trash contains: its items and their `.trashinfo` records.
///
/// Both halves, because emptying a trash that keeps its `info/` is not emptying it — the records
/// would outlive the files they describe and the next listing would be full of ghosts. Records
/// whose item is already gone are swept up for the same reason.
pub fn everything_in(trash: &TrashDir) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for directory in [trash.files(), trash.info()] {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        paths.extend(entries.filter_map(|entry| entry.ok().map(|entry| entry.path())));
    }
    paths.sort();
    paths
}

/// The newest trashed copy of `original`, if this trash holds one.
///
/// Only for recovering the name [`trash`] chose when a crash landed between the rename and the
/// journal write: on the ordinary path the executor records that name as it happens, because a
/// path trashed more than once has several records here and this cannot tell them apart.
pub fn find_trashed(original: &Path, trash: &TrashDir) -> Option<PathBuf> {
    let original = absolute(original);
    list(trash)
        .ok()?
        .into_iter()
        .find(|item| item.original_path == original)
        .map(|item| item.trashed_path)
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
    use std::os::unix::fs::MetadataExt;
    let absolute = absolute(path);
    let device = absolute.symlink_metadata().ok()?.dev();
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
fn percent_encode(value: &OsStr) -> String {
    let mut out = String::with_capacity(value.as_bytes().len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn percent_decode(value: &str) -> Result<OsString> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() { bail!("incomplete percent escape in trash Path"); }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).context("invalid percent escape in trash Path")?;
            let byte = u8::from_str_radix(hex, 16).context("invalid percent escape in trash Path")?;
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    if out.contains(&0) { bail!("trash Path contains NUL"); }
    Ok(OsString::from_vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(percent_decode(&percent_encode(OsStr::new(value))).unwrap(), OsStr::new(value));
        }
        assert_eq!(percent_encode(OsStr::new("/a b")), "/a%20b");
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

    /// A stand-in for a mounted volume: a directory with its own `.Trash-$uid` already made, the
    /// way a removable disk that has been trashed to before looks.
    fn volume_with_a_trash(top: &Path) -> TrashDir {
        let trash = volume_trash(top);
        trash.ensure().unwrap();
        trash
    }

    #[test]
    fn the_sweep_covers_the_home_trash_and_every_volume_that_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        let with_trash = dir.path().join("mnt/stick");
        let without_trash = dir.path().join("mnt/plain");
        fs::create_dir_all(&with_trash).unwrap();
        fs::create_dir_all(&without_trash).unwrap();
        volume_with_a_trash(&with_trash);

        let dirs = all_trash_dirs(&data_home, &[with_trash.clone(), without_trash.clone()]);
        let roots: Vec<&Path> = dirs.iter().map(TrashDir::root).collect();
        assert_eq!(roots.len(), 2, "{roots:?}");
        assert_eq!(roots[0], data_home.join("Trash"));
        assert!(roots[1].starts_with(&with_trash));
        assert!(
            !without_trash
                .join(format!(".Trash-{}", current_uid()))
                .exists(),
            "listing the trash must not create one on a volume that has none"
        );
    }

    #[test]
    fn listing_every_trash_puts_the_newest_deletion_first() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        let volume = dir.path().join("mnt/stick");
        fs::create_dir_all(&volume).unwrap();
        let volume_trash_dir = volume_with_a_trash(&volume);

        let old = dir.path().join("old.txt");
        fs::write(&old, b"old").unwrap();
        trash(&old, &home_trash(&data_home), "2026-01-01T09:00:00").unwrap();

        let recent = volume.join("recent.txt");
        fs::write(&recent, b"recent").unwrap();
        trash(&recent, &volume_trash_dir, "2026-08-30T18:00:00").unwrap();

        let folder = dir.path().join("folder");
        fs::create_dir(&folder).unwrap();
        trash(&folder, &home_trash(&data_home), "2026-05-05T12:00:00").unwrap();

        let dirs = all_trash_dirs(&data_home, std::slice::from_ref(&volume));
        let entries = list_all(&dirs);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["recent.txt", "folder", "old.txt"]);

        let from_volume = &entries[0];
        assert_eq!(from_volume.original_path, recent);
        assert_eq!(from_volume.trash_root, *volume_trash_dir.root());
        assert!(!from_volume.is_directory);
        assert_eq!(from_volume.size_bytes, 6);
        assert!(entries[1].is_directory, "a trashed folder says it is one");
        assert_eq!(entries[2].original_path, old);
        assert_eq!(entries[2].trash_root, data_home.join("Trash"));
    }

    #[test]
    fn only_a_direct_child_of_a_trash_counts_as_a_trash_entry() {
        let dir = tempfile::tempdir().unwrap();
        let data_home = dir.path().join("data");
        let home = home_trash(&data_home);
        let source = dir.path().join("note.txt");
        fs::write(&source, b"x").unwrap();
        let item = trash(&source, &home, "2026-08-21T14:32:00").unwrap();
        let dirs = all_trash_dirs(&data_home, &[]);

        assert_eq!(
            owning_trash(&item.trashed_path, &dirs).map(TrashDir::root),
            Some(home.root())
        );
        assert_eq!(
            info_path_of(&item.trashed_path, &home),
            Some(item.info_path.clone())
        );
        for outsider in [
            dir.path().join("note.txt"),
            home.files().join("sub/deeper.txt"),
            home.files().join("../../../etc/passwd"),
            home.root().to_path_buf(),
            home.files(),
        ] {
            assert!(
                owning_trash(&outsider, &dirs).is_none(),
                "{} was accepted as a trash entry",
                outsider.display()
            );
        }
    }

    #[test]
    fn emptying_names_the_items_and_their_records_including_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let home = home_trash(&dir.path().join("data"));
        let source = dir.path().join("note.txt");
        fs::write(&source, b"x").unwrap();
        let item = trash(&source, &home, "2026-08-21T14:32:00").unwrap();
        // A record whose item is gone: the residue of an interrupted restore.
        let orphan = home.info().join("vanished.txt.trashinfo");
        fs::write(
            &orphan,
            "[Trash Info]\nPath=/gone\nDeletionDate=2026-01-01T00:00:00\n",
        )
        .unwrap();

        let everything = everything_in(&home);
        assert!(everything.contains(&item.trashed_path));
        assert!(everything.contains(&item.info_path));
        assert!(everything.contains(&orphan), "an orphan record survives");
        assert_eq!(everything.len(), 3);
    }

    #[test]
    fn the_local_timestamp_has_the_specified_shape() {
        let now = now_local();
        assert_eq!(now.len(), 19, "{now}");
        assert_eq!(now.as_bytes()[10], b'T');
        assert!(now[..4].parse::<u32>().unwrap() >= 1970);
    }
}
