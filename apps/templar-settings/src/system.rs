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
//! profile, and the profile belongs to `Power1` — which is the fourth row: how long the session
//! may sit idle before the screen turns off. That one *is* enforced (the compositor is the
//! session's only idle source; see `compositor/templar-comp/src/idle.rs`), so it is a real control
//! and not a stored intention. The profile's `dim_after` and `lock_after` are deliberately not
//! offered here: nothing enforces either yet, and a control that changes only a stored number is
//! the kind of promise this page does not make.
//!
//! Like every other page, the real work is a pure function behind a small trait per interface, so
//! the tests are fixtures and never a bus.

use anyhow::{Context, Result};
use std::time::Duration;
use templar_client::status::{AudioState, BacklightState, BatteryState};
use templar_session_core::power::{PowerProfile, PowerSource};

/// The delays the page offers, in the order they are drawn. Minutes, because that is the unit a
/// person thinks in for this; `Power1` stores seconds, and 0 is never.
pub(crate) const SCREEN_OFF_CHOICES: [(&str, u32); 7] = [
    ("Never", 0),
    ("1 min", 60),
    ("2 min", 120),
    ("5 min", 300),
    ("10 min", 600),
    ("15 min", 900),
    ("30 min", 1800),
];

/// What the three interfaces answered. `None` for one that did not answer at all, which is a
/// different thing from a machine with no battery or no backlight — those answer, and say so.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemState {
    pub(crate) battery: Option<BatteryState>,
    pub(crate) audio: Option<AudioState>,
    pub(crate) backlight: Option<BacklightState>,
    /// The stored power profile for the source this machine is currently on, and which source
    /// that is — a setting written for "on battery" while the laptop is plugged in would be a
    /// control that appears to do nothing.
    pub(crate) power: Option<(PowerSource, PowerProfile)>,
}

/// The bus operations this page reads.
pub(crate) trait SystemSource {
    async fn battery(&self) -> Result<String>;
    async fn audio(&self) -> Result<String>;
    async fn backlight(&self) -> Result<String>;
    /// `Power1.GetProfile` for one source, as its stored JSON.
    async fn power_profile(&self, source: &str) -> Result<String>;
}

/// The bus operations this page writes. Separate from [`SystemSource`] because a test of what the
/// page *says* has no business being able to change the machine's volume.
pub(crate) trait SystemControl {
    async fn set_volume(&self, percent: u32) -> Result<()>;
    async fn adjust_volume(&self, delta: i32) -> Result<()>;
    async fn toggle_muted(&self) -> Result<()>;
    async fn set_brightness(&self, percent: u32) -> Result<()>;
    async fn adjust_brightness(&self, delta: i32) -> Result<()>;
    /// Store `seconds` as this source's screen-off delay, leaving every other field of the
    /// profile as it was. 0 is never.
    async fn set_screen_off_after(&self, source: &str, seconds: u32) -> Result<()>;
}

/// The three proxies as one source, so the page needs one connection and one call site.
pub(crate) struct Bus<'a> {
    pub(crate) battery: templar_client::proxies::BatteryProxy<'a>,
    pub(crate) audio: templar_client::proxies::AudioProxy<'a>,
    pub(crate) backlight: templar_client::proxies::BacklightProxy<'a>,
    pub(crate) power: templar_client::proxies::PowerProxy<'a>,
}

impl Bus<'_> {
    pub(crate) async fn connect(connection: &zbus::Connection) -> Result<Bus<'_>> {
        Ok(Bus {
            battery: templar_client::proxies::BatteryProxy::new(connection)
                .await
                .context("Battery1")?,
            audio: templar_client::proxies::AudioProxy::new(connection)
                .await
                .context("Audio1")?,
            backlight: templar_client::proxies::BacklightProxy::new(connection)
                .await
                .context("Backlight1")?,
            power: templar_client::proxies::PowerProxy::new(connection)
                .await
                .context("Power1")?,
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
    async fn power_profile(&self, source: &str) -> Result<String> {
        self.power
            .get_profile(source)
            .await
            .context("Power1.GetProfile")
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
    /// Read-modify-write, unlike the levels above: a profile is one document with six fields, and
    /// `Power1.SetProfile` replaces the whole of it. Writing a freshly-defaulted profile with only
    /// this field set would silently discard the lid actions and the other intervals.
    async fn set_screen_off_after(&self, source: &str, seconds: u32) -> Result<()> {
        let json = self.power_profile(source).await?;
        let mut profile: PowerProfile = serde_json::from_str(&json)
            .with_context(|| format!("Power1 returned a profile this build cannot read: {json}"))?;
        profile.screen_off_after = (seconds > 0).then(|| Duration::from_secs(u64::from(seconds)));
        let encoded = serde_json::to_string(&profile).context("encoding the power profile")?;
        self.power
            .set_profile(source, &encoded)
            .await
            .context("Power1.SetProfile")
    }
}

/// Read all three. A service that fails or answers unreadably becomes `None` rather than an error
/// for the whole page: one absent interface must not blank the other two.
pub(crate) async fn read<S: SystemSource>(source: &S) -> SystemState {
    let battery: Option<BatteryState> = source
        .battery()
        .await
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok());
    // Which stored profile applies is a hardware reading, not a preference: a machine with no
    // battery service at all is a machine on mains.
    let power_source = power_source_for(battery.as_ref());
    let power = source
        .power_profile(source_name(power_source))
        .await
        .ok()
        .and_then(|json| serde_json::from_str::<PowerProfile>(&json).ok())
        .map(|profile| (power_source, profile));
    SystemState {
        battery,
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
        power,
    }
}

/// Which `Power1` source's profile is in force right now.
pub(crate) fn power_source_for(battery: Option<&BatteryState>) -> PowerSource {
    match battery {
        Some(state) if state.present && !state.on_mains => PowerSource::Battery,
        _ => PowerSource::Mains,
    }
}

/// `Power1`'s wire name for a source — the string its three methods take.
pub(crate) fn source_name(source: PowerSource) -> &'static str {
    match source {
        PowerSource::Battery => "battery",
        PowerSource::Mains => "mains",
    }
}

/// The stored screen-off delay in seconds, 0 for never. `None` when `Power1` did not answer: a
/// page that cannot read the setting must not draw a selected choice, because every choice would
/// be a guess.
pub(crate) fn screen_off_seconds(state: &SystemState) -> Option<u32> {
    let (_, profile) = state.power.as_ref()?;
    Some(profile.screen_off_after.map_or(0, |delay| {
        u32::try_from(delay.as_secs()).unwrap_or(u32::MAX)
    }))
}

/// The screen-off row's supporting line: which power source the setting applies to, or why there
/// is nothing to set.
pub(crate) fn screen_off_line(state: &SystemState) -> String {
    let Some((source, profile)) = state.power.as_ref() else {
        return silent("Power1");
    };
    let which = match source {
        PowerSource::Battery => "on battery",
        PowerSource::Mains => "on mains power",
    };
    match profile.screen_off_after {
        None => format!("Never, {which} — the display stays on until you turn it off"),
        Some(delay) => format!(
            "After {} {which}. The screen turns off; it is not locked",
            humanize(delay)
        ),
    }
}

/// A delay as the page says it: whole minutes where it is whole minutes, seconds otherwise.
fn humanize(delay: Duration) -> String {
    let seconds = delay.as_secs();
    if seconds >= 60 && seconds.is_multiple_of(60) {
        let minutes = seconds / 60;
        if minutes == 1 {
            return "1 minute".to_owned();
        }
        return format!("{minutes} minutes");
    }
    format!("{seconds} seconds")
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
    // The wording is `templar_client::status`', not this page's: the Home Field's Settings widget
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
        // A reading over full scale is something else's doing, not this page's, and a person who
        // sees 126% next to a full track deserves to be told which of the two is lying.
        Some(state) if state.boosted() => {
            "Boosted past full scale by something else; the steppers bring it back down, and \
             cannot push it higher"
                .to_owned()
        }
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

/// The page as text, for `templar-settings --print --page system`.
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
        format!("  screen off {}", screen_off_line(state)),
    ]
    .join("\n")
}

/// Connects over the session bus, where `templar-sessiond` owns all three interfaces.
pub(crate) async fn render(connection: &zbus::Connection) -> String {
    match Bus::connect(connection).await {
        Ok(bus) => render_state(&read(&bus).await),
        Err(error) => format!("service unavailable: {error:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boosted_reading_is_explained_rather_than_left_next_to_a_full_track() {
        let state: AudioState =
            serde_json::from_str(r#"{"present":true,"volume_percent":126,"muted":false}"#).unwrap();
        let line = volume_line(Some(&state));
        assert!(line.contains("Boosted past full scale"), "{line}");
        assert!(line.contains("cannot push it higher"), "{line}");
    }

    /// A fixture standing in for all three interfaces. Nothing in this module's tests touches a
    /// bus, a sysfs node or this machine's actual volume.
    struct Fixture {
        battery: Option<&'static str>,
        audio: Option<&'static str>,
        backlight: Option<&'static str>,
        /// What `Power1.GetProfile` answers, whichever source is asked for — the fixture is the
        /// machine, and the machine has one profile in force at a time.
        power: Option<&'static str>,
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
        async fn power_profile(&self, _source: &str) -> Result<String> {
            self.power
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("no reply"))
        }
    }

    /// A profile as `Power1` stores one: `Option<Duration>` fields, `serde`-encoded.
    const MAINS_PROFILE: &str = r#"{"dim_after":{"secs":600,"nanos":0},
        "screen_off_after":{"secs":900,"nanos":0},"lock_after":{"secs":900,"nanos":0},
        "suspend_after":null,"lid_closed":"suspend","lid_closed_docked":"ignore"}"#;

    const LAPTOP: Fixture = Fixture {
        battery: Some(
            r#"{"present":true,"percent":72,"status":"discharging","on_mains":false,
                "time_to_empty_minutes":143,"time_to_full_minutes":null}"#,
        ),
        audio: Some(r#"{"present":true,"volume_percent":45,"muted":false}"#),
        backlight: Some(
            r#"{"present":true,"percent":60,"device":"intel_backlight","raw":600,"max_raw":1000}"#,
        ),
        power: Some(MAINS_PROFILE),
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
            power: Some(MAINS_PROFILE),
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
            power: LAPTOP.power,
        })
        .await;
        let rendered = render_state(&state);
        assert!(rendered.contains("Battery1 is not answering"), "{rendered}");
        assert!(rendered.contains("volume     45%"), "{rendered}");
        assert!(volume_settable(&state));
    }

    /// The profile read is the one for the source the machine is actually on: a laptop running
    /// on its battery must not be shown the mains delay it is not using.
    #[tokio::test]
    async fn the_profile_read_is_the_one_for_the_source_the_machine_is_on() {
        let discharging: BatteryState = serde_json::from_str(
            r#"{"present":true,"percent":72,"status":"discharging","on_mains":false}"#,
        )
        .unwrap();
        assert_eq!(power_source_for(Some(&discharging)), PowerSource::Battery);
        let desktop: BatteryState =
            serde_json::from_str(r#"{"present":false,"status":"unknown","on_mains":true}"#)
                .unwrap();
        assert_eq!(power_source_for(Some(&desktop)), PowerSource::Mains);
        assert_eq!(
            power_source_for(None),
            PowerSource::Mains,
            "no battery service is a machine on mains, not an unknown"
        );
    }

    /// The screen-off row says what it does and, just as importantly, what it does not do:
    /// turning the screen off is not locking it.
    #[tokio::test]
    async fn the_screen_off_row_reports_the_stored_delay_and_the_source_it_applies_to() {
        let state = read(&LAPTOP).await;
        assert_eq!(screen_off_seconds(&state), Some(900));
        let line = screen_off_line(&state);
        assert!(line.contains("15 minutes"), "{line}");
        assert!(line.contains("on battery"), "{line}");
        assert!(line.contains("not locked"), "{line}");
        assert!(render_state(&state).contains("screen off"));
    }

    /// A stored `None` is "never", and is said as such rather than as a zero.
    #[test]
    fn never_is_a_setting_and_reads_as_one() {
        let state = SystemState {
            power: Some((
                PowerSource::Mains,
                PowerProfile {
                    screen_off_after: None,
                    ..PowerProfile::default_for(PowerSource::Mains)
                },
            )),
            ..SystemState::default()
        };
        assert_eq!(screen_off_seconds(&state), Some(0));
        assert!(screen_off_line(&state).starts_with("Never"));
        assert_eq!(SCREEN_OFF_CHOICES[0], ("Never", 0));
    }

    /// `Power1` not answering leaves no choice selected — every one of them would be a guess.
    #[test]
    fn a_silent_power_service_selects_no_choice_at_all() {
        let state = SystemState::default();
        assert_eq!(screen_off_seconds(&state), None);
        assert!(screen_off_line(&state).contains("Power1 is not answering"));
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_treated_as_no_answer_rather_than_a_zero() {
        let state = read(&Fixture {
            battery: Some("{not json"),
            audio: Some("{not json"),
            backlight: Some("{not json"),
            power: Some("{not json"),
        })
        .await;
        assert_eq!(state, SystemState::default());
        assert!(!volume_settable(&state));
        assert!(!brightness_settable(&state));
        assert!(render_state(&state).contains("Audio1 is not answering"));
    }
}
