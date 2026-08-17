//! Notifications are searchable events with causes and actions, not transient interruptions.
//! `lege-sessiond` hosts `org.freedesktop.Notifications` (when it is the selected provider),
//! records every notification into history, and the shell only renders.

use crate::history::{Event, EventAction, EventId, EventKind};
use lege_types::AppId;
use serde::{Deserialize, Serialize};

/// Urgency per the Desktop Notifications spec.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

/// A notification as received on `org.freedesktop.Notifications.Notify`, already attributed
/// to a canonical application where possible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingNotification {
    pub app_name: String,
    pub app_id: Option<AppId>,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    /// `(action_key, label)` pairs.
    pub actions: Vec<(String, String)>,
    pub expire_timeout_ms: i32,
}

impl IncomingNotification {
    /// Convert into a history event so nothing is lost when the shell restarts.
    pub fn into_event(self, at_ms: i64) -> Event {
        let message = if self.body.is_empty() {
            format!("{}: {}", self.app_name, self.summary)
        } else {
            format!("{}: {} — {}", self.app_name, self.summary, self.body)
        };
        Event {
            id: EventId::new(),
            at_ms,
            kind: EventKind::Notification,
            app_id: self.app_id,
            instance_id: None,
            message,
            cause: Some(format!("urgency:{:?}", self.urgency).to_lowercase()),
            actions: self
                .actions
                .into_iter()
                .map(|(key, label)| EventAction {
                    label,
                    command: format!("notify:action:{key}"),
                })
                .collect(),
            reversible: false,
        }
    }
}
