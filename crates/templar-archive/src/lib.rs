//! Streaming archive operations for the native Linux desktop.
//!
//! ZIP creation uses UTF-8 member names and DEFLATE. Extraction uses the system
//! libarchive, never its disk writer: untrusted entries are validated and written
//! inside a private same-filesystem staging directory before publication. Existing
//! outputs are never replaced. Ownership, timestamps, special permission bits,
//! ACLs and xattrs are deliberately not restored.
//!
//! Requires Linux `/proc/self/fd`, atomic `renameat2(RENAME_NOREPLACE)`, the
//! `C.UTF-8` locale, and system libarchive 3.4+. Gzip integrity is checked by a
//! streaming Rust decoder because libarchive does not validate gzip trailer CRC.
//! Encrypted ZIPs can request a password; encrypted RAR/7z, special files, cyclic
//! links, and native codec/metadata warnings fail explicitly before publication.
//! ZIP creation rejects non-UTF-8 names and never follows source symbolic links.
//! ZIP symlink payloads are additionally CRC-checked: only the ZIP crate's
//! supported stored/DEFLATE methods are accepted for them; gzip-wrapped ZIP
//! symlinks and unsupported symlink encryption fail explicitly.

mod compress;
mod destination;
mod extract;
// libarchive is a C ABI. Confine its unavoidable unsafe calls to the owning Reader boundary;
// every pointer lifetime and callback invariant is documented at the call site.
#[allow(unsafe_code)]
mod native;
mod paths;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub use compress::compress;
pub use extract::extract;

#[derive(Debug, Clone)]
pub enum ExtractMode {
    Here,
    Smart,
    Directory(PathBuf),
}

#[derive(Debug)]
pub enum Progress {
    /// The relative member currently being processed.
    Entry(PathBuf),
    /// Newly processed, uncompressed bytes (a delta, not a running total).
    Bytes(u64),
}

#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Archive operation cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// An encrypted ZIP needs a password. Incorrect supplied passwords and formats
/// whose encryption libarchive cannot decode are ordinary, explicit errors.
#[derive(Debug)]
pub struct PasswordRequired;

impl fmt::Display for PasswordRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("This ZIP archive requires a password")
    }
}

impl std::error::Error for PasswordRequired {}

/// A batch stopped at `archive`. Earlier outputs remain available and are never
/// silently discarded, including outputs retained after a publication race.
/// This marker is present when at least one output was retained. With no outputs,
/// the original error is returned with archive-path context, so password and
/// cancellation markers remain directly downcastable. Inspect the error chain
/// for those markers when this wrapper is present.
#[derive(Debug)]
pub struct ExtractFailure {
    pub completed: Vec<PathBuf>,
    pub archive: PathBuf,
    cause: anyhow::Error,
}

impl ExtractFailure {
    pub(crate) fn new(completed: Vec<PathBuf>, archive: &Path, cause: anyhow::Error) -> Self {
        Self {
            completed,
            archive: archive.to_path_buf(),
            cause,
        }
    }
}

impl fmt::Display for ExtractFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Cannot extract {}: {}",
            self.archive.display(),
            self.cause
        )?;
        if !self.completed.is_empty() {
            f.write_str("; already extracted:")?;
            for path in &self.completed {
                write!(f, " {}", path.display())?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ExtractFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.cause.as_ref())
    }
}

pub(crate) fn check_cancel(cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}

const BUFFER_SIZE: usize = 128 * 1024;
