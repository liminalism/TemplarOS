//! Background permissions page: "every running background application and why it is running"
//! (`docs/architecture/providers.md`).
//!
//! **No dedicated interface exists for this.** `interfaces/dbus/` has no `Background1` or
//! similar — autostart approvals are internal to `lege-sessiond`
//! (`crates/lege-session-core/src/autostart.rs`), never exposed over the bus. This page
//! reconstructs the honest answer from three interfaces that *do* exist:
//!
//! * `AppIdentity1.ListInstances()` — who is running right now.
//! * `History1.Recent()` — cross-referenced by `app_id` against `autostart-decision` events,
//!   whose `message` field already reads like a sentence explaining the decision.
//! * `Providers1.Report()` — an instance whose `app_id` is the resolved provider for some role
//!   is running *because* it is that role's provider, independent of any autostart event.
//!
//! An instance can match both, neither, or several roles; every match found is reported, and a
//! match found is never invented — an instance with no match says so plainly rather than
//! guessing. See this module's doc comment in the final report for what a purpose-built
//! interface would need to expose instead.

use crate::history::{Event, EventKind, HistorySource};
use crate::providers::{self, ProvidersSource, Resolution};
use anyhow::{Context, Result};
use lege_types::ProviderRole;

/// How many recent history events to scan for an `autostart-decision` naming this app. Autostart
/// decisions are made once per login, so this is generous enough to cover a normal session, but
/// it is a scan of *recent* history, not "since this instance started" — `History1` has no
/// "events for this app since boot" query, which is exactly the gap a dedicated interface would
/// close (see this module's doc comment).
const AUTOSTART_LOOKBACK: u32 = 500;

/// One running instance and everything this page could find to explain it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningApp {
    pub(crate) instance_id: String,
    pub(crate) app_id: String,
    /// Roles for which this app is the currently resolved provider.
    pub(crate) provider_roles: Vec<ProviderRole>,
    /// The most recent `autostart-decision` event's message naming this app, if any.
    pub(crate) autostart_reason: Option<String>,
}

impl RunningApp {
    pub(crate) fn has_a_reason(&self) -> bool {
        !self.provider_roles.is_empty() || self.autostart_reason.is_some()
    }
}

/// Cross-reference running instances against provider resolutions and autostart-decision
/// history. Pure and unit-testable against literal fixtures — no D-Bus call.
pub(crate) fn cross_reference(
    instances: &[(String, String, String, String)],
    autostart_events: &[Event],
    providers: &[(ProviderRole, Resolution)],
) -> Vec<RunningApp> {
    instances
        .iter()
        .map(|(instance_id, app_id, _unit, _cgroup)| {
            let provider_roles = providers
                .iter()
                .filter(|(_, resolution)| {
                    resolution
                        .provider_app_id()
                        .is_some_and(|provider| provider.as_str() == app_id)
                })
                .map(|(role, _)| *role)
                .collect();

            // `Recent` is newest-first, so the first match is the most recent decision.
            let autostart_reason = autostart_events
                .iter()
                .find(|event| {
                    event.kind == EventKind::AutostartDecision
                        && event
                            .app_id
                            .as_ref()
                            .is_some_and(|event_app| event_app.as_str() == app_id)
                })
                .map(|event| event.message.clone());

            RunningApp {
                instance_id: instance_id.clone(),
                app_id: app_id.clone(),
                provider_roles,
                autostart_reason,
            }
        })
        .collect()
}

fn render_running_apps(apps: &[RunningApp]) -> String {
    if apps.is_empty() {
        return "No application instances are running.".to_owned();
    }
    apps.iter()
        .map(|app| {
            let mut reasons = Vec::new();
            if !app.has_a_reason() {
                reasons.push(
                    "no recorded reason found (not a role provider, and no matching autostart-decision in recent history — likely launched directly)."
                        .to_owned(),
                );
            }
            if !app.provider_roles.is_empty() {
                let roles = app
                    .provider_roles
                    .iter()
                    .copied()
                    .map(ProviderRole::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                reasons.push(format!("selected provider for: {roles}"));
            }
            if let Some(reason) = &app.autostart_reason {
                reasons.push(format!("autostart: {reason}"));
            }
            format!("{} [{}]\n  {}", app.app_id, app.instance_id, reasons.join("\n  "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The bus operation this page needs beyond [`HistorySource`] and [`ProvidersSource`].
pub(crate) trait AppIdentitySource {
    async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>>;
}

impl AppIdentitySource for lege_client::proxies::AppIdentityProxy<'_> {
    async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>> {
        self.list_instances()
            .await
            .context("AppIdentity1.ListInstances")
    }
}

pub(crate) async fn render_with<A, H, P>(app_identity: &A, history: &H, providers: &P) -> String
where
    A: AppIdentitySource,
    H: HistorySource,
    P: ProvidersSource,
{
    let instances = match app_identity.list_instances().await {
        Ok(instances) => instances,
        Err(error) => return format!("service unavailable: {error:#}"),
    };

    let raw_events = match history.recent(AUTOSTART_LOOKBACK).await {
        Ok(events) => events,
        Err(error) => {
            return format!(
                "service unavailable: could not cross-reference autostart history: {error:#}"
            );
        }
    };
    let events: Vec<Event> = raw_events
        .iter()
        .filter_map(|raw| serde_json::from_str(raw).ok())
        .collect();

    let report = match providers.report().await {
        Ok(report) => report,
        Err(error) => {
            return format!(
                "service unavailable: could not cross-reference provider roles: {error:#}"
            );
        }
    };
    let parsed_report = providers::parse_report(&report);

    render_running_apps(&cross_reference(&instances, &events, &parsed_report))
}

pub(crate) async fn render(connection: &zbus::Connection) -> String {
    let app_identity = match lege_client::proxies::AppIdentityProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => {
            return format!("service unavailable: could not reach AppIdentity1 ({error})");
        }
    };
    let history = match lege_client::proxies::HistoryProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return format!("service unavailable: could not reach History1 ({error})"),
    };
    let providers = match lege_client::proxies::ProvidersProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return format!("service unavailable: could not reach Providers1 ({error})"),
    };
    render_with(&app_identity, &history, &providers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, app_id: &str, message: &str) -> Event {
        serde_json::from_value(serde_json::json!({
            "id": "00000000-0000-4000-8000-000000000000",
            "at_ms": 1,
            "kind": kind.as_str(),
            "app_id": app_id,
            "instance_id": null,
            "message": message,
            "cause": null,
            "actions": [],
            "reversible": false,
        }))
        .unwrap()
    }

    #[test]
    fn a_role_provider_is_explained_without_any_history_event() {
        let instances = vec![(
            "instance-1".to_owned(),
            "org.legeos.NetworkTray".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )];
        let providers = vec![(
            ProviderRole::NetworkStatusUi,
            Resolution::Selected {
                app_id: "org.legeos.NetworkTray".parse().unwrap(),
            },
        )];
        let apps = cross_reference(&instances, &[], &providers);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].provider_roles, vec![ProviderRole::NetworkStatusUi]);
        assert!(apps[0].autostart_reason.is_none());
        assert!(apps[0].has_a_reason());
    }

    #[test]
    fn an_autostart_decision_is_matched_by_app_id() {
        let instances = vec![(
            "instance-1".to_owned(),
            "org.example.Sync".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )];
        let events = vec![event(
            EventKind::AutostartDecision,
            "org.example.Sync",
            "org.example.Sync was allowed to autostart (continuous-background approved).",
        )];
        let apps = cross_reference(&instances, &events, &[]);
        assert_eq!(apps.len(), 1);
        assert!(apps[0].provider_roles.is_empty());
        assert_eq!(
            apps[0].autostart_reason.as_deref(),
            Some("org.example.Sync was allowed to autostart (continuous-background approved).")
        );
    }

    #[test]
    fn the_most_recent_matching_event_wins() {
        let instances = vec![(
            "instance-1".to_owned(),
            "org.example.Sync".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )];
        // Recent() is newest-first, so the first element is the latest decision.
        let events = vec![
            event(
                EventKind::AutostartDecision,
                "org.example.Sync",
                "second decision",
            ),
            event(
                EventKind::AutostartDecision,
                "org.example.Sync",
                "first decision",
            ),
        ];
        let apps = cross_reference(&instances, &events, &[]);
        assert_eq!(apps[0].autostart_reason.as_deref(), Some("second decision"));
    }

    #[test]
    fn a_non_autostart_event_for_the_same_app_is_ignored() {
        let instances = vec![(
            "instance-1".to_owned(),
            "org.example.Sync".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )];
        let events = vec![event(
            EventKind::SettingChange,
            "org.example.Sync",
            "unrelated",
        )];
        let apps = cross_reference(&instances, &events, &[]);
        assert!(apps[0].autostart_reason.is_none());
    }

    #[test]
    fn an_instance_with_no_match_says_so() {
        let instances = vec![(
            "instance-1".to_owned(),
            "org.example.Mystery".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )];
        let apps = cross_reference(&instances, &[], &[]);
        assert!(!apps[0].has_a_reason());
        let rendered = render_running_apps(&apps);
        assert!(rendered.contains("no recorded reason found"));
    }

    struct FakeAppIdentity(Vec<(String, String, String, String)>);
    impl AppIdentitySource for FakeAppIdentity {
        async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>> {
            Ok(self.0.clone())
        }
    }

    struct FakeHistory(Vec<String>);
    impl HistorySource for FakeHistory {
        async fn recent(&self, _limit: u32) -> Result<Vec<String>> {
            Ok(self.0.clone())
        }
        async fn search(&self, _query: &str, _limit: u32) -> Result<Vec<String>> {
            Ok(self.0.clone())
        }
        async fn undo(&self, _event_id: &str) -> Result<bool> {
            Ok(false)
        }
    }

    struct FakeProviders(Vec<(String, String)>);
    impl ProvidersSource for FakeProviders {
        async fn report(&self) -> Result<Vec<(String, String)>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn render_with_combines_all_three_sources() {
        let app_identity = FakeAppIdentity(vec![(
            "instance-1".to_owned(),
            "org.legeos.NetworkTray".to_owned(),
            "unit".to_owned(),
            "cgroup".to_owned(),
        )]);
        let history = FakeHistory(vec![]);
        let providers = FakeProviders(vec![(
            "network-status-ui".to_owned(),
            r#"{"kind":"selected","app_id":"org.legeos.NetworkTray"}"#.to_owned(),
        )]);
        let rendered = render_with(&app_identity, &history, &providers).await;
        assert!(rendered.contains("org.legeos.NetworkTray"));
        assert!(rendered.contains("selected provider for: network-status-ui"));
    }

    #[tokio::test]
    async fn render_with_degrades_when_app_identity_is_unavailable() {
        struct Failing;
        impl AppIdentitySource for Failing {
            async fn list_instances(&self) -> Result<Vec<(String, String, String, String)>> {
                anyhow::bail!("no reply from AppIdentity1")
            }
        }
        let rendered = render_with(&Failing, &FakeHistory(vec![]), &FakeProviders(vec![])).await;
        assert!(rendered.starts_with("service unavailable:"));
    }
}
