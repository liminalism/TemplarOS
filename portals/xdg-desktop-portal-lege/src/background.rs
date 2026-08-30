//! `org.freedesktop.impl.portal.Background`.
//!
//! `GetAppState` is real: it combines `AppIdentity1.ListInstances` (the running roster) with
//! `Resources1.Report`'s per-instance `foreground`/`visible` enrichment to classify each
//! application as Background(0)/Running(1)/Active(2). `NotifyBackground` is real for the common
//! case too — an application sessiond already launched was already vetted by the autostart
//! arbiter at login (`crates/lege-session-core::autostart`), so re-litigating that decision here
//! would be theatre; answering Allow immediately for anything we can already see running is the
//! honest reflection of a decision this OS already made. An `app_id` we cannot vouch for falls
//! back to the [`BackgroundUi`] seam, which — with no toolkit yet — refuses rather than silently
//! granting background permission to an unknown application. `RunningApplicationsChanged` is
//! served by a real periodic poll-and-diff task ([`spawn_change_poller`]): `AppIdentity1` has no
//! changed-signal of its own for this backend to piggyback on, so polling is the honest choice
//! here rather than a signal that silently never fires.

use crate::{
    clients::AppStateClient,
    request::{owned_value, response, with_request},
};
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc, time::Duration};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// `GetAppState`'s per-application values.
pub mod app_state {
    pub const BACKGROUND: u32 = 0;
    pub const RUNNING: u32 = 1;
    pub const ACTIVE: u32 = 2;
}

/// `NotifyBackground`'s `result` values.
pub mod notify_result {
    pub const FORBID: u32 = 0;
    pub const ALLOW: u32 = 1;
    #[allow(dead_code)]
    pub const ALLOW_INSTANCE: u32 = 2;
}

/// The interactive consent prompt no toolkit exists to provide yet.
#[async_trait]
pub trait BackgroundUi: Send + Sync + std::fmt::Debug {
    async fn ask(&self, app_id: &str, display_name: &str) -> Option<u32>;
}

/// Headless default: an application we cannot already vouch for is not silently trusted.
#[derive(Debug, Default)]
pub struct NoUiAvailable;

#[async_trait]
impl BackgroundUi for NoUiAvailable {
    async fn ask(&self, _app_id: &str, _display_name: &str) -> Option<u32> {
        None
    }
}

/// Pixelkit consent prompt used for applications sessiond cannot already vouch for.
#[derive(Debug, Default)]
pub struct NativeUi;

#[async_trait]
impl BackgroundUi for NativeUi {
    async fn ask(&self, app_id: &str, display_name: &str) -> Option<u32> {
        let title = "Background access".to_owned();
        let message = format!("Allow {display_name} ({app_id}) to keep running in the background?");
        tokio::task::spawn_blocking(move || {
            crate::dialog::choice(title, message, vec!["Forbid".into(), "Allow".into()]).map(
                |choice| {
                    if choice == "Allow" {
                        notify_result::ALLOW
                    } else {
                        notify_result::FORBID
                    }
                },
            )
        })
        .await
        .ok()
        .flatten()
    }
}

/// The one slice of `Resources1.Report`'s JSON this backend needs. `lege_resourced::state::Report`
/// only derives `Serialize` (it is produced by the governor, never consumed back), so this is a
/// deliberately partial, local mirror rather than a dependency on a type that does not exist.
#[derive(serde::Deserialize)]
struct ReportInstance {
    instance_id: String,
    #[serde(default)]
    foreground: bool,
    #[serde(default)]
    visible: bool,
}

#[derive(serde::Deserialize, Default)]
struct ReportSlice {
    #[serde(default)]
    instances: Vec<ReportInstance>,
}

/// Classify every running application from the identity roster plus resource enrichment. An
/// instance the resource report has not (yet) enriched still counts as [`app_state::RUNNING`]:
/// `AppIdentity1` already proves the process exists, even without foreground/visible data.
pub fn classify_app_states(
    instances: &[(String, String, String, String)],
    report_json: &str,
) -> HashMap<String, u32> {
    let report: ReportSlice = serde_json::from_str(report_json).unwrap_or_default();
    let mut states: HashMap<String, u32> = HashMap::new();
    for (instance_id, app_id, _unit, _cgroup) in instances {
        let found = report
            .instances
            .iter()
            .find(|instance| &instance.instance_id == instance_id);
        let state = match found {
            Some(instance) if instance.foreground => app_state::ACTIVE,
            Some(instance) if instance.visible => app_state::RUNNING,
            Some(_) => app_state::BACKGROUND,
            None => app_state::RUNNING,
        };
        let entry = states
            .entry(app_id.clone())
            .or_insert(app_state::BACKGROUND);
        if state > *entry {
            *entry = state;
        }
    }
    states
}

/// `org.freedesktop.impl.portal.Background`.
#[derive(Debug)]
pub struct BackgroundApi {
    pub app_state: Arc<dyn AppStateClient>,
    pub ui: Arc<dyn BackgroundUi>,
}

impl BackgroundApi {
    async fn states(&self) -> HashMap<String, u32> {
        let instances = self.app_state.list_instances().await.unwrap_or_default();
        let report_json = self.app_state.report().await.unwrap_or_default();
        classify_app_states(&instances, &report_json)
    }

    async fn notify(&self, app_id: &str, name: &str) -> (u32, HashMap<String, OwnedValue>) {
        let running = self.states().await.contains_key(app_id);
        let result = if running {
            Some(notify_result::ALLOW)
        } else {
            self.ui.ask(app_id, name).await
        };
        let mut results = HashMap::new();
        results.insert(
            "result".to_owned(),
            owned_value(result.unwrap_or(notify_result::FORBID)),
        );
        (response::SUCCESS, results)
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Background")]
impl BackgroundApi {
    #[zbus(name = "GetAppState")]
    async fn get_app_state(&self) -> HashMap<String, OwnedValue> {
        self.states()
            .await
            .into_iter()
            .map(|(app_id, state)| (app_id, owned_value(state)))
            .collect()
    }

    #[zbus(name = "NotifyBackground")]
    async fn notify_background(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        name: &str,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        with_request(connection, &handle, || self.notify(app_id, name)).await
    }

    /// Deprecated upstream ("new versions of xdg-desktop-portal will not call this method");
    /// LegeOS's own autostart arbiter (`crates/lege-session-core::autostart`,
    /// `Providers1`/`History1` — see `docs/architecture/providers.md`) is the one authority for
    /// what may start with the session, so this never needs to act. The full parameter list and
    /// `&self` receiver mirror the fixed spec signature exactly, not something to redesign here.
    #[zbus(name = "EnableAutostart")]
    #[allow(clippy::unused_self, unused_variables, clippy::needless_pass_by_value)]
    fn enable_autostart(
        &self,
        app_id: &str,
        enable: bool,
        commandline: Vec<String>,
        flags: u32,
    ) -> bool {
        false
    }

    #[zbus(signal, name = "RunningApplicationsChanged")]
    pub async fn running_applications_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
}

/// D-Bus interface name and path, for [`spawn_change_poller`].
pub const INTERFACE: &str = "org.freedesktop.impl.portal.Background";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// Poll [`classify_app_states`] on an interval and emit `RunningApplicationsChanged` whenever the
/// classification differs from the last poll. `AppIdentity1` has no change signal to subscribe to
/// instead, so this is the honest way to keep the signal real rather than silently never firing.
pub async fn spawn_change_poller(
    connection: zbus::Connection,
    app_state: Arc<dyn AppStateClient>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut last: Option<HashMap<String, u32>> = None;
        loop {
            tokio::time::sleep(interval).await;
            let Ok(instances) = app_state.list_instances().await else {
                continue;
            };
            let report_json = app_state.report().await.unwrap_or_default();
            let current = classify_app_states(&instances, &report_json);
            if last.as_ref() != Some(&current) {
                if let Err(error) = connection
                    .emit_signal(
                        None::<&str>,
                        PATH,
                        INTERFACE,
                        "RunningApplicationsChanged",
                        &(),
                    )
                    .await
                {
                    tracing::debug!(%error, "failed to emit RunningApplicationsChanged");
                }
                last = Some(current);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_json(entries: &[(&str, bool, bool)]) -> String {
        let instances: Vec<serde_json::Value> = entries
            .iter()
            .map(|(id, foreground, visible)| {
                serde_json::json!({
                    "instance_id": id,
                    "app_id": "unused",
                    "memory_current_bytes": 0,
                    "memory_growth_bytes_per_second": 0,
                    "full_psi_avg10": 0.0,
                    "foreground": foreground,
                    "visible": visible,
                    "protected": false,
                    "disposable": false,
                    "has_unsaved_work": false,
                    "victim_score": 0.0,
                    "score_breakdown": {
                        "memory_term": 0.0, "growth_term": 0.0, "psi_term": 0.0,
                        "disposable_term": 0.0, "visible_term": 0.0, "foreground_term": 0.0,
                        "unsaved_term": 0.0, "total": 0.0
                    }
                })
            })
            .collect();
        serde_json::json!({
            "level": "normal", "stage": "observe-only", "uptime_seconds": 0,
            "mem_available_bytes": 0,
            "global_memory_psi": {"some": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_us":0},
                                    "full": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_us":0}},
            "global_io_psi": {"some": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_us":0},
                                "full": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_us":0}},
            "instances": instances
        })
        .to_string()
    }

    fn instance_id() -> String {
        lege_types::InstanceId::new().to_string()
    }

    #[test]
    fn active_beats_running_beats_background() {
        let active = instance_id();
        let background = instance_id();
        let instances = vec![
            (
                active.clone(),
                "org.legeos.Active".into(),
                String::new(),
                String::new(),
            ),
            (
                background.clone(),
                "org.legeos.Quiet".into(),
                String::new(),
                String::new(),
            ),
        ];
        let report = report_json(&[(&active, true, true), (&background, false, false)]);
        let states = classify_app_states(&instances, &report);
        assert_eq!(states["org.legeos.Active"], app_state::ACTIVE);
        assert_eq!(states["org.legeos.Quiet"], app_state::BACKGROUND);
    }

    #[test]
    fn an_instance_missing_from_the_report_still_counts_as_running() {
        let id = instance_id();
        let instances = vec![(
            id,
            "org.legeos.Unknown".into(),
            String::new(),
            String::new(),
        )];
        let states = classify_app_states(&instances, "{}");
        assert_eq!(states["org.legeos.Unknown"], app_state::RUNNING);
    }

    #[derive(Debug, Default)]
    struct FakeAppState {
        instances: Vec<(String, String, String, String)>,
        report: String,
    }

    #[async_trait]
    impl AppStateClient for FakeAppState {
        async fn list_instances(&self) -> anyhow::Result<Vec<(String, String, String, String)>> {
            Ok(self.instances.clone())
        }
        async fn report(&self) -> anyhow::Result<String> {
            Ok(self.report.clone())
        }
    }

    #[tokio::test]
    async fn a_currently_running_app_is_allowed_without_asking() {
        let id = instance_id();
        let api = BackgroundApi {
            app_state: Arc::new(FakeAppState {
                instances: vec![(
                    id.clone(),
                    "org.legeos.Sync".into(),
                    String::new(),
                    String::new(),
                )],
                report: report_json(&[(&id, false, false)]),
            }),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api.notify("org.legeos.Sync", "Sync").await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            u32::try_from(results["result"].clone()).unwrap(),
            notify_result::ALLOW
        );
    }

    #[tokio::test]
    async fn an_unknown_app_with_no_ui_is_forbidden_not_silently_granted() {
        let api = BackgroundApi {
            app_state: Arc::new(FakeAppState::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api.notify("org.example.Stranger", "Stranger").await;
        assert_eq!(code, response::SUCCESS);
        assert_eq!(
            u32::try_from(results["result"].clone()).unwrap(),
            notify_result::FORBID
        );
    }

    #[test]
    fn enable_autostart_is_inert() {
        let api = BackgroundApi {
            app_state: Arc::new(FakeAppState::default()),
            ui: Arc::new(NoUiAvailable),
        };
        assert!(!api.enable_autostart("org.example.App", true, Vec::new(), 0));
    }
}
