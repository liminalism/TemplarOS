//! The notification lifecycle over an owned private bus: every committed transition
//! publishes exactly once on each side that owns it, replacements never let a stale timer
//! expire the newer row, and a restart replays nothing while losing nothing new.
use super::*;
use anyhow::Context as _;
use futures_util::StreamExt as _;
use std::time::Duration;
use templar_state::Store;

const BUS_NAME: &str = "org.test.TemplarSession";
const CALL: Duration = Duration::from_secs(5);
/// Long enough that a missing signal is a finding, short enough that a passing suite stays fast.
const QUIET: Duration = Duration::from_millis(300);

struct PrivateBus {
    child: tokio::process::Child,
    address: String,
}

impl PrivateBus {
    async fn run() -> anyhow::Result<Self> {
        let mut child = tokio::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut output = tokio::io::BufReader::new(child.stdout.take().context("bus stdout")?);
        let mut address = String::new();
        tokio::time::timeout(
            CALL,
            tokio::io::AsyncBufReadExt::read_line(&mut output, &mut address),
        )
        .await??;
        anyhow::ensure!(
            address.starts_with("unix:"),
            "fixture bus has no Unix address"
        );
        Ok(Self {
            child,
            address: address.trim().to_owned(),
        })
    }

    async fn connect(&self) -> anyhow::Result<zbus::Connection> {
        Ok(zbus::connection::Builder::address(self.address.as_str())?
            .build()
            .await?)
    }

    async fn stop(mut self) -> anyhow::Result<()> {
        self.child.kill().await?;
        self.child.wait().await?;
        Ok(())
    }
}

struct Fixture {
    bus: PrivateBus,
    journal: NotificationJournal,
    server: zbus::Connection,
    client: zbus::Connection,
    _store: Store,
}

impl Fixture {
    async fn run() -> anyhow::Result<Self> {
        let store = Store::in_memory()?;
        Self::run_on(store).await
    }

    async fn run_on(store: Store) -> anyhow::Result<Self> {
        let bus = PrivateBus::run().await?;
        let server = bus.connect().await?;
        server.request_name(BUS_NAME).await?;
        let journal = NotificationJournal::new();
        // The tests stand in for the provider task's claim; see the gating test below.
        journal.set_fdo_hosting(true);
        let history = Arc::new(SqliteHistory::new(store.clone()));
        let registry = Arc::new(Registry::default());
        server
            .object_server()
            .at(
                FDO_PATH,
                NotificationsApi::new(store.clone(), Arc::clone(&history), Arc::clone(&registry))
                    .with_journal(journal.clone()),
            )
            .await?;
        server
            .object_server()
            .at(
                names::NOTIFICATIONS_PATH,
                NotificationsControlApi::new(store.clone()).with_journal(journal.clone()),
            )
            .await?;
        let client = bus.connect().await?;
        Ok(Self {
            bus,
            journal,
            server,
            client,
            _store: store,
        })
    }

    async fn stop(self) -> anyhow::Result<()> {
        drop(self.client);
        drop(self.server);
        self.bus.stop().await
    }

    async fn fdo(&self) -> zbus::Proxy<'_> {
        zbus::Proxy::new(&self.client, BUS_NAME, FDO_PATH, FDO_IFACE)
            .await
            .expect("fixture freedesktop proxy builds")
    }

    async fn companion(&self) -> zbus::Proxy<'_> {
        zbus::Proxy::new(
            &self.client,
            BUS_NAME,
            names::NOTIFICATIONS_PATH,
            names::NOTIFICATIONS_IFACE,
        )
        .await
        .expect("fixture companion proxy builds")
    }

    async fn changed_stream(&self) -> zbus::proxy::SignalStream<'_> {
        self.companion()
            .await
            .receive_signal("Changed")
            .await
            .expect("Changed subscription installs")
    }

    async fn closed_stream(&self) -> zbus::proxy::SignalStream<'_> {
        self.fdo()
            .await
            .receive_signal("NotificationClosed")
            .await
            .expect("NotificationClosed subscription installs")
    }

    async fn invoked_stream(&self) -> zbus::proxy::SignalStream<'_> {
        self.fdo()
            .await
            .receive_signal("ActionInvoked")
            .await
            .expect("ActionInvoked subscription installs")
    }

    async fn notify(
        &self,
        app: &str,
        replaces: u32,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<String, OwnedValue>,
        timeout: i32,
    ) -> anyhow::Result<u32> {
        let actions: Vec<String> = actions.iter().map(ToString::to_string).collect();
        let id: u32 = tokio::time::timeout(
            CALL,
            self.fdo().await.call(
                "Notify",
                &(app, replaces, "", summary, body, actions, hints, timeout),
            ),
        )
        .await??;
        Ok(id)
    }

    async fn call_fdo<T>(
        &self,
        member: &str,
        args: &(impl serde::Serialize + zbus::zvariant::Type),
    ) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        Ok(tokio::time::timeout(CALL, self.fdo().await.call(member, args)).await??)
    }

    async fn call_companion<T>(
        &self,
        member: &str,
        args: &(impl serde::Serialize + zbus::zvariant::Type),
    ) -> anyhow::Result<T>
    where
        T: serde::de::DeserializeOwned + zbus::zvariant::Type,
    {
        Ok(tokio::time::timeout(CALL, self.companion().await.call(member, args)).await??)
    }

    async fn list_active(&self) -> anyhow::Result<Vec<ActiveNotification>> {
        let rows: Vec<String> = self.call_companion("ListActive", &()).await?;
        rows.into_iter()
            .map(|row| serde_json::from_str(&row).map_err(anyhow::Error::from))
            .collect()
    }

    async fn snapshot(&self) -> anyhow::Result<SnapshotDoc> {
        let document: String = self.call_companion("Snapshot", &()).await?;
        serde_json::from_str(&document).map_err(anyhow::Error::from)
    }

    async fn transitions_since(
        &self,
        sequence: u64,
        owner_epoch: &str,
    ) -> anyhow::Result<TransitionsDoc> {
        let document: String = self
            .call_companion("TransitionsSince", &(sequence, owner_epoch))
            .await?;
        serde_json::from_str(&document).map_err(anyhow::Error::from)
    }
}

fn owned_str(value: &str) -> OwnedValue {
    zbus::zvariant::Value::from(value)
        .try_to_owned()
        .expect("string hint serializes")
}

/// Receive one signal body, failing when nothing arrives within the call budget.
async fn next_body<T>(stream: &mut zbus::proxy::SignalStream<'_>) -> anyhow::Result<T>
where
    T: serde::de::DeserializeOwned + zbus::zvariant::Type,
{
    let message = tokio::time::timeout(CALL, stream.next())
        .await?
        .context("signal stream ended")?;
    Ok(message.body().deserialize::<T>()?)
}

/// Assert the stream stays silent: exactly-once means the second receive times out.
async fn assert_quiet(stream: &mut zbus::proxy::SignalStream<'_>) {
    assert!(
        tokio::time::timeout(QUIET, stream.next()).await.is_err(),
        "a second signal arrived where exactly one was promised"
    );
}

#[tokio::test]
async fn arrival_publishes_one_companion_change_with_full_presentation() {
    let fixture = Fixture::run().await.unwrap();
    let mut changed = fixture.changed_stream().await;
    // Subscribe-before-snapshot: the subscription above predates this empty baseline.
    let before = fixture.snapshot().await.unwrap();
    assert!(before.notifications.is_empty());
    assert_eq!(before.format_version, PRESENTATION_FORMAT_VERSION);

    let mut hints = HashMap::new();
    hints.insert("category".to_owned(), owned_str("terminal.failed"));
    hints.insert("urgency".to_owned(), OwnedValue::from(1u8));
    let id = fixture
        .notify(
            "Terminal",
            0,
            "make failed",
            "Failed after 12s",
            &[],
            hints,
            0,
        )
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    assert_quiet(&mut changed).await;

    // The same arrival is visible as state, as a snapshot row and as a journal transition,
    // all carrying the full presentation metadata rather than the summary alone.
    let rows = fixture.list_active().await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.app_name, "Terminal");
    assert_eq!(row.summary, "make failed");
    assert_eq!(row.body, "Failed after 12s");
    assert_eq!(row.category, "terminal.failed");
    assert_eq!(row.urgency, 1);
    assert!(!row.event_id.is_empty());
    assert_eq!(row.format_version, PRESENTATION_FORMAT_VERSION);
    assert_eq!(row.sequence, before.sequence + 1);

    let after = fixture.snapshot().await.unwrap();
    assert_eq!(after.owner_epoch, before.owner_epoch);
    assert_eq!(after.sequence, before.sequence + 1);
    assert_eq!(after.notifications, rows);

    let catchup = fixture
        .transitions_since(before.sequence, &before.owner_epoch)
        .await
        .unwrap();
    assert!(!catchup.must_resync);
    assert_eq!(catchup.transitions.len(), 1);
    let transition = &catchup.transitions[0];
    assert_eq!(transition.kind, TransitionKind::Created);
    assert_eq!(transition.id, id);
    assert_eq!(transition.event_id, row.event_id);
    assert_eq!(
        transition.notification.as_ref().unwrap().summary,
        "make failed"
    );
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn replacement_keeps_the_id_with_a_new_generation_and_guards_expiry() {
    let fixture = Fixture::run().await.unwrap();
    let mut changed = fixture.changed_stream().await;
    let mut closed = fixture.closed_stream().await;

    let first = fixture
        .notify("Terminal", 0, "first", "", &[], HashMap::new(), 250)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    let before = fixture.list_active().await.unwrap();
    assert!(!before[0].event_id.is_empty());

    // Replace well before the first row's 250 ms timer fires, with no expiry of its own.
    let second = fixture
        .notify("Terminal", first, "second", "", &[], HashMap::new(), 0)
        .await
        .unwrap();
    assert_eq!(second, first);
    next_body::<()>(&mut changed).await.unwrap();

    let rows = fixture.list_active().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, first);
    assert_eq!(rows[0].summary, "second");
    assert_ne!(rows[0].event_id, before[0].event_id);

    // The stale timer fires into the guard: the newer row survives and nothing publishes.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_quiet(&mut closed).await;
    assert_quiet(&mut changed).await;
    let rows = fixture.list_active().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary, "second");
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn sender_close_expiry_dismiss_and_action_close_exactly_once_on_both_paths() {
    let fixture = Fixture::run().await.unwrap();
    let mut changed = fixture.changed_stream().await;
    let mut closed = fixture.closed_stream().await;
    let mut invoked = fixture.invoked_stream().await;

    // Sender close: reason 3 on the freedesktop path, one companion change.
    let id = fixture
        .notify("App", 0, "close me", "", &[], HashMap::new(), 0)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    fixture
        .call_fdo::<()>("CloseNotification", &(id,))
        .await
        .unwrap();
    assert_eq!(next_body::<(u32, u32)>(&mut closed).await.unwrap(), (id, 3));
    next_body::<()>(&mut changed).await.unwrap();
    assert!(fixture.list_active().await.unwrap().is_empty());

    // Expiry: reason 1, from the timer rather than any call.
    let id = fixture
        .notify("App", 0, "expire me", "", &[], HashMap::new(), 150)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    assert_eq!(next_body::<(u32, u32)>(&mut closed).await.unwrap(), (id, 1));
    next_body::<()>(&mut changed).await.unwrap();
    assert!(fixture.list_active().await.unwrap().is_empty());

    // Dismiss: reason 2, committed through the companion API.
    let id = fixture
        .notify("App", 0, "dismiss me", "", &[], HashMap::new(), 0)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    let removed: bool = fixture.call_companion("Dismiss", &(id,)).await.unwrap();
    assert!(removed);
    assert_eq!(next_body::<(u32, u32)>(&mut closed).await.unwrap(), (id, 2));
    next_body::<()>(&mut changed).await.unwrap();
    assert!(fixture.list_active().await.unwrap().is_empty());

    // Action and close are two committed transitions, each with one companion change.
    let id = fixture
        .notify("App", 0, "act", "", &["open", "Open"], HashMap::new(), 0)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    let invoked_ok: bool = fixture
        .call_companion("Invoke", &(id, "open"))
        .await
        .unwrap();
    assert!(invoked_ok);
    assert_eq!(
        next_body::<(u32, String)>(&mut invoked).await.unwrap(),
        (id, "open".to_owned())
    );
    assert_eq!(next_body::<(u32, u32)>(&mut closed).await.unwrap(), (id, 2));
    next_body::<()>(&mut changed).await.unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    assert!(fixture.list_active().await.unwrap().is_empty());

    assert_quiet(&mut changed).await;
    assert_quiet(&mut closed).await;
    assert_quiet(&mut invoked).await;
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn unknown_ids_and_keys_publish_nothing() {
    let fixture = Fixture::run().await.unwrap();
    let mut changed = fixture.changed_stream().await;
    let mut closed = fixture.closed_stream().await;
    let mut invoked = fixture.invoked_stream().await;

    // A stray sender close for an unknown ID is swallowed, not published.
    fixture
        .call_fdo::<()>("CloseNotification", &(4242u32,))
        .await
        .unwrap();
    let removed: bool = fixture
        .call_companion("Dismiss", &(4242u32,))
        .await
        .unwrap();
    assert!(!removed);
    let acted: bool = fixture
        .call_companion("Invoke", &(4242u32, "open"))
        .await
        .unwrap();
    assert!(!acted);

    // A live row with an undeclared action key likewise stays silent.
    let id = fixture
        .notify("App", 0, "live", "", &["open", "Open"], HashMap::new(), 0)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    let acted: bool = fixture
        .call_companion("Invoke", &(id, "nope"))
        .await
        .unwrap();
    assert!(!acted);
    assert_eq!(fixture.list_active().await.unwrap().len(), 1);

    assert_quiet(&mut changed).await;
    assert_quiet(&mut closed).await;
    assert_quiet(&mut invoked).await;
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn standard_signals_stay_home_while_another_server_hosts_the_name() {
    let fixture = Fixture::run().await.unwrap();
    // Another notification server takes the name: the companion API keeps serving, but its
    // transitions must not emit freedesktop closes for IDs it no longer owns.
    fixture.journal.set_fdo_hosting(false);
    let mut changed = fixture.changed_stream().await;
    let mut closed = fixture.closed_stream().await;

    let id = fixture
        .notify("App", 0, "dismiss me", "", &[], HashMap::new(), 0)
        .await
        .unwrap();
    next_body::<()>(&mut changed).await.unwrap();
    let removed: bool = fixture.call_companion("Dismiss", &(id,)).await.unwrap();
    assert!(removed);
    next_body::<()>(&mut changed).await.unwrap();
    assert_quiet(&mut closed).await;
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn a_burst_is_observable_transition_by_transition_without_loss() {
    let fixture = Fixture::run().await.unwrap();
    let mut changed = fixture.changed_stream().await;
    let before = fixture.snapshot().await.unwrap();

    for index in 0..5 {
        fixture
            .notify(
                "Burst",
                0,
                &format!("member {index}"),
                "",
                &[],
                HashMap::new(),
                0,
            )
            .await
            .unwrap();
    }
    for _ in 0..5 {
        next_body::<()>(&mut changed).await.unwrap();
    }
    let catchup = fixture
        .transitions_since(before.sequence, &before.owner_epoch)
        .await
        .unwrap();
    assert!(!catchup.must_resync);
    assert_eq!(catchup.transitions.len(), 5);
    for (index, transition) in catchup.transitions.iter().enumerate() {
        assert_eq!(transition.kind, TransitionKind::Created);
        assert_eq!(transition.sequence, before.sequence + 1 + index as u64);
        assert_eq!(
            transition.notification.as_ref().unwrap().summary,
            format!("member {index}")
        );
    }
    // A cursor at the head sees nothing new, and a foreign epoch resynchronizes.
    let quiet = fixture
        .transitions_since(catchup.sequence, &catchup.owner_epoch)
        .await
        .unwrap();
    assert!(!quiet.must_resync);
    assert!(quiet.transitions.is_empty());
    let stale = fixture
        .transitions_since(0, "not-this-owner")
        .await
        .unwrap();
    assert!(stale.must_resync);
    assert!(stale.transitions.is_empty());
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn restart_replays_nothing_stale_and_loses_no_new_arrival() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite3");
    let epoch_before;
    {
        let store = Store::open(&path).unwrap();
        let fixture = Fixture::run_on(store).await.unwrap();
        epoch_before = fixture.journal.owner_epoch().to_owned();
        // One persistent row (survives) and one short-lived row (stale by restart).
        fixture
            .notify("App", 0, "persistent", "", &[], HashMap::new(), 0)
            .await
            .unwrap();
        fixture
            .notify("App", 0, "short", "", &[], HashMap::new(), 100)
            .await
            .unwrap();
        // Let the short row's timer fire while the owner is still alive, so the restart
        // below carries exactly one live row and no pending timer.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fixture.list_active().await.unwrap().len(), 1);
        fixture.stop().await.unwrap();
    }
    {
        let store = Store::open(&path).unwrap();
        let fixture = Fixture::run_on(store).await.unwrap();
        assert_ne!(fixture.journal.owner_epoch(), epoch_before);
        let mut changed = fixture.changed_stream().await;
        // The pre-restart row is state, not an arrival: sequence 0 under the new owner.
        let snapshot = fixture.snapshot().await.unwrap();
        assert_eq!(snapshot.notifications.len(), 1);
        assert_eq!(snapshot.notifications[0].summary, "persistent");
        assert_eq!(snapshot.notifications[0].sequence, 0);
        // Nothing publishes for it, and the old epoch resynchronizes rather than replays.
        assert_quiet(&mut changed).await;
        let stale = fixture.transitions_since(0, &epoch_before).await.unwrap();
        assert!(stale.must_resync);
        // A new arrival after the restart publishes exactly as before.
        let id = fixture
            .notify("App", 0, "fresh", "", &[], HashMap::new(), 0)
            .await
            .unwrap();
        next_body::<()>(&mut changed).await.unwrap();
        let catchup = fixture
            .transitions_since(snapshot.sequence, &snapshot.owner_epoch)
            .await
            .unwrap();
        assert!(!catchup.must_resync);
        assert_eq!(catchup.transitions.len(), 1);
        assert_eq!(catchup.transitions[0].id, id);
        fixture.stop().await.unwrap();
    }
}

#[test]
fn an_evicted_cursor_resynchronizes_through_a_fresh_snapshot() {
    let journal = NotificationJournal::new();
    let epoch = journal.owner_epoch().to_owned();
    for index in 0..(JOURNAL_RING_CAP + 4) {
        journal.commit(
            TransitionKind::Created,
            index as u32,
            &format!("event-{index}"),
            None,
            None,
        );
    }
    // The first entries fell out of the bounded ring: their cursor resynchronizes.
    let evicted = journal.transitions_since(2, &epoch);
    assert!(evicted.must_resync);
    assert!(evicted.transitions.is_empty());
    // A cursor inside the ring still catches up precisely.
    let live = journal.transitions_since(JOURNAL_RING_CAP as u64, &epoch);
    assert!(!live.must_resync);
    assert_eq!(live.transitions.len(), 4);
    assert_eq!(live.transitions[0].sequence, JOURNAL_RING_CAP as u64 + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_snapshots_never_pair_old_rows_with_a_new_cursor() {
    let fixture = Fixture::run().await.unwrap();
    let producer = async {
        for index in 0..100 {
            fixture.notify("App", 0, &format!("arrival-{index}"), "", &[], HashMap::new(), 0)
                .await.unwrap();
        }
    };
    let reader = async {
        for _ in 0..100 {
            let snapshot = fixture.snapshot().await.unwrap();
            assert_eq!(snapshot.notifications.len() as u64, snapshot.sequence);
            assert!(snapshot.notifications.iter().all(|row|
                row.sequence > 0 && row.sequence <= snapshot.sequence));
        }
    };
    tokio::join!(producer, reader);
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn clear_all_publishes_one_change_per_committed_dismissal() {
    let fixture = Fixture::run().await.unwrap();
    for _ in 0..3 {
        fixture.notify("App", 0, "clear", "", &[], HashMap::new(), 0).await.unwrap();
    }
    let mut changed = fixture.changed_stream().await;
    let before = fixture.snapshot().await.unwrap();
    assert_eq!(fixture.call_companion::<u32>("ClearAll", &()).await.unwrap(), 3);
    for _ in 0..3 { next_body::<()>(&mut changed).await.unwrap(); }
    assert_quiet(&mut changed).await;
    let batch = fixture.transitions_since(before.sequence, &before.owner_epoch).await.unwrap();
    assert_eq!(batch.transitions.len(), 3);
    assert!(batch.transitions.iter().all(|entry| entry.kind == TransitionKind::Dismissed));
    fixture.stop().await.unwrap();
}

#[tokio::test]
async fn persisted_future_expiry_is_rearmed_without_a_new_notify() {
    let store = Store::in_memory().unwrap();
    let expires = templar_state::now_ms() + 600;
    store.transaction(|tx| {
        tx.execute(
            "INSERT INTO notifications_active
             (event_id, app_name, created_ms, summary, body, icon, urgency, category, actions_json, expires_ms)
             VALUES ('persisted', 'App', ?1, 'before restart', '', '', 1, '', '[]', ?2)",
            params![templar_state::now_ms(), expires],
        )?;
        Ok(())
    }).unwrap();
    let fixture = Fixture::run_on(store).await.unwrap();
    let mut changed = fixture.changed_stream().await;
    let before = fixture.snapshot().await.unwrap();
    assert_eq!(before.notifications.len(), 1);
    assert_eq!(before.notifications[0].sequence, 0);
    next_body::<()>(&mut changed).await.unwrap();
    assert!(fixture.snapshot().await.unwrap().notifications.is_empty());
    let batch = fixture.transitions_since(before.sequence, &before.owner_epoch).await.unwrap();
    assert_eq!(batch.transitions.len(), 1);
    assert_eq!(batch.transitions[0].kind, TransitionKind::Expired);
    fixture.stop().await.unwrap();
}

#[test]
fn journal_generation_index_releases_retired_and_replaced_rows() {
    let journal = NotificationJournal::new();
    for sequence in 0..200 {
        let event = sequence.to_string();
        journal.commit(TransitionKind::Created, 7, &event, None, None);
        assert_eq!(journal.0.state.lock().last_change.len(), 1);
        journal.commit(TransitionKind::Closed, 7, &event, None, None);
        assert!(journal.0.state.lock().last_change.is_empty());
    }
}

#[tokio::test]
async fn snapshot_waits_for_the_whole_inflight_mutation() {
    let fixture = Fixture::run().await.unwrap();
    let guard = fixture.journal.0.mutation.lock().await;
    assert!(tokio::time::timeout(Duration::from_millis(30), fixture.snapshot()).await.is_err(),
        "Snapshot must not pair partially committed database rows with a journal cursor");
    drop(guard);
    assert_eq!(fixture.snapshot().await.unwrap().sequence, 0);
    fixture.stop().await.unwrap();
}
