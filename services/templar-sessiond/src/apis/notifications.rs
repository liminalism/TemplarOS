//! `org.freedesktop.Notifications`, hosted only while sessiond is the selected
//! `notification-server` provider (`crate::background::notification_provider`). Every
//! notification becomes a searchable history event (`docs/architecture/history.md`) so nothing
//! is lost if the shell restarts; the shell only renders.
//!
//! Attribution to a canonical [`AppId`] is a best-effort match of the caller-supplied
//! `app_name` against an installed manifest's display name or desktop-file ID — the spec does
//! not hand this interface the caller's PID/cgroup the way `AppIdentity1.ResolveWindow` gets
//! one, so it cannot be cgroup-verified the way window identity is. A future improvement would
//! read the D-Bus message header's sender and resolve its peer credentials.
//!
//! Every committed transition — creation, replacement, sender close, expiry, dismiss and
//! action — is published exactly once on each side that owns it: the standard
//! `NotificationClosed`/`ActionInvoked` on the freedesktop path/interface, and the companion
//! `Changed` on the TemplarOS path/interface. The journal below is the read model behind
//! them: a per-owner sequence over a bounded transition ring, so the shell can
//! subscribe-before-snapshot, tell a same-ID replacement apart from already-presented
//! content by `(id, event_id)`, and reconcile a service-owner change by epoch instead of by
//! polling or by guessing from the highest numeric ID.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use templar_appd::Registry;
use templar_session_core::history::{
    Event, EventAction, EventId, EventKind, HistoryStore, SqliteHistory,
};
use templar_state::Store;
use templar_types::{AppCatalog, AppId, dbus::session as names};
use zbus::{fdo, object_server::SignalEmitter, zvariant::OwnedValue};

/// The freedesktop notification path/interface, served only while sessiond holds the
/// provider selection. Mirrors `crate::background::notification_provider`.
const FDO_PATH: &str = "/org/freedesktop/Notifications";
const FDO_IFACE: &str = "org.freedesktop.Notifications";

/// `NotificationClosed` reasons, per the Desktop Notifications spec.
const REASON_EXPIRED: u32 = 1;
const REASON_DISMISSED: u32 = 2;
const REASON_CLOSED_BY_CALLER: u32 = 3;

/// Version of the structured presentation documents (`ActiveNotification`, `SnapshotDoc`,
/// `TransitionsDoc`). Bump when a field's meaning changes; additive fields do not bump.
pub const PRESENTATION_FORMAT_VERSION: u32 = 1;

/// How many committed transitions the journal retains for `TransitionsSince`. Bursts longer
/// than this resynchronize through `Snapshot` instead of replaying without bound.
pub const JOURNAL_RING_CAP: usize = 64;

/// One committed transition, in journal order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionKind {
    Created,
    Replaced,
    Closed,
    Expired,
    Dismissed,
    Action,
}

/// A journal entry: what changed, at which sequence, carrying the full row for arrivals so a
/// burst needs no extra fetch per member.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transition {
    pub sequence: u64,
    pub kind: TransitionKind,
    pub id: u32,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification: Option<ActiveNotification>,
}

/// The whole active set at one sequence, under one owner epoch: the snapshot half of the
/// subscribe-before-snapshot read model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotDoc {
    pub format_version: u32,
    pub owner_epoch: String,
    pub sequence: u64,
    pub notifications: Vec<ActiveNotification>,
}

/// The transitions after a cursor: the catch-up half. `must_resync` means the cursor is
/// from another owner lifetime or fell out of the ring; the caller re-snapshots instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitionsDoc {
    pub format_version: u32,
    pub owner_epoch: String,
    pub sequence: u64,
    pub must_resync: bool,
    pub transitions: Vec<Transition>,
}

#[derive(Debug)]
struct JournalInner {
    owner_epoch: String,
    /// Serializes the database mutation, journal commit and publication with snapshots.
    /// A database lock alone cannot protect the cursor read after the query.
    mutation: tokio::sync::Mutex<()>,
    recovered: AtomicBool,
    state: parking_lot::Mutex<JournalState>,
    /// Whether sessiond currently hosts the freedesktop name. The companion `Changed` is
    /// always served; the standard signals are only emitted while hosted, so a third-party
    /// notification server never sees a stray close for an ID it owns.
    fdo_hosting: AtomicBool,
    /// The session-bus connection, stashed from the first `Notify` call so the detached
    /// expiry tasks can publish without their own connection per timer.
    expiry_bus: parking_lot::Mutex<Option<zbus::Connection>>,
}

#[derive(Debug)]
struct JournalState {
    sequence: u64,
    ring: VecDeque<Transition>,
    /// The sequence at which each live `(id, event_id)` last changed. Rows the journal
    /// never saw — active across a restart — read as 0, which is precisely "predates this
    /// owner" and must never toast as new.
    last_change: HashMap<u32, (String, u64)>,
}

/// The per-owner notification journal, shared by the freedesktop API (which commits
/// arrivals, replacements, closes and expiries) and the TemplarOS companion API (which
/// commits dismissals and actions and serves the read model).
#[derive(Clone, Debug)]
pub struct NotificationJournal(Arc<JournalInner>);

impl NotificationJournal {
    pub fn new() -> Self {
        Self(Arc::new(JournalInner {
            owner_epoch: uuid::Uuid::new_v4().to_string(),
            mutation: tokio::sync::Mutex::new(()),
            recovered: AtomicBool::new(false),
            state: parking_lot::Mutex::new(JournalState {
                sequence: 0,
                ring: VecDeque::new(),
                last_change: HashMap::new(),
            }),
            fdo_hosting: AtomicBool::new(false),
            expiry_bus: parking_lot::Mutex::new(None),
        }))
    }

    pub fn owner_epoch(&self) -> &str {
        &self.0.owner_epoch
    }

    pub fn sequence(&self) -> u64 {
        self.0.state.lock().sequence
    }

    pub fn set_fdo_hosting(&self, hosting: bool) {
        self.0.fdo_hosting.store(hosting, Ordering::SeqCst);
    }

    pub fn fdo_hosting(&self) -> bool {
        self.0.fdo_hosting.load(Ordering::SeqCst)
    }

    pub fn stash_bus(&self, connection: zbus::Connection) {
        *self.0.expiry_bus.lock() = Some(connection);
    }

    fn expiry_bus(&self) -> Option<zbus::Connection> {
        self.0.expiry_bus.lock().clone()
    }

    /// Record one committed transition. Returns its sequence. Callers publish the signals
    /// afterwards; the journal itself never touches the bus.
    pub fn commit(
        &self,
        kind: TransitionKind,
        id: u32,
        event_id: &str,
        action_key: Option<String>,
        notification: Option<ActiveNotification>,
    ) -> u64 {
        let mut state = self.0.state.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        if matches!(kind, TransitionKind::Created | TransitionKind::Replaced) {
            state.last_change.insert(id, (event_id.to_owned(), sequence));
        } else if kind != TransitionKind::Action
            && state.last_change.get(&id).is_some_and(|(event, _)| event == event_id)
        {
            state.last_change.remove(&id);
        }
        let mut notification = notification;
        if let Some(row) = notification.as_mut() {
            row.sequence = sequence;
        }
        state.ring.push_back(Transition {
            sequence,
            kind,
            id,
            event_id: event_id.to_owned(),
            action_key,
            notification,
        });
        while state.ring.len() > JOURNAL_RING_CAP {
            state.ring.pop_front();
        }
        sequence
    }

    pub fn last_change(&self, id: u32, event_id: &str) -> u64 {
        self.0
            .state
            .lock()
            .last_change
            .get(&id)
            .filter(|(event, _)| event == event_id)
            .map_or(0, |(_, sequence)| *sequence)
    }

    pub fn transitions_since(&self, cursor: u64, owner_epoch: &str) -> TransitionsDoc {
        let state = self.0.state.lock();
        let epoch = self.0.owner_epoch.clone();
        if owner_epoch != epoch || cursor > state.sequence {
            return TransitionsDoc {
                format_version: PRESENTATION_FORMAT_VERSION,
                owner_epoch: epoch,
                sequence: state.sequence,
                must_resync: true,
                transitions: Vec::new(),
            };
        }
        if let Some(oldest) = state.ring.front()
            && cursor < oldest.sequence.saturating_sub(1)
        {
            return TransitionsDoc {
                format_version: PRESENTATION_FORMAT_VERSION,
                owner_epoch: epoch,
                sequence: state.sequence,
                must_resync: true,
                transitions: Vec::new(),
            };
        }
        TransitionsDoc {
            format_version: PRESENTATION_FORMAT_VERSION,
            owner_epoch: epoch,
            sequence: state.sequence,
            must_resync: false,
            transitions: state
                .ring
                .iter()
                .filter(|transition| transition.sequence > cursor)
                .cloned()
                .collect(),
        }
    }
}

impl Default for NotificationJournal {
    fn default() -> Self {
        Self::new()
    }
}

/// The daemon's journal. One sessiond per process means one owner epoch per process, so the
/// default constructors share this rather than threading a handle through the serve wiring;
/// tests pass fresh journals via `with_journal` for isolated sequences and epochs.
pub fn global_journal() -> NotificationJournal {
    static GLOBAL: std::sync::OnceLock<NotificationJournal> = std::sync::OnceLock::new();
    GLOBAL.get_or_init(NotificationJournal::new).clone()
}

/// The companion `Changed` on the TemplarOS path: one per committed transition.
async fn emit_changed(connection: &zbus::Connection) {
    let _ = connection
        .emit_signal(
            None::<&str>,
            names::NOTIFICATIONS_PATH,
            names::NOTIFICATIONS_IFACE,
            "Changed",
            &(),
        )
        .await;
}

/// A standard `NotificationClosed` on the freedesktop path. The caller gates on
/// [`NotificationJournal::fdo_hosting`]; the path/interface are always explicit so a
/// companion-API call can never emit onto its own path by accident.
async fn emit_fdo_closed(connection: &zbus::Connection, id: u32, reason: u32) {
    let _ = connection
        .emit_signal(
            None::<&str>,
            FDO_PATH,
            FDO_IFACE,
            "NotificationClosed",
            &(id, reason),
        )
        .await;
}

async fn emit_fdo_action(connection: &zbus::Connection, id: u32, action_key: &str) {
    let _ = connection
        .emit_signal(
            None::<&str>,
            FDO_PATH,
            FDO_IFACE,
            "ActionInvoked",
            &(id, action_key),
        )
        .await;
}

#[derive(Debug)]
pub struct NotificationsApi {
    store: Store,
    history: Arc<SqliteHistory>,
    registry: Arc<Registry>,
    journal: NotificationJournal,
}

impl NotificationsApi {
    pub fn new(store: Store, history: Arc<SqliteHistory>, registry: Arc<Registry>) -> Self {
        Self {
            store,
            history,
            registry,
            journal: global_journal(),
        }
    }

    pub fn with_journal(mut self, journal: NotificationJournal) -> Self {
        self.journal = journal;
        self
    }

    fn attribute(&self, app_name: &str) -> Option<AppId> {
        self.registry
            .manifests()
            .into_iter()
            .find(|manifest| {
                manifest.display_name == app_name
                    || manifest
                        .primary_desktop_file_id
                        .trim_end_matches(".desktop")
                        == app_name
            })
            .map(|manifest| manifest.id.clone())
    }

    /// Insert the notification, or replace the row `replaces_id` names. Returns the live ID.
    fn store_active(&self, row: &ActiveRow<'_>) -> anyhow::Result<u32> {
        let ActiveRow {
            replaces_id,
            event_id,
            app_name,
            app_id,
            created_ms,
            summary,
            body,
            icon,
            urgency,
            category,
            actions_json,
            expires_ms,
        } = *row;
        self.store.transaction(|tx| -> anyhow::Result<u32> {
            if replaces_id == 0 {
                tx.execute(
                    "INSERT INTO notifications_active
                        (event_id, app_name, app_id, created_ms, summary, body, icon, urgency,
                         category, actions_json, expires_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    params![
                        event_id,
                        app_name,
                        app_id,
                        created_ms,
                        summary,
                        body,
                        icon,
                        urgency,
                        category,
                        actions_json,
                        expires_ms
                    ],
                )?;
                return Ok(u32::try_from(tx.last_insert_rowid()).unwrap_or(u32::MAX));
            }
            tx.execute(
                "INSERT INTO notifications_active
                    (id, event_id, app_name, app_id, created_ms, summary, body, icon,
                     urgency, category, actions_json, expires_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (id) DO UPDATE SET
                    event_id = excluded.event_id, app_name = excluded.app_name,
                    app_id = excluded.app_id, created_ms = excluded.created_ms,
                    summary = excluded.summary, body = excluded.body, icon = excluded.icon,
                    urgency = excluded.urgency, category = excluded.category,
                    actions_json = excluded.actions_json, expires_ms = excluded.expires_ms",
                params![
                    replaces_id,
                    event_id,
                    app_name,
                    app_id,
                    created_ms,
                    summary,
                    body,
                    icon,
                    urgency,
                    category,
                    actions_json,
                    expires_ms
                ],
            )?;
            Ok(replaces_id)
        })
    }

    /// Remove the notification once it expires. Guarded by `event_id` so replacing an ID cannot
    /// let the outgoing notification's timer remove the one that took its place — and the guard
    /// gates publication too: a stale timer publishes nothing.
    fn spawn_expiry(&self, id: u32, event_id: String, expires_ms: i64) {
        spawn_expiry(self.store.clone(), self.journal.clone(), id, event_id, expires_ms);
    }
}

fn spawn_expiry(
    store: Store,
    journal: NotificationJournal,
    id: u32,
    event_id: String,
    expires_ms: i64,
) {
    tokio::spawn(async move {
        let delay = expires_ms.saturating_sub(templar_state::now_ms()).max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let _mutation = journal.0.mutation.lock().await;
        let removed = store
            .transaction(|tx| {
                Ok(tx.execute(
                    "DELETE FROM notifications_active WHERE id = ?1 AND event_id = ?2",
                    params![id, event_id],
                )? != 0)
            })
            .unwrap_or(false);
        if !removed {
            return;
        }
        journal.commit(TransitionKind::Expired, id, &event_id, None, None);
        if let Some(connection) = journal.expiry_bus() {
            if journal.fdo_hosting() {
                emit_fdo_closed(&connection, id, REASON_EXPIRED).await;
            }
            emit_changed(&connection).await;
        }
    });
}

/// The columns of one `notifications_active` row, gathered so the write is a single argument.
struct ActiveRow<'a> {
    replaces_id: u32,
    event_id: &'a str,
    app_name: &'a str,
    app_id: Option<&'a str>,
    created_ms: i64,
    summary: &'a str,
    body: &'a str,
    icon: &'a str,
    urgency: u8,
    category: &'a str,
    actions_json: &'a str,
    expires_ms: Option<i64>,
}

/// Complete presentation data for one still-active notification: everything the shell's
/// toast card renders, not the summary alone. `event_id` is the replacement generation — a
/// same-ID replacement carries a new one — and `sequence` is the journal position of this
/// row's last change, or 0 when the row predates the current owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveNotification {
    pub format_version: u32,
    pub id: u32,
    pub event_id: String,
    pub sequence: u64,
    pub app_name: String,
    pub app_id: Option<String>,
    pub summary: String,
    pub body: String,
    pub icon: String,
    /// freedesktop urgency: 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    pub category: String,
    pub actions: Vec<(String, String)>,
    pub created_ms: i64,
    pub expires_ms: Option<i64>,
}

/// TemplarOS-side read/control API.  The freedesktop interface intentionally has no enumeration
/// method; this companion keeps the shell a renderer while the notification server owns active
/// state, expiry and dismissal.
#[derive(Debug)]
pub struct NotificationsControlApi {
    store: Store,
    journal: NotificationJournal,
}

impl NotificationsControlApi {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            journal: global_journal(),
        }
    }

    #[cfg(test)]
    pub fn with_journal(mut self, journal: NotificationJournal) -> Self {
        self.journal = journal;
        self
    }

    /// Once per owner, discard already-expired persisted rows and re-arm future expiries.
    /// Called under `mutation`; live rows must never be silently pruned ahead of their timer.
    fn recover_expiry(&self, connection: &zbus::Connection) -> anyhow::Result<()> {
        self.journal.stash_bus(connection.clone());
        if self.journal.0.recovered.load(Ordering::SeqCst) {
            return Ok(());
        }
        let now = templar_state::now_ms();
        for row in self.active_rows()? {
            if row.sequence != 0 {
                continue;
            }
            if let Some(expires) = row.expires_ms {
                if expires <= now {
                    self.store.transaction(|tx| {
                        tx.execute(
                            "DELETE FROM notifications_active WHERE id = ?1 AND event_id = ?2",
                            params![row.id, row.event_id],
                        )?;
                        Ok(())
                    })?;
                } else {
                    spawn_expiry(
                        self.store.clone(), self.journal.clone(), row.id, row.event_id, expires,
                    );
                }
            }
        }
        self.journal.0.recovered.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn active_rows(&self) -> anyhow::Result<Vec<ActiveNotification>> {
        let connection = self.store.lock();
        let mut statement = connection.prepare(
            "SELECT id, event_id, app_name, app_id, summary, body, icon, urgency, category,
                    actions_json, created_ms, expires_ms
               FROM notifications_active ORDER BY created_ms DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            let id: u32 = row.get(0)?;
            let event_id: String = row.get(1)?;
            let actions_json: String = row.get(9)?;
            Ok(ActiveNotification {
                format_version: PRESENTATION_FORMAT_VERSION,
                id,
                sequence: self.journal.last_change(id, &event_id),
                event_id,
                app_name: row.get(2)?,
                app_id: row.get(3)?,
                summary: row.get(4)?,
                body: row.get(5)?,
                icon: row.get(6)?,
                urgency: row.get(7)?,
                category: row.get(8)?,
                actions: serde_json::from_str(&actions_json).unwrap_or_default(),
                created_ms: row.get(10)?,
                expires_ms: row.get(11)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(anyhow::Error::from)
    }
}

#[zbus::interface(name = "org.templaros.Notifications1")]
impl NotificationsControlApi {
    async fn list_active(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<Vec<String>> {
        let _mutation = self.journal.0.mutation.lock().await;
        self.recover_expiry(connection)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let rows = self
            .active_rows()
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        rows.into_iter()
            .map(|notification| {
                serde_json::to_string(&notification)
                    .map_err(|error| fdo::Error::Failed(error.to_string()))
            })
            .collect()
    }

    /// The whole active set at one journal position, under one owner epoch: the snapshot
    /// half of subscribe-before-snapshot. Subscribe to `Changed` first, then call this; rows
    /// already present are existing state, and every later `Changed` is answered with
    /// `TransitionsSince`, never with a highest-ID guess.
    async fn snapshot(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let _mutation = self.journal.0.mutation.lock().await;
        self.recover_expiry(connection)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let rows = self
            .active_rows()
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let document = SnapshotDoc {
            format_version: PRESENTATION_FORMAT_VERSION,
            owner_epoch: self.journal.owner_epoch().to_owned(),
            sequence: self.journal.sequence(),
            notifications: rows,
        };
        serde_json::to_string(&document).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    /// The committed transitions after `sequence` under `owner_epoch`. An epoch from another
    /// owner lifetime — a service restart — answers `must_resync`, and so does a cursor that
    /// fell out of the ring; both re-snapshot instead of replaying or losing arrivals.
    async fn transitions_since(&self, sequence: u64, owner_epoch: &str) -> fdo::Result<String> {
        let _mutation = self.journal.0.mutation.lock().await;
        let document = self.journal.transitions_since(sequence, owner_epoch);
        serde_json::to_string(&document).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    async fn dismiss(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let _mutation = self.journal.0.mutation.lock().await;
        let event_id: Option<String> = self
            .store
            .transaction(|tx| {
                let event_id: Option<String> = tx
                    .query_row(
                        "SELECT event_id FROM notifications_active WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )
                    .ok();
                if event_id.is_some() {
                    tx.execute("DELETE FROM notifications_active WHERE id = ?1", [id])?;
                }
                Ok(event_id)
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let Some(event_id) = event_id else {
            return Ok(false);
        };
        self.journal
            .commit(TransitionKind::Dismissed, id, &event_id, None, None);
        if self.journal.fdo_hosting() {
            emit_fdo_closed(emitter.connection(), id, REASON_DISMISSED).await;
        }
        let _ = emitter.changed().await;
        Ok(true)
    }

    async fn clear_all(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<u32> {
        let _mutation = self.journal.0.mutation.lock().await;
        let removed: Vec<(u32, String)> = self
            .store
            .transaction(|tx| {
                let mut statement = tx.prepare("SELECT id, event_id FROM notifications_active")?;
                let rows: Vec<(u32, String)> = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                tx.execute("DELETE FROM notifications_active", [])?;
                Ok(rows)
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        if removed.is_empty() {
            return Ok(0);
        }
        for (id, event_id) in &removed {
            self.journal
                .commit(TransitionKind::Dismissed, *id, event_id, None, None);
        }
        if self.journal.fdo_hosting() {
            for (id, _) in &removed {
                emit_fdo_closed(emitter.connection(), *id, REASON_DISMISSED).await;
            }
        }
        for _ in &removed {
            let _ = emitter.changed().await;
        }
        Ok(u32::try_from(removed.len()).unwrap_or(u32::MAX))
    }

    /// Invoke one of a notification's declared actions on the user's behalf. The shell is a
    /// renderer: it names the id and the key, and this server emits the freedesktop
    /// `ActionInvoked` the owning application is listening for, then closes the notification
    /// (reason 2, dismissed by the user) — the default non-resident behaviour.
    async fn invoke(
        &self,
        id: u32,
        action_key: String,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let _mutation = self.journal.0.mutation.lock().await;
        let event_id: Option<String> = self
            .store
            .transaction(|tx| {
                use rusqlite::OptionalExtension as _;
                let found: Option<(String, String)> = tx.query_row(
                    "SELECT event_id, actions_json FROM notifications_active WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                ).optional()?;
                let Some((event_id, actions_json)) = found else {
                    return Ok(None);
                };
                let actions: Vec<(String, String)> =
                    serde_json::from_str(&actions_json).unwrap_or_default();
                if !actions.iter().any(|(key, _)| *key == action_key) {
                    return Ok(None);
                }
                tx.execute("DELETE FROM notifications_active WHERE id = ?1", [id])?;
                Ok(Some(event_id))
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let Some(event_id) = event_id else {
            return Ok(false);
        };
        self.journal.commit(
            TransitionKind::Action,
            id,
            &event_id,
            Some(action_key.clone()),
            None,
        );
        self.journal
            .commit(TransitionKind::Closed, id, &event_id, None, None);
        if self.journal.fdo_hosting() {
            emit_fdo_action(emitter.connection(), id, &action_key).await;
        }
        let _ = emitter.changed().await;
        if self.journal.fdo_hosting() {
            emit_fdo_closed(emitter.connection(), id, REASON_DISMISSED).await;
        }
        let _ = emitter.changed().await;
        Ok(true)
    }

    /// One committed transition happened — creation, replacement, close, expiry, dismiss or
    /// action. Re-read with `TransitionsSince`, not with a highest-ID guess.
    #[zbus(signal)]
    async fn changed(signal_emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsApi {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<u32> {
        let _mutation = self.journal.0.mutation.lock().await;
        let urgency = hints
            .get("urgency")
            .and_then(|value| u8::try_from(value).ok())
            .unwrap_or(1)
            .min(2);
        let category = hints
            .get("category")
            .and_then(|value| <&str>::try_from(value).ok())
            .map(str::to_owned)
            .unwrap_or_default();
        let app_id = self.attribute(app_name);
        let action_pairs: Vec<(String, String)> = actions
            .chunks(2)
            .filter(|pair| pair.len() == 2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        let message = if body.is_empty() {
            format!("{app_name}: {summary}")
        } else {
            format!("{app_name}: {summary} — {body}")
        };

        let event = Event {
            id: EventId::new(),
            at_ms: templar_state::now_ms(),
            kind: EventKind::Notification,
            app_id: app_id.clone(),
            instance_id: None,
            message,
            cause: Some(format!("urgency:{urgency};category:{category}")),
            actions: action_pairs
                .iter()
                .cloned()
                .map(|(key, label)| EventAction {
                    label,
                    command: format!("notify:action:{key}"),
                })
                .collect(),
            reversible: false,
        };
        let event_id = event.id;
        self.history
            .append(event)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        let app_id_str = app_id.as_ref().map(AppId::as_str);
        let created_ms = templar_state::now_ms();
        // The specification uses 0 for never, a positive value in milliseconds, and -1 for the
        // server's default. Five seconds is deliberately conservative and deterministic.
        let expires_ms = match expire_timeout {
            0 => None,
            timeout if timeout > 0 => Some(created_ms.saturating_add(i64::from(timeout))),
            _ => Some(created_ms.saturating_add(5_000)),
        };
        let actions_json = serde_json::to_string(&action_pairs)
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let event_id_str = event_id.to_string();
        let id = self
            .store_active(&ActiveRow {
                replaces_id,
                event_id: &event_id_str,
                app_name,
                app_id: app_id_str,
                created_ms,
                summary,
                body,
                icon: app_icon,
                urgency,
                category: &category,
                actions_json: &actions_json,
                expires_ms,
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        // The arrival is committed: journal it with the full row, then publish the companion
        // change. (The freedesktop side has no creation signal; the returned ID is it.)
        // Stash the connection before arming expiry tasks.
        self.journal.stash_bus(emitter.connection().clone());
        let kind = if replaces_id == 0 {
            TransitionKind::Created
        } else {
            TransitionKind::Replaced
        };
        let notification = ActiveNotification {
            format_version: PRESENTATION_FORMAT_VERSION,
            id,
            event_id: event_id_str.clone(),
            // Stamped with the commit's sequence inside `commit`.
            sequence: 0,
            app_name: app_name.to_owned(),
            app_id: app_id.as_ref().map(|app_id| app_id.as_str().to_owned()),
            summary: summary.to_owned(),
            body: body.to_owned(),
            icon: app_icon.to_owned(),
            urgency,
            category: category.clone(),
            actions: action_pairs,
            created_ms,
            expires_ms,
        };
        self.journal
            .commit(kind, id, &event_id_str, None, Some(notification));
        if let Some(expires_ms) = expires_ms {
            self.spawn_expiry(id, event_id_str.clone(), expires_ms);
        }
        emit_changed(emitter.connection()).await;
        Ok(id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let _mutation = self.journal.0.mutation.lock().await;
        let event_id: Option<String> = self
            .store
            .transaction(|tx| {
                let event_id: Option<String> = tx
                    .query_row(
                        "SELECT event_id FROM notifications_active WHERE id = ?1",
                        params![id],
                        |row| row.get(0),
                    )
                    .ok();
                if event_id.is_some() {
                    tx.execute(
                        "DELETE FROM notifications_active WHERE id = ?1",
                        params![id],
                    )?;
                }
                Ok(event_id)
            })
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        // Only a live notification closes: a stray close for an unknown ID publishes
        // nothing, so a producer can never conjure a close event out of thin air.
        let Some(event_id) = event_id else {
            return Ok(());
        };
        self.journal
            .commit(TransitionKind::Closed, id, &event_id, None, None);
        // Reason 3: dismissed by CloseNotification, per the spec.
        let _ = emitter
            .notification_closed(id, REASON_CLOSED_BY_CALLER)
            .await;
        emit_changed(emitter.connection()).await;
        Ok(())
    }

    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into(), "persistence".into()]
    }

    /// `(name, vendor, version, spec_version)`
    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "templar-sessiond".into(),
            "TemplarOS".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[zbus(signal)]
    async fn notification_closed(
        signal_emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        signal_emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
#[path = "notifications_native_tests.rs"]
mod native_tests;
