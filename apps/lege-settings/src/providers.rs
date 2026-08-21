//! Providers page (`org.legeos.Providers1`): which application fills each exclusive system
//! role, and why.
//!
//! `Report()` returns `(role, resolution_json)` pairs. [`Resolution`] mirrors the wire shape
//! that `crates/lege-session-core/src/providers.rs`'s `Resolution` enum serializes to. The JSON
//! is the contract between sessiond and every client — this crate keeps its own copy of the
//! shape rather than depending on `lege-session-core` (which also drags in `lege-state`/SQLite
//! for no reason a settings page needs).

use anyhow::{Context, Result};
use lege_types::{AppId, ProviderRole};
use serde::Deserialize;

/// Mirrors `lege_session_core::providers::Resolution`'s `#[serde(tag = "kind")]` wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub(crate) enum Resolution {
    /// Explicitly selected and installed.
    Selected { app_id: AppId },
    /// No explicit selection: highest-priority declaration wins.
    DefaultedByPriority {
        app_id: AppId,
        competitors: Vec<AppId>,
    },
    /// Selected app is not installed / no longer declares the role.
    SelectionStale { app_id: AppId },
    /// Nothing declares this role.
    NoProvider,
}

impl Resolution {
    /// The application allowed to fill the role under this resolution, if any — mirrors
    /// `ProviderRegistry::provider()`. Used by the background page to say "running because it is
    /// the selected provider for X" without a second D-Bus round trip.
    pub(crate) fn provider_app_id(&self) -> Option<&AppId> {
        match self {
            Self::Selected { app_id } | Self::DefaultedByPriority { app_id, .. } => Some(app_id),
            Self::SelectionStale { .. } | Self::NoProvider => None,
        }
    }
}

/// One full sentence explaining a role's resolution, for a person. Pure and unit-testable
/// against literal [`Resolution`] values — no D-Bus call needed.
///
/// `DefaultedByPriority::competitors` carries only the losing app IDs, not their priorities (see
/// `Resolution`'s doc comment in `lege-session-core`), so the sentence is phrased around what is
/// actually known rather than inventing a priority number.
pub(crate) fn explain(role: ProviderRole, resolution: &Resolution) -> String {
    match resolution {
        Resolution::Selected { app_id } => format!("{app_id} is selected for {role}."),
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } if competitors.is_empty() => {
            format!(
                "{app_id} is the default for {role} by priority (no other application declares it)."
            )
        }
        Resolution::DefaultedByPriority {
            app_id,
            competitors,
        } => {
            format!(
                "{app_id} is the default for {role} by priority, ahead of {}.",
                join_app_ids(competitors)
            )
        }
        Resolution::SelectionStale { app_id } => format!(
            "{app_id} was selected for {role}, but no longer declares it (uninstalled, or its declaration was withdrawn)."
        ),
        Resolution::NoProvider => format!("No application declares {role}."),
    }
}

fn join_app_ids(ids: &[AppId]) -> String {
    ids.iter().map(AppId::as_str).collect::<Vec<_>>().join(", ")
}

/// The one bus operation this page needs.
pub(crate) trait ProvidersSource {
    async fn report(&self) -> Result<Vec<(String, String)>>;
}

impl ProvidersSource for lege_client::proxies::ProvidersProxy<'_> {
    async fn report(&self) -> Result<Vec<(String, String)>> {
        self.report().await.context("Providers1.Report")
    }
}

/// Render every role in [`ProviderRole::ALL`] order (or just `role`, if given) against a live or
/// fake [`ProvidersSource`]. Never panics: a `Report()` failure or an unparseable resolution
/// becomes a line in the output, not an aborted page.
pub(crate) async fn render_with<S: ProvidersSource>(
    source: &S,
    role: Option<ProviderRole>,
) -> String {
    match source.report().await {
        Ok(pairs) => render_report(&pairs, role),
        Err(error) => format!("service unavailable: {error:#}"),
    }
}

fn render_report(pairs: &[(String, String)], filter: Option<ProviderRole>) -> String {
    ProviderRole::ALL
        .into_iter()
        .filter(|role| filter.is_none_or(|wanted| wanted == *role))
        .map(|role| explain_role(role, pairs))
        .collect::<Vec<_>>()
        .join("\n")
}

fn explain_role(role: ProviderRole, pairs: &[(String, String)]) -> String {
    let Some((_, json)) = pairs.iter().find(|(name, _)| name == role.as_str()) else {
        return format!("{role}: not reported by Providers1.Report.");
    };
    match serde_json::from_str::<Resolution>(json) {
        Ok(resolution) => explain(role, &resolution),
        Err(error) => format!("{role}: could not parse resolution ({error}): {json}"),
    }
}

/// Parse every `(role, resolution_json)` pair `Report()` returned into typed `(role,
/// [`Resolution`])` pairs, dropping entries that name an unknown role or fail to parse (a role
/// the settings page doesn't know yet is a `lege-types` skew problem, not something to crash
/// over). Used by the background page to answer "is this instance a role's provider?" without a
/// second D-Bus round trip.
pub(crate) fn parse_report(pairs: &[(String, String)]) -> Vec<(ProviderRole, Resolution)> {
    pairs
        .iter()
        .filter_map(|(role_name, json)| {
            let role = ProviderRole::ALL
                .into_iter()
                .find(|role| role.as_str() == role_name)?;
            let resolution = serde_json::from_str::<Resolution>(json).ok()?;
            Some((role, resolution))
        })
        .collect()
}

/// Connect to the session bus and render the page. The one impure entry point — everything it
/// delegates to is pure or trait-mediated and covered by tests.
pub(crate) async fn render(connection: &zbus::Connection, role: Option<ProviderRole>) -> String {
    match lege_client::proxies::ProvidersProxy::new(connection).await {
        Ok(proxy) => render_with(&proxy, role).await,
        Err(error) => format!("service unavailable: could not reach Providers1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> AppId {
        id.parse().unwrap()
    }

    #[test]
    fn selected_is_a_plain_sentence() {
        let resolution = Resolution::Selected {
            app_id: app("org.legeos.NetworkTray"),
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.NetworkTray is selected for network-status-ui."
        );
    }

    #[test]
    fn defaulted_by_priority_names_the_competitors_without_inventing_priorities() {
        let resolution = Resolution::DefaultedByPriority {
            app_id: app("org.legeos.NetworkTray"),
            competitors: vec![app("org.gnome.NmApplet")],
        };
        let sentence = explain(ProviderRole::NetworkStatusUi, &resolution);
        assert_eq!(
            sentence,
            "org.legeos.NetworkTray is the default for network-status-ui by priority, ahead of org.gnome.NmApplet."
        );
        assert!(
            !sentence.contains("priority 100"),
            "no priority number is available to report"
        );
    }

    #[test]
    fn defaulted_by_priority_with_no_competitors_says_so() {
        let resolution = Resolution::DefaultedByPriority {
            app_id: app("org.legeos.NetworkTray"),
            competitors: vec![],
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.NetworkTray is the default for network-status-ui by priority (no other application declares it)."
        );
    }

    #[test]
    fn selection_stale_says_it_no_longer_declares_the_role() {
        let resolution = Resolution::SelectionStale {
            app_id: app("org.legeos.OldTray"),
        };
        assert_eq!(
            explain(ProviderRole::NetworkStatusUi, &resolution),
            "org.legeos.OldTray was selected for network-status-ui, but no longer declares it (uninstalled, or its declaration was withdrawn)."
        );
    }

    #[test]
    fn no_provider_names_the_role() {
        assert_eq!(
            explain(ProviderRole::PowerManager, &Resolution::NoProvider),
            "No application declares power-manager."
        );
    }

    #[test]
    fn provider_app_id_matches_provider_registry_semantics() {
        assert_eq!(
            Resolution::Selected {
                app_id: app("org.legeos.A")
            }
            .provider_app_id(),
            Some(&app("org.legeos.A"))
        );
        assert_eq!(
            Resolution::DefaultedByPriority {
                app_id: app("org.legeos.A"),
                competitors: vec![]
            }
            .provider_app_id(),
            Some(&app("org.legeos.A"))
        );
        assert_eq!(
            Resolution::SelectionStale {
                app_id: app("org.legeos.A")
            }
            .provider_app_id(),
            None
        );
        assert_eq!(Resolution::NoProvider.provider_app_id(), None);
    }

    #[test]
    fn resolution_deserializes_from_the_real_wire_shape() {
        let json = r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#;
        let resolution: Resolution = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolution,
            Resolution::Selected {
                app_id: app("org.legeos.NetworkTray")
            }
        );
    }

    struct FakeProviders(Vec<(String, String)>);

    impl ProvidersSource for FakeProviders {
        async fn report(&self) -> Result<Vec<(String, String)>> {
            Ok(self.0.clone())
        }
    }

    struct FailingProviders;

    impl ProvidersSource for FailingProviders {
        async fn report(&self) -> Result<Vec<(String, String)>> {
            anyhow::bail!("no reply from Providers1")
        }
    }

    #[tokio::test]
    async fn render_with_covers_every_role_and_flags_the_missing_ones() {
        let fake = FakeProviders(vec![(
            "network-status-ui".to_owned(),
            r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#.to_owned(),
        )]);
        let rendered = render_with(&fake, None).await;
        assert!(rendered.contains("org.legeos.NetworkTray is selected for network-status-ui."));
        assert!(rendered.contains("power-manager: not reported by Providers1.Report."));
        assert_eq!(rendered.lines().count(), ProviderRole::ALL.len());
    }

    #[tokio::test]
    async fn render_with_can_filter_to_one_role() {
        let fake = FakeProviders(vec![]);
        let rendered = render_with(&fake, Some(ProviderRole::Greeter)).await;
        assert_eq!(rendered, "greeter: not reported by Providers1.Report.");
    }

    #[test]
    fn parse_report_drops_unknown_roles_and_bad_json() {
        let pairs = vec![
            (
                "network-status-ui".to_owned(),
                r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#.to_owned(),
            ),
            ("not-a-real-role".to_owned(), "{}".to_owned()),
            ("power-manager".to_owned(), "not json".to_owned()),
        ];
        let parsed = parse_report(&pairs);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, ProviderRole::NetworkStatusUi);
    }

    #[tokio::test]
    async fn render_with_degrades_on_a_report_failure() {
        let rendered = render_with(&FailingProviders, None).await;
        assert!(rendered.starts_with("service unavailable:"));
        assert!(rendered.contains("no reply from Providers1"));
    }
}
