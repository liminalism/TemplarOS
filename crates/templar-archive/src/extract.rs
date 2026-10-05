use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, ensure};
use libarchive3_sys::ffi;

use crate::destination::{Destination, safe_leaf_name};
use crate::native::{Entry, Reader};
use crate::paths::{
    absolute_leaf, archive_stem, link_target, member_path, raw_name, resolved_links,
};
use crate::{BUFFER_SIZE, ExtractFailure, ExtractMode, Progress, check_cancel};

pub fn extract(
    paths: &[PathBuf],
    mode: &ExtractMode,
    password: Option<&str>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<Vec<PathBuf>> {
    ensure!(!paths.is_empty(), "Select at least one archive to extract");
    let mut completed = Vec::new();
    for path in paths {
        match extract_one(path, mode, password, cancel, progress) {
            Ok(outputs) => completed.extend(outputs),
            Err(error) => {
                if let Some(partial) = error.downcast_ref::<PublicationFailure>() {
                    completed.extend(partial.outputs.iter().cloned());
                }
                if completed.is_empty() {
                    return Err(error)
                        .with_context(|| format!("Cannot extract {}", path.display()));
                }
                return Err(ExtractFailure::new(completed, path, error).into());
            }
        }
    }
    Ok(completed)
}

fn extract_one(
    archive: &Path,
    mode: &ExtractMode,
    password: Option<&str>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<Vec<PathBuf>> {
    check_cancel(cancel)?;
    let archive = absolute_leaf(archive)?;
    let parent = match mode {
        ExtractMode::Directory(path) => path.as_path(),
        ExtractMode::Here | ExtractMode::Smart => {
            archive.parent().context("Archive has no parent")?
        }
    };
    let destination = Destination::open(parent)?;
    let mut stage = Stage::new(&destination)?;
    let mut reader = Reader::open(&archive, password)?;
    let mut buffer = vec![0_u8; BUFFER_SIZE];
    while let Some(entry) = {
        check_cancel(cancel)?;
        reader.next(cancel)?
    } {
        extract_entry(
            &archive,
            entry,
            &mut stage,
            &mut reader,
            &mut buffer,
            cancel,
            progress,
        )?;
    }
    if stage.entries.is_empty() && reader.empty_compressed_stream() {
        check_cancel(cancel)?;
        let path = member_path(raw_name(&archive).as_bytes())?;
        progress(Progress::Entry(path.clone()));
        stage.register(&path, Kind::File)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage.root.join(path))?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
    }
    reader.finish(cancel)?;
    check_cancel(cancel)?;
    let flattened = stage.prepare(matches!(mode, ExtractMode::Smart), cancel)?;
    check_cancel(cancel)?;
    if matches!(mode, ExtractMode::Smart) {
        let name = archive_stem(&archive);
        safe_leaf_name(&name)?;
        let source = flattened.map_or_else(|| stage.root.clone(), |path| stage.root.join(path));
        // No cancellation points within publication: a completed atomic commit
        // is reported as success rather than cancelled with an undisclosed output.
        let output = destination.publish_unique(&source, &name, false)?;
        stage.committed = true;
        return Ok(vec![output]);
    }
    let outputs = publish_members(&stage, &destination)?;
    stage.committed = true;
    Ok(outputs)
}

fn extract_entry(
    archive: &Path,
    entry: Entry,
    stage: &mut Stage,
    reader: &mut Reader,
    buffer: &mut [u8],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<()> {
    check_cancel(cancel)?;
    let path = if entry.raw {
        ensure!(
            reader.is_compressed(),
            "Input is not a recognized archive or compressed stream"
        );
        member_path(raw_name(archive).as_bytes())?
    } else {
        member_path(&entry.path)?
    };
    progress(Progress::Entry(path.clone()));
    check_cancel(cancel)?;
    if path.as_os_str().is_empty() {
        ensure!(
            entry.kind == ffi::AE_IFDIR && entry.symlink.is_none() && entry.hardlink.is_none(),
            "Non-directory archive entry refers to extraction root"
        );
        ensure!(
            drain(reader, None, buffer, cancel, progress)? == 0,
            "Archive root directory contains unexpected data"
        );
        return Ok(());
    }
    if let Some(target) = entry.hardlink {
        ensure!(
            matches!(entry.kind, 0 | ffi::AE_IFREG),
            "Archive hard link has an unsupported file type"
        );
        ensure!(
            entry.symlink.is_none(),
            "Archive entry is both a symbolic and hard link"
        );
        let target = member_path(&target)?;
        ensure!(
            !target.as_os_str().is_empty(),
            "Hard link targets extraction root"
        );
        stage.register(&path, Kind::Hardlink)?;
        stage.hardlinks.insert(path.clone(), target);
        ensure!(
            drain(reader, None, buffer, cancel, progress)? == 0,
            "Archive hard link contains unexpected data"
        );
    } else if let Some(target) = entry.symlink {
        ensure!(
            matches!(entry.kind, 0 | ffi::AE_IFLNK),
            "Archive symbolic link has an unsupported file type"
        );
        stage.register(&path, Kind::Symlink)?;
        stage.links.insert(path.clone(), link_target(&target)?);
        ensure!(
            drain(reader, None, buffer, cancel, progress)? == 0,
            "Archive symbolic link contains unexpected data"
        );
    } else {
        match entry.kind {
            ffi::AE_IFDIR => {
                stage.register(&path, Kind::Directory { explicit: true })?;
                stage.directories.insert(path.clone(), entry.mode);
                ensure!(
                    drain(reader, None, buffer, cancel, progress)? == 0,
                    "Archive directory contains unexpected data"
                );
            }
            ffi::AE_IFREG => {
                stage.register(&path, Kind::File)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(stage.root.join(&path))?;
                let count = drain(reader, Some(&mut file), buffer, cancel, progress)?;
                if let Some(expected) = entry.size {
                    ensure!(
                        count == expected,
                        "Truncated or invalid archive member {}: expected {expected} bytes, received {count}",
                        path.display()
                    );
                }
                file.set_permissions(fs::Permissions::from_mode(entry.mode))?;
            }
            _ => anyhow::bail!(
                "Unsafe special or unsupported archive entry: {}",
                path.display()
            ),
        }
    }
    Ok(())
}

fn drain(
    reader: &mut Reader,
    mut file: Option<&mut fs::File>,
    buffer: &mut [u8],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> anyhow::Result<u64> {
    let mut total = 0_u64;
    loop {
        check_cancel(cancel)?;
        let count = reader.read(buffer)?;
        if count == 0 {
            break;
        }
        if let Some(file) = file.as_mut() {
            file.write_all(&buffer[..count])?;
        }
        total = total
            .checked_add(count as u64)
            .context("Archive member size overflow")?;
        progress(Progress::Bytes(count as u64));
    }
    Ok(total)
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    Directory { explicit: bool },
    File,
    Symlink,
    Hardlink,
}

#[derive(Debug)]
struct Stage {
    // Kept alive through publication; Drop first restores staging permissions so
    // tempfile cleanup still works after malicious archive directories mode 000.
    _temporary: tempfile::TempDir,
    committed: bool,
    root: PathBuf,
    entries: BTreeMap<PathBuf, Kind>,
    directories: BTreeMap<PathBuf, u32>,
    links: BTreeMap<PathBuf, PathBuf>,
    hardlinks: BTreeMap<PathBuf, PathBuf>,
}

impl Stage {
    fn new(destination: &Destination) -> anyhow::Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix(".templar-extract-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(destination.anchored())?;
        let root = temporary.path().join("contents");
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            _temporary: temporary,
            committed: false,
            root,
            entries: BTreeMap::new(),
            directories: BTreeMap::new(),
            links: BTreeMap::new(),
            hardlinks: BTreeMap::new(),
        })
    }

    fn register(&mut self, path: &Path, kind: Kind) -> anyhow::Result<()> {
        let mut parent = PathBuf::new();
        let mut components = path.iter().peekable();
        while let Some(component) = components.next() {
            parent.push(component);
            if components.peek().is_none() {
                break;
            }
            match self.entries.get(&parent) {
                Some(Kind::Directory { .. }) => {}
                Some(_) => anyhow::bail!(
                    "Archive member traverses a file or link: {}",
                    path.display()
                ),
                None => {
                    fs::create_dir(self.root.join(&parent))?;
                    self.entries
                        .insert(parent.clone(), Kind::Directory { explicit: false });
                    self.directories.insert(parent.clone(), 0o755);
                }
            }
        }
        if let Some(existing) = self.entries.get(path) {
            ensure!(
                matches!(
                    (existing, kind),
                    (
                        Kind::Directory { explicit: false },
                        Kind::Directory { explicit: true }
                    )
                ),
                "Duplicate or conflicting archive pathname: {}",
                path.display()
            );
        } else if matches!(kind, Kind::Directory { .. }) {
            fs::create_dir(self.root.join(path))?;
            self.directories.insert(path.to_path_buf(), 0o755);
        }
        self.entries.insert(path.to_path_buf(), kind);
        Ok(())
    }

    fn top_level(&self) -> Vec<PathBuf> {
        self.entries
            .keys()
            .filter(|path| path.iter().nth(1).is_none())
            .cloned()
            .collect()
    }

    fn prepare(&mut self, smart: bool, cancel: &AtomicBool) -> anyhow::Result<Option<PathBuf>> {
        let flattened = if smart {
            let mut top = self
                .entries
                .iter()
                .filter(|(path, _)| path.iter().nth(1).is_none());
            match (top.next(), top.next()) {
                (Some((path, Kind::Directory { .. })), None) => Some(path.clone()),
                _ => None,
            }
        } else {
            None
        };
        let resolved = resolved_links(&self.links)?;
        for (path, target) in &resolved {
            let first = target
                .iter()
                .next()
                .context("Symbolic link refers outside its published tree")?;
            ensure!(
                self.entries.contains_key(Path::new(first)),
                "Symbolic link targets a tree not supplied by the archive: {}",
                path.display()
            );
            if let Some(root) = &flattened {
                ensure!(
                    target.starts_with(root),
                    "Symbolic link would escape the smart extraction folder: {}",
                    path.display()
                );
                // Renaming the top directory must not break links such as
                // root/link -> ../root/file. Rewrite to the resolved internal
                // relative target before publishing root under the archive name.
                self.links.insert(
                    path.clone(),
                    relative_target(path.parent().expect("link parent"), target),
                );
            }
        }
        let mut pending = std::mem::take(&mut self.hardlinks);
        while !pending.is_empty() {
            check_cancel(cancel)?;
            let mut linked = Vec::new();
            for (path, target) in &pending {
                ensure!(
                    matches!(self.entries.get(target), Some(Kind::File | Kind::Hardlink)),
                    "Hard link must target an archived regular file: {}",
                    path.display()
                );
                if pending.contains_key(target) {
                    continue;
                }
                fs::hard_link(self.root.join(target), self.root.join(path))
                    .with_context(|| format!("Cannot preserve hard link {}", path.display()))?;
                linked.push(path.clone());
            }
            ensure!(
                !linked.is_empty(),
                "Cyclic archive hard links are unsupported"
            );
            for path in linked {
                pending.remove(&path);
            }
        }
        for (path, target) in &self.links {
            check_cancel(cancel)?;
            symlink(target, self.root.join(path))?;
        }
        for (path, mode) in self.directories.iter().rev() {
            check_cancel(cancel)?;
            fs::set_permissions(self.root.join(path), fs::Permissions::from_mode(*mode))?;
        }
        fs::set_permissions(&self.root, fs::Permissions::from_mode(0o755))?;
        Ok(flattened)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700));
        for path in self.directories.keys() {
            let _ = fs::set_permissions(self.root.join(path), fs::Permissions::from_mode(0o700));
        }
        // `temporary` is dropped after this method, recursively removing only
        // uncommitted contents. Paths already atomically published are absent.
    }
}

fn relative_target(parent: &Path, target: &Path) -> PathBuf {
    let mut parent = parent.iter().peekable();
    let mut target = target.iter().peekable();
    while parent.peek().is_some() && parent.peek() == target.peek() {
        parent.next();
        target.next();
    }
    let mut relative = PathBuf::new();
    for _ in parent {
        relative.push("..");
    }
    for component in target {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    relative
}

#[derive(Debug)]
struct PublicationFailure {
    outputs: Vec<PathBuf>,
    cause: anyhow::Error,
}

impl std::fmt::Display for PublicationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; {} top-level output(s) were already published and retained",
            self.cause,
            self.outputs.len()
        )
    }
}

impl std::error::Error for PublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

fn publish_members(stage: &Stage, destination: &Destination) -> anyhow::Result<Vec<PathBuf>> {
    let members = stage.top_level();
    for path in &members {
        destination.ensure_free(path.as_os_str())?;
    }
    let mut published = Vec::new();
    for path in members {
        let name = path.as_os_str();
        match destination.publish_exact(&stage.root.join(&path), name) {
            Ok(output) => published.push(output),
            Err(error) => {
                // Do not roll back public paths: a concurrent user could have
                // replaced one, and moving it into staging would delete their
                // data. Report every commit instead; all were fully validated.
                if published.is_empty() {
                    return Err(error);
                }
                return Err(PublicationFailure {
                    outputs: published,
                    cause: error,
                }
                .into());
            }
        }
    }
    Ok(published)
}
