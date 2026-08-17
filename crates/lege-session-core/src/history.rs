//! System-wide change history: every important system decision can be explained, and many can
//! be undone. File operations, installs, default-handler changes, settings changes, autostart
//! decisions, driver changes, updates, pressure actions and crashes all land here.

use lege_types::{AppId, InstanceId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub Uuid);

impl EventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

/// Coarse category, for filtering and icons.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    FileOperation,
    AssociationChange,
    SettingChange,
    ProviderChange,
    AutostartDecision,
    ResourceAction,
    Crash,
    Update,
    DriverChange,
    Notification,
    Launch,
}

/// A user-invocable follow-up ("Undo", "Restore", "Open folder").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventAction {
    pub label: String,
    /// Opaque, interpreted by the originating domain (e.g. `assoc:undo:<change-id>`).
    pub command: String,
}

/// One entry in the history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    /// Unix time, milliseconds.
    pub at_ms: i64,
    pub kind: EventKind,
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    /// Human sentence, e.g. "Waterfox was terminated at 14:32 after its group reached 13.4 GiB".
    pub message: String,
    /// Machine-readable cause, e.g. `pressure:critical:full-psi=2.4`.
    pub cause: Option<String>,
    pub actions: Vec<EventAction>,
    /// True while an "Undo" action is still valid.
    pub reversible: bool,
}

/// Storage boundary; implemented over `lege-state`.
pub trait HistoryStore: Send + Sync {
    fn append(&self, event: Event) -> anyhow::Result<()>;
    fn recent(&self, limit: usize) -> anyhow::Result<Vec<Event>>;
    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Event>>;
}

/// In-memory store for tests and observe-only runs.
#[derive(Debug, Default)]
pub struct MemoryHistory(std::sync::Mutex<Vec<Event>>);

impl HistoryStore for MemoryHistory {
    fn append(&self, event: Event) -> anyhow::Result<()> {
        self.0.lock().expect("history poisoned").push(event);
        Ok(())
    }

    fn recent(&self, limit: usize) -> anyhow::Result<Vec<Event>> {
        let events = self.0.lock().expect("history poisoned");
        Ok(events.iter().rev().take(limit).cloned().collect())
    }

    fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Event>> {
        let needle = query.to_lowercase();
        let events = self.0.lock().expect("history poisoned");
        Ok(events
            .iter()
            .rev()
            .filter(|event| event.message.to_lowercase().contains(&needle))
            .take(limit)
            .cloned()
            .collect())
    }
}
