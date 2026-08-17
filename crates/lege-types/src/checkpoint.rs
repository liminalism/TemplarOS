//! The session-checkpoint protocol first-party applications implement so that pressure
//! containment does not lose work (planning doc 01 §5).
//!
//! ```text
//! prepare-for-pressure  →  checkpoint-now  →  checkpoint-complete  →  restore-instance
//! ```

use crate::InstanceId;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A request sent from the governor (through the session daemon) to an application instance.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CheckpointRequest {
    /// Trim caches; pressure is building. No state must be lost.
    PrepareForPressure { instance: InstanceId },

    /// Persist enough state to be restored later, within `deadline`.
    CheckpointNow {
        instance: InstanceId,
        deadline: Duration,
    },

    /// Re-open a previously checkpointed instance.
    RestoreInstance {
        instance: InstanceId,
        previous: InstanceId,
    },
}

/// The application's answer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CheckpointResponse {
    /// Caches trimmed; `released_bytes` is best-effort.
    Prepared {
        instance: InstanceId,
        released_bytes: u64,
    },

    /// State saved; `token` is opaque to the OS and handed back on restore.
    CheckpointComplete {
        instance: InstanceId,
        token: String,
        has_unsaved_work: bool,
    },

    /// The instance cannot checkpoint right now (for example mid-transaction).
    Refused {
        instance: InstanceId,
        reason: String,
    },
}
