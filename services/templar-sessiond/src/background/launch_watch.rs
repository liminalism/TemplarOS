//! Watch launched applications for dying, and say so out loud.
//!
//! `Launcher1.Launch` returns once systemd has *accepted* the unit; an application that
//! exits a second later — a snap denied its keyring, a QML module missing — looks exactly
//! like a successful launch to everyone upstream. Nothing depends on the dying application
//! to report itself (it cannot: it is dead), so this task sweeps every non-running
//! `app-templaros-*` unit, and each newly failed one becomes a notification through
//! `org.freedesktop.Notifications` — the same route every other producer uses, so the
//! failure lands in history, in the shell's feed, and on screen.
//!
//! What reports, and what does not:
//!
//! * A unit whose `Result` is `success` is a normal exit, however fast. Single-instance
//!   applications that forward to a running copy and exit 0 stay silent, as they should.
//! * A failure the watcher did not witness — already failed on the first sweep, or quiet
//!   for longer than [`STALE_AFTER_US`] — is stale news and is recorded as seen without
//!   a notification. The journal keeps it; a toast hours later would be noise.
//! * Every other newly failed unit notifies exactly once. Failed units linger loaded,
//!   so the seen set (pruned against each sweep) is what keeps one death to one notice.

use std::{collections::HashSet, sync::Arc, time::Duration};
use templar_appd::{Registry, systemd1::AppUnitState};
use templar_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    notifications::{LaunchFailure, launch_failure_fields},
};
use templar_types::AppCatalog as _;
use tokio::process::Command;

use crate::apis::history::HistoryChangeNotifier;

/// A failure older than this when first seen is stale news, not something to toast.
/// Sweeps run every few seconds, so anything witnessed live is far younger; this only
/// bites for failures that happened while the watcher was down.
const STALE_AFTER_US: u64 = 120_000_000;

/// How many of the application's own last journal lines the notification body quotes.
/// Two lines carry a traceback tail (`...password_lookup_sync` + the `GError`); more
/// would flood the popup.
const DETAIL_LINES: usize = 2;

/// The journal detail, bounded before the notification fields cap the whole body.
const DETAIL_LIMIT: usize = 160;

/// What one sweep learned that the watcher had not already dealt with.
#[derive(Debug, Default)]
pub struct WatchState {
    seen_failed: HashSet<String>,
    first_sweep: bool,
}

impl WatchState {
    fn new() -> Self {
        Self {
            seen_failed: HashSet::new(),
            first_sweep: true,
        }
    }
}

/// Which of this sweep's dead units deserve a notification right now. Pure: snapshots
/// in, the newly failed out, `state` updated so the next sweep knows what it already
/// dealt with. Units systemd has garbage-collected since fall out of the seen set, so
/// it cannot grow without bound.
pub fn decide<'a>(
    states: &'a [AppUnitState],
    state: &mut WatchState,
    now_us: u64,
) -> Vec<&'a AppUnitState> {
    let live: HashSet<&str> = states.iter().map(|unit| unit.unit_name.as_str()).collect();
    state
        .seen_failed
        .retain(|unit| live.contains(unit.as_str()));

    let mut notify = Vec::new();
    for unit in states {
        if unit.result == "success" || state.seen_failed.contains(&unit.unit_name) {
            continue;
        }
        state.seen_failed.insert(unit.unit_name.clone());
        if state.first_sweep {
            continue; // Backlog from before the watcher started: stale by definition.
        }
        if unit.inactive_enter_us != 0
            && now_us.saturating_sub(unit.inactive_enter_us) > STALE_AFTER_US
        {
            continue; // Died while the watcher was down and stayed dead: stale news.
        }
        notify.push(unit);
    }
    state.first_sweep = false;
    notify
}

/// The most significant thing the application said before it died: the last
/// [`DETAIL_LINES`] non-empty journal messages that came from the application itself,
/// not from systemd narrating the unit around it (`Started…`, `Main process exited…`,
/// `Failed with result…` carry no information the notification does not already have).
/// Empty when the journal had nothing of the application's own to quote.
pub fn select_detail(journal_json: &str) -> String {
    let mut messages = Vec::new();
    for line in journal_json.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry["SYSLOG_IDENTIFIER"].as_str() == Some("systemd") {
            continue;
        }
        if let Some(message) = entry["MESSAGE"].as_str() {
            let message = message.trim();
            if !message.is_empty() {
                messages.push(message.to_owned());
            }
        }
    }
    let mut detail = messages
        .into_iter()
        .rev()
        .take(DETAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" / ");
    if detail.len() > DETAIL_LIMIT {
        let mut end = DETAIL_LIMIT;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail.truncate(end);
    }
    detail
}

/// Sweep once: snapshot every non-running application unit, notify for the newly
/// failed, and record what was dealt with. A sweep that cannot reach systemd fails
/// the sweep, never the watcher — the next one retries.
async fn sweep_once(
    connection: &zbus::Connection,
    registry: &Registry,
    history: &SqliteHistory,
    notifier: &HistoryChangeNotifier,
    state: &mut WatchState,
) -> anyhow::Result<()> {
    let states = templar_appd::systemd1::list_app_unit_states(connection).await?;
    let now_us = unix_us();
    for unit in decide(&states, state, now_us) {
        report_failure(connection, registry, history, notifier, unit).await;
    }
    Ok(())
}

fn unix_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_micros().try_into().unwrap_or(u64::MAX)
        })
}

async fn report_failure(
    connection: &zbus::Connection,
    registry: &Registry,
    history: &SqliteHistory,
    notifier: &HistoryChangeNotifier,
    unit: &AppUnitState,
) {
    let manifest = registry.manifest(&unit.app_id);
    let display_name = manifest.as_ref().map_or_else(
        || unit.app_id.to_string(),
        |manifest| manifest.display_name.clone(),
    );
    let icon = manifest
        .as_ref()
        .and_then(|manifest| manifest.icon.clone())
        .unwrap_or_else(|| "dialog-error".to_owned());
    let failure = LaunchFailure {
        result: unit.result.clone(),
        exec_code: unit.exec_code,
        exec_status: unit.exec_status,
        exec_start_us: unit.exec_start_us,
        exec_exit_us: unit.exec_exit_us,
    };
    let detail = journal_detail(&unit.unit_name, unit.exec_main_pid).await;
    let (summary, body, category, urgency) =
        launch_failure_fields(&display_name, &failure, &detail);

    match send_notification(
        connection,
        &display_name,
        &icon,
        &summary,
        &body,
        category,
        urgency as u8,
    )
    .await
    {
        Ok(_) => {
            tracing::info!(unit = %unit.unit_name, %summary, "reported an application failure");
        }
        Err(error) => {
            // The notification server is unreachable (mid provider switch, or nobody
            // hosts it): the failure is still significant, so record it as history
            // directly rather than losing it with the toast.
            tracing::warn!(%error, unit = %unit.unit_name, "could not send the failure notification; recording history instead");
            let event = Event {
                id: EventId::new(),
                at_ms: templar_state::now_ms(),
                kind: EventKind::Launch,
                app_id: Some(unit.app_id.clone()),
                instance_id: Some(unit.instance_id),
                message: format!("{summary} — {body}"),
                cause: Some(unit.result.clone()),
                actions: Vec::new(),
                reversible: false,
            };
            if let Err(error) = history.append(event) {
                tracing::warn!(%error, unit = %unit.unit_name, "could not record the failure either");
            }
            notifier.notify().await;
        }
    }
}

/// The journal query for one dead unit's own output. By main PID, not by unit: a
/// snap re-scopes the process out of the transient unit after `ExecStart`, so `-u`
/// finds only systemd narrating the unit while the application's stderr carries the
/// snap scope — `_PID` finds it wherever it went. PID 0 (nothing ever ran) falls back
/// to the unit, whose narration usually filters out to an empty detail.
fn journal_query_args(unit: &str, pid: u32) -> Vec<String> {
    let selector = if pid == 0 {
        vec!["-u".to_owned(), unit.to_owned()]
    } else {
        vec![format!("_PID={pid}")]
    };
    ["--user", "-n", "30", "-o", "json", "--no-pager"]
        .into_iter()
        .map(str::to_owned)
        .chain(selector)
        .collect()
}

/// The application's own last journal lines for one unit, as [`select_detail`] reads
/// them. Whatever goes wrong — no journal, no lines, a timeout — degrades to an empty
/// detail, never to failing the report: the outcome is known from systemd either way.
async fn journal_detail(unit: &str, pid: u32) -> String {
    let mut command = Command::new("journalctl");
    command
        .args(journal_query_args(unit, pid))
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(output) = tokio::time::timeout(Duration::from_secs(5), command.output()).await else {
        return String::new();
    };
    let Ok(output) = output else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let recent: Vec<&str> = text.lines().take(30).collect();
    select_detail(&recent.join("\n"))
}

/// Deliver one notification on `org.freedesktop.Notifications`, whoever hosts it — the
/// same call the terminal-completion route makes (`cli/templar/src/notify.rs`), so a
/// third-party server selected over sessiond still shows the failure.
async fn send_notification(
    connection: &zbus::Connection,
    app_name: &str,
    icon: &str,
    summary: &str,
    body: &str,
    category: &str,
    urgency: u8,
) -> anyhow::Result<u32> {
    use zbus::zvariant::{OwnedValue, Value};
    let mut hints = std::collections::HashMap::new();
    hints.insert("category".to_owned(), Value::from(category).try_to_owned()?);
    hints.insert("urgency".to_owned(), OwnedValue::from(urgency));
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?;
    let id: u32 = proxy
        .call(
            "Notify",
            &(
                app_name,
                0u32,
                icon,
                summary,
                body,
                Vec::<String>::new(),
                hints,
                -1i32,
            ),
        )
        .await?;
    Ok(id)
}

/// Sweep for newly dead application units every `interval`, forever. Polling rather
/// than systemd signals, like the neighbouring watchers: one D-Bus call per quiet
/// sweep, no subscription to lose, and trivially reasoned about.
pub async fn run(
    connection: zbus::Connection,
    registry: Arc<Registry>,
    history: Arc<SqliteHistory>,
    notifier: HistoryChangeNotifier,
    interval: Duration,
) {
    let mut state = WatchState::new();
    loop {
        if let Err(error) =
            sweep_once(&connection, &registry, &history, &notifier, &mut state).await
        {
            tracing::warn!(%error, "failed to sweep application units");
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;
    use templar_appd::systemd1::AppUnitState;
    use templar_types::{AppId, InstanceId};

    fn unit(name: &str, result: &str, inactive_enter_us: u64) -> AppUnitState {
        AppUnitState {
            app_id: AppId::from_str("desktop-import.app.desktop").expect("static id is valid"),
            instance_id: InstanceId::new(),
            unit_name: name.to_owned(),
            active_state: "failed".to_owned(),
            result: result.to_owned(),
            exec_code: 1,
            exec_status: 1,
            exec_main_pid: 1234,
            exec_start_us: 1_000_000,
            exec_exit_us: 2_000_000,
            inactive_enter_us,
        }
    }

    const NOW: u64 = 10_000_000_000;

    #[test]
    fn the_first_sweep_records_the_backlog_silently_and_later_deaths_notify_once() {
        let mut state = WatchState::new();
        let states = vec![unit("app-a", "exit-code", NOW - 1_000_000)];
        assert!(decide(&states, &mut state, NOW).is_empty());

        let states = vec![
            unit("app-a", "exit-code", NOW - 1_000_000),
            unit("app-b", "exit-code", NOW - 1_000_000),
        ];
        let notify = decide(&states, &mut state, NOW);
        assert_eq!(
            notify
                .iter()
                .map(|unit| unit.unit_name.as_str())
                .collect::<Vec<_>>(),
            ["app-b"]
        );
        // And the same sweep twice never double-notifies.
        assert!(decide(&states, &mut state, NOW).is_empty());
    }

    #[test]
    fn clean_exits_never_notify_and_stale_failures_stay_quiet() {
        let mut state = WatchState::new();
        let backlog = vec![unit("seed", "exit-code", NOW - 1_000_000)];
        decide(&backlog, &mut state, NOW);

        let states = vec![
            unit("clean", "success", NOW - 1_000_000),
            unit("stale", "exit-code", NOW - STALE_AFTER_US - 1),
        ];
        assert!(decide(&states, &mut state, NOW).is_empty());
        // A unit with no timestamp cannot prove itself stale, so it notifies once.
        let states = [unit("dateless", "exit-code", 0)];
        let notify = decide(&states, &mut state, NOW);
        assert_eq!(notify.len(), 1);
    }

    #[test]
    fn collected_units_leave_the_seen_set_so_it_cannot_grow_forever() {
        let mut state = WatchState::new();
        let states = vec![unit("gone", "exit-code", NOW - 1_000_000)];
        decide(&states, &mut state, NOW);
        assert!(state.seen_failed.contains("gone"));
        decide(&[], &mut state, NOW);
        assert!(state.seen_failed.is_empty());
    }

    #[test]
    fn the_detail_is_the_apps_own_last_lines_not_systemds_narration() {
        let journal = [
            r#"{"SYSLOG_IDENTIFIER":"proton-vpn","MESSAGE":"  traceback line  " }"#,
            r#"{"SYSLOG_IDENTIFIER":"proton-vpn","MESSAGE":"GError: not authorized"}"#,
            r#"{"SYSLOG_IDENTIFIER":"systemd","MESSAGE":"app-x.service: Failed with result 'exit-code'."}"#,
            "not json at all",
            r#"{"SYSLOG_IDENTIFIER":"proton-vpn","MESSAGE":"   "}"#,
        ]
        .join("\n");
        assert_eq!(
            select_detail(&journal),
            "traceback line / GError: not authorized"
        );
    }

    #[test]
    fn an_empty_or_systemd_only_journal_yields_no_detail() {
        assert_eq!(select_detail(""), "");
        assert_eq!(
            select_detail("{\"SYSLOG_IDENTIFIER\":\"systemd\",\"MESSAGE\":\"Started x.\"}"),
            ""
        );
    }

    #[test]
    fn the_journal_query_follows_the_pid_and_falls_back_to_the_unit() {
        assert_eq!(
            journal_query_args("app-x.service", 107838),
            [
                "--user",
                "-n",
                "30",
                "-o",
                "json",
                "--no-pager",
                "_PID=107838"
            ]
        );
        assert_eq!(
            journal_query_args("app-x.service", 0),
            [
                "--user",
                "-n",
                "30",
                "-o",
                "json",
                "--no-pager",
                "-u",
                "app-x.service"
            ]
        );
    }

    #[test]
    fn the_detail_is_capped_at_a_character_boundary() {
        let journal = format!(
            "{{\"SYSLOG_IDENTIFIER\":\"app\",\"MESSAGE\":\"é{}\"}}",
            "x".repeat(500)
        );
        let detail = select_detail(&journal);
        assert!(detail.len() <= DETAIL_LIMIT);
        assert!(detail.starts_with("éx"));
    }
}
