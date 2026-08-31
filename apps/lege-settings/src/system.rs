//! System page — the battery, the output volume and the display brightness, over `Battery1`,
//! `Audio1` and `Backlight1`.
//!
//! The Home Field's Settings widget reports these three and offers one step at a time; this is
//! where the same three become something a person can set outright. That is the whole division:
//! a widget is a glance and a nudge, a window is where a figure gets typed in and where the
//! reason a control does not work has room to be a sentence.
//!
//! Battery is read-only, and that is the honest shape rather than a gap — there is nothing to do
//! to a battery from a settings page. A control here would have to be either a lie or a power
//! profile, and the profile belongs to `Power1`.
//!
//! Like every other page, the real work is a pure function behind a small trait per interface, so
//! the tests are fixtures and never a bus.

use anyhow::{Context, Result};
use lege_client::status::{AudioState, BacklightState, BatteryState};

/// What the three interfaces answered. `None` for one that did not answer at all, which is a
/// different thing from a machine with no battery or no backlight — those answer, and say so.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemState {
    pub(crate) battery: Option<BatteryState>,
    pub(crate) audio: Option<AudioState>,
    pub(crate) backlight: Option<BacklightState>,
}

/// The bus operations this page reads.
pub(crate) trait SystemSource {
    async fn battery(&self) -> Result<String>;
    async fn audio(&self) -> Result<String>;
    async fn backlight(&self) -> Result<String>;
}

/// The bus operations this page writes. Separate from [`SystemSource`] because a test of what the
/// page *says* has no business being able to change the machine's volume.
pub(crate) trait SystemControl {
    async fn set_volume(&self, percent: u32) -> Result<()>;
    async fn adjust_volume(&self, delta: i32) -> Result<()>;
    async fn toggle_muted(&self) -> Result<()>;
    async fn set_brightness(&self, percent: u32) -> Result<()>;
    async fn adjust_brightness(&self, delta: i32) -> Result<()>;
}

/// The three proxies as one source, so the page needs one connection and one call site.
pub(crate) struct Bus<'a> {
    pub(crate) battery: lege_client::proxies::BatteryProxy<'a>,
    pub(crate) audio: lege_client::proxies::AudioProxy<'a>,
    pub(crate) backlight: lege_client::proxies::BacklightProxy<'a>,
}

impl Bus<'_> {
    pub(crate) async fn connect(connection: &zbus::Connection) -> Result<Bus<'_>> {
        Ok(Bus {
            battery: lege_client::proxies::BatteryProxy::new(connection)
                .await
                .context("Battery1")?,
            audio: lege_client::proxies::AudioProxy::new(connection)
                .await
                .context("Audio1")?,
            backlight: lege_client::proxies::BacklightProxy::new(connection)
                .await
                .context("Backlight1")?,
        })
    }
}

impl SystemSource for Bus<'_> {
    async fn battery(&self) -> Result<String> {
        self.battery.get().await.context("Battery1.Get")
    }
    async fn audio(&self) -> Result<String> {
        self.audio.get().await.context("Audio1.Get")
    }
    async fn backlight(&self) -> Result<String> {
        self.backlight.get().await.context("Backlight1.Get")
    }
}

impl SystemControl for Bus<'_> {
    async fn set_volume(&self, percent: u32) -> Result<()> {
        self.audio
            .set_volume(percent)
            .await
            .context("Audio1.SetVolume")
    }
    async fn adjust_volume(&self, delta: i32) -> Result<()> {
        self.audio
            .adjust_volume(delta)
            .await
            .map(|_| ())
            .context("Audio1.AdjustVolume")
    }
    async fn toggle_muted(&self) -> Result<()> {
        self.audio
            .toggle_muted()
            .await
            .map(|_| ())
            .context("Audio1.ToggleMuted")
    }
    async fn set_brightness(&self, percent: u32) -> Result<()> {
        self.backlight
            .set_brightness(percent)
            .await
            .context("Backlight1.SetBrightness")
    }
    async fn adjust_brightness(&self, delta: i32) -> Result<()> {
        self.backlight
            .adjust_brightness(delta)
            .await
            .map(|_| ())
            .context("Backlight1.AdjustBrightness")
    }
}

/// Read all three. A service that fails or answers unreadably becomes `None` rather than an error
/// for the whole page: one absent interface must not blank the other two.
pub(crate) async fn read<S: SystemSource>(source: &S) -> SystemState {
    SystemState {
        battery: source
            .battery()
            .await
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok()),
        audio: source
            .audio()
            .await
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok()),
        backlight: source
            .backlight()
            .await
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok()),
    }
}

/// The line under a heading when its service did not answer — never a blank, and never a zero.
pub(crate) fn silent(interface: &str) -> String {
    format!("{interface} is not answering; this reading is unavailable")
}

/// The battery, in one sentence: the charge, what it is doing, and how long that leaves.
pub(crate) fn battery_line(state: Option<&BatteryState>) -> String {
    let Some(state) = state else {
        return silent("Battery1");
    };
    if !state.present {
        // Not a fault: a desktop has no battery, and saying so is the answer.
        return if state.on_mains {
            "No battery — this machine runs on mains power".to_owned()
        } else {
            "No battery detected".to_owned()
        };
    }
    // The wording is `lege_client::status`', not this page's: the Home Field's Settings widget
    // renders the same fact, and two surfaces that phrase one charge differently are two surfaces
    // a person has to reconcile.
    state.reading()
}

/// The volume's supporting line, and whether its controls can do anything.
pub(crate) fn volume_line(state: Option<&AudioState>) -> String {
    match state {
        None => silent("Audio1"),
        Some(state) if !state.present => {
            "No audio service on this machine; nothing to set".to_owned()
        }
        Some(state) if state.muted => format!("Muted at {}%", state.volume_percent),
        Some(_) => "Default output".to_owned(),
    }
}

/// The brightness's supporting line: the backlight device, so someone debugging "the brightness
/// keys do nothing" can see which of two backlights is being driven.
pub(crate) fn brightness_line(state: Option<&BacklightState>) -> String {
    match state {
        None => silent("Backlight1"),
        Some(state) if !state.present => {
            "This display has no backlight control; nothing to set".to_owned()
        }
        Some(state) => state.device.clone(),
    }
}

/// Whether the volume controls would do anything. A page that draws live controls over a service
/// that cannot act is a promise it cannot keep.
pub(crate) fn volume_settable(state: &SystemState) -> bool {
    state.audio.as_ref().is_some_and(|audio| audio.present)
}

pub(crate) fn brightness_settable(state: &SystemState) -> bool {
    state
        .backlight
        .as_ref()
        .is_some_and(|backlight| backlight.present)
}

/// The page as text, for `lege-settings --print --page system`.
pub(crate) fn render_state(state: &SystemState) -> String {
    let volume = state
        .audio
        .as_ref()
        .map_or_else(|| silent("Audio1"), AudioState::headline);
    let brightness = state
        .backlight
        .as_ref()
        .map_or_else(|| silent("Backlight1"), BacklightState::headline);
    [
        "System:".to_owned(),
        format!("  battery    {}", battery_line(state.battery.as_ref())),
        format!(
            "  volume     {volume}  ({})",
            volume_line(state.audio.as_ref())
        ),
        format!(
            "  brightness {brightness}  ({})",
            brightness_line(state.backlight.as_ref())
        ),
    ]
    .join("\n")
}

/// Connects over the session bus, where `lege-sessiond` owns all three interfaces.
pub(crate) async fn render(connection: &zbus::Connection) -> String {
    match Bus::connect(connection).await {
        Ok(bus) => render_state(&read(&bus).await),
        Err(error) => format!("service unavailable: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture standing in for all three interfaces. Nothing in this module's tests touches a
    /// bus, a sysfs node or this machine's actual volume.
    struct Fixture {
        battery: Option<&'static str>,
        audio: Option<&'static str>,
        backlight: Option<&'static str>,
    }

    impl SystemSource for Fixture {
        async fn battery(&self) -> Result<String> {
            self.battery
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("no reply"))
        }
        async fn audio(&self) -> Result<String> {
            self.audio
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("no reply"))
        }
        async fn backlight(&self) -> Result<String> {
            self.backlight
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("no reply"))
        }
    }

    const LAPTOP: Fixture = Fixture {
        battery: Some(
            r#"{"present":true,"percent":72,"status":"discharging","on_mains":false,
                "time_to_empty_minutes":143,"time_to_full_minutes":null}"#,
        ),
        audio: Some(r#"{"present":true,"volume_percent":45,"muted":false}"#),
        backlight: Some(
            r#"{"present":true,"percent":60,"device":"intel_backlight","raw":600,"max_raw":1000}"#,
        ),
    };

    #[tokio::test]
    async fn a_laptop_reports_all_three_readings() {
        let state = read(&LAPTOP).await;
        let rendered = render_state(&state);
        assert!(
            rendered.contains("72% · 2 h 23 min remaining"),
            "{rendered}"
        );
        assert!(rendered.contains("volume     45%"), "{rendered}");
        assert!(rendered.contains("brightness 60%"), "{rendered}");
        assert!(rendered.contains("intel_backlight"), "{rendered}");
        assert!(volume_settable(&state));
        assert!(brightness_settable(&state));
    }

    /// A desktop has no battery and a monitor has no backlight. Neither is a fault, and neither
    /// may be reported as one — but the brightness control must not pretend it would work.
    #[tokio::test]
    async fn a_desktop_says_what_it_lacks_without_calling_it_a_failure() {
        let state = read(&Fixture {
            battery: Some(r#"{"present":false,"status":"unknown","on_mains":true}"#),
            audio: Some(r#"{"present":true,"volume_percent":30,"muted":true}"#),
            backlight: Some(r#"{"present":false,"percent":0,"device":"","raw":0,"max_raw":0}"#),
        })
        .await;
        let rendered = render_state(&state);
        assert!(rendered.contains("runs on mains power"), "{rendered}");
        assert!(!rendered.contains("unavailable"), "{rendered}");
        assert!(rendered.contains("Muted at 30%"), "{rendered}");
        assert!(volume_settable(&state), "muted audio is still settable");
        assert!(!brightness_settable(&state));
    }

    /// A service that is not there is said to be not there, per reading. One silent interface
    /// must not blank the other two.
    #[tokio::test]
    async fn one_silent_interface_does_not_take_the_others_with_it() {
        let state = read(&Fixture {
            battery: None,
            audio: LAPTOP.audio,
            backlight: LAPTOP.backlight,
        })
        .await;
        let rendered = render_state(&state);
        assert!(rendered.contains("Battery1 is not answering"), "{rendered}");
        assert!(rendered.contains("volume     45%"), "{rendered}");
        assert!(volume_settable(&state));
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_treated_as_no_answer_rather_than_a_zero() {
        let state = read(&Fixture {
            battery: Some("{not json"),
            audio: Some("{not json"),
            backlight: Some("{not json"),
        })
        .await;
        assert_eq!(state, SystemState::default());
        assert!(!volume_settable(&state));
        assert!(!brightness_settable(&state));
        assert!(render_state(&state).contains("Audio1 is not answering"));
    }
}
