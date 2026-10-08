//! Stored profiles and the single session policy owner. Inspect separates storage from effects.
use crate::apis::{failed, lifecycle::SessionEdges};
use anyhow::{Context, Result, ensure};
use rusqlite::OptionalExtension;
use std::{sync::Arc, time::Duration};
use templar_session_core::power::{PowerProfile, PowerSource};
use templar_state::Store;
use zbus::fdo;

#[path = "power_runtime.rs"]
mod runtime;
pub const PATH: &str = "/org/templaros/Session/Power";
const OVERRIDE_KEY_PREFIX: &str = "_internal.power.profile.";

pub struct PowerApi {
    store: Store,
    commands: Arc<tokio::sync::mpsc::Sender<runtime::Command>>,
    task: tokio::task::AbortHandle,
}
impl Drop for PowerApi { fn drop(&mut self) { self.task.abort(); } }
impl PowerApi {
    pub async fn new(store: Store, session: zbus::Connection, system: Option<zbus::Connection>, edges: SessionEdges, sleep_ready: tokio::sync::watch::Receiver<bool>) -> Self {
        let (commands, task) = runtime::start(store.clone(), session, system, edges, sleep_ready).await;
        Self { store, commands, task }
    }
    async fn change(&self, source: PowerSource, profile: Option<PowerProfile>) -> fdo::Result<()> {
        let (reply, received) = tokio::sync::oneshot::channel();
        let operation = async {
            self.commands.send(runtime::Command::Change { source, profile, reply }).await.context("power policy owner stopped")?;
            received.await.context("power policy reply owner stopped")?
        };
        tokio::time::timeout(Duration::from_secs(45), operation).await.map_err(|_| failed("power policy change timed out; inspect stored and observed values before retrying"))?.map_err(failed)
    }
}
fn parse_source(source: &str) -> fdo::Result<PowerSource> {
    match source {
        "battery" => Ok(PowerSource::Battery), "mains" => Ok(PowerSource::Mains),
        _ => Err(fdo::Error::InvalidArgs("power source must be battery or mains".into())),
    }
}
fn read_profile(store: &Store, source: PowerSource) -> Result<PowerProfile> {
    let key = format!("{OVERRIDE_KEY_PREFIX}{}", source.as_str());
    let stored: Option<String> = store.lock().query_row("SELECT value_json FROM settings_values WHERE key = ?1", [&key], |row| row.get(0)).optional()?;
    let profile = match stored { Some(json) => serde_json::from_str(&json).context("stored power profile is invalid; reset this source explicitly")?, None => PowerProfile::default_for(source) };
    validate_profile(&profile)?;
    Ok(profile)
}
fn validate_profile(profile: &PowerProfile) -> Result<()> {
    for interval in [profile.dim_after, profile.screen_off_after, profile.lock_after, profile.suspend_after].into_iter().flatten() {
        ensure!(u64::try_from(interval.as_millis()).is_ok(), "power interval exceeds the native millisecond range");
    }
    Ok(())
}
fn store_profile(store: &Store, source: PowerSource, profile: Option<&PowerProfile>) -> Result<()> {
    let key = format!("{OVERRIDE_KEY_PREFIX}{}", source.as_str());
    let json = profile.map(serde_json::to_string).transpose()?;
    store.transaction(|tx| {
        if let Some(json) = &json {
            tx.execute("INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3) ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms", rusqlite::params![key, json, templar_state::now_ms()])?;
        } else { tx.execute("DELETE FROM settings_values WHERE key = ?1", [&key])?; }
        Ok(())
    })
}
#[zbus::interface(name = "org.templaros.Power1")]
impl PowerApi {
    async fn get_profile(&self, source: &str) -> fdo::Result<String> {
        serde_json::to_string(&read_profile(&self.store, parse_source(source)?).map_err(failed)?).map_err(failed)
    }
    /// A selected-source change reconciles actual owners; inactive-source edits remain stored.
    async fn set_profile(&self, source: &str, profile_json: &str) -> fdo::Result<()> {
        let source = parse_source(source)?;
        let profile: PowerProfile = serde_json::from_str(profile_json).map_err(|error| fdo::Error::InvalidArgs(format!("invalid power profile: {error}")))?;
        validate_profile(&profile).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        self.change(source, Some(profile)).await
    }
    async fn reset_profile(&self, source: &str) -> fdo::Result<()> { self.change(parse_source(source)?, None).await }
    async fn inspect(&self) -> fdo::Result<String> {
        let (reply, received) = tokio::sync::oneshot::channel();
        let operation = async {
            self.commands.send(runtime::Command::Inspect(reply)).await.context("power policy owner stopped")?;
            received.await.context("power inspection owner stopped")?
        };
        tokio::time::timeout(Duration::from_secs(45), operation).await.map_err(|_| failed("power inspection timed out; no applied state is confirmed"))?.map_err(failed)
    }
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>, revision_json: &str) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profiles_store_reset_and_remain_source_independent() {
        let store = Store::in_memory().unwrap();
        let mains = read_profile(&store, PowerSource::Mains).unwrap();
        let mut battery = read_profile(&store, PowerSource::Battery).unwrap();
        battery.lock_after = Some(Duration::from_secs(42));
        battery.lid_closed = templar_session_core::power::LidAction::Lock;
        battery.lid_panel = templar_session_core::power::PanelPolicy::ExternalOnly;
        store_profile(&store, PowerSource::Battery, Some(&battery)).unwrap();
        assert_eq!(read_profile(&store, PowerSource::Battery).unwrap(), battery);
        assert_eq!(read_profile(&store, PowerSource::Mains).unwrap(), mains);
        store_profile(&store, PowerSource::Battery, None).unwrap();
        assert_eq!(read_profile(&store, PowerSource::Battery).unwrap(), PowerProfile::default_for(PowerSource::Battery));
    }
    #[test]
    fn bad_stored_profiles_do_not_become_successful_defaults() {
        let store = Store::in_memory().unwrap();
        store.lock().execute("INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, 0)",
            rusqlite::params!["_internal.power.profile.battery", "{\"broken\":true}"]).unwrap();
        assert!(read_profile(&store, PowerSource::Battery).is_err());
        store_profile(&store, PowerSource::Battery, None).unwrap();
        assert!(read_profile(&store, PowerSource::Battery).is_ok());
        let mut invalid = PowerProfile::default_for(PowerSource::Battery);
        invalid.suspend_after = Some(Duration::from_secs(u64::MAX));
        assert!(validate_profile(&invalid).is_err());
    }
}
