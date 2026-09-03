//! What can go wrong, and the two failures that are not failures.
//!
//! [`IndexError::Corrupt`] means "rebuild me", not "give up": the database is derived.
//! [`IndexError::SnapshotRequired`] means "your continuity is gone, ask again from scratch" — the
//! one answer a service must give instead of inventing the history it cannot prove.

use crate::model::{BudgetKind, RootId, RootState};

pub type Result<T> = std::result::Result<T, IndexError>;

#[derive(Debug)]
pub enum IndexError {
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
    Cancelled,
    /// A cursor or a change sequence has aged out. The client re-asks; the service does not guess.
    SnapshotRequired {
        reason: &'static str,
    },
    BudgetExhausted(BudgetKind),
    /// The file is not something this build understands. Answered by rebuilding it.
    Corrupt {
        detail: String,
    },
    Unsupported {
        detail: String,
    },
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            Self::Unsupported { detail } => write!(formatter, "unsupported: {detail}"),
        }
    }
}

impl std::error::Error for IndexError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for IndexError {
    fn from(error: rusqlite::Error) -> Self {
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
    /// The stable name a D-Bus client matches on. Only `SnapshotRequired` gets its own: it is the
    /// one error a correct client is expected to handle rather than report.
    #[must_use]
    pub fn dbus_name(&self) -> &'static str {
        match self {
            Self::SnapshotRequired { .. } => "org.legeos.Files.Index1.Error.SnapshotRequired",
            Self::UnknownRoot(_) | Self::RootExists(_) | Self::IllegalTransition { .. } => {
                "org.freedesktop.DBus.Error.InvalidArgs"
            }
            _ => "org.freedesktop.DBus.Error.Failed",
        }
    }
}
