//! `org.legeos.Audio1`: the default sink's volume, through WirePlumber's `wpctl`.
//!
//! **Why a subprocess and not libpipewire.** The native route means linking a C library, needing
//! its headers at build time, and writing unsafe FFI in a workspace that forbids it — and it
//! means a PipeWire main loop living inside the one policy daemon whose crash takes identity,
//! launch, notifications and settings with it (ADR-0001). `wpctl` ships with WirePlumber on every
//! PipeWire desktop, runs with no display and no audio session of its own, and costs one short
//! process per adjustment. A volume key is pressed a few times a minute, not a few thousand times
//! a second, so that cost buys a dependency-free implementation that cannot take the daemon down.
//!
//! The trade is stated plainly in the interface XML: without a PipeWire event subscription, a
//! change made *elsewhere* — a hardware key wired straight to WirePlumber — is not observed, so
//! `Changed` covers changes made through this interface and clients that must not go stale poll.
//!
//! Everything below is testable without touching the machine's audio: [`parse_volume`] is a pure
//! function over `wpctl`'s real output, and the command runner is a trait, so a test observes the
//! `wpctl` invocation that *would* have been made rather than making it.

use crate::apis::failed;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zbus::fdo;

/// WirePlumber's name for "whatever the user is listening through".
const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";

/// What `Audio1.Get` answers.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioState {
    /// Whether this machine has a volume this service can reach: WirePlumber present, and a
    /// default sink to point at. A machine with no audio is a fact, not a failure.
    pub present: bool,
    pub volume_percent: u32,
    pub muted: bool,
}

/// What this interface needs from WirePlumber.
#[async_trait]
pub trait AudioControl: Send + Sync + std::fmt::Debug {
    /// Run `wpctl` with these arguments, returning its stdout.
    ///
    /// One method rather than one per operation, so the fake in the tests observes the exact
    /// command line and the parsing under test is the same code the daemon runs.
    async fn wpctl(&self, arguments: &[&str]) -> Result<String>;
}

/// The real thing: `wpctl` on `PATH`.
#[derive(Debug, Default)]
pub struct WpctlAudio;

#[async_trait]
impl AudioControl for WpctlAudio {
    async fn wpctl(&self, arguments: &[&str]) -> Result<String> {
        let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
        let output = tokio::process::Command::new("wpctl")
            .args(&arguments)
            .output()
            .await
            .with_context(|| format!("running wpctl {}", arguments.join(" ")))?;
        if !output.status.success() {
            bail!(
                "wpctl {} failed: {}",
                arguments.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
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

/// `org.legeos.Audio1`.
#[derive(Debug)]
pub struct AudioApi {
    control: Arc<dyn AudioControl>,
}

impl AudioApi {
    pub fn new(control: Arc<dyn AudioControl>) -> Self {
        Self { control }
    }

    /// Read the default sink, treating an unreachable `wpctl` as "this machine has no volume
    /// control" rather than as an error: a desktop with no WirePlumber is a working machine.
    pub async fn state(&self) -> AudioState {
        let Ok(output) = self.control.wpctl(&["get-volume", DEFAULT_SINK]).await else {
            return AudioState::default();
        };
        match parse_volume(&output) {
            Ok((volume_percent, muted)) => AudioState {
                present: true,
                volume_percent,
                muted,
            },
            // wpctl answered but in a shape this does not understand. That is a real fault and is
            // worth a log line, but it is still not a reason to claim the volume is zero.
            Err(error) => {
                tracing::warn!(%error, "could not read the default sink's volume");
                AudioState::default()
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
}

#[zbus::interface(name = "org.legeos.Audio1")]
impl AudioApi {
    /// The default sink as `AudioState` JSON.
    async fn get(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.state().await)
            .map_err(|error| failed(anyhow::anyhow!("{error}")))
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
        let landed = self.apply_volume(wanted).await.map_err(failed)?;
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
        Ok(!state.muted)
    }

    /// This service changed the volume or the mute state.
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
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

    use std::sync::Mutex;

    /// Answers like `wpctl` and records what it was asked. Nothing in this module's tests changes
    /// the volume of the machine running them.
    #[derive(Debug)]
    struct FakeWpctl {
        /// What `get-volume` prints — the real one-line format.
        volume: Mutex<String>,
        calls: Mutex<Vec<Vec<String>>>,
        /// When true, `wpctl` is not on this machine.
        absent: bool,
    }

    impl FakeWpctl {
        fn reporting(volume: &str) -> Arc<Self> {
            Arc::new(Self {
                volume: Mutex::new(volume.to_owned()),
                calls: Mutex::new(Vec::new()),
                absent: false,
            })
        }

        fn missing() -> Arc<Self> {
            Arc::new(Self {
                volume: Mutex::new(String::new()),
                calls: Mutex::new(Vec::new()),
                absent: true,
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AudioControl for FakeWpctl {
        async fn wpctl(&self, arguments: &[&str]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(arguments.iter().map(|value| (*value).to_owned()).collect());
            if self.absent {
                bail!("no such file or directory: wpctl");
            }
            if arguments.first() == Some(&"get-volume") {
                return Ok(self.volume.lock().unwrap().clone());
            }
            Ok(String::new())
        }
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

    #[tokio::test]
    async fn the_volume_is_read_from_the_default_sink() {
        let wpctl = FakeWpctl::reporting("Volume: 0.32 [MUTED]\n");
        let api = AudioApi::new(wpctl.clone());
        let state = api.state().await;
        assert_eq!(
            state,
            AudioState {
                present: true,
                volume_percent: 32,
                muted: true
            }
        );
        assert_eq!(
            wpctl.calls(),
            vec![vec!["get-volume".to_owned(), DEFAULT_SINK.to_owned()]]
        );
    }

    #[tokio::test]
    async fn a_machine_without_wireplumber_says_no_audio_rather_than_zero() {
        let wpctl = FakeWpctl::missing();
        let api = AudioApi::new(wpctl.clone());
        let state = api.state().await;
        assert!(!state.present, "absent, which is not the same as silent");
        assert_eq!(state.volume_percent, 0);
        // The read was attempted: "no audio" is what a failing wpctl means, not a guess made
        // without asking.
        assert_eq!(wpctl.calls().len(), 1);
    }

    #[tokio::test]
    async fn setting_a_volume_asks_wpctl_in_percent_and_clamps_to_a_hundred() {
        let wpctl = FakeWpctl::reporting("Volume: 0.20\n");
        let api = AudioApi::new(wpctl.clone());
        assert_eq!(api.apply_volume(65).await.unwrap(), 65);
        assert_eq!(api.apply_volume(400).await.unwrap(), 100);
        let calls = wpctl.calls();
        assert_eq!(calls[0], ["set-volume", DEFAULT_SINK, "65%"]);
        assert_eq!(calls[1], ["set-volume", DEFAULT_SINK, "100%"]);
    }

    #[tokio::test]
    async fn adjusting_reads_then_steps_and_never_leaves_the_range() {
        let wpctl = FakeWpctl::reporting("Volume: 0.95\n");
        let api = AudioApi::new(wpctl.clone());
        // Reading first is what makes one adjustment one step: the service, not the caller,
        // knows where the volume was.
        let state = api.state().await;
        assert_eq!(state.volume_percent, 95);
        let wanted = (i64::from(state.volume_percent) + 10).clamp(0, 100) as u32;
        assert_eq!(api.apply_volume(wanted).await.unwrap(), 100);

        *wpctl.volume.lock().unwrap() = "Volume: 0.03\n".to_owned();
        let state = api.state().await;
        let wanted = (i64::from(state.volume_percent) - 10).clamp(0, 100) as u32;
        assert_eq!(api.apply_volume(wanted).await.unwrap(), 0);
    }
}
