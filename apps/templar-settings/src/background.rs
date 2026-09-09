//! Background permissions page: "every currently running background application and why it is
//! running" (`docs/architecture/providers.md`), over `org.templaros.Background1`.
//!
//! This page used to reconstruct that answer by joining `AppIdentity1.ListInstances`,
//! `History1.Recent` and `Providers1.Report` itself. `Background1` now exists precisely so no
//! client has to — the interface's own commentary calls that three-interface join "exactly what
//! [`@templaros.constraint.services-not-indicators`] forbids" — and it answers more than the join
//! could: what each application *asked for*, whether the session said yes, and, for a running
//! one, whether this instance came from autostart or from a launch.
//!
//! `List()` returns `(app_id, permissions_json, approval, running, reason_json)`.
//! [`parse_entries`] mirrors that wire shape; every parse is lenient in one direction only — a
//! field this page cannot read becomes a plainly-worded absence, never a guess.

use anyhow::{Context, Result};
use serde::Deserialize;

/// The wire row of `Background1.List`.
pub(crate) type Row = (String, String, String, bool, String);

/// What the session has decided about an application's declared background activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Approval {
    Approved,
    Denied,
    /// Declared, never answered — the state a freshly installed application starts in.
    Undecided,
}

impl Approval {
    fn parse(value: &str) -> Self {
        match value {
            "approved" => Self::Approved,
            "denied" => Self::Denied,
            _ => Self::Undecided,
        }
    }

    /// The word this page shows for the decision. Deliberately the plain past tense of what a
    /// person did, not the service's enum spelling.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Approved => "Allowed",
            Self::Denied => "Blocked",
            Self::Undecided => "Not decided",
        }
    }
}

/// Why a running application is running, as `Background1` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Reason {
    pub(crate) instance_id: String,
    /// `"autostart"` or `"launch"`.
    pub(crate) started_via: String,
    /// The provider role the application declares, if it declares one.
    #[serde(default)]
    pub(crate) role: Option<String>,
    /// Whether it is the *resolved* provider for that role, rather than merely a candidate.
    #[serde(default)]
    pub(crate) role_selected: bool,
}

/// One application that declares background activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackgroundEntry {
    pub(crate) app_id: String,
    /// The kebab-case permission names it declares, in the order the service reported them.
    pub(crate) permissions: Vec<String>,
    pub(crate) approval: Approval,
    pub(crate) running: bool,
    /// Present only while it is running; `None` for a declaration with nothing behind it.
    pub(crate) reason: Option<Reason>,
}

/// Parse `List()`'s rows. Pure and unit-testable against literal wire values.
///
/// A row whose `permissions_json` or `reason_json` will not parse still yields an entry: the
/// application and its approval are the part a person acts on, and losing the whole row because
/// a detail did not decode would hide a permission that is genuinely in force.
pub(crate) fn parse_entries(rows: &[Row]) -> Vec<BackgroundEntry> {
    rows.iter()
        .map(
            |(app_id, permissions_json, approval, running, reason_json)| BackgroundEntry {
                app_id: app_id.clone(),
                permissions: serde_json::from_str(permissions_json).unwrap_or_default(),
                approval: Approval::parse(approval),
                running: *running,
                reason: if *running {
                    serde_json::from_str(reason_json).ok()
                } else {
                    None
                },
            },
        )
        .collect()
}

/// What the application asked for, as a sentence.
pub(crate) fn permissions_line(entry: &BackgroundEntry) -> String {
    if entry.permissions.is_empty() {
        return "Declares background activity without naming a permission.".to_owned();
    }
    format!("Asks for {}.", join_words(&entry.permissions))
}

/// Why it is running now — or that it is not.
pub(crate) fn running_line(entry: &BackgroundEntry) -> String {
    let Some(reason) = entry.reason.as_ref() else {
        return if entry.running {
            "Running, but the session did not report why.".to_owned()
        } else {
            "Not running.".to_owned()
        };
    };
    let start = match reason.started_via.as_str() {
        "autostart" => "Started at login",
        "launch" => "Started when it was opened",
        other => return format!("Running ({other})."),
    };
    match (&reason.role, reason.role_selected) {
        (Some(role), true) => format!("{start}; it is the selected provider for {role}."),
        (Some(role), false) => {
            format!("{start}; it offers to be the provider for {role} but another one is selected.")
        }
        (None, _) => format!("{start}."),
    }
}

/// `a, b and c` — the list a person would say out loud, not a comma-joined machine list.
fn join_words(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [only] => only.replace('-', " "),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(|word| word.replace('-', " "))
                .collect::<Vec<_>>()
                .join(", "),
            last.replace('-', " ")
        ),
    }
}

fn render_entries(entries: &[BackgroundEntry]) -> String {
    if entries.is_empty() {
        return "No application declares background activity.".to_owned();
    }
    entries
        .iter()
        .map(|entry| {
            format!(
                "{} — {}\n  {}\n  {}",
                entry.app_id,
                entry.approval.label(),
                permissions_line(entry),
                running_line(entry)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The bus operations this page needs.
pub(crate) trait BackgroundSource {
    async fn list(&self) -> Result<Vec<Row>>;
    async fn grant(&self, app_id: &str) -> Result<()>;
    async fn revoke(&self, app_id: &str) -> Result<()>;
}

impl BackgroundSource for templar_client::proxies::BackgroundProxy<'_> {
    async fn list(&self) -> Result<Vec<Row>> {
        self.list().await.context("Background1.List")
    }
    async fn grant(&self, app_id: &str) -> Result<()> {
        self.grant(app_id).await.context("Background1.Grant")
    }
    async fn revoke(&self, app_id: &str) -> Result<()> {
        self.revoke(app_id).await.context("Background1.Revoke")
    }
}

/// The page's entries, or the reason there are none. The graphical page needs the parsed values
/// rather than the rendered text, so the fetch is separate from the rendering.
pub(crate) async fn entries_with<S: BackgroundSource>(
    source: &S,
) -> Result<Vec<BackgroundEntry>, String> {
    match source.list().await {
        Ok(rows) => Ok(parse_entries(&rows)),
        Err(error) => Err(format!("service unavailable: {error:#}")),
    }
}

pub(crate) async fn render_with<S: BackgroundSource>(source: &S) -> String {
    match entries_with(source).await {
        Ok(entries) => render_entries(&entries),
        Err(message) => message,
    }
}

/// Connect to `Background1`, for a caller that already has a session connection.
pub(crate) async fn connect(
    connection: &zbus::Connection,
) -> Result<templar_client::proxies::BackgroundProxy<'static>> {
    templar_client::proxies::BackgroundProxy::new(connection)
        .await
        .context("Background1")
}

pub(crate) async fn render(connection: &zbus::Connection) -> String {
    match connect(connection).await {
        Ok(proxy) => render_with(&proxy).await,
        Err(error) => format!("service unavailable: could not reach Background1 ({error:#})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(app_id: &str, permissions: &str, approval: &str, running: bool, reason: &str) -> Row {
        (
            app_id.to_owned(),
            permissions.to_owned(),
            approval.to_owned(),
            running,
            reason.to_owned(),
        )
    }

    #[test]
    fn a_running_selected_provider_says_which_role_it_fills() {
        let entries = parse_entries(&[row(
            "org.templaros.NetworkTray",
            r#"["continuous-background","network-while-hidden"]"#,
            "approved",
            true,
            r#"{"instance_id":"i-1","started_via":"autostart","role":"network-status-ui","role_selected":true}"#,
        )]);
        assert_eq!(entries[0].approval, Approval::Approved);
        assert_eq!(
            permissions_line(&entries[0]),
            "Asks for continuous background and network while hidden."
        );
        assert_eq!(
            running_line(&entries[0]),
            "Started at login; it is the selected provider for network-status-ui."
        );
    }

    #[test]
    fn a_candidate_that_did_not_win_the_role_is_not_described_as_the_provider() {
        let entries = parse_entries(&[row(
            "org.example.OtherTray",
            r#"["continuous-background"]"#,
            "approved",
            true,
            r#"{"instance_id":"i-2","started_via":"launch","role":"network-status-ui","role_selected":false}"#,
        )]);
        assert_eq!(
            running_line(&entries[0]),
            "Started when it was opened; it offers to be the provider for network-status-ui but another one is selected."
        );
    }

    #[test]
    fn a_declaration_that_is_not_running_carries_no_reason() {
        let entries = parse_entries(&[row(
            "org.example.Sync",
            r#"["login-autostart"]"#,
            "undecided",
            false,
            "",
        )]);
        assert_eq!(entries[0].approval, Approval::Undecided);
        assert!(entries[0].reason.is_none());
        assert_eq!(running_line(&entries[0]), "Not running.");
        assert_eq!(permissions_line(&entries[0]), "Asks for login autostart.");
    }

    #[test]
    fn an_unreadable_detail_never_costs_the_row_its_approval() {
        let entries = parse_entries(&[row(
            "org.example.Broken",
            "not json",
            "denied",
            true,
            "also not json",
        )]);
        assert_eq!(entries[0].approval, Approval::Denied);
        assert!(entries[0].permissions.is_empty());
        assert_eq!(
            permissions_line(&entries[0]),
            "Declares background activity without naming a permission."
        );
        assert_eq!(
            running_line(&entries[0]),
            "Running, but the session did not report why."
        );
    }

    #[test]
    fn an_unknown_approval_word_is_read_as_undecided_rather_than_as_consent() {
        let entries = parse_entries(&[row("org.example.New", "[]", "pending-review", false, "")]);
        assert_eq!(entries[0].approval, Approval::Undecided);
        assert_eq!(entries[0].approval.label(), "Not decided");
    }

    struct FakeBackground(Vec<Row>);
    impl BackgroundSource for FakeBackground {
        async fn list(&self) -> Result<Vec<Row>> {
            Ok(self.0.clone())
        }
        async fn grant(&self, _app_id: &str) -> Result<()> {
            Ok(())
        }
        async fn revoke(&self, _app_id: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn render_with_reports_the_decision_and_the_reason_together() {
        let rendered = render_with(&FakeBackground(vec![row(
            "org.templaros.NetworkTray",
            r#"["continuous-background"]"#,
            "approved",
            true,
            r#"{"instance_id":"i-1","started_via":"autostart","role":null,"role_selected":false}"#,
        )]))
        .await;
        assert!(rendered.contains("org.templaros.NetworkTray — Allowed"));
        assert!(rendered.contains("Started at login."));
    }

    #[tokio::test]
    async fn render_with_says_so_when_nothing_declares_background_activity() {
        let rendered = render_with(&FakeBackground(vec![])).await;
        assert_eq!(rendered, "No application declares background activity.");
    }

    #[tokio::test]
    async fn render_with_degrades_when_the_service_is_unavailable() {
        struct Failing;
        impl BackgroundSource for Failing {
            async fn list(&self) -> Result<Vec<Row>> {
                anyhow::bail!("no reply from Background1")
            }
            async fn grant(&self, _app_id: &str) -> Result<()> {
                Ok(())
            }
            async fn revoke(&self, _app_id: &str) -> Result<()> {
                Ok(())
            }
        }
        let rendered = render_with(&Failing).await;
        assert!(rendered.starts_with("service unavailable:"));
    }
}
