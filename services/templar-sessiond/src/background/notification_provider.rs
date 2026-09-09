//! Claims (or releases) `org.freedesktop.Notifications` and serves (or unserves)
//! [`NotificationsApi`] depending on whether sessiond is currently the selected
//! `notification-server` provider (`docs/architecture/history.md`).
//!
//! sessiond declares itself a `notification-server` candidate at distribution priority (100) at
//! startup (see [`crate::apis::providers`] wiring in `main.rs`), so a fresh install has a
//! working notification server without any user action, while a third-party notification daemon
//! can still declare a higher priority or be explicitly selected instead.

use crate::apis::notifications::NotificationsApi;
use parking_lot::RwLock;
use std::{str::FromStr, sync::Arc, time::Duration};
use templar_appd::Registry;
use templar_session_core::{history::SqliteHistory, providers::ProviderRegistry};
use templar_state::Store;
use templar_types::{AppId, ProviderRole};

/// The `AppId` sessiond declares itself under for the `notification-server` role.
pub fn sessiond_app_id() -> AppId {
    AppId::from_str("org.templaros.Sessiond").expect("static app id is valid")
}

const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";

/// Poll `providers` for whether sessiond is the selected `notification-server` and claim/release
/// the name accordingly. Polling (rather than reacting to `Providers1.Changed`) keeps this task
/// independent of the D-Bus dispatch path and simple to reason about; a selection change takes
/// effect within one `interval`.
pub async fn run(
    connection: zbus::Connection,
    providers: Arc<RwLock<ProviderRegistry>>,
    store: Store,
    history: Arc<SqliteHistory>,
    registry: Arc<Registry>,
    interval: Duration,
) {
    let sessiond_app_id = sessiond_app_id();
    let mut hosting = false;
    loop {
        let selected = providers
            .read()
            .provider(ProviderRole::NotificationServer)
            .as_ref()
            == Some(&sessiond_app_id);

        if selected && !hosting {
            let api =
                NotificationsApi::new(store.clone(), Arc::clone(&history), Arc::clone(&registry));
            match connection.object_server().at(NOTIFICATIONS_PATH, api).await {
                Ok(_) => match connection.request_name(NOTIFICATIONS_NAME).await {
                    Ok(()) => {
                        hosting = true;
                        tracing::info!("now hosting org.freedesktop.Notifications");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to claim org.freedesktop.Notifications");
                        let _ = connection
                            .object_server()
                            .remove::<NotificationsApi, _>(NOTIFICATIONS_PATH)
                            .await;
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "failed to serve org.freedesktop.Notifications");
                }
            }
        } else if !selected && hosting {
            let _ = connection.release_name(NOTIFICATIONS_NAME).await;
            let _ = connection
                .object_server()
                .remove::<NotificationsApi, _>(NOTIFICATIONS_PATH)
                .await;
            hosting = false;
            tracing::info!("no longer hosting org.freedesktop.Notifications");
        }

        tokio::time::sleep(interval).await;
    }
}
