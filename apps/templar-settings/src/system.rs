//! System overview — battery, output volume and display brightness readings, over `Battery1`,
//! `Audio1` and `Backlight1`. Sound owns volume controls; Displays owns brightness controls.
//!
//! Battery remains read-only. Stored power drafts and the native policy owner's capabilities,
//! readback and errors belong to `crate::power`; this module only owns the three hardware readings.

use anyhow::{Context, Result};
use templar_client::status::{AudioState, BacklightState, BatteryState};

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

/// Brightness operations shared with Displays. Volume mutation belongs to `crate::sound`.
pub(crate) trait SystemControl {
    async fn set_brightness(&self, percent: u32) -> Result<()>;
    async fn adjust_brightness(&self, delta: i32) -> Result<()>;
}

/// The three proxies as one source, so the page needs one connection and one call site.
pub(crate) struct Bus<'a> {
    pub(crate) battery: templar_client::proxies::BatteryProxy<'a>,
    pub(crate) audio: templar_client::proxies::AudioProxy<'a>,
    pub(crate) backlight: templar_client::proxies::BacklightProxy<'a>,
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
    let battery: Option<BatteryState> = source
        .battery()
        .await
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok());
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
            "Boosted past full scale; adjust the output in Sound".to_owned()
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
