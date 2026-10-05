use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};

/// A held directory descriptor pins the publication parent. `/proc/self/fd`
/// paths avoid following a destination swapped to a symlink during an operation.
#[derive(Debug)]
pub(crate) struct Destination {
    pub(crate) path: PathBuf,
    _directory: File,
    anchor: PathBuf,
}

impl Destination {
    pub(crate) fn open(path: &Path) -> anyhow::Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("Cannot access destination {}", path.display()))?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&path)?;
        let anchor = fd_path(&directory);
        Ok(Self {
            path,
            _directory: directory,
            anchor,
        })
    }

    pub(crate) fn anchored(&self) -> &Path {
        &self.anchor
    }

    pub(crate) fn output(&self, name: &OsStr) -> PathBuf {
        self.path.join(name)
    }

    pub(crate) fn ensure_free(&self, name: &OsStr) -> anyhow::Result<()> {
        match fs::symlink_metadata(self.anchored().join(name)) {
            Ok(_) => anyhow::bail!(
                "Destination already exists: {}",
                self.output(name).display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn publish_unique(
        &self,
        source: &Path,
        name: &OsStr,
        zip: bool,
    ) -> anyhow::Result<PathBuf> {
        for index in 0_u64.. {
            let candidate = numbered_name(name, index, zip);
            match rename_noreplace(source, &self.anchored().join(&candidate)) {
                Ok(()) => return Ok(self.output(&candidate)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).context("Cannot publish archive output without overwriting");
                }
            }
        }
        unreachable!("u64 collision counter exhausted")
    }

    pub(crate) fn publish_exact(&self, source: &Path, name: &OsStr) -> anyhow::Result<PathBuf> {
        rename_noreplace(source, &self.anchored().join(name)).with_context(|| {
            format!(
                "Cannot publish {} without overwriting",
                self.output(name).display()
            )
        })?;
        Ok(self.output(name))
    }
}

pub(crate) fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn numbered_name(name: &OsStr, number: u64, zip: bool) -> OsString {
    if number == 0 {
        return name.to_os_string();
    }
    let path = Path::new(name);
    let extension = if zip { path.extension() } else { None };
    let mut result = if extension.is_some() {
        path.file_stem().unwrap_or(name).to_os_string()
    } else {
        name.to_os_string()
    };
    result.push(format!(" ({number})"));
    if let Some(extension) = extension {
        result.push(".");
        result.push(extension);
    }
    result
}

// Linux has no safe std wrapper for the atomic no-clobber publication operation.
#[allow(unsafe_code)]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let target = CString::new(target.as_os_str().as_bytes())?;
    // SAFETY: both C strings are live and NUL-terminated for this call. Linux
    // renameat2 does not retain pointers. RENAME_NOREPLACE is the atomic no-clobber
    // operation; a check followed by ordinary rename would have a race.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn safe_leaf_name(name: &OsStr) -> anyhow::Result<()> {
    ensure!(
        Path::new(name).components().count() == 1 && name != "." && name != "..",
        "Output must have a filename"
    );
    Ok(())
}
