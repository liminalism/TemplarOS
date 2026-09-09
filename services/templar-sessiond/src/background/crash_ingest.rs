//! Ingest `systemd-coredump` journal entries into the crash history (ADR-0011).
//!
//! Shells out to `journalctl -o json -t systemd-coredump`, resuming from a persisted journal
//! cursor so a restart never re-ingests or misses an entry. PID/cgroup → instance/AppId comes
//! from the unit name the journal already records (`COREDUMP_UNIT`), which is self-describing
//! (`app-templaros-<AppId>@<InstanceId>.service`, ADR-0002) — no live registry lookup needed, so a
//! crash whose instance record is already gone is still attributable.

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use std::time::Duration;
use templar_session_core::history::{Event, EventAction, EventId, EventKind, append_within};
use templar_state::Store;
use templar_types::unit;
use tokio::process::Command;

#[derive(Debug, Default, Deserialize)]
struct CoredumpEntry {
    #[serde(rename = "__CURSOR")]
    cursor: Option<String>,
    #[serde(rename = "__REALTIME_TIMESTAMP")]
    realtime_timestamp: Option<String>,
    #[serde(rename = "COREDUMP_UNIT")]
    unit: Option<String>,
    #[serde(rename = "COREDUMP_EXE")]
    exe: Option<String>,
}

fn parse_line(line: &str) -> Option<CoredumpEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Turn one journal entry into a crash [`Event`], or `None` if it is not one of ours (its
/// `COREDUMP_UNIT` does not follow the `app-templaros-*` naming scheme — a crash of some other
/// system service, not a TemplarOS-launched application).
fn crash_event(entry: &CoredumpEntry) -> Option<Event> {
    let unit_name = entry.unit.as_deref()?;
    let (app_id, instance_id) = unit::parse_unit_name(unit_name)?;
    let at_ms = entry
        .realtime_timestamp
        .as_deref()
        .and_then(|value| value.parse::<i64>().ok())
        .map_or_else(templar_state::now_ms, |microseconds| microseconds / 1000);
    Some(Event {
        id: EventId::new(),
        at_ms,
        kind: EventKind::Crash,
        app_id: Some(app_id.clone()),
        instance_id: Some(instance_id),
        message: format!("{app_id} crashed. Restore?"),
        cause: entry.exe.clone(),
        actions: vec![EventAction {
            label: "Restore".into(),
            command: format!("crash:restore:{instance_id}"),
        }],
        reversible: false,
    })
}

fn load_cursor(store: &Store) -> Result<Option<String>> {
    store
        .lock()
        .query_row(
            "SELECT cursor FROM coredump_cursor WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .context("reading coredump_cursor")
}

fn save_cursor_within(tx: &rusqlite::Transaction<'_>, cursor: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO coredump_cursor (id, cursor, updated_ms) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET cursor = excluded.cursor, updated_ms = excluded.updated_ms",
        params![cursor, templar_state::now_ms()],
    )
    .context("persisting coredump_cursor")?;
    Ok(())
}

/// One ingestion pass: fetch entries newer than the persisted cursor, turn the ones that are
/// ours into history events, and advance the cursor. Returns how many events were appended.
pub async fn ingest_once(
    store: &Store,
    notifier: &crate::apis::history::HistoryChangeNotifier,
) -> Result<usize> {
    let cursor = load_cursor(store)?;
    let mut command = Command::new("journalctl");
    command.args([
        "-o",
        "json",
        "-t",
        "systemd-coredump",
        "--no-pager",
        "--quiet",
    ]);
    if let Some(cursor) = &cursor {
        command.arg(format!("--after-cursor={cursor}"));
    } else {
        // First run: do not replay the whole journal's history of coredumps.
        command.args(["--since", "-1 hour"]);
    }

    let output = command.output().await.context("running journalctl")?;
    if !output.status.success() {
        bail!("journalctl exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut ingested = 0;
    let mut latest_cursor = cursor;
    for line in text.lines() {
        let Some(entry) = parse_line(line) else {
            continue;
        };
        if let Some(cursor) = &entry.cursor {
            latest_cursor = Some(cursor.clone());
        }
        let Some(event) = crash_event(&entry) else {
            continue;
        };
        let cursor_to_save = latest_cursor.clone();
        store.transaction(|tx| {
            append_within(tx, &event)?;
            if let Some(cursor) = &cursor_to_save {
                save_cursor_within(tx, cursor)?;
            }
            Ok(())
        })?;
        notifier.notify().await;
        ingested += 1;
    }

    // Advance the cursor even when nothing crash-worthy was found, so a burst of unrelated
    // coredump-tagged journal noise is never replayed on every poll.
    if ingested == 0
        && let Some(cursor) = latest_cursor
    {
        store.transaction(|tx| save_cursor_within(tx, &cursor))?;
    }
    Ok(ingested)
}

/// Poll forever, spaced `interval` apart. Intended to be spawned once at startup.
pub async fn run(
    store: Store,
    notifier: crate::apis::history::HistoryChangeNotifier,
    interval: Duration,
) {
    loop {
        match ingest_once(&store, &notifier).await {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "ingested crash events from systemd-coredump"),
            Err(error) => tracing::warn!(%error, "failed to poll the systemd-coredump journal"),
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"__CURSOR":"s=abc;i=1","__REALTIME_TIMESTAMP":"1700000000000000","COREDUMP_UNIT":"app-templaros-org.templaros.Viewer@0123456789abcdef0123456789abcdef.service","COREDUMP_EXE":"/usr/bin/templar-viewer","MESSAGE":"Process 1234 dumped core"}"#;

    #[test]
    fn parses_a_templaros_unit_into_a_restorable_crash_event() {
        let entry = parse_line(SAMPLE).unwrap();
        let event = crash_event(&entry).unwrap();
        assert_eq!(event.kind, EventKind::Crash);
        assert_eq!(event.app_id.unwrap().as_str(), "org.templaros.Viewer");
        assert_eq!(event.at_ms, 1_700_000_000_000);
        assert_eq!(event.cause.as_deref(), Some("/usr/bin/templar-viewer"));
        assert_eq!(event.actions.len(), 1);
        assert!(event.actions[0].command.starts_with("crash:restore:"));
    }

    #[test]
    fn a_non_templaros_unit_yields_no_event() {
        let entry = parse_line(
            r#"{"COREDUMP_UNIT":"some-other-daemon.service","__REALTIME_TIMESTAMP":"1700000000000000"}"#,
        )
        .unwrap();
        assert!(crash_event(&entry).is_none());
    }

    #[test]
    fn blank_and_malformed_lines_are_skipped_not_fatal() {
        assert!(parse_line("").is_none());
        assert!(parse_line("not json at all").is_none());
    }

    #[test]
    fn cursor_persists_across_loads() {
        let store = Store::in_memory().unwrap();
        assert_eq!(load_cursor(&store).unwrap(), None);
        store
            .transaction(|tx| save_cursor_within(tx, "s=abc;i=1"))
            .unwrap();
        assert_eq!(load_cursor(&store).unwrap(), Some("s=abc;i=1".to_owned()));
        store
            .transaction(|tx| save_cursor_within(tx, "s=abc;i=2"))
            .unwrap();
        assert_eq!(load_cursor(&store).unwrap(), Some("s=abc;i=2".to_owned()));
    }
}
