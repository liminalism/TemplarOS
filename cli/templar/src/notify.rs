//! `templar notify` — the terminal job-completion producer.
//!
//! The shell hooks (`session/profile.d/61-templaros-completion.*`) measure how long a
//! foreground command ran and how it ended; this route applies the completion policy
//! (`templar_session_core::notifications`) and sends at most one notification per job. The
//! same route serves `templar run --notify`, which reuses its owned child's exit instead of
//! a hook measurement.
//!
//! Everything cross-process sits behind [`CompletionDeps`] so the decision — policy,
//! focus, dedupe — is unit-testable without a bus. Bus failures degrade to silence with an
//! error: a hook that printed a paragraph on every prompt would be worse than no notice.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::{collections::BTreeMap, time::Duration};
use templar_session_core::{
    notifications::{
        COMPLETION_APP_NAME, CompletionEvent, CompletionPolicy, FocusState, classify_exit,
        completion_fields,
    },
    settings::SettingValue,
};

/// The freedesktop notification destination, served by sessiond while it holds the
/// provider selection.
const FDO_BUS_NAME: &str = "org.freedesktop.Notifications";
const FDO_PATH: &str = "/org/freedesktop/Notifications";
const FDO_IFACE: &str = "org.freedesktop.Notifications";

/// How recently a terminal-native notification must have arrived to count as this job's
/// duplicate. Terminals notify at completion; anything older belongs to another job.
const NATIVE_DEDUPE_WINDOW_MS: i64 = 20_000;
/// The same window for our own completion route, covering `templar run --notify` sending
/// while its own shell hook fires a moment later.
const SELF_DEDUPE_WINDOW_MS: i64 = 10_000;

/// One finished job, as the hooks report it. The label is already safe (a bare command
/// name); raw command lines never cross into this route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionRequest {
    pub elapsed_ms: u64,
    pub status: i32,
    pub label: String,
    /// The terminal emulator's `app_name` for native-dedupe matching; empty when unknown.
    pub emulator: String,
}

/// How the route should determine terminal focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusRequest {
    Focused,
    Unfocused,
    Unknown,
    #[default]
    Auto,
}

impl std::str::FromStr for FocusRequest {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "focused" => Ok(Self::Focused),
            "unfocused" => Ok(Self::Unfocused),
            "unknown" => Ok(Self::Unknown),
            "auto" => Ok(Self::Auto),
            other => Err(format!(
                "unknown focus {other:?}: expected focused, unfocused, unknown or auto"
            )),
        }
    }
}

/// What the route decided and, when it notified, the ID the server assigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionReport {
    pub notify: bool,
    pub reason: &'static str,
    pub id: Option<u32>,
}

/// Everything the decision needs from outside the process.
#[async_trait]
pub trait CompletionDeps {
    async fn setting(&self, key: &str) -> Option<SettingValue>;
    /// Active notifications as tolerant JSON rows (`ListActive` documents).
    async fn active(&self) -> Vec<serde_json::Value>;
    async fn send(&self, summary: &str, body: &str, category: &str, urgency: u8) -> Result<u32>;
    fn now_ms(&self) -> i64;
}

/// Decide and, when the policy says so, send one completion notification.
pub async fn terminal_completed(
    deps: &impl CompletionDeps,
    request: &CompletionRequest,
    focus: FocusState,
) -> Result<CompletionReport> {
    let mut values = BTreeMap::new();
    for key in [
        templar_session_core::notifications::LONG_JOB_SECONDS_KEY,
        templar_session_core::notifications::NOTIFY_EVERY_JOB_KEY,
        templar_session_core::notifications::NOTIFY_CANCELED_KEY,
        templar_session_core::notifications::SKIP_WHEN_FOCUSED_KEY,
    ] {
        if let Some(value) = deps.setting(key).await {
            values.insert(key.to_owned(), value);
        }
    }
    let policy = CompletionPolicy::from_values(&values);
    decide_and_send(deps, &policy, request, focus).await
}

/// Decide under an explicit policy: `templar run --notify` forces every-job semantics and
/// skips the focus check (the flag is the opt-in), while keeping canceled suppression and
/// dedupe.
pub async fn terminal_completed_with_policy(
    deps: &impl CompletionDeps,
    policy: &CompletionPolicy,
    request: &CompletionRequest,
    focus: FocusState,
) -> Result<CompletionReport> {
    decide_and_send(deps, policy, request, focus).await
}

async fn decide_and_send(
    deps: &impl CompletionDeps,
    policy: &CompletionPolicy,
    request: &CompletionRequest,
    focus: FocusState,
) -> Result<CompletionReport> {
    let outcome = classify_exit(request.status);
    let rows = deps.active().await;
    let now_ms = deps.now_ms();
    let event = CompletionEvent {
        elapsed_ms: request.elapsed_ms,
        outcome,
        focus,
        label: request.label.clone(),
        emulator: request.emulator.clone(),
        native_already_notified: native_duplicate(&rows, &request.emulator, now_ms),
        self_already_notified: self_duplicate(&rows, &request.label, now_ms),
    };
    let decision = policy.decide(&event);
    if !decision.notify {
        return Ok(CompletionReport {
            notify: false,
            reason: decision.reason,
            id: None,
        });
    }
    let (summary, body, category, urgency) = completion_fields(&event);
    let id = deps.send(&summary, &body, category, urgency as u8).await?;
    Ok(CompletionReport {
        notify: true,
        reason: decision.reason,
        id: Some(id),
    })
}

/// Whether a terminal-native notification for this job is already active: a recent row from
/// the emulator itself. Our own rows never match — they carry our producer name.
pub fn native_duplicate(rows: &[serde_json::Value], emulator: &str, now_ms: i64) -> bool {
    if emulator.is_empty() || emulator == COMPLETION_APP_NAME {
        return false;
    }
    rows.iter().any(|row| {
        row["app_name"].as_str() == Some(emulator)
            && row["app_name"].as_str() != Some(COMPLETION_APP_NAME)
            && row["created_ms"]
                .as_i64()
                .is_some_and(|created| now_ms.saturating_sub(created) < NATIVE_DEDUPE_WINDOW_MS)
    })
}

/// Whether our own completion route already notified for this label within the self window:
/// the `templar run --notify` backstop when the hook fires for the same job anyway.
pub fn self_duplicate(rows: &[serde_json::Value], label: &str, now_ms: i64) -> bool {
    let prefix = format!("{label} ");
    rows.iter().any(|row| {
        row["app_name"].as_str() == Some(COMPLETION_APP_NAME)
            && row["summary"]
                .as_str()
                .is_some_and(|summary| summary.starts_with(&prefix))
            && row["created_ms"]
                .as_i64()
                .is_some_and(|created| now_ms.saturating_sub(created) < SELF_DEDUPE_WINDOW_MS)
    })
}

/// Read terminal focus out of `kitty @ ls` output for one kitty window ID. `Some(true)` only
/// when the whole chain — OS window, tab, window — reports focused; `None` when the output
/// has no such window or no focus fields at all, which degrades to [`FocusState::Unknown`].
pub fn kitty_focused(ls_json: &str, window_id: &str) -> Option<bool> {
    let windows: Vec<serde_json::Value> = serde_json::from_str(ls_json).ok()?;
    let wanted: u32 = window_id.parse().ok()?;
    for os_window in &windows {
        let Some(tabs) = os_window["tabs"].as_array() else {
            continue;
        };
        for tab in tabs {
            for window in tab["windows"].as_array()? {
                if window["id"].as_u64() == Some(u64::from(wanted)) {
                    let os_focused = os_window["is_focused"].as_bool()?;
                    let tab_focused = tab["is_focused"].as_bool()?;
                    let window_focused = window["is_focused"].as_bool()?;
                    return Some(os_focused && tab_focused && window_focused);
                }
            }
        }
    }
    None
}

/// Resolve a [`FocusRequest`] against the process environment: explicit readings win, `auto`
/// asks kitty over remote control when the shell runs inside it, and anything unanswerable
/// is [`FocusState::Unknown`], which the policy notifies.
pub async fn resolve_focus(request: FocusRequest) -> FocusState {
    match request {
        FocusRequest::Focused => FocusState::Focused,
        FocusRequest::Unfocused => FocusState::Unfocused,
        FocusRequest::Unknown => FocusState::Unknown,
        FocusRequest::Auto => kitty_focus().await.unwrap_or(FocusState::Unknown),
    }
}

async fn kitty_focus() -> Option<FocusState> {
    std::env::var_os("KITTY_PID")?;
    let window_id = std::env::var("KITTY_WINDOW_ID").ok()?;
    let listen_on = std::env::var("KITTY_LISTEN_ON").ok()?;
    let mut command = tokio::process::Command::new("kitty");
    command
        .args(["@", "--to", &listen_on, "ls"])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(1), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(match kitty_focused(&text, &window_id) {
        Some(true) => FocusState::Focused,
        Some(false) => FocusState::Unfocused,
        None => FocusState::Unknown,
    })
}

/// Guess the terminal emulator's producer name from the environment, for native-dedupe
/// matching. Only kitty — the default terminal — is recognized today; anything else leaves
/// the match empty rather than guessing wrong.
pub fn detect_emulator() -> String {
    if std::env::var_os("KITTY_PID").is_some() {
        return "kitty".to_owned();
    }
    String::new()
}

/// The hook-prefilter line a new shell evaluates at startup: the effective long-job
/// threshold in seconds, or 0 when every-job mode makes the prefilter pass everything.
pub fn policy_sh(policy: &CompletionPolicy) -> String {
    let seconds = if policy.notify_every_job {
        0
    } else {
        policy.long_job_ms / 1_000
    };
    format!("TEMPLAR_NOTIFY_MIN_SECONDS={seconds}")
}

/// The bus-backed [`CompletionDeps`]: policy from `Settings1`, activity from
/// `Notifications1`, delivery on `org.freedesktop.Notifications`. Proxies are built per
/// call from the one connection — construction does no I/O, so nothing is cached.
#[derive(Debug)]
pub struct BusDeps {
    connection: zbus::Connection,
}

impl BusDeps {
    pub fn connect(connection: zbus::Connection) -> Self {
        Self { connection }
    }

    pub async fn session() -> Result<Self> {
        Ok(Self {
            connection: zbus::Connection::session().await?,
        })
    }
}

#[async_trait]
impl CompletionDeps for BusDeps {
    async fn setting(&self, key: &str) -> Option<SettingValue> {
        let proxy = templar_client::proxies::SettingsProxy::new(&self.connection)
            .await
            .ok()?;
        let json = proxy.get(key).await.ok()?;
        serde_json::from_str(&json).ok()
    }

    async fn active(&self) -> Vec<serde_json::Value> {
        let Ok(proxy) = templar_client::proxies::NotificationsProxy::new(&self.connection).await
        else {
            return Vec::new();
        };
        proxy
            .list_active()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|row| serde_json::from_str(row).ok())
            .collect()
    }

    async fn send(&self, summary: &str, body: &str, category: &str, urgency: u8) -> Result<u32> {
        use zbus::zvariant::{OwnedValue, Value};
        let mut hints = std::collections::HashMap::new();
        hints.insert("category".to_owned(), Value::from(category).try_to_owned()?);
        hints.insert("urgency".to_owned(), OwnedValue::from(urgency));
        let proxy = zbus::Proxy::new(&self.connection, FDO_BUS_NAME, FDO_PATH, FDO_IFACE).await?;
        let id: u32 = proxy
            .call(
                "Notify",
                &(
                    COMPLETION_APP_NAME,
                    0u32,
                    "utilities-terminal",
                    summary,
                    body,
                    Vec::<String>::new(),
                    hints,
                    -1i32,
                ),
            )
            .await
            .context("sending the completion notification")?;
        Ok(id)
    }

    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

/// Read the effective policy over the session bus, degrading key by key to the defaults
/// when the service is unreachable — a hook prefilter must never fail a shell startup.
pub async fn read_policy() -> CompletionPolicy {
    let Ok(deps) = BusDeps::session().await else {
        return CompletionPolicy::default();
    };
    read_policy_on(&deps).await
}

/// The same read over an existing connection, for callers that already hold one.
pub async fn read_policy_on(deps: &impl CompletionDeps) -> CompletionPolicy {
    let mut values = BTreeMap::new();
    for key in [
        templar_session_core::notifications::LONG_JOB_SECONDS_KEY,
        templar_session_core::notifications::NOTIFY_EVERY_JOB_KEY,
        templar_session_core::notifications::NOTIFY_CANCELED_KEY,
        templar_session_core::notifications::SKIP_WHEN_FOCUSED_KEY,
    ] {
        if let Some(value) = deps.setting(key).await {
            values.insert(key.to_owned(), value);
        }
    }
    CompletionPolicy::from_values(&values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_session_core::notifications::CompletionOutcome;

    struct FakeDeps {
        values: BTreeMap<String, SettingValue>,
        rows: Vec<serde_json::Value>,
        sent: std::sync::Mutex<Vec<(String, String, String, u8)>>,
        now_ms: i64,
    }

    impl FakeDeps {
        fn row(app: &str, summary: &str, created_ms: i64) -> serde_json::Value {
            serde_json::json!({
                "app_name": app,
                "summary": summary,
                "created_ms": created_ms,
            })
        }
    }

    #[async_trait]
    impl CompletionDeps for FakeDeps {
        async fn setting(&self, key: &str) -> Option<SettingValue> {
            self.values.get(key).cloned()
        }

        async fn active(&self) -> Vec<serde_json::Value> {
            self.rows.clone()
        }

        async fn send(
            &self,
            summary: &str,
            body: &str,
            category: &str,
            urgency: u8,
        ) -> Result<u32> {
            self.sent.lock().unwrap().push((
                summary.to_owned(),
                body.to_owned(),
                category.to_owned(),
                urgency,
            ));
            Ok(7)
        }

        fn now_ms(&self) -> i64 {
            self.now_ms
        }
    }

    fn request(elapsed_ms: u64, status: i32) -> CompletionRequest {
        CompletionRequest {
            elapsed_ms,
            status,
            label: "make".to_owned(),
            emulator: String::new(),
        }
    }

    fn deps() -> FakeDeps {
        FakeDeps {
            values: BTreeMap::new(),
            rows: Vec::new(),
            sent: std::sync::Mutex::new(Vec::new()),
            now_ms: 1_000_000,
        }
    }

    #[tokio::test]
    async fn a_long_success_notifies_once_with_its_distinct_fields() {
        let deps = deps();
        let report = terminal_completed(&deps, &request(12_000, 0), FocusState::Unknown)
            .await
            .unwrap();
        assert_eq!(
            report,
            CompletionReport {
                notify: true,
                reason: "long-job",
                id: Some(7),
            }
        );
        assert_eq!(
            deps.sent.lock().unwrap().as_slice(),
            [(
                "make finished".to_owned(),
                "Completed in 12s".to_owned(),
                templar_session_core::notifications::COMPLETION_CATEGORY_SUCCESS.to_owned(),
                0,
            )]
        );
    }

    #[tokio::test]
    async fn failures_carry_the_failure_category_and_normal_urgency() {
        let deps = deps();
        let report = terminal_completed(&deps, &request(61_000, 2), FocusState::Unknown)
            .await
            .unwrap();
        assert!(report.notify);
        assert_eq!(
            deps.sent.lock().unwrap().as_slice(),
            [(
                "make failed".to_owned(),
                "Failed after 1m 01s".to_owned(),
                templar_session_core::notifications::COMPLETION_CATEGORY_FAILURE.to_owned(),
                1,
            )]
        );
    }

    #[tokio::test]
    async fn short_focused_and_canceled_jobs_send_nothing() {
        for (request, focus, reason) in [
            (request(9_999, 0), FocusState::Unknown, "short-job"),
            (request(60_000, 0), FocusState::Focused, "focused"),
            (request(60_000, 130), FocusState::Unknown, "canceled"),
        ] {
            let deps = deps();
            let report = terminal_completed(&deps, &request, focus).await.unwrap();
            assert!(!report.notify);
            assert_eq!(report.reason, reason);
            assert!(deps.sent.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn settings_drive_the_policy_key_by_key() {
        let mut deps = deps();
        deps.values.insert(
            templar_session_core::notifications::NOTIFY_EVERY_JOB_KEY.to_owned(),
            SettingValue::Bool(true),
        );
        let report = terminal_completed(&deps, &request(5, 0), FocusState::Unknown)
            .await
            .unwrap();
        assert!(report.notify);
        assert_eq!(report.reason, "every-job");
    }

    #[test]
    fn native_dedupe_matches_only_the_emulator_and_only_when_recent() {
        let rows = vec![
            FakeDeps::row("kitty", "old", 1_000_000 - NATIVE_DEDUPE_WINDOW_MS - 1),
            FakeDeps::row("kitty", "fresh", 1_000_000 - 1_000),
            FakeDeps::row("Terminal", "make finished", 1_000_000 - 500),
        ];
        assert!(native_duplicate(&rows, "kitty", 1_000_000));
        assert!(!native_duplicate(&rows[..1], "kitty", 1_000_000));
        assert!(!native_duplicate(&rows, "foot", 1_000_000));
        assert!(!native_duplicate(&rows, "", 1_000_000));
        // Our own rows never count as the emulator's, even if misconfigured to match.
        assert!(!native_duplicate(
            &rows[2..],
            COMPLETION_APP_NAME,
            1_000_000
        ));
    }

    #[test]
    fn self_dedupe_matches_only_our_label_within_the_tight_window() {
        let rows = vec![
            FakeDeps::row("Terminal", "make finished", 1_000_000 - 2_000),
            FakeDeps::row("Terminal", "makefile finished", 1_000_000 - 500),
            FakeDeps::row(
                "Terminal",
                "old finished",
                1_000_000 - SELF_DEDUPE_WINDOW_MS - 1,
            ),
        ];
        assert!(self_duplicate(&rows, "make", 1_000_000));
        assert!(!self_duplicate(&rows[1..], "make", 1_000_000));
        assert!(!self_duplicate(&rows, "old", 1_000_000));
        assert!(!self_duplicate(&[], "make", 1_000_000));
    }

    #[test]
    fn kitty_focus_needs_the_whole_chain_and_degrades_to_unknown() {
        let ls = |os: bool, tab: bool, window: bool| {
            format!(
                r#"[{{"id": 1, "is_focused": {os}, "tabs": [{{"id": 1, "is_focused": {tab}, "windows": [{{"id": 3, "is_focused": {window}}}, {{"id": 4, "is_focused": true}}]}}]}}]"#
            )
        };
        assert_eq!(kitty_focused(&ls(true, true, true), "3"), Some(true));
        assert_eq!(kitty_focused(&ls(true, true, false), "3"), Some(false));
        assert_eq!(kitty_focused(&ls(true, false, true), "3"), Some(false));
        assert_eq!(kitty_focused(&ls(false, true, true), "3"), Some(false));
        // The wrong window, malformed JSON and missing fields all answer unknown.
        assert_eq!(kitty_focused(&ls(true, true, true), "9"), None);
        assert_eq!(kitty_focused("not json", "3"), None);
        assert_eq!(kitty_focused("[]", "3"), None);
        assert_eq!(kitty_focused(r#"[{"id": 1}]"#, "3"), None);
        assert_eq!(kitty_focused(&ls(true, true, true), "nope"), None);
        let mut unrelated: Vec<serde_json::Value> =
            serde_json::from_str(&ls(true, true, true)).unwrap();
        unrelated.insert(0, serde_json::json!({"id": 99, "tabs": []}));
        assert_eq!(
            kitty_focused(&serde_json::to_string(&unrelated).unwrap(), "3"),
            Some(true)
        );
    }

    #[test]
    fn the_hook_prefilter_line_folds_every_job_mode_to_zero() {
        assert_eq!(
            policy_sh(&CompletionPolicy::default()),
            "TEMPLAR_NOTIFY_MIN_SECONDS=10"
        );
        assert_eq!(
            policy_sh(&CompletionPolicy {
                notify_every_job: true,
                ..CompletionPolicy::default()
            }),
            "TEMPLAR_NOTIFY_MIN_SECONDS=0"
        );
        assert_eq!(
            policy_sh(&CompletionPolicy {
                long_job_ms: 45_000,
                ..CompletionPolicy::default()
            }),
            "TEMPLAR_NOTIFY_MIN_SECONDS=45"
        );
    }

    #[test]
    fn focus_requests_parse_and_outcomes_classify_for_the_route() {
        use std::str::FromStr as _;
        assert_eq!(FocusRequest::from_str("auto").unwrap(), FocusRequest::Auto);
        assert_eq!(
            FocusRequest::from_str("focused").unwrap(),
            FocusRequest::Focused
        );
        assert!(FocusRequest::from_str("sometimes").is_err());
        assert_eq!(classify_exit(0), CompletionOutcome::Success);
        assert_eq!(classify_exit(130), CompletionOutcome::Canceled);
    }

    // Shell-hook behavior, driven the way the garden hook's tests drive it: a `templar`
    // on PATH that only records its arguments, and the hook loaded the way the
    // installed one is — as an rc file read by an interactive shell.
    mod hook_tests {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        const BASH_HOOK: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/profile.d/61-templaros-completion.sh"
        );
        const ZSH_HOOK: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/profile.d/61-templaros-completion.zsh"
        );
        const FISH_HOOK: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/profile.d/61-templaros-completion.fish"
        );
        const GARDEN_HOOK: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../session/profile.d/60-templaros-garden.sh"
        );

        /// A `templar` stub that records every invocation. When `policy` is set it also
        /// answers `notify policy-sh`, the way the real route feeds the hook prefilter.
        fn stub_with_policy(directory: &std::path::Path, policy: Option<&str>) {
            let stub = directory.join("templar");
            let mut script = String::from("#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$TRACE_FILE\"\n");
            if let Some(policy) = policy {
                use std::fmt::Write as _;
                let _ = write!(
                    script,
                    "if [ \"$1\" = notify ] && [ \"$2\" = policy-sh ]; then\necho \"{policy}\"\nfi\n"
                );
            }
            std::fs::write(&stub, script).unwrap();
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fn trace_lines(trace: &std::path::Path) -> Vec<String> {
            std::fs::read_to_string(trace)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn completions(trace: &std::path::Path) -> Vec<String> {
            trace_lines(trace)
                .into_iter()
                .filter(|line| line.starts_with("notify terminal-completed "))
                .collect()
        }

        /// Run an interactive bash with piped stdin, the hook as its rc file, and the
        /// stub on PATH. Returns stdout; prompts and job-control noise stay on stderr.
        fn run_bash(
            directory: &std::path::Path,
            rcfile: &str,
            desktop: &str,
            extra_env: &[(&str, &str)],
            script: &str,
        ) -> String {
            let mut child = std::process::Command::new("bash")
                .args(["--rcfile", rcfile, "-i"])
                .env("TRACE_FILE", directory.join("trace"))
                .env("XDG_CURRENT_DESKTOP", desktop)
                .env(
                    "PATH",
                    format!("{}:{}", directory.display(), std::env::var("PATH").unwrap()),
                )
                .envs(extra_env.iter().copied())
                .current_dir(directory)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(script.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        #[test]
        fn bash_reports_one_completion_per_pipeline_with_status_and_label() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            let out = run_bash(
                tmp.path(),
                BASH_HOOK,
                "TemplarOS",
                &[],
                "false | true\nsh -c 'exit 130'\ncommand true\nenv FOO=bar true\nCFLAGS=\"-O2 -g\" true\n",
            );
            assert!(out.is_empty(), "the hook reports silently, got: {out:?}");
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 5, "one call per job: {calls:?}");
            // One pipeline, one timing: the exit status is the pipeline's, the label its
            // first stage.
            assert!(calls[0].contains("--status 0"), "{}", calls[0]);
            assert!(calls[0].contains("--label false"), "{}", calls[0]);
            assert!(calls[0].contains("--elapsed-ms "), "{}", calls[0]);
            // A 130 outlives the hook untouched for the policy to classify as canceled.
            assert!(calls[1].contains("--status 130"), "{}", calls[1]);
            assert!(calls[1].contains("--label sh"), "{}", calls[1]);
            // Wrappers and env prefixes never become the label — even when the value
            // carries spaces inside quotes.
            assert!(calls[2].ends_with("--label true"), "{}", calls[2]);
            assert!(calls[3].ends_with("--label true"), "{}", calls[3]);
            assert!(calls[4].ends_with("--label true"), "{}", calls[4]);
        }

        #[test]
        fn bash_skips_launches_assignments_and_self_notifying_runs() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            run_bash(
                tmp.path(),
                BASH_HOOK,
                "TemplarOS",
                &[],
                "sleep 0.2 &\nFOO=bar\ntemplar run --notify -- true\ntemplar run -- true\n",
            );
            let lines = trace_lines(&tmp.path().join("trace"));
            // The stub ran for the two `templar run` lines themselves, but only the
            // plain one produced a completion — labeled with its workload.
            assert!(lines.iter().any(|line| line == "run --notify -- true"));
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert!(calls[0].contains("--label true"), "{}", calls[0]);
        }

        #[test]
        fn bash_reports_reaped_background_work_but_not_its_launch() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            run_bash(
                tmp.path(),
                BASH_HOOK,
                "TemplarOS",
                &[],
                "sleep 0.05 & wait\n",
            );
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert!(calls[0].contains("--label sleep"), "{}", calls[0]);
        }

        #[test]
        fn bash_prefilter_keeps_fast_commands_from_spawning_the_route() {
            let tmp = tempfile::tempdir().unwrap();
            // No policy answer: the hook falls back to the 10 s default.
            stub_with_policy(tmp.path(), None);
            // Ends in `true`: bash exits with the last job's status and the hook
            // preserves it, so a bare `false` script would fail run_bash's guard.
            run_bash(tmp.path(), BASH_HOOK, "TemplarOS", &[], "false\ntrue\n");
            let lines = trace_lines(&tmp.path().join("trace"));
            // The prefilter read policy once and never spawned the route. Garden lines
            // from the host's own /etc hook are filtered, not asserted.
            assert!(lines.contains(&"notify policy-sh".to_owned()), "{lines:?}");
            assert!(
                completions(&tmp.path().join("trace")).is_empty(),
                "{lines:?}"
            );
            // An explicit export overrides the policy read entirely.
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), None);
            run_bash(
                tmp.path(),
                BASH_HOOK,
                "TemplarOS",
                &[("TEMPLAR_NOTIFY_MIN_SECONDS", "0")],
                "false\ntrue\n",
            );
            assert_eq!(completions(&tmp.path().join("trace")).len(), 2);
        }

        #[test]
        fn bash_preserves_status_prompt_cwd_and_existing_hooks() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            let rc = tmp.path().join("rc");
            std::fs::write(
                &rc,
                format!(
                    "__prior_debug_probe() {{ printf 'prior-debug\\n' >> \"$TRACE_FILE\"; }}\n\
                     trap '__prior_debug_probe' DEBUG\n\
                     __prior_prompt_probe() {{ printf 'prior-prompt\\n' >> \"$TRACE_FILE\"; }}\n\
                     PROMPT_COMMAND=\"__prior_prompt_probe\"\n\
                     source \"{BASH_HOOK}\"\n"
                ),
            )
            .unwrap();
            let out = run_bash(
                tmp.path(),
                rc.to_str().unwrap(),
                "TemplarOS",
                &[],
                "false\necho \"s1-$?\"\ntrue\necho \"s2-$?\"\necho \"pwd-$PWD\"\necho \"pc-$PROMPT_COMMAND\"\n",
            );
            assert!(out.contains("s1-1"), "{out:?}");
            assert!(out.contains("s2-0"), "{out:?}");
            assert!(
                out.contains(&format!("pwd-{}", tmp.path().display())),
                "{out:?}"
            );
            assert!(out.contains("__prior_prompt_probe"), "{out:?}");
            assert!(out.contains("__templaros_completion_precmd"), "{out:?}");
            let lines = trace_lines(&tmp.path().join("trace"));
            assert!(lines.contains(&"prior-debug".to_owned()), "{lines:?}");
            assert!(lines.contains(&"prior-prompt".to_owned()), "{lines:?}");
            // Six foreground jobs, every one reported exactly once under a zero
            // prefilter — including the probes themselves.
            assert_eq!(completions(&tmp.path().join("trace")).len(), 6, "{lines:?}");
        }

        #[test]
        fn bash_array_prompt_reads_job_status_before_prior_hooks() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            let rc = tmp.path().join("rc");
            std::fs::write(&rc, format!(
                "__prior_prompt_probe() {{ printf 'prior-status-%s\\n' \"$?\" >> \"$TRACE_FILE\"; return 0; }}\n\
                 PROMPT_COMMAND=(__prior_prompt_probe)\nsource \"{BASH_HOOK}\"\n"
            )).unwrap();
            run_bash(
                tmp.path(),
                rc.to_str().unwrap(),
                "TemplarOS",
                &[],
                "false\ntrue\n",
            );
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 2, "{calls:?}");
            assert!(calls[0].contains("--status 1"), "{calls:?}");
            assert!(calls[1].contains("--status 0"), "{calls:?}");
            assert!(trace_lines(&tmp.path().join("trace")).contains(&"prior-status-1".into()));
        }

        #[test]
        fn bash_loads_only_for_interactive_templaros_shells() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), None);
            let probe = [
                "--rcfile",
                BASH_HOOK,
                "-i",
                "-c",
                "declare -F __templaros_completion_precmd",
            ];
            // Not a TemplarOS session: the hook returns before defining anything.
            let status = std::process::Command::new("bash")
                .args(probe)
                .env("XDG_CURRENT_DESKTOP", "GNOME")
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success());
            // A non-interactive shell never arms: the guard holds even if sourced.
            let status = std::process::Command::new("bash")
                .args([
                    "--rcfile",
                    BASH_HOOK,
                    "-c",
                    "declare -F __templaros_completion_precmd",
                ])
                .env("XDG_CURRENT_DESKTOP", "TemplarOS")
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(!status.success());
            assert!(!tmp.path().join("trace").exists());
        }

        #[test]
        fn bash_coexists_with_the_garden_hook_on_one_prompt() {
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            let rc = tmp.path().join("rc");
            std::fs::write(
                &rc,
                format!("source \"{GARDEN_HOOK}\"\nsource \"{BASH_HOOK}\"\n"),
            )
            .unwrap();
            run_bash(
                tmp.path(),
                rc.to_str().unwrap(),
                "TemplarOS",
                &[],
                "cd /\ntrue\n",
            );
            let lines = trace_lines(&tmp.path().join("trace"));
            let garden: Vec<_> = lines
                .iter()
                .filter(|line| line.starts_with("garden observe "))
                .collect();
            // The first prompt records the starting directory, the `cd` records `/`.
            assert_eq!(garden.len(), 2, "{lines:?}");
            assert!(garden[1].ends_with("terminal-entered / --role terminal"));
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 2, "{lines:?}");
            assert!(calls[0].contains("--label cd"), "{}", calls[0]);
            assert!(calls[1].contains("--label true"), "{}", calls[1]);
        }

        #[test]
        fn zsh_reports_completion_when_zsh_exists() {
            if std::process::Command::new("zsh")
                .arg("--version")
                .output()
                .is_err()
            {
                eprintln!("skipping zsh completion execution: zsh is not installed");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            let zdotdir = tmp.path().join("zdot");
            std::fs::create_dir(&zdotdir).unwrap();
            std::fs::write(zdotdir.join(".zshrc"), format!("source \"{ZSH_HOOK}\"\n")).unwrap();
            let mut child = std::process::Command::new("zsh")
                .arg("-i")
                .env("ZDOTDIR", &zdotdir)
                .env("TRACE_FILE", tmp.path().join("trace"))
                .env("XDG_CURRENT_DESKTOP", "TemplarOS")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        tmp.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .current_dir(tmp.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(b"false\n").unwrap();
            child.wait().unwrap();
            let calls = completions(&tmp.path().join("trace"));
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert!(calls[0].contains("--status 1"), "{}", calls[0]);
            assert!(calls[0].contains("--label false"), "{}", calls[0]);
        }

        #[tokio::test]
        async fn fish_reports_completion_when_fish_exists() {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            async fn prompt(reader: &mut tokio::io::BufReader<tokio::process::ChildStdout>) {
                // A rendered prompt is not a line: fish captures fish_prompt output and
                // strips trailing newlines. Read terminal bytes, including split markers.
                const MARKER: &[u8] = b"__TEMPLAR_TEST_PROMPT__";
                let mut received = Vec::new();
                let ready = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let mut chunk = [0; 512];
                    loop {
                        let count = reader.read(&mut chunk).await.unwrap();
                        assert!(
                            count > 0,
                            "fish exited before its prompt: {:?}",
                            String::from_utf8_lossy(&received)
                        );
                        received.extend_from_slice(&chunk[..count]);
                        if received.windows(MARKER.len()).any(|bytes| bytes == MARKER) {
                            break;
                        }
                    }
                })
                .await;
                assert!(
                    ready.is_ok(),
                    "fish did not reach its prompt; terminal output: {:?}",
                    String::from_utf8_lossy(&received)
                );
            }
            if std::process::Command::new("fish")
                .arg("--version")
                .output()
                .is_err()
            {
                eprintln!("skipping fish completion execution: fish is not installed");
                return;
            }
            let tmp = tempfile::tempdir().unwrap();
            stub_with_policy(tmp.path(), Some("TEMPLAR_NOTIFY_MIN_SECONDS=0"));
            // fish_postexec requires an actual interactive terminal, not merely -i with
            // piped stdin. util-linux script owns the controlling PTY; wait for the real
            // prompt before each input rather than racing shell startup with a sleep.
            let mut child = tokio::process::Command::new("script")
                .args([
                    "--quiet", "--return", "--flush", "--command",
                    "fish --no-config -i -C 'source \"$TEMPLAR_COMPLETION_TEST_HOOK\"; function fish_prompt; printf \"__TEMPLAR_TEST_PROMPT__\"; end'",
                    "/dev/null",
                ])
                .kill_on_drop(true)
                .env("TERM", "xterm-256color")
                .env("TEMPLAR_COMPLETION_TEST_HOOK", FISH_HOOK)
                .env("XDG_CONFIG_HOME", tmp.path().join("config"))
                .env("TRACE_FILE", tmp.path().join("trace"))
                .env("XDG_CURRENT_DESKTOP", "TemplarOS")
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        tmp.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .current_dir(tmp.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("fish completion test needs util-linux script for a controlling PTY");
            let mut input = child.stdin.take().unwrap();
            let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
            prompt(&mut output).await;
            input.write_all(b"false\n").await.unwrap();
            prompt(&mut output).await;
            // Observe the completed workload before the cleanup command: fish also
            // emits postexec for `exit`, which is not part of this scenario.
            let calls = completions(&tmp.path().join("trace"));
            input.write_all(b"exit 0\n").await.unwrap();
            let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
                .await
                .expect("fish PTY session did not exit")
                .unwrap();
            assert!(status.success(), "fish PTY session failed: {status}");
            assert_eq!(calls.len(), 1, "{calls:?}");
            assert!(calls[0].contains("--status 1"), "{}", calls[0]);
            assert!(calls[0].contains("--label false"), "{}", calls[0]);
        }
    }
}
