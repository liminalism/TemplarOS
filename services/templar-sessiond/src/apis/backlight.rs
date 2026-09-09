//! `org.templaros.Backlight1`: the display backlight, read from sysfs and written through logind.
//!
//! Reading is a file; writing is not. `/sys/class/backlight/<device>/brightness` is root-owned on
//! every distribution, and the supported way for an unprivileged session to change it is
//! `org.freedesktop.login1.Session.SetBrightness`, which logind allows for the active session on
//! its own seat. That means no polkit action, no udev rule shipped by TemplarOS, and nothing in
//! `templar-hostd` — the same reasoning that keeps `Session.Lifecycle1` unprivileged (ADR-0003).
//!
//! Both halves are injectable and for the same reason: a test must never read the developer's
//! `/sys` and must never change the brightness of the machine it is running on. The sysfs root is
//! a path, so tests read a fixture tree; the writer is a trait, so tests observe the call that
//! *would* have been made instead of making it.

use crate::apis::failed;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zbus::fdo;

/// Where the kernel publishes backlights on a real machine.
pub const SYSFS_BACKLIGHT: &str = "/sys/class/backlight";

/// The lowest percentage this interface will set.
///
/// Not zero. A backlight really can be driven to a black screen, and on most panels there is then
/// no way to see the control that would turn it back up. Turning the display off is a different
/// action from dimming it, and this is not it.
///
/// Ten rather than one, because "not black" was never the whole requirement: on a panel with a
/// fine scale, 1% is a screen a person cannot read in a lit room, and every caller — the shell's
/// slider, `brightness.decrease`, the CLI — could reach it. The floor belongs here rather than in
/// any one of them, so there is one number and nobody can dim past it. A slider whose track starts
/// at the floor then spends none of its length on levels the display cannot be used at.
pub const MINIMUM_PERCENT: u32 = 10;

/// What `Backlight1.Get` answers.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct BacklightState {
    /// Whether this machine has a backlight at all. A desktop with an external monitor is
    /// `false`, and that is a fact rather than a failure.
    pub present: bool,
    pub percent: u32,
    /// The sysfs name logind is addressed with, e.g. `intel_backlight`.
    pub device: String,
    /// The hardware's own scale, kept because a backlight with 12 steps and one with 19200 behave
    /// differently under a percentage and a settings UI may want to say so.
    pub raw: u64,
    pub max_raw: u64,
}

/// What this interface needs in order to change a backlight.
#[async_trait]
pub trait BrightnessWriter: Send + Sync + std::fmt::Debug {
    /// logind's `Session.SetBrightness(subsystem, name, brightness)`, in raw hardware units.
    async fn set_brightness(&self, subsystem: &str, device: &str, raw: u32) -> Result<()>;
}

/// The real thing: `org.freedesktop.login1.Session` for this session, on the system bus.
///
/// The connection is held lazily and retried, exactly as `apis::lifecycle::SystemLogind` does and
/// for the same reason: an interface registered only if the system bus happened to answer during
/// startup is an interface that is silently absent for the whole session when it loses that race.
#[derive(Debug)]
pub struct LogindBrightness {
    connection: parking_lot::Mutex<Option<zbus::Connection>>,
}

impl LogindBrightness {
    pub fn new(connection: Option<zbus::Connection>) -> Self {
        Self {
            connection: parking_lot::Mutex::new(connection),
        }
    }

    async fn connection(&self) -> Result<zbus::Connection> {
        if let Some(connection) = self.connection.lock().clone() {
            return Ok(connection);
        }
        let connection = zbus::connection::Builder::system()
            .context("preparing a system bus connection")?
            .build()
            .await
            .context("connecting to the system bus to reach logind")?;
        *self.connection.lock() = Some(connection.clone());
        Ok(connection)
    }
}

#[async_trait]
impl BrightnessWriter for LogindBrightness {
    async fn set_brightness(&self, subsystem: &str, device: &str, raw: u32) -> Result<()> {
        self.connection()
            .await?
            .call_method(
                Some("org.freedesktop.login1"),
                // "auto" is logind's own name for the caller's session, so nothing has to
                // discover or carry a session id.
                "/org/freedesktop/login1/session/auto",
                Some("org.freedesktop.login1.Session"),
                "SetBrightness",
                &(subsystem, device, raw),
            )
            .await
            .context("login1.Session.SetBrightness")?;
        Ok(())
    }
}

/// Reads backlights out of a sysfs-shaped directory.
#[derive(Clone, Debug)]
pub struct SysfsBacklight {
    root: PathBuf,
}

impl SysfsBacklight {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The real `/sys/class/backlight`.
    pub fn system() -> Self {
        Self::new(SYSFS_BACKLIGHT)
    }

    /// The backlight to drive.
    ///
    /// A laptop can publish more than one — a firmware `acpi_video0` beside the native
    /// `intel_backlight` — and driving the wrong one does nothing visible. Preferring the one
    /// with the finest scale picks the native device on every machine seen so far, and ties break
    /// by name so the choice is the same on every read.
    fn device(&self) -> Option<(String, u64, u64)> {
        let entries = std::fs::read_dir(&self.root).ok()?;
        let mut devices: Vec<(String, u64, u64)> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let name = path.file_name()?.to_string_lossy().into_owned();
                let max = number(&path, "max_brightness")?;
                if max == 0 {
                    return None;
                }
                Some((name, number(&path, "brightness").unwrap_or(0).min(max), max))
            })
            .collect();
        devices.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
        devices.into_iter().next()
    }

    pub fn read(&self) -> BacklightState {
        let Some((device, raw, max_raw)) = self.device() else {
            return BacklightState::default();
        };
        BacklightState {
            present: true,
            percent: percent_of(raw, max_raw),
            device,
            raw,
            max_raw,
        }
    }
}

/// Raw units to a percentage, rounded to nearest so the number a person just set is the number
/// they read back.
fn percent_of(raw: u64, max_raw: u64) -> u32 {
    if max_raw == 0 {
        return 0;
    }
    ((u128::from(raw) * 100 + u128::from(max_raw) / 2) / u128::from(max_raw)).min(100) as u32
}

/// A percentage to raw units, never below one raw step: on a four-step backlight even
/// [`MINIMUM_PERCENT`] would round to zero and black the screen, which is exactly what that floor
/// exists to prevent.
fn raw_of(percent: u32, max_raw: u64) -> u64 {
    let percent = u64::from(percent.clamp(MINIMUM_PERCENT, 100));
    ((max_raw * percent + 50) / 100).max(1).min(max_raw)
}

fn number(device: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(device.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// `org.templaros.Backlight1`.
#[derive(Debug)]
pub struct BacklightApi {
    sysfs: SysfsBacklight,
    writer: Arc<dyn BrightnessWriter>,
}

impl BacklightApi {
    pub fn new(sysfs: SysfsBacklight, writer: Arc<dyn BrightnessWriter>) -> Self {
        Self { sysfs, writer }
    }

    async fn state(&self) -> Result<BacklightState> {
        let sysfs = self.sysfs.clone();
        Ok(tokio::task::spawn_blocking(move || sysfs.read()).await?)
    }

    /// Apply `percent` and report where it landed, or `None` when there is no backlight.
    async fn apply(&self, percent: u32) -> Result<Option<u32>> {
        let state = self.state().await?;
        if !state.present {
            return Ok(None);
        }
        let percent = percent.clamp(MINIMUM_PERCENT, 100);
        let raw = raw_of(percent, state.max_raw);
        self.writer
            .set_brightness(
                "backlight",
                &state.device,
                u32::try_from(raw).unwrap_or(u32::MAX),
            )
            .await?;
        Ok(Some(percent))
    }
}

#[zbus::interface(name = "org.templaros.Backlight1")]
impl BacklightApi {
    /// The backlight as `BacklightState` JSON.
    async fn get(&self) -> fdo::Result<String> {
        let state = self.state().await.map_err(failed)?;
        serde_json::to_string(&state).map_err(|error| failed(anyhow::anyhow!("{error}")))
    }

    /// Set the backlight, clamped to `MINIMUM_PERCENT..=100`.
    async fn set_brightness(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        percent: u32,
    ) -> fdo::Result<()> {
        let Some(_) = self.apply(percent).await.map_err(failed)? else {
            return Err(fdo::Error::NotSupported(
                "this machine has no display backlight".to_owned(),
            ));
        };
        // Best effort: a brightness change must not fail because nobody was listening.
        let _ = Self::changed(&emitter).await;
        Ok(())
    }

    /// Move the backlight by `delta` percentage points and report where it landed.
    async fn adjust_brightness(
        &self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
        delta: i32,
    ) -> fdo::Result<u32> {
        let state = self.state().await.map_err(failed)?;
        if !state.present {
            return Err(fdo::Error::NotSupported(
                "this machine has no display backlight".to_owned(),
            ));
        }
        let wanted = i64::from(state.percent) + i64::from(delta);
        let wanted = wanted.clamp(0, 100) as u32;
        let landed = self
            .apply(wanted)
            .await
            .map_err(failed)?
            .unwrap_or(state.percent);
        let _ = Self::changed(&emitter).await;
        Ok(landed)
    }

    /// This service changed the backlight.
    #[zbus(signal)]
    async fn changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what logind would have been asked to do. Nothing in this module's tests changes
    /// the brightness of the machine running them.
    #[derive(Debug, Default)]
    struct FakeWriter(Mutex<Vec<(String, String, u32)>>);

    #[async_trait]
    impl BrightnessWriter for FakeWriter {
        async fn set_brightness(&self, subsystem: &str, device: &str, raw: u32) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push((subsystem.to_owned(), device.to_owned(), raw));
            Ok(())
        }
    }

    fn backlight(root: &Path, name: &str, brightness: u64, max: u64) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("brightness"), format!("{brightness}\n")).unwrap();
        std::fs::write(directory.join("max_brightness"), format!("{max}\n")).unwrap();
    }

    #[test]
    fn the_native_backlight_wins_over_the_firmware_one() {
        let root = tempfile::tempdir().unwrap();
        backlight(root.path(), "acpi_video0", 5, 15);
        backlight(root.path(), "intel_backlight", 7680, 19200);

        let state = SysfsBacklight::new(root.path()).read();
        assert!(state.present);
        assert_eq!(state.device, "intel_backlight");
        assert_eq!(state.percent, 40);
        assert_eq!(state.max_raw, 19200);
    }

    #[test]
    fn a_machine_with_no_backlight_says_so_rather_than_reporting_darkness() {
        let root = tempfile::tempdir().unwrap();
        let state = SysfsBacklight::new(root.path()).read();
        assert_eq!(state, BacklightState::default());
        assert!(!state.present);
        // A max of zero is a device that cannot be driven, not a device at 0%.
        backlight(root.path(), "broken", 0, 0);
        assert!(!SysfsBacklight::new(root.path()).read().present);
    }

    #[tokio::test]
    async fn setting_the_brightness_asks_logind_in_the_hardwares_own_units() {
        let root = tempfile::tempdir().unwrap();
        backlight(root.path(), "intel_backlight", 7680, 19200);
        let writer = Arc::new(FakeWriter::default());
        let api = BacklightApi::new(SysfsBacklight::new(root.path()), writer.clone());

        assert_eq!(api.apply(75).await.unwrap(), Some(75));
        assert_eq!(
            *writer.0.lock().unwrap(),
            vec![("backlight".to_owned(), "intel_backlight".to_owned(), 14400)]
        );
    }

    #[tokio::test]
    async fn the_backlight_is_never_driven_to_a_screen_nobody_can_read() {
        let root = tempfile::tempdir().unwrap();
        // A coarse backlight: 1% of 12 steps rounds to zero, which is a black panel with the
        // control to undo it invisible.
        backlight(root.path(), "coarse", 6, 12);
        let writer = Arc::new(FakeWriter::default());
        let api = BacklightApi::new(SysfsBacklight::new(root.path()), writer.clone());

        // The floor is a decision, not an accident: a number this one is spelled out so lowering
        // it has to be deliberate.
        assert_eq!(MINIMUM_PERCENT, 10);
        assert_eq!(api.apply(0).await.unwrap(), Some(MINIMUM_PERCENT));
        assert_eq!(api.apply(200).await.unwrap(), Some(100));
        let calls = writer.0.lock().unwrap();
        assert_eq!(calls[0].2, 1, "never zero raw units");
        assert_eq!(calls[1].2, 12);
    }

    #[tokio::test]
    async fn a_machine_with_no_backlight_refuses_a_set_instead_of_pretending() {
        let root = tempfile::tempdir().unwrap();
        let writer = Arc::new(FakeWriter::default());
        let api = BacklightApi::new(SysfsBacklight::new(root.path()), writer.clone());
        assert_eq!(api.apply(50).await.unwrap(), None);
        assert!(
            writer.0.lock().unwrap().is_empty(),
            "nothing may be written when there is nothing to write to"
        );
    }

    #[test]
    fn percentages_round_trip_through_the_hardware_scale() {
        for (raw, max, percent) in [(0, 100, 0), (50, 100, 50), (7680, 19200, 40), (12, 12, 100)] {
            assert_eq!(percent_of(raw, max), percent, "{raw}/{max}");
        }
        // On a scale with at least a step per percent, what a person sets is exactly what they
        // read back.
        for max in [100u64, 19200] {
            for percent in [MINIMUM_PERCENT, 25, 40, 75, 100] {
                assert_eq!(
                    percent_of(raw_of(percent, max), max),
                    percent,
                    "{percent}/{max}"
                );
            }
        }
        // A coarse backlight cannot represent every percentage and this does not pretend
        // otherwise: 1% of twelve steps is one step, which reads back as 8%. What must hold is
        // that the result is the nearest representable level and never zero — a screen a person
        // can still see, and a number that does not drift on repeated reads.
        for percent in [MINIMUM_PERCENT, 25, 40, 75, 100] {
            let raw = raw_of(percent, 12);
            assert!(
                (1..=12).contains(&raw),
                "{percent}% became {raw} of 12 steps"
            );
            let settled = percent_of(raw, 12);
            assert_eq!(
                raw_of(settled, 12),
                raw,
                "reading {percent}% back as {settled}% and setting that must not move the level"
            );
        }
    }
}
