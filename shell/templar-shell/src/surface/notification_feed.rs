//! Event-driven notification read model. Cursor and rows advance together, and each bus
//! owner gets its own subscription. No timer, ID watermark or independent ListActive read.

use std::collections::HashMap;

use anyhow::{Context as _, Result, ensure};
use futures_util::StreamExt as _;
use serde::Deserialize;

use super::notification_popup::ToastPresentation;

const SERVICE: &str = "org.templaros.Session";

#[derive(Debug)]
pub struct NotificationUpdate {
    pub active: Vec<ToastPresentation>,
    /// Reset presentation and silently adopt rows through this sequence. Initial connection
    /// adopts everything; a new owner adopts only persisted rows (sequence zero).
    pub quiet_through: Option<u64>,
}

impl NotificationUpdate {
    fn invalidate() -> Self {
        Self {
            active: Vec::new(),
            quiet_through: Some(u64::MAX),
        }
    }
}

#[derive(Deserialize)]
struct Snapshot {
    format_version: u32,
    owner_epoch: String,
    sequence: u64,
    notifications: Vec<ToastPresentation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Created,
    Replaced,
    Closed,
    Expired,
    Dismissed,
    Action,
}

#[derive(Deserialize)]
struct Transition {
    sequence: u64,
    kind: Kind,
    id: u32,
    event_id: String,
    notification: Option<ToastPresentation>,
}

#[derive(Deserialize)]
struct Transitions {
    format_version: u32,
    owner_epoch: String,
    sequence: u64,
    must_resync: bool,
    transitions: Vec<Transition>,
}

#[derive(Default)]
struct ReadModel {
    epoch: String,
    sequence: u64,
    active: HashMap<u32, ToastPresentation>,
}

impl ReadModel {
    fn snapshot(&mut self, raw: &str, quiet_through: u64) -> Result<NotificationUpdate> {
        let snapshot: Snapshot = serde_json::from_str(raw)?;
        ensure!(
            snapshot.format_version == 1,
            "unsupported notification snapshot version"
        );
        ensure!(
            !snapshot.owner_epoch.is_empty(),
            "notification owner epoch is empty"
        );
        let mut active = HashMap::with_capacity(snapshot.notifications.len());
        for row in snapshot.notifications {
            ensure!(
                !row.event_id.is_empty() && row.sequence <= snapshot.sequence,
                "notification snapshot generation is inconsistent"
            );
            ensure!(
                active.insert(row.id, row).is_none(),
                "duplicate notification ID"
            );
        }
        self.epoch = snapshot.owner_epoch;
        self.sequence = snapshot.sequence;
        self.active = active;
        Ok(self.update(Some(quiet_through)))
    }

    /// `None` is a journal gap. Validate the entire batch before touching either cursor or
    /// model, so a malformed/incomplete reply cannot silently lose the rest of a burst.
    fn transitions(&mut self, raw: &str) -> Result<Option<NotificationUpdate>> {
        let batch: Transitions = serde_json::from_str(raw)?;
        ensure!(
            batch.format_version == 1,
            "unsupported notification transition version"
        );
        if batch.must_resync || batch.owner_epoch != self.epoch {
            return Ok(None);
        }
        let mut expected = self.sequence;
        for transition in &batch.transitions {
            expected = expected
                .checked_add(1)
                .context("notification cursor overflow")?;
            ensure!(
                transition.sequence == expected,
                "notification transition gap"
            );
            if matches!(transition.kind, Kind::Created | Kind::Replaced) {
                let row = transition
                    .notification
                    .as_ref()
                    .context("arrival without presentation")?;
                ensure!(
                    row.id == transition.id
                        && row.event_id == transition.event_id
                        && row.sequence == transition.sequence
                        && !row.event_id.is_empty(),
                    "notification transition generation mismatch"
                );
            }
        }
        ensure!(
            batch.sequence == expected,
            "notification cursor does not cover its batch"
        );
        for transition in batch.transitions {
            match transition.kind {
                Kind::Created | Kind::Replaced => {
                    let row = transition.notification.expect("validated arrival");
                    self.active.insert(transition.id, row);
                }
                Kind::Closed | Kind::Expired | Kind::Dismissed => {
                    if self
                        .active
                        .get(&transition.id)
                        .is_some_and(|row| row.event_id == transition.event_id)
                    {
                        self.active.remove(&transition.id);
                    }
                }
                Kind::Action => {}
            }
        }
        self.sequence = batch.sequence;
        Ok(Some(self.update(None)))
    }

    fn update(&self, quiet_through: Option<u64>) -> NotificationUpdate {
        let mut active: Vec<_> = self.active.values().cloned().collect();
        active
            .sort_unstable_by_key(|row| std::cmp::Reverse((row.sequence, row.created_ms, row.id)));
        NotificationUpdate {
            active,
            quiet_through,
        }
    }
}

async fn observe_owner(
    connection: &zbus::Connection,
    owner: &str,
    initial: bool,
    publish: &impl Fn(NotificationUpdate) -> bool,
) -> Result<()> {
    // Pin methods and signals to the same unique owner. A restart cannot mix one owner's
    // snapshot with another's cursor while the NameOwnerChanged task catches up.
    let proxy = templar_client::proxies::NotificationsProxy::builder(connection)
        .destination(owner)?
        .build()
        .await?;
    let mut changed = proxy.receive_changed().await?;
    let mut model = ReadModel::default();
    if !publish(model.snapshot(&proxy.snapshot().await?, if initial { u64::MAX } else { 0 })?) {
        return Ok(());
    }
    while changed.next().await.is_some() {
        let raw = proxy
            .transitions_since(model.sequence, &model.epoch)
            .await?;
        let update = match model.transitions(&raw)? {
            Some(update) => update,
            None => {
                let cursor = model.sequence;
                model.snapshot(&proxy.snapshot().await?, cursor)?
            }
        };
        if !publish(update) {
            return Ok(());
        }
    }
    anyhow::bail!("notification signal stream ended")
}

/// Subscribe to ownership before resolving it, then subscribe to Changed before Snapshot.
/// Ownership loss cancels in-flight reads and immediately invalidates all old presentation.
/// The new owner's nonzero generations are arrivals, not persisted restart history.
pub async fn observe(
    connection: &zbus::Connection,
    publish: impl Fn(NotificationUpdate) -> bool,
) -> Result<()> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let mut owners = bus
        .receive_name_owner_changed_with_args(&[(0, SERVICE)])
        .await?;
    let mut owner = bus
        .get_name_owner(SERVICE.try_into()?)
        .await
        .ok()
        .map(|name| name.to_string());
    let mut initial = true;
    loop {
        let next_owner = {
            let observe = async {
                if let Some(owner) = &owner {
                    observe_owner(connection, owner, initial, &publish).await
                } else {
                    std::future::pending::<Result<()>>().await
                }
            };
            tokio::pin!(observe);
            let mut ended = false;
            loop {
                tokio::select! {
                    biased;
                    event = owners.next() => {
                        event.context("notification owner stream ended")?.args()?;
                        // Resolve the current owner rather than queued obsolete edges.
                        let next = bus.get_name_owner(SERVICE.try_into()?).await.ok()
                            .map(|name| name.to_string());
                        if next != owner { break next; }
                    }
                    result = &mut observe, if !ended => {
                        match result {
                            Ok(()) => return Ok(()),
                            Err(error) => {
                                tracing::warn!(%error, "notification owner observation ended");
                                if !publish(NotificationUpdate::invalidate()) { return Ok(()); }
                                // No polling after failure; a real ownership edge re-arms us.
                                ended = true;
                            }
                        }
                    }
                }
            }
        };
        if !publish(NotificationUpdate::invalidate()) {
            return Ok(());
        }
        owner = next_owner;
        initial = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn row(id: u32, generation: &str, sequence: u64) -> Value {
        json!({"id": id, "event_id": generation, "sequence": sequence,
            "app_name": "Terminal", "app_id": null, "summary": "Done", "body": "",
            "icon": "", "urgency": 1, "category": "", "actions": [],
            "created_ms": 1000, "expires_ms": null})
    }

    fn snapshot(epoch: &str, sequence: u64, rows: Vec<Value>) -> String {
        json!({"format_version": 1, "owner_epoch": epoch, "sequence": sequence,
            "notifications": rows})
        .to_string()
    }

    fn batch(sequence: u64, transitions: Vec<Value>) -> String {
        json!({"format_version": 1, "owner_epoch": "owner", "sequence": sequence,
            "must_resync": false, "transitions": transitions})
        .to_string()
    }

    fn arrival(id: u32, generation: &str, sequence: u64) -> Value {
        json!({"sequence": sequence, "kind": "created", "id": id, "event_id": generation,
            "notification": row(id, generation, sequence)})
    }

    #[test]
    fn burst_replacement_and_close_advance_one_consistent_model() {
        let mut model = ReadModel::default();
        model
            .snapshot(&snapshot("owner", 0, vec![]), u64::MAX)
            .unwrap();
        let update = model
            .transitions(&batch(
                4,
                vec![
                    arrival(1, "a", 1),
                    arrival(2, "b", 2),
                    arrival(1, "c", 3),
                    json!({"sequence":4, "kind":"closed", "id":2, "event_id":"b"}),
                ],
            ))
            .unwrap()
            .unwrap();
        assert_eq!(model.sequence, 4);
        assert_eq!(update.active.len(), 1);
        assert_eq!(update.active[0].event_id, "c");
        assert_eq!(update.quiet_through, None);
    }

    #[test]
    fn incomplete_batch_never_advances_cursor_or_model() {
        let mut model = ReadModel::default();
        model
            .snapshot(&snapshot("owner", 0, vec![]), u64::MAX)
            .unwrap();
        assert!(
            model
                .transitions(&batch(3, vec![arrival(1, "a", 1), arrival(2, "b", 3)]))
                .is_err()
        );
        assert_eq!(model.sequence, 0);
        assert!(model.active.is_empty());
    }

    #[test]
    fn owner_restart_separates_persisted_rows_from_new_arrivals() {
        let mut model = ReadModel::default();
        model
            .snapshot(&snapshot("old", 50, vec![row(9, "old", 50)]), u64::MAX)
            .unwrap();
        let update = model
            .snapshot(
                &snapshot("new", 1, vec![row(9, "old", 0), row(10, "new", 1)]),
                0,
            )
            .unwrap();
        assert_eq!(model.epoch, "new");
        assert_eq!(model.sequence, 1);
        assert_eq!(update.quiet_through, Some(0));
        assert_eq!(
            update.active.iter().filter(|row| row.sequence > 0).count(),
            1
        );
    }
}
