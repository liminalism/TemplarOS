//! Notifications are searchable events with causes and actions, not transient interruptions.
//! `templar-sessiond` hosts `org.freedesktop.Notifications` (when it is the selected provider),
//! records every notification into history, and the shell only renders.
//!
//! The second half of this module is the terminal job-completion policy: when a finished
//! foreground command deserves a notification, and what that notification carries. The shell
//! hooks (`session/profile.d/61-templaros-completion.*`) measure elapsed time and exit status;
//! `templar notify terminal-completed` applies this policy and sends at most one notification.

use crate::history::{Event, EventAction, EventId, EventKind};
use serde::{Deserialize, Serialize};
use templar_types::AppId;

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

/// `Settings1` keys for the terminal-completion policy. The schema lives in
/// `crate::settings`; these constants keep the CLI and the schema spelling one key.
pub const LONG_JOB_SECONDS_KEY: &str = "shell.notifications.long-job-seconds";
pub const NOTIFY_EVERY_JOB_KEY: &str = "shell.notifications.notify-every-job";
pub const NOTIFY_CANCELED_KEY: &str = "shell.notifications.notify-canceled";
pub const SKIP_WHEN_FOCUSED_KEY: &str = "shell.notifications.skip-when-focused";

/// The `app_name` every terminal-completion notification is sent under: one stable,
/// brand-neutral producer, so the shell can style completions and the dedupe check can tell
/// our own notifications apart from the terminal emulator's native ones.
pub const COMPLETION_APP_NAME: &str = "Terminal";

/// freedesktop categories for the three completion outcomes. Distinct categories (rather than
/// urgency alone) so the shell can dwell success/failure/canceled differently.
pub const COMPLETION_CATEGORY_SUCCESS: &str = "terminal.completed";
pub const COMPLETION_CATEGORY_FAILURE: &str = "terminal.failed";
pub const COMPLETION_CATEGORY_CANCELED: &str = "terminal.canceled";

/// How a finished foreground job ended, from its wait status.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CompletionOutcome {
    Success,
    Failure,
    /// Interrupted from the keyboard (SIGINT, conventionally exit status 130).
    Canceled,
}

/// Classify a conventional Unix exit status: 0 succeeded, 130 was Ctrl-C, anything else
/// failed. A status produced as `128 + signal` (the `templar run` convention) classifies by
/// the same rule, so only signal 2 counts as canceled.
pub fn classify_exit(status: i32) -> CompletionOutcome {
    match status {
        0 => CompletionOutcome::Success,
        130 => CompletionOutcome::Canceled,
        _ => CompletionOutcome::Failure,
    }
}

/// Whether the terminal that ran the job holds keyboard focus.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum FocusState {
    Focused,
    Unfocused,
    /// The terminal could not say (no remote-control channel, unknown emulator). Policy
    /// treats unknown as worth notifying: a toast for a long job the person walked away
    /// from is the point, and a transient toast while focused is harmless.
    #[default]
    Unknown,
}

impl std::str::FromStr for FocusState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "focused" | "yes" => Ok(Self::Focused),
            "unfocused" | "no" => Ok(Self::Unfocused),
            "unknown" | "" => Ok(Self::Unknown),
            other => Err(format!(
                "unknown focus state {other:?}: expected focused, unfocused or unknown"
            )),
        }
    }
}

/// One finished job, as the completion route decides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionEvent {
    pub elapsed_ms: u64,
    pub outcome: CompletionOutcome,
    pub focus: FocusState,
    /// Safe label (a bare command name, never arguments); see [`safe_label`].
    pub label: String,
    /// The terminal emulator's `app_name` (empty when unknown), for native-dedupe matching.
    pub emulator: String,
    /// A terminal-native notification for this job is already active.
    pub native_already_notified: bool,
    /// One of our own completion notifications for this job is already active (the
    /// `templar run --notify` route sending while its own shell hook fires too).
    pub self_already_notified: bool,
}

/// The terminal-completion policy. Defaults are the proposed policy: long jobs only,
/// canceled suppressed, focused terminals skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionPolicy {
    /// Notify when a job ran at least this long. 0 with `notify_every_job` unset still
    /// notifies every job; prefer the explicit flag for that.
    pub long_job_ms: u64,
    pub notify_every_job: bool,
    pub notify_canceled: bool,
    pub skip_when_focused: bool,
}

impl Default for CompletionPolicy {
    fn default() -> Self {
        Self {
            long_job_ms: 10_000,
            notify_every_job: false,
            notify_canceled: false,
            skip_when_focused: true,
        }
    }
}

impl CompletionPolicy {
    /// Read the policy out of effective settings values, falling back to [`Self::default`]
    /// for every key that is missing or mistyped. A completion decision must never fail
    /// because one setting is unreadable.
    pub fn from_values(
        values: &std::collections::BTreeMap<String, crate::settings::SettingValue>,
    ) -> Self {
        use crate::settings::SettingValue;
        let defaults = Self::default();
        let int = |key: &str| match values.get(key) {
            Some(SettingValue::Int(seconds)) => u64::try_from(*seconds).unwrap_or(u64::MAX),
            _ => defaults.long_job_ms / 1_000,
        };
        let flag = |key: &str, fallback: bool| match values.get(key) {
            Some(SettingValue::Bool(value)) => *value,
            _ => fallback,
        };
        Self {
            long_job_ms: int(LONG_JOB_SECONDS_KEY).saturating_mul(1_000),
            notify_every_job: flag(NOTIFY_EVERY_JOB_KEY, defaults.notify_every_job),
            notify_canceled: flag(NOTIFY_CANCELED_KEY, defaults.notify_canceled),
            skip_when_focused: flag(SKIP_WHEN_FOCUSED_KEY, defaults.skip_when_focused),
        }
    }
}

/// Why [`CompletionPolicy::decide`] answered the way it did. The CLI prints the reason with
/// `--json` and stays silent otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletionDecision {
    pub notify: bool,
    pub reason: &'static str,
}

impl CompletionPolicy {
    pub fn decide(&self, event: &CompletionEvent) -> CompletionDecision {
        if event.native_already_notified {
            return CompletionDecision {
                notify: false,
                reason: "native-duplicate",
            };
        }
        if event.self_already_notified {
            return CompletionDecision {
                notify: false,
                reason: "already-notified",
            };
        }
        if event.outcome == CompletionOutcome::Canceled && !self.notify_canceled {
            return CompletionDecision {
                notify: false,
                reason: "canceled",
            };
        }
        if event.focus == FocusState::Focused && self.skip_when_focused {
            return CompletionDecision {
                notify: false,
                reason: "focused",
            };
        }
        if self.notify_every_job {
            return CompletionDecision {
                notify: true,
                reason: "every-job",
            };
        }
        if event.elapsed_ms >= self.long_job_ms {
            return CompletionDecision {
                notify: true,
                reason: "long-job",
            };
        }
        CompletionDecision {
            notify: false,
            reason: "short-job",
        }
    }
}

/// Reduce a raw command line to a safe label: the basename of its first word, with no
/// arguments, redirections or pipeline stages. Command lines carry secrets (`curl -H
/// "Authorization: ..."`, `mysql -psecret`); a notification must never persist them.
///
/// An empty or unreadable line becomes `"command"`.
pub fn safe_label(command_line: &str) -> String {
    let first = command_line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|char| matches!(char, '"' | '\''));
    let base = first.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        return "command".to_owned();
    }
    base.chars().take(64).collect()
}

/// A short elapsed duration for the notification body: `12s`, `3m 20s`, `1h 02m`.
pub fn format_elapsed(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms.div_ceil(1_000);
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m {:02}s", seconds % 60);
    }
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// The summary, body, category and urgency for a completion notification. Success and
/// failure are distinct in every field so the shell can dwell and style them differently.
pub fn completion_fields(event: &CompletionEvent) -> (String, String, &'static str, Urgency) {
    let elapsed = format_elapsed(event.elapsed_ms);
    match event.outcome {
        CompletionOutcome::Success => (
            format!("{} finished", event.label),
            format!("Completed in {elapsed}"),
            COMPLETION_CATEGORY_SUCCESS,
            Urgency::Low,
        ),
        CompletionOutcome::Failure => (
            format!("{} failed", event.label),
            format!("Failed after {elapsed}"),
            COMPLETION_CATEGORY_FAILURE,
            Urgency::Normal,
        ),
        CompletionOutcome::Canceled => (
            format!("{} canceled", event.label),
            format!("Canceled after {elapsed}"),
            COMPLETION_CATEGORY_CANCELED,
            Urgency::Low,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SettingValue;
    use std::collections::BTreeMap;

    fn event(elapsed_ms: u64, outcome: CompletionOutcome) -> CompletionEvent {
        CompletionEvent {
            elapsed_ms,
            outcome,
            focus: FocusState::Unknown,
            label: "make".to_owned(),
            emulator: String::new(),
            native_already_notified: false,
            self_already_notified: false,
        }
    }

    #[test]
    fn exit_status_zero_succeeds_130_cancels_and_the_rest_fails() {
        assert_eq!(classify_exit(0), CompletionOutcome::Success);
        assert_eq!(classify_exit(130), CompletionOutcome::Canceled);
        for status in [1, 2, 127, 129, 131, 137, 143, -1] {
            assert_eq!(
                classify_exit(status),
                CompletionOutcome::Failure,
                "status {status}"
            );
        }
    }

    #[test]
    fn the_default_policy_notifies_long_jobs_only() {
        let policy = CompletionPolicy::default();
        assert_eq!(policy.long_job_ms, 10_000);
        let notify = policy.decide(&event(10_000, CompletionOutcome::Success));
        assert_eq!(
            notify,
            CompletionDecision {
                notify: true,
                reason: "long-job"
            }
        );
        // A millisecond under the threshold stays silent, success or failure alike.
        for outcome in [CompletionOutcome::Success, CompletionOutcome::Failure] {
            let quiet = policy.decide(&event(9_999, outcome));
            assert_eq!(
                quiet,
                CompletionDecision {
                    notify: false,
                    reason: "short-job"
                }
            );
        }
    }

    #[test]
    fn every_job_mode_notifies_short_jobs_but_still_suppresses_canceled() {
        let policy = CompletionPolicy {
            notify_every_job: true,
            ..CompletionPolicy::default()
        };
        let notify = policy.decide(&event(12, CompletionOutcome::Failure));
        assert!(notify.notify);
        assert_eq!(notify.reason, "every-job");
        let canceled = policy.decide(&event(60_000, CompletionOutcome::Canceled));
        assert_eq!(
            canceled,
            CompletionDecision {
                notify: false,
                reason: "canceled"
            }
        );
        let policy = CompletionPolicy {
            notify_canceled: true,
            ..policy
        };
        assert!(
            policy
                .decide(&event(60_000, CompletionOutcome::Canceled))
                .notify
        );
    }

    #[test]
    fn a_focused_terminal_stays_silent_unless_focus_is_unknown_or_unchecked() {
        let policy = CompletionPolicy::default();
        let mut focused = event(60_000, CompletionOutcome::Success);
        focused.focus = FocusState::Focused;
        assert_eq!(policy.decide(&focused).reason, "focused");
        assert!(!policy.decide(&focused).notify);
        // Unknown focus notifies: failing open is the point of a completion notice.
        assert!(
            policy
                .decide(&event(60_000, CompletionOutcome::Success))
                .notify
        );
        let policy = CompletionPolicy {
            skip_when_focused: false,
            ..policy
        };
        assert!(policy.decide(&focused).notify);
    }

    #[test]
    fn duplicates_stay_silent_whichever_producer_sent_first() {
        let policy = CompletionPolicy::default();
        let mut native = event(60_000, CompletionOutcome::Success);
        native.native_already_notified = true;
        assert_eq!(policy.decide(&native).reason, "native-duplicate");
        let mut owned = event(60_000, CompletionOutcome::Success);
        owned.self_already_notified = true;
        assert_eq!(policy.decide(&owned).reason, "already-notified");
        // Dedupe wins over every-job mode too: one job, one notification.
        let policy = CompletionPolicy {
            notify_every_job: true,
            ..policy
        };
        assert!(!policy.decide(&native).notify);
        assert!(!policy.decide(&owned).notify);
    }

    #[test]
    fn labels_keep_the_command_name_and_drop_everything_secret_bearing() {
        assert_eq!(safe_label("make -j8 world"), "make");
        assert_eq!(
            safe_label("/usr/bin/curl -H @headers https://example.invalid"),
            "curl"
        );
        assert_eq!(safe_label("mysql -psecret < dump.sql"), "mysql");
        assert_eq!(safe_label("make 2>&1 | tee log"), "make");
        assert_eq!(safe_label("  ./run.sh --token abc"), "run.sh");
        assert_eq!(safe_label(""), "command");
        assert_eq!(safe_label("   "), "command");
        assert_eq!(safe_label("\"quoted\" --flag"), "quoted");
        assert!(safe_label(&"x".repeat(200)).len() <= 64);
    }

    #[test]
    fn elapsed_formats_as_seconds_minutes_and_hours() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(12_000), "12s");
        assert_eq!(format_elapsed(12_001), "13s");
        assert_eq!(format_elapsed(200_000), "3m 20s");
        assert_eq!(format_elapsed(3_600_000), "1h 00m");
        assert_eq!(format_elapsed(5_400_000), "1h 30m");
    }

    #[test]
    fn success_failure_and_canceled_carry_distinct_presentation() {
        let (summary, body, category, urgency) =
            completion_fields(&event(12_000, CompletionOutcome::Success));
        assert_eq!(summary, "make finished");
        assert_eq!(body, "Completed in 12s");
        assert_eq!(category, COMPLETION_CATEGORY_SUCCESS);
        assert_eq!(urgency, Urgency::Low);
        let (summary, body, category, urgency) =
            completion_fields(&event(185_000, CompletionOutcome::Failure));
        assert_eq!(summary, "make failed");
        assert_eq!(body, "Failed after 3m 05s");
        assert_eq!(category, COMPLETION_CATEGORY_FAILURE);
        assert_eq!(urgency, Urgency::Normal);
        let (summary, _, category, _) =
            completion_fields(&event(5_000, CompletionOutcome::Canceled));
        assert_eq!(summary, "make canceled");
        assert_eq!(category, COMPLETION_CATEGORY_CANCELED);
    }

    #[test]
    fn focus_states_parse_from_hook_spellings_and_reject_the_rest() {
        use std::str::FromStr as _;
        assert_eq!(
            FocusState::from_str("focused").unwrap(),
            FocusState::Focused
        );
        assert_eq!(FocusState::from_str("yes").unwrap(), FocusState::Focused);
        assert_eq!(FocusState::from_str("no").unwrap(), FocusState::Unfocused);
        assert_eq!(
            FocusState::from_str("unknown").unwrap(),
            FocusState::Unknown
        );
        assert_eq!(FocusState::from_str("").unwrap(), FocusState::Unknown);
        assert!(FocusState::from_str("maybe").is_err());
    }

    #[test]
    fn policy_reads_settings_and_falls_back_per_key() {
        let mut values = BTreeMap::new();
        values.insert(LONG_JOB_SECONDS_KEY.to_owned(), SettingValue::Int(30));
        values.insert(NOTIFY_EVERY_JOB_KEY.to_owned(), SettingValue::Bool(true));
        // notify-canceled is missing, skip-when-focused is mistyped: both fall back.
        values.insert(SKIP_WHEN_FOCUSED_KEY.to_owned(), SettingValue::Int(0));
        let policy = CompletionPolicy::from_values(&values);
        assert_eq!(policy.long_job_ms, 30_000);
        assert!(policy.notify_every_job);
        assert!(!policy.notify_canceled);
        assert!(policy.skip_when_focused);
        let policy = CompletionPolicy::from_values(&BTreeMap::new());
        assert_eq!(policy, CompletionPolicy::default());
    }
}

/// The freedesktop category every launch-failure notification carries. It contains
/// "fail" on purpose: the shell dwells failure categories longer, and a notice about
/// an application that died is exactly what that dwell is for.
pub const LAUNCH_FAILURE_CATEGORY: &str = "app.failed";

/// How long after its main process started a death still counts as "failed to start"
/// rather than "quit unexpectedly": thirty seconds. A startup crash (a missing keyring
/// permission, an unimportable QML module) lands in the first seconds; anything later
/// ran long enough that "failed to start" would read as a lie.
pub const STARTUP_WINDOW_US: u64 = 30_000_000;

/// How the launcher's watchdog read one dead unit: systemd's `Result`, the waitid
/// `ExecMainCode`/`ExecMainStatus`, and the main process's start/exit timestamps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchFailure {
    /// `Result`: `exit-code`, `signal`, `core-dump`, `oom-kill`, `timeout`, …
    pub result: String,
    /// `ExecMainCode`: the waitid code — 1 exited, 2 killed, 3 dumped.
    pub exec_code: i32,
    /// `ExecMainStatus`: the exit status, or the signal number.
    pub exec_status: i32,
    /// `ExecMainStartTimestamp` / `ExecMainExitTimestamp`, microseconds since the epoch;
    /// 0 when the process never ran or systemd never recorded it.
    pub exec_start_us: u64,
    pub exec_exit_us: u64,
}

/// Whether this death happened inside the startup window. A process that never ran
/// (both timestamps 0 — the executable would not even spawn) is the purest startup
/// failure there is.
pub fn failed_at_startup(failure: &LaunchFailure) -> bool {
    if failure.exec_start_us == 0 {
        return true;
    }
    failure.exec_exit_us.saturating_sub(failure.exec_start_us) < STARTUP_WINDOW_US
}

/// One plain-language phrase for how the process ended, preferring the waitid reading
/// (which says what actually happened to the process) over the unit result (which says
/// how systemd classifies the unit). Unknown codes fall back to the result string
/// rather than guessing: "failed (resources)" is honest, "exited with status 0" for a
/// unit that never ran would not be.
pub fn describe_failure(failure: &LaunchFailure) -> String {
    match failure.exec_code {
        // CLD_EXITED.
        1 => format!("exited with status {}", failure.exec_status),
        // CLD_KILLED.
        2 => format!("killed by signal {}", failure.exec_status),
        // CLD_DUMPED.
        3 => format!("crashed (signal {})", failure.exec_status),
        _ => match failure.result.as_str() {
            "oom-kill" | "oom_kill" => "killed: out of memory".to_owned(),
            "timeout" => "timed out while starting".to_owned(),
            "resources" => "could not be started (resources)".to_owned(),
            "success" => "stopped".to_owned(),
            other => format!("failed ({other})"),
        },
    }
}

/// The summary, body, category and urgency for a launch-failure notification.
///
/// `detail` is the application's own last journal line(s), already selected and
/// bounded by the watcher — empty when the journal had nothing to say, in which case
/// the body is the failure phrase alone. The whole body is capped so one verbose
/// traceback tail cannot flood the popup.
pub fn launch_failure_fields(
    display_name: &str,
    failure: &LaunchFailure,
    detail: &str,
) -> (String, String, &'static str, Urgency) {
    const BODY_LIMIT: usize = 240;
    let summary = if failed_at_startup(failure) {
        format!("{display_name} failed to start")
    } else {
        format!("{display_name} quit unexpectedly")
    };
    let phrase = describe_failure(failure);
    let mut body = if detail.is_empty() {
        phrase
    } else {
        format!("{phrase} — {detail}")
    };
    if body.len() > BODY_LIMIT {
        let mut end = BODY_LIMIT;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    (summary, body, LAUNCH_FAILURE_CATEGORY, Urgency::Normal)
}

#[cfg(test)]
mod launch_failure_tests {
    use super::*;

    fn failure(result: &str, code: i32, status: i32, start_us: u64, exit_us: u64) -> LaunchFailure {
        LaunchFailure {
            result: result.to_owned(),
            exec_code: code,
            exec_status: status,
            exec_start_us: start_us,
            exec_exit_us: exit_us,
        }
    }

    #[test]
    fn a_fast_death_is_a_startup_failure_and_a_late_one_is_not() {
        let fast = failure("exit-code", 1, 1, 1_000_000, 4_000_000);
        assert!(failed_at_startup(&fast));
        let slow = failure("exit-code", 1, 1, 1_000_000, 31_000_001);
        assert!(!failed_at_startup(&slow));
        // Never ran at all: the purest startup failure.
        assert!(failed_at_startup(&failure("resources", 0, 0, 0, 0)));
    }

    #[test]
    fn the_waitid_reading_beats_the_unit_result_and_unknowns_fall_back() {
        assert_eq!(
            describe_failure(&failure("exit-code", 1, 1, 0, 0)),
            "exited with status 1"
        );
        assert_eq!(
            describe_failure(&failure("signal", 2, 9, 0, 0)),
            "killed by signal 9"
        );
        assert_eq!(
            describe_failure(&failure("core-dump", 3, 11, 0, 0)),
            "crashed (signal 11)"
        );
        assert_eq!(
            describe_failure(&failure("oom-kill", 0, 0, 0, 0)),
            "killed: out of memory"
        );
        assert_eq!(
            describe_failure(&failure("timeout", 0, 0, 0, 0)),
            "timed out while starting"
        );
        assert_eq!(
            describe_failure(&failure("start-limit-hit", 0, 0, 0, 0)),
            "failed (start-limit-hit)"
        );
    }

    #[test]
    fn fields_name_the_app_state_the_outcome_and_carry_the_failure_category() {
        let failure = failure("exit-code", 1, 1, 1_000_000, 2_000_000);
        let (summary, body, category, urgency) =
            launch_failure_fields("Proton VPN", &failure, "Sender is not authorized");
        assert_eq!(summary, "Proton VPN failed to start");
        assert_eq!(body, "exited with status 1 — Sender is not authorized");
        assert_eq!(category, LAUNCH_FAILURE_CATEGORY);
        assert!(category.contains("fail"));
        assert_eq!(urgency, Urgency::Normal);
    }

    #[test]
    fn a_late_death_is_phrased_as_quitting_and_an_empty_detail_leaves_the_phrase() {
        let failure = failure("exit-code", 1, 1, 1_000_000, 61_000_000);
        let (summary, body, _, _) = launch_failure_fields("Haruna", &failure, "");
        assert_eq!(summary, "Haruna quit unexpectedly");
        assert_eq!(body, "exited with status 1");
    }

    #[test]
    fn a_verbose_detail_is_capped_at_a_character_boundary() {
        let failure = failure("exit-code", 1, 1, 1_000_000, 2_000_000);
        let detail = format!("é{}", "x".repeat(500));
        let (_, body, _, _) = launch_failure_fields("App", &failure, &detail);
        assert!(body.len() <= 240);
        assert!(body.starts_with("exited with status 1 — éx"));
    }
}
