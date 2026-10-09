//! What can go wrong, and the failures that are not failures.
//!
//! [`IndexError::Corrupt`] and [`IndexError::Incompatible`] mean "quarantine me and rebuild", not
//! "give up": the database is derived. Every other failure to open the cache — a busy owner, a
//! read-only file, a full disk — is [`IndexError::Unavailable`], which leaves the files exactly as
//! they were. [`IndexError::SnapshotRequired`] means "your continuity is gone, ask again from
//! scratch" — the one answer a service must give instead of inventing the history it cannot prove.

use crate::model::{BudgetKind, RootId, RootState};
use serde::{Deserialize, Serialize};

pub type Result<T, E = IndexError> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum IndexError {
    #[cfg(feature = "store")]
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    UnknownRoot(RootId),
    RootExists(String),
    IllegalTransition {
        root: RootId,
        from: RootState,
        to: RootState,
    },
    ScanInFlight(RootId),
    /// The caller withdrew the request, a newer request with the same tag superseded it, its
    /// deadline passed, or SQLite was interrupted on its behalf.
    Cancelled,
    /// A cursor or a change sequence has aged out. The client re-asks; the service does not guess.
    SnapshotRequired {
        reason: &'static str,
    },
    BudgetExhausted(BudgetKind),
    /// The file is not a database this build can read. Answered by quarantining and rebuilding it.
    Corrupt {
        detail: String,
    },
    /// The file is a readable index of another format or a newer schema. Quarantined, not
    /// deleted: a newer build may still want it.
    Incompatible {
        found: String,
        supported: String,
    },
    /// Admission refused the request before any work started: a full queue, a caller at its own
    /// limit, or no read connection within the wait.
    Busy {
        what: &'static str,
    },
    /// A snapshot would exceed the service's limits. The frame was abandoned before sealing.
    TooLarge {
        entries: u64,
        bytes: u64,
    },
    /// The cache cannot be opened right now and was left untouched.
    Unavailable {
        reason: OpenFailure,
    },
    Unsupported {
        detail: String,
    },
}

/// Why the cache could not be opened, classified before anything is recovered.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "failure", rename_all = "kebab-case")]
pub enum OpenFailure {
    /// Not a database, or a database SQLite reports as corrupt.
    Corrupt { detail: String },
    /// An index of another format or a newer schema.
    Incompatible { found: String, supported: String },
    /// Another owner holds the cache.
    Busy,
    /// The environment refused: full disk, read-only file or filesystem, permissions, i/o.
    Environment {
        kind: EnvironmentKind,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvironmentKind {
    Full,
    ReadOnly,
    Permission,
    Io,
    CannotOpen,
}

impl OpenFailure {
    /// Whether this failure is answered by quarantining the cache and rebuilding it. Only proof
    /// that the file itself is unusable qualifies; everything else leaves it alone.
    #[must_use]
    pub fn recoverable_by_quarantine(&self) -> bool {
        matches!(self, Self::Corrupt { .. } | Self::Incompatible { .. })
    }
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Corrupt { detail } => write!(formatter, "corrupt: {detail}"),
            Self::Incompatible { found, supported } => {
                write!(
                    formatter,
                    "incompatible: found {found}, this build supports {supported}"
                )
            }
            Self::Busy => write!(formatter, "busy: another owner holds the index cache"),
            Self::Environment { kind, detail } => {
                let kind = match kind {
                    EnvironmentKind::Full => "disk full",
                    EnvironmentKind::ReadOnly => "read-only",
                    EnvironmentKind::Permission => "permission denied",
                    EnvironmentKind::Io => "i/o error",
                    EnvironmentKind::CannotOpen => "cannot open",
                };
                write!(formatter, "{kind}: {detail}")
            }
        }
    }
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "store")]
            Self::Sqlite(error) => write!(formatter, "index database: {error}"),
            Self::Io(error) => write!(formatter, "index i/o: {error}"),
            Self::Json(error) => write!(formatter, "index json: {error}"),
            Self::UnknownRoot(root) => write!(formatter, "no such indexed root: {}", root.0),
            Self::RootExists(path) => write!(formatter, "root already indexed: {path}"),
            Self::IllegalTransition { root, from, to } => write!(
                formatter,
                "root {} cannot go from {} to {}",
                root.0,
                from.as_str(),
                to.as_str()
            ),
            Self::ScanInFlight(root) => {
                write!(formatter, "a scan of root {} is already running", root.0)
            }
            Self::Cancelled => write!(formatter, "cancelled"),
            Self::SnapshotRequired { reason } => {
                write!(formatter, "snapshot required: {reason}")
            }
            Self::BudgetExhausted(kind) => {
                write!(formatter, "scan budget exhausted: {}", kind.as_str())
            }
            Self::Corrupt { detail } => write!(formatter, "index database unusable: {detail}"),
            Self::Incompatible { found, supported } => write!(
                formatter,
                "index database incompatible: found {found}, this build supports {supported}"
            ),
            Self::Busy { what } => write!(formatter, "busy: {what}"),
            Self::TooLarge { entries, bytes } => write!(
                formatter,
                "snapshot too large: more than {entries} entries or {bytes} bytes"
            ),
            Self::Unavailable { reason } => write!(formatter, "index unavailable: {reason}"),
            Self::Unsupported { detail } => write!(formatter, "unsupported: {detail}"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "store")]
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(feature = "store")]
impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
        // An interrupted statement is a cancellation the caller asked for, never a database fault.
        if error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
            return Self::Cancelled;
        }
        Self::Sqlite(error)
    }
}

impl From<std::io::Error> for IndexError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for IndexError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl IndexError {
    /// The stable name a D-Bus client matches on. Each error a correct client is expected to
    /// handle rather than report has its own.
    #[must_use]
    pub fn dbus_name(&self) -> &'static str {
        match self {
            Self::SnapshotRequired { .. } => "org.templaros.Files.Index1.Error.SnapshotRequired",
            Self::Busy { .. } => "org.templaros.Files.Index1.Error.Busy",
            Self::Cancelled => "org.templaros.Files.Index1.Error.Cancelled",
            Self::TooLarge { .. } => "org.templaros.Files.Index1.Error.TooLarge",
            Self::Unavailable { .. } => "org.templaros.Files.Index1.Error.Unavailable",
            Self::UnknownRoot(_) | Self::RootExists(_) | Self::IllegalTransition { .. } => {
                "org.freedesktop.DBus.Error.InvalidArgs"
            }
            _ => "org.freedesktop.DBus.Error.Failed",
        }
    }

    /// How a failure to open or migrate the cache is answered (`Index::open`). Only proof that
    /// the file is unusable is `Corrupt`/`Incompatible`; an ambiguous `SQLITE_ERROR` is resolved
    /// by the caller with `PRAGMA quick_check` before it may be called corruption.
    #[must_use]
    pub fn open_failure(&self) -> OpenFailure {
        let environment = |kind, detail: String| OpenFailure::Environment { kind, detail };
        match self {
            Self::Unavailable { reason } => reason.clone(),
            Self::Corrupt { detail } => OpenFailure::Corrupt {
                detail: detail.clone(),
            },
            Self::Incompatible { found, supported } => OpenFailure::Incompatible {
                found: found.clone(),
                supported: supported.clone(),
            },
            #[cfg(feature = "store")]
            Self::Sqlite(error) => {
                use rusqlite::ErrorCode as Code;
                let detail = error.to_string();
                match error.sqlite_error_code() {
                    Some(Code::DatabaseCorrupt | Code::NotADatabase) => {
                        OpenFailure::Corrupt { detail }
                    }
                    Some(Code::DatabaseBusy | Code::DatabaseLocked) => OpenFailure::Busy,
                    Some(Code::DiskFull) => environment(EnvironmentKind::Full, detail),
                    Some(Code::ReadOnly) => environment(EnvironmentKind::ReadOnly, detail),
                    Some(Code::PermissionDenied | Code::AuthorizationForStatementDenied) => {
                        environment(EnvironmentKind::Permission, detail)
                    }
                    Some(Code::CannotOpen) => environment(EnvironmentKind::CannotOpen, detail),
                    _ => environment(EnvironmentKind::Io, detail),
                }
            }
            Self::Io(error) => {
                use std::io::ErrorKind as Kind;
                let detail = error.to_string();
                match error.kind() {
                    Kind::PermissionDenied => environment(EnvironmentKind::Permission, detail),
                    Kind::StorageFull => environment(EnvironmentKind::Full, detail),
                    Kind::ReadOnlyFilesystem => environment(EnvironmentKind::ReadOnly, detail),
                    _ => environment(EnvironmentKind::Io, detail),
                }
            }
            other => environment(EnvironmentKind::Io, other.to_string()),
        }
    }
}
