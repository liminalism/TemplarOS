//! `lege providers` — one selected provider per exclusive system role
//! (`docs/architecture/providers.md`): the report explains why each role resolved the way it
//! did, so "why did my network tray change" is answerable without reading a database.
//!
//! Every D-Bus call goes through [`ProvidersClient`] so the rendering below is testable with a
//! fixture, never a live `org.legeos.Providers1`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What `lege providers` needs from `org.legeos.Providers1`.
#[async_trait]
pub trait ProvidersClient {
    async fn report(&self) -> Result<Vec<(String, String)>>;
    async fn select(&self, role: &str, app_id: &str) -> Result<()>;
    async fn declare(&self, app_id: &str, role: &str, priority: u32) -> Result<()>;
    async fn undeclare(&self, app_id: &str) -> Result<()>;
}

#[async_trait]
impl ProvidersClient for lege_client::proxies::ProvidersProxy<'_> {
    async fn report(&self) -> Result<Vec<(String, String)>> {
        self.report().await.context("Providers1.Report")
    }
    async fn select(&self, role: &str, app_id: &str) -> Result<()> {
        self.select(role, app_id).await.context("Providers1.Select")
    }
    async fn declare(&self, app_id: &str, role: &str, priority: u32) -> Result<()> {
        self.declare(app_id, role, priority)
            .await
            .context("Providers1.Declare")
    }
    async fn undeclare(&self, app_id: &str) -> Result<()> {
        self.undeclare(app_id).await.context("Providers1.Undeclare")
    }
}

/// Mirrors `lege_session_core::providers::Resolution` (a dependency this crate does not need
/// just to parse the JSON `Providers1.Report` already produces).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Resolution {
    Selected {
        app_id: String,
    },
    DefaultedByPriority {
        app_id: String,
        #[serde(default)]
        competitors: Vec<String>,
    },
    SelectionStale {
        app_id: String,
    },
    NoProvider,
}

/// Parse one `(role, resolution_json)` pair from `Report`.
pub fn parse_resolution(resolution_json: &str) -> Result<Resolution> {
    serde_json::from_str(resolution_json).context("parsing Providers1.Report resolution JSON")
}

/// One line per role, in the fixed [`lege_types::ProviderRole::ALL`] order the daemon reports.
pub fn render_report(report: &[(String, Resolution)]) -> String {
    let rows: Vec<Vec<String>> = report
        .iter()
        .map(|(role, resolution)| vec![role.clone(), describe(resolution)])
        .collect();
    crate::format::table(&rows)
}

/// One sentence: why this role is (or is not) filled the way it is.
pub fn describe(resolution: &Resolution) -> String {
    match resolution {
        Resolution::Selected { app_id } => format!("{app_id} (explicitly selected)"),
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } if competitors.is_empty() => {
            format!("{app_id} (only declared provider)")
        }
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } => format!(
            "{app_id} (highest priority; beat {})",
            competitors.join(", ")
        ),
        Resolution::SelectionStale { app_id } => {
            format!("{app_id} was selected but no longer declares this role")
        }
        Resolution::NoProvider => "no application declares this role".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolutions_parse_and_describe() {
        assert_eq!(
            describe(
                &parse_resolution(r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#)
                    .unwrap()
            ),
            "org.legeos.NetworkTray (explicitly selected)"
        );
        assert_eq!(
            describe(
                &parse_resolution(
                    r#"{"kind":"defaulted-by-priority","app_id":"org.legeos.NetworkTray","competitors":["org.gnome.NmApplet"]}"#
                )
                .unwrap()
            ),
            "org.legeos.NetworkTray (highest priority; beat org.gnome.NmApplet)"
        );
        assert_eq!(
            describe(
                &parse_resolution(r#"{"kind":"selection-stale","app_id":"org.gnome.NmApplet"}"#)
                    .unwrap()
            ),
            "org.gnome.NmApplet was selected but no longer declares this role"
        );
        assert_eq!(
            describe(&parse_resolution(r#"{"kind":"no-provider"}"#).unwrap()),
            "no application declares this role"
        );
    }

    #[test]
    fn report_renders_one_row_per_role() {
        let report = vec![
            (
                "network-status-ui".to_owned(),
                parse_resolution(r#"{"kind":"no-provider"}"#).unwrap(),
            ),
            (
                "lock-screen".to_owned(),
                parse_resolution(r#"{"kind":"selected","app_id":"org.legeos.Session"}"#).unwrap(),
            ),
        ];
        let rendered = render_report(&report);
        assert!(rendered.contains("network-status-ui"));
        assert!(rendered.contains("lock-screen"));
        assert!(rendered.contains("org.legeos.Session"));
    }
}
