//! Displays1 exposes native observations; preview deadlines and rollback stay in the compositor.
use crate::apis::failed;
use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc, time::Duration};
use templar_client::compositor_control::CompositorControl;
use templar_state::Store;
use templar_types::{
    compositor_control::{ControlRequest, ControlResponse, MAX_CONTROL_FRAME},
    display::{self, DisplayId, DisplayObservation, DisplayProfile},
    settings::Revision,
};
use uuid::Uuid;
use zbus::fdo;

pub const PATH: &str = "/org/templaros/Session/Displays";
const KEY: &str = "_internal.display.confirmed-profiles";
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedProfile {
    profile: DisplayProfile,
    panel_closed: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences {
    version: u32,
    profiles: Vec<ConfirmedProfile>,
}
#[derive(Clone, Debug)]
pub struct DisplaysApi {
    control: CompositorControl,
    store: Store,
    writes: Arc<tokio::sync::Mutex<()>>,
}
impl DisplaysApi {
    pub fn new(store: Store) -> Result<Self> {
        let api = Self {
            control: CompositorControl::from_runtime()?,
            store,
            writes: Arc::new(tokio::sync::Mutex::new(())),
        };
        let _ = api.preferences()?;
        Ok(api)
    }
    fn preferences(&self) -> Result<Preferences> {
        let json: Option<String> = self
            .store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key=?1",
                [KEY],
                |row| row.get(0),
            )
            .optional()?;
        let preferences = match json {
            Some(json) => serde_json::from_str(&json)?,
            None => Preferences {
                version: 1,
                profiles: Vec::new(),
            },
        };
        if preferences.version != 1 {
            bail!("unsupported confirmed display preference version");
        }
        Ok(preferences)
    }
    fn save_preferences(&self, preferences: &Preferences) -> Result<()> {
        let json = serde_json::to_string(preferences)?;
        self.store.transaction(|tx| {
            tx.execute("INSERT INTO settings_values(key,value_json,updated_ms) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json,updated_ms=excluded.updated_ms", rusqlite::params![KEY, json, templar_state::now_ms()])?;
            Ok(())
        })
    }
    async fn request(&self, request: ControlRequest) -> Result<DisplayObservation> {
        match self.control.request(request).await? {
            ControlResponse::Displays(observation) => Ok(observation),
            _ => bail!("the compositor returned a different observation for a display operation"),
        }
    }
    async fn actual(&self) -> Result<DisplayObservation> {
        self.request(ControlRequest::Displays).await
    }
    async fn restore_confirmed(&self) -> Result<()> {
        let _write = self.writes.lock().await;
        let observation = self.actual().await?;
        if !observation.session_active || observation.preview.is_some() {
            return Ok(());
        }
        if observation
            .displays
            .iter()
            .filter(|display| display.connected)
            .any(|display| {
                !matches!(
                    display.persistence,
                    templar_types::settings::Capability::Available { .. }
                )
            })
        {
            return Ok(());
        }
        let Some(saved) = self.preferences()?.profiles.into_iter().find(|saved| {
            saved.panel_closed == observation.panel_closed
                && display::matches_connected_inventory(&saved.profile, &observation.displays)
        }) else {
            return Ok(());
        };
        // This is system restoration of a previously confirmed profile, not an interactive UI
        // change. It may run while locked; the compositor still queues protected scenes and
        // completes only after exact physical generation presentation plus actual KMS readback.
        display::validate_inventory(
            observation.revision,
            &observation.displays,
            observation.session_active,
            false,
            observation.revision,
            &saved.profile,
        )?;
        if saved.profile == observation.profile {
            return Ok(());
        }
        let target = saved.profile;
        let applied = self
            .request(ControlRequest::RestoreConfirmedDisplays {
                expected: observation.revision,
                profile: target.clone(),
            })
            .await?;
        if applied.profile != target || applied.preview.is_some() {
            bail!("confirmed display restoration did not complete with exact native readback");
        }
        Ok(())
    }
}
fn decode<T: serde::de::DeserializeOwned>(json: &str) -> fdo::Result<T> {
    if json.len() > MAX_CONTROL_FRAME {
        return Err(fdo::Error::InvalidArgs(
            "display argument exceeds the bounded compositor frame".into(),
        ));
    }
    serde_json::from_str(json).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
}
fn token(value: &str) -> fdo::Result<Uuid> {
    value
        .parse()
        .map_err(|error: uuid::Error| fdo::Error::InvalidArgs(error.to_string()))
}
#[zbus::interface(name = "org.templaros.Displays1")]
impl DisplaysApi {
    async fn observe(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.actual().await.map_err(failed)?).map_err(failed)
    }
    async fn validate(&self, expected: &str, profile: &str) -> fdo::Result<String> {
        let observation = self
            .request(ControlRequest::ValidateDisplays {
                expected: decode(expected)?,
                profile: decode(profile)?,
            })
            .await
            .map_err(failed)?;
        serde_json::to_string(&observation).map_err(failed)
    }
    async fn preview(
        &self,
        expected: &str,
        profile: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let _write = self.writes.lock().await;
        let observation = self
            .request(ControlRequest::PreviewDisplays {
                expected: decode(expected)?,
                profile: decode(profile)?,
            })
            .await
            .map_err(failed)?;
        let _ = Self::changed(&emitter).await;
        serde_json::to_string(&observation).map_err(failed)
    }
    async fn confirm(
        &self,
        expected: &str,
        preview_token: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let _write = self.writes.lock().await;
        let mut preferences = self.preferences().map_err(failed)?;
        let observation = self
            .request(ControlRequest::ConfirmDisplays {
                expected: decode(expected)?,
                token: token(preview_token)?,
            })
            .await
            .map_err(failed)?;
        if observation.preview.is_some() {
            return Err(failed(
                "the compositor did not confirm this display generation",
            ));
        }
        if observation
            .displays
            .iter()
            .filter(|display| display.connected)
            .any(|display| {
                !matches!(
                    display.persistence,
                    templar_types::settings::Capability::Available { .. }
                )
            })
        {
            let _ = Self::changed(&emitter).await;
            return Err(failed(
                "The physical generation was confirmed, but this topology has an unstable monitor identity and cannot be persisted",
            ));
        }
        let json = serde_json::to_string(&observation).map_err(failed)?;
        preferences.profiles.retain(|saved| {
            saved.panel_closed != observation.panel_closed
                || !display::matches_connected_inventory(&saved.profile, &observation.displays)
        });
        preferences.profiles.push(ConfirmedProfile {
            profile: observation.profile,
            panel_closed: observation.panel_closed,
        });
        self.save_preferences(&preferences).map_err(|error| {
            failed(format!(
                "physical display generation confirmed, but durable profile commit failed: {error}"
            ))
        })?;
        let _ = Self::changed(&emitter).await;
        Ok(json)
    }
    async fn revert(
        &self,
        preview_token: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        let _write = self.writes.lock().await;
        let observation = self
            .request(ControlRequest::RevertDisplays {
                token: token(preview_token)?,
            })
            .await
            .map_err(failed)?;
        let _ = Self::changed(&emitter).await;
        serde_json::to_string(&observation).map_err(failed)
    }
    async fn identify(
        &self,
        expected: &str,
        outputs: &str,
        calibration: bool,
    ) -> fdo::Result<String> {
        let outputs: Vec<DisplayId> = decode(outputs)?;
        let observation = self
            .request(ControlRequest::Identify {
                expected: decode(expected)?,
                outputs,
                calibration,
            })
            .await
            .map_err(failed)?;
        serde_json::to_string(&observation).map_err(failed)
    }
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}
pub fn spawn_watch(connection: zbus::Connection) {
    tokio::spawn(async move {
        // Losing a working stream is one transition, however many reconnects it then refuses:
        // signalling every retry woke every consumer every two seconds for a whole session.
        let mut down = false;
        let mut delay = Duration::from_secs(2);
        loop {
            let mut connected = false;
            if let Err(error) = watch_once(&connection, &mut connected).await {
                if connected {
                    down = false;
                    delay = Duration::from_secs(2);
                }
                if down {
                    tracing::debug!(%error, "native display observation still unavailable");
                } else {
                    down = true;
                    tracing::warn!(%error, "native display observation disconnected");
                    if let Ok(interface) = connection
                        .object_server()
                        .interface::<_, DisplaysApi>(PATH)
                        .await
                    {
                        let _ = DisplaysApi::changed(interface.signal_emitter()).await;
                    }
                }
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    });
}
async fn watch_once(connection: &zbus::Connection, connected: &mut bool) -> Result<()> {
    let interface = connection
        .object_server()
        .interface::<_, DisplaysApi>(PATH)
        .await?;
    let api = interface.get().await.clone();
    let mut stream = api.control.subscribe_activity().await?;
    *connected = true;
    let mut previous_source = None;
    let mut previous_topology = None;
    while let Some(activity) = stream.next().await {
        let activity = activity?;
        let source = (
            activity.display_revision,
            activity.session_active,
            activity.locked,
        );
        if previous_source == Some(source) {
            continue;
        }
        let observation = api.actual().await?;
        let topology: BTreeSet<_> = observation
            .displays
            .iter()
            .filter(|display| display.connected)
            .map(|display| display.id.clone())
            .collect();
        let recovered =
            previous_source.is_none_or(|(revision, active, locked): (Revision, bool, bool)| {
                revision.owner_epoch != activity.display_revision.owner_epoch
                    || !active && activity.session_active
                    || locked && !activity.locked
            });
        let changed_topology =
            previous_topology
                .as_ref()
                .is_none_or(|(old, closed): &(BTreeSet<DisplayId>, bool)| {
                    old != &topology || *closed != observation.panel_closed
                });
        // Owned preview/presentation revisions are not reconnect events. Update before restore
        // so a real hardware failure cannot feed its own setter changes into endless retries.
        previous_source = Some(source);
        previous_topology = Some((topology, observation.panel_closed));
        if (recovered || changed_topology)
            && let Err(error) = api.restore_confirmed().await
        {
            tracing::warn!(%error, "confirmed native display profile could not be restored");
        }
        let _ = DisplaysApi::changed(interface.signal_emitter()).await;
    }
    bail!("native compositor activity stream ended")
}
