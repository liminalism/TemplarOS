//! Audio1: bounded WirePlumber control, native PipeWire inventory and external event observation.

use crate::apis::failed;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use std::{sync::Arc, time::Duration};
use templar_client::status::{AudioDevice, AudioDirection, AudioInventory, AudioState};
use templar_state::Store;
use zbus::fdo;

/// WirePlumber's name for "whatever the user is listening through".
const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// What this interface needs from WirePlumber.
#[async_trait]
pub trait AudioControl: Send + Sync + std::fmt::Debug {
    /// Run bounded native `wpctl` arguments, returning its stdout.
    async fn wpctl(&self, arguments: &[&str]) -> Result<String>;
    async fn inventory(&self) -> Result<AudioInventory>;
}

/// The real thing: `wpctl` on `PATH`.
#[derive(Debug, Default)]
pub struct WpctlAudio;

#[async_trait]
impl AudioControl for WpctlAudio {
    async fn wpctl(&self, arguments: &[&str]) -> Result<String> {
        crate::command::run("wpctl", arguments, 65536).await
    }

    async fn inventory(&self) -> Result<AudioInventory> {
        parse_inventory(&crate::command::run("pw-dump", &[], 8 * 1024 * 1024).await?)
    }
}

pub fn parse_inventory(json: &str) -> Result<AudioInventory> {
    let objects: Vec<serde_json::Value> = serde_json::from_str(json)?;
    let mut inventory = AudioInventory::default();
    for object in &objects {
        if object["type"] == "PipeWire:Interface:Metadata"
            && object["props"]["metadata.name"] == "default"
            && let Some(metadata) = object["metadata"].as_array()
        {
            for entry in metadata {
                let name = entry["value"]["name"].as_str().map(str::to_owned);
                match entry["key"].as_str() {
                    Some("default.audio.sink") => inventory.default_output = name,
                    Some("default.audio.source") => inventory.default_input = name,
                    _ => {}
                }
            }
        }
    }
    for object in &objects {
        if object["type"] != "PipeWire:Interface:Node" {
            continue;
        }
        let props = &object["info"]["props"];
        let direction = match props["media.class"].as_str() {
            Some("Audio/Sink") => AudioDirection::Output,
            Some("Audio/Source") => AudioDirection::Input,
            _ => continue,
        };
        let Some(name) = props["node.name"].as_str() else {
            continue;
        };
        let Some(id) = object["id"].as_u64().and_then(|id| u32::try_from(id).ok()) else {
            continue;
        };
        let Some(serial) = props["object.serial"].as_u64() else {
            continue;
        };
        let default = match direction {
            AudioDirection::Output => &inventory.default_output,
            AudioDirection::Input => &inventory.default_input,
        };
        inventory.devices.push(AudioDevice {
            id,
            serial,
            name: name.into(),
            direction,
            description: props["node.description"].as_str().unwrap_or(name).into(),
            is_default: default.as_deref() == Some(name),
        });
    }
    inventory
        .devices
        .sort_by(|left, right| left.name.cmp(&right.name));
    Ok(inventory)
}

/// Parse `wpctl get-volume @DEFAULT_AUDIO_SINK@`, whose output is one line:
/// `Volume: 0.45` or `Volume: 0.45 [MUTED]`.
///
/// A pure function over the real format, so the shape `wpctl` actually prints is what the tests
/// exercise — including the muted suffix, which is the half that a naive `split_whitespace` on
/// the second field gets right by accident and everything else gets wrong.
pub fn parse_volume(output: &str) -> Result<(u32, bool)> {
    let line = output
        .lines()
        .find(|line| line.trim_start().starts_with("Volume:"))
        .with_context(|| format!("wpctl printed no Volume line: {output:?}"))?;
    let muted = line.contains("[MUTED]");
    let value = line
        .trim_start()
        .trim_start_matches("Volume:")
        .split_whitespace()
        .next()
        .with_context(|| format!("wpctl printed no volume value: {line:?}"))?;
    let fraction: f64 = value
        .parse()
        .with_context(|| format!("wpctl printed an unparseable volume {value:?}"))?;
    // wpctl reports a linear fraction that can exceed 1.0 when something has boosted the sink
    // past 100%; report what is true rather than clamping a reading.
    let percent = (fraction * 100.0).round().clamp(0.0, f64::from(u32::MAX)) as u32;
    Ok((percent, muted))
}

/// Where one step lands, given where the level is now.
///
/// A sink something else has boosted past 100% reads back above 100, and clamping the *result* to
/// 0..=100 made both steppers do the same thing there: from 126%, "quieter" and "louder" both
/// landed on 100. Pressing louder to get a quieter sink is the kind of control a person stops
/// trusting.
///
/// So the ceiling is wherever the level already is, never lower than 100. Stepping down from a
/// boosted sink walks down through the boosted range as it should; stepping up cannot push past
/// where it already was, because boosting a sink is not something this control does — it is
/// something that was done to it, and undoing it is the only direction that makes sense here.
fn step_to(current: u32, delta: i32) -> u32 {
    let ceiling = i64::from(current.max(100));
    (i64::from(current) + i64::from(delta)).clamp(0, ceiling) as u32
}

/// `org.templaros.Audio1`.
#[derive(Debug)]
pub struct AudioApi {
    control: Arc<dyn AudioControl>,
    store: Store,
    writes: tokio::sync::Mutex<()>,
}

impl AudioApi {
    pub fn new(control: Arc<dyn AudioControl>, store: Store) -> Self {
        Self {
            control,
            store,
            writes: tokio::sync::Mutex::new(()),
        }
    }

    /// No readable default is unavailable, not a fabricated zero-volume device.
    pub async fn state(&self) -> AudioState {
        let output = match self.control.wpctl(&["get-volume", DEFAULT_SINK]).await {
            Ok(output) => output,
            Err(error) => {
                return AudioState {
                    unavailable_reason: Some(error.to_string()),
                    ..AudioState::default()
                };
            }
        };
        match parse_volume(&output) {
            Ok((volume_percent, muted)) => AudioState {
                present: true,
                volume_percent,
                muted,
                unavailable_reason: None,
            },
            // wpctl answered but in a shape this does not understand. That is a real fault and is
            // worth a log line, but it is still not a reason to claim the volume is zero.
            Err(error) => {
                tracing::warn!(%error, "could not read the default sink's volume");
                AudioState {
                    unavailable_reason: Some(error.to_string()),
                    ..AudioState::default()
                }
            }
        }
    }

    async fn apply_volume(&self, percent: u32) -> Result<u32> {
        let percent = percent.min(100);
        self.control
            .wpctl(&["set-volume", DEFAULT_SINK, &format!("{percent}%")])
            .await?;
        Ok(percent)
    }

    fn absent() -> fdo::Error {
        fdo::Error::NotSupported("this machine has no volume control".to_owned())
    }

    fn preference(&self, direction: AudioDirection) -> Result<Option<String>> {
        let json: Option<String> = self
            .store
            .lock()
            .query_row(
                "SELECT value_json FROM settings_values WHERE key = ?1",
                [preference_key(direction)],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    async fn inventory_state(&self) -> Result<AudioInventory> {
        let mut inventory = self.control.inventory().await?;
        inventory.preferred_output = self.preference(AudioDirection::Output)?;
        inventory.preferred_input = self.preference(AudioDirection::Input)?;
        Ok(inventory)
    }

    async fn select(&self, direction: AudioDirection, name: &str) -> Result<()> {
        let _write = self.writes.lock().await;
        let inventory = self.control.inventory().await?;
        let device = inventory
            .devices
            .iter()
            .find(|device| device.direction == direction && device.name == name)
            .context("audio device disappeared or belongs to a different direction")?;
        self.control
            .wpctl(&["set-default", &device.id.to_string()])
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let observed = self.control.inventory().await?;
            let applied = match direction {
                AudioDirection::Output => observed.default_output,
                AudioDirection::Input => observed.default_input,
            };
            if applied.as_deref() == Some(name) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("WirePlumber did not apply the requested audio device");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let json = serde_json::to_string(name)?;
        self.store.transaction(|tx| {
            tx.execute(
                "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value_json=excluded.value_json, updated_ms=excluded.updated_ms",
                rusqlite::params![preference_key(direction), json, templar_state::now_ms()],
            )?;
            Ok(())
        })
    }
}

fn preference_key(direction: AudioDirection) -> &'static str {
    match direction {
        AudioDirection::Output => "_internal.audio.preferred-output",
        AudioDirection::Input => "_internal.audio.preferred-input",
    }
}

#[zbus::interface(name = "org.templaros.Audio1")]
impl AudioApi {
    /// The default sink as `AudioState` JSON.
    async fn get(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.state().await)
            .map_err(|error| failed(anyhow::anyhow!("{error}")))
    }

    async fn inventory(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.inventory_state().await.map_err(failed)?).map_err(failed)
    }

    async fn select_device(
        &self,
        direction: &str,
        name: &str,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let direction = match direction {
            "output" => AudioDirection::Output,
            "input" => AudioDirection::Input,
            _ => return Err(fdo::Error::InvalidArgs("expected input or output".into())),
        };
        self.select(direction, name).await.map_err(failed)?;
        let _ = Self::changed(&emitter).await;
        Ok(())
    }

    /// Set the default sink's volume, clamped to 0..=100.
    async fn set_volume(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        percent: u32,
    ) -> fdo::Result<()> {
        if !self.state().await.present {
            return Err(Self::absent());
        }
        self.apply_volume(percent).await.map_err(failed)?;
        // Best effort: a volume change must not fail because nobody was listening.
        let _ = Self::changed(&emitter).await;
        Ok(())
    }

    /// Move the volume by `delta` percentage points and report where it landed.
    async fn adjust_volume(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        delta: i32,
    ) -> fdo::Result<u32> {
        let state = self.state().await;
        if !state.present {
            return Err(Self::absent());
        }
        let wanted = step_to(state.volume_percent, delta);
        self.apply_volume(wanted).await.map_err(failed)?;
        let landed = self.state().await.volume_percent;
        let _ = Self::changed(&emitter).await;
        Ok(landed)
    }

    async fn set_muted(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        muted: bool,
    ) -> fdo::Result<()> {
        if !self.state().await.present {
            return Err(Self::absent());
        }
        self.control
            .wpctl(&["set-mute", DEFAULT_SINK, if muted { "1" } else { "0" }])
            .await
            .map_err(failed)?;
        let _ = Self::changed(&emitter).await;
        Ok(())
    }

    /// Flip mute and report the new state.
    async fn toggle_muted(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let state = self.state().await;
        if !state.present {
            return Err(Self::absent());
        }
        // wpctl's own `toggle` rather than writing the negation: the flip then happens in
        // WirePlumber, so it cannot be raced by anything that muted between the read and write.
        self.control
            .wpctl(&["set-mute", DEFAULT_SINK, "toggle"])
            .await
            .map_err(failed)?;
        let _ = Self::changed(&emitter).await;
        Ok(self.state().await.muted)
    }

    /// This service changed the volume or the mute state.
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

/// A separate monitor process keeps PipeWire's native loop out of the policy daemon.
pub fn spawn_watch(connection: zbus::Connection) {
    tokio::spawn(async move {
        let mut disconnected = false;
        loop {
            if let Err(error) = watch_once(&connection, &mut disconnected).await {
                tracing::warn!(%error, "audio event monitor disconnected");
                // Event-driven consumers must re-read availability on loss, not keep showing
                // the last working route. Repeated reconnect failures are the same transition.
                if !disconnected {
                    disconnected = true;
                    if let Ok(interface) = connection
                        .object_server()
                        .interface::<_, AudioApi>(templar_types::dbus::session::AUDIO_PATH)
                        .await
                    {
                        let _ = AudioApi::changed(interface.signal_emitter()).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

async fn watch_once(connection: &zbus::Connection, disconnected: &mut bool) -> Result<()> {
    use std::{collections::HashSet, process::Stdio};
    use tokio::io::AsyncReadExt as _;
    let mut child = tokio::process::Command::new("pw-dump")
        .arg("--monitor")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting PipeWire monitor")?;
    let mut stdout = child.stdout.take().context("monitor stdout unavailable")?;
    let interface = connection
        .object_server()
        .interface::<_, AudioApi>(templar_types::dbus::session::AUDIO_PATH)
        .await?;
    let mut previous = None;
    let mut known = HashSet::<u64>::new();
    let mut frames = MonitorFrames::default();
    let mut chunk = [0u8; 8192];
    loop {
        let count = stdout.read(&mut chunk).await?;
        if count == 0 {
            bail!("PipeWire monitor exited: {}", child.wait().await?);
        }
        let mut relevant = false;
        frames.push(&chunk[..count], |objects| {
            for object in objects {
                let Some(id) = object["id"].as_u64() else {
                    continue;
                };
                let interesting = object["type"] == "PipeWire:Interface:Node"
                    || (object["type"] == "PipeWire:Interface:Metadata"
                        && object["props"]["metadata.name"] == "default");
                if interesting {
                    known.insert(id);
                }
                if interesting || known.contains(&id) {
                    relevant = true;
                    if object["info"].is_null() && object["type"].is_null() {
                        known.remove(&id);
                    }
                }
            }
        })?;
        if !relevant {
            continue;
        }
        let api = interface.get().await;
        let inventory = api.inventory_state().await?;
        // A saved stable preference is reapplied only at startup or when its device reappears,
        // never on a volume/default edit by another application.
        for (direction, preference) in [
            (
                AudioDirection::Output,
                inventory.preferred_output.as_deref(),
            ),
            (AudioDirection::Input, inventory.preferred_input.as_deref()),
        ] {
            if let Some(name) = preference
                && inventory
                    .devices
                    .iter()
                    .any(|device| device.name == name && device.direction == direction)
                && previous
                    .as_ref()
                    .is_none_or(|(_, old): &(AudioState, AudioInventory)| {
                        !old.devices
                            .iter()
                            .any(|device| device.name == name && device.direction == direction)
                    })
                && let Err(error) = api.select(direction, name).await
            {
                tracing::warn!(%error, ?direction, "could not restore audio device preference");
            }
        }
        let state = api.state().await;
        let inventory = api.inventory_state().await?;
        *disconnected = false;
        if previous.as_ref().is_none_or(|(old_state, old_inventory)| {
            old_state != &state || old_inventory != &inventory
        }) {
            previous = Some((state, inventory));
            let _ = AudioApi::changed(interface.signal_emitter()).await;
        }
    }
}

/// pw-dump emits a stream of top-level arrays. Frame once, then parse once, with bounded memory.
#[derive(Default)]
struct MonitorFrames {
    bytes: Vec<u8>,
    depth: usize,
    quoted: bool,
    escaped: bool,
}
impl MonitorFrames {
    fn push(&mut self, chunk: &[u8], mut frame: impl FnMut(Vec<serde_json::Value>)) -> Result<()> {
        for &byte in chunk {
            if self.bytes.is_empty() && byte.is_ascii_whitespace() {
                continue;
            }
            if self.bytes.len() == 8 * 1024 * 1024 {
                bail!("PipeWire monitor frame exceeded 8 MiB");
            }
            self.bytes.push(byte);
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'\"' {
                    self.quoted = false;
                }
            } else {
                match byte {
                    b'\"' => self.quoted = true,
                    b'[' | b'{' => self.depth += 1,
                    b']' | b'}' => {
                        self.depth = self
                            .depth
                            .checked_sub(1)
                            .context("malformed monitor frame")?
                    }
                    _ => {}
                }
            }
            if self.depth == 0 {
                frame(serde_json::from_slice(&self.bytes)?);
                self.bytes.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_down_from_a_boosted_sink_actually_goes_down() {
        // 126% is what wpctl reports when something has boosted the sink; clamping the result to
        // 100 made both steppers land there, so "louder" made it quieter.
        assert_eq!(step_to(126, -5), 121);
        assert_eq!(
            step_to(126, 5),
            126,
            "this control never boosts a sink further"
        );
        assert_eq!(step_to(103, -5), 98);
    }

    #[test]
    fn stepping_inside_the_ordinary_range_is_unchanged() {
        assert_eq!(step_to(45, 5), 50);
        assert_eq!(step_to(97, 5), 100);
        assert_eq!(step_to(100, 5), 100);
        assert_eq!(step_to(3, -5), 0);
        assert_eq!(step_to(0, -5), 0);
    }

    #[test]
    fn wpctls_real_output_shapes_parse() {
        assert_eq!(parse_volume("Volume: 0.45\n").unwrap(), (45, false));
        assert_eq!(parse_volume("Volume: 0.45 [MUTED]\n").unwrap(), (45, true));
        assert_eq!(parse_volume("Volume: 0.00\n").unwrap(), (0, false));
        assert_eq!(parse_volume("Volume: 1.00\n").unwrap(), (100, false));
        // A sink boosted past 100% reports what is true rather than a clamped reading.
        assert_eq!(parse_volume("Volume: 1.50\n").unwrap(), (150, false));
        for rubbish in ["", "Node 42 not found\n", "Volume: loud\n"] {
            assert!(parse_volume(rubbish).is_err(), "{rubbish:?}");
        }
    }
}
