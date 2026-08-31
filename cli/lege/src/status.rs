//! `lege status` — volume, backlight and battery (`Audio1`, `Backlight1`, `Battery1`).
//!
//! Everything the shell offers must be reachable here too
//! (`docs/architecture/accessibility.md`: "everything the shell shows is available through `lege`
//! too"), and for these three that is not a formality: a volume or brightness key that the
//! compositor has not been taught yet still has a working route, and a headless machine has a way
//! to read its own battery.
//!
//! One trait per interface, so every renderer below is testable against a fixture rather than a
//! live sink or a real `/sys` — the same shape `crate::lifecycle` uses.

use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_client::status::{AudioState, BacklightState, BatteryState, duration_minutes};

#[async_trait]
pub trait AudioClient {
    async fn get(&self) -> Result<String>;
    async fn set_volume(&self, percent: u32) -> Result<()>;
    async fn adjust_volume(&self, delta: i32) -> Result<u32>;
    async fn toggle_muted(&self) -> Result<bool>;
}

#[async_trait]
impl AudioClient for lege_client::proxies::AudioProxy<'_> {
    async fn get(&self) -> Result<String> {
        self.get().await.context("Audio1.Get")
    }
    async fn set_volume(&self, percent: u32) -> Result<()> {
        self.set_volume(percent).await.context("Audio1.SetVolume")
    }
    async fn adjust_volume(&self, delta: i32) -> Result<u32> {
        self.adjust_volume(delta)
            .await
            .context("Audio1.AdjustVolume")
    }
    async fn toggle_muted(&self) -> Result<bool> {
        self.toggle_muted().await.context("Audio1.ToggleMuted")
    }
}

#[async_trait]
pub trait BacklightClient {
    async fn get(&self) -> Result<String>;
    async fn set_brightness(&self, percent: u32) -> Result<()>;
    async fn adjust_brightness(&self, delta: i32) -> Result<u32>;
}

#[async_trait]
impl BacklightClient for lege_client::proxies::BacklightProxy<'_> {
    async fn get(&self) -> Result<String> {
        self.get().await.context("Backlight1.Get")
    }
    async fn set_brightness(&self, percent: u32) -> Result<()> {
        self.set_brightness(percent)
            .await
            .context("Backlight1.SetBrightness")
    }
    async fn adjust_brightness(&self, delta: i32) -> Result<u32> {
        self.adjust_brightness(delta)
            .await
            .context("Backlight1.AdjustBrightness")
    }
}

#[async_trait]
pub trait BatteryClient {
    async fn get(&self) -> Result<String>;
}

#[async_trait]
impl BatteryClient for lege_client::proxies::BatteryProxy<'_> {
    async fn get(&self) -> Result<String> {
        self.get().await.context("Battery1.Get")
    }
}

pub fn parse_audio(state_json: &str) -> Result<AudioState> {
    serde_json::from_str(state_json).context("parsing Audio1.Get JSON")
}

pub fn parse_backlight(state_json: &str) -> Result<BacklightState> {
    serde_json::from_str(state_json).context("parsing Backlight1.Get JSON")
}

pub fn parse_battery(state_json: &str) -> Result<BatteryState> {
    serde_json::from_str(state_json).context("parsing Battery1.Get JSON")
}

pub fn render_audio(state: &AudioState) -> String {
    if !state.present {
        return "volume     no audio control on this machine".to_owned();
    }
    format!(
        "volume     {}%{}",
        state.volume_percent,
        if state.muted { "  (muted)" } else { "" }
    )
}

pub fn render_backlight(state: &BacklightState) -> String {
    if !state.present {
        return "brightness no display backlight on this machine".to_owned();
    }
    // The device and its scale are worth printing here and nowhere else: this is where somebody
    // debugging "the brightness keys do nothing" finds out which of two backlights is being
    // driven and how many steps it really has.
    format!(
        "brightness {}%  ({}, {} of {} steps)",
        state.percent, state.device, state.raw, state.max_raw
    )
}

pub fn render_battery(state: &BatteryState) -> String {
    if !state.present {
        return format!(
            "battery    no battery{}",
            if state.on_mains { "  (on mains)" } else { "" }
        );
    }
    let charge = state
        .percent
        .map_or_else(|| "unknown".to_owned(), |percent| format!("{percent}%"));
    let estimate = state
        .time_remaining()
        .map_or_else(String::new, |(word, minutes)| {
            format!("  {} {word}", duration_minutes(minutes))
        });
    format!(
        "battery    {charge}  {}{estimate}{}",
        state.status,
        if state.on_mains && state.status != "charging" {
            "  (on mains)"
        } else {
            ""
        }
    )
}

/// All three at once — what plain `lege status` prints.
pub fn render_all(
    audio: &AudioState,
    backlight: &BacklightState,
    battery: &BatteryState,
) -> String {
    [
        render_audio(audio),
        render_backlight(backlight),
        render_battery(battery),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_laptop_prints_its_level_its_backlight_and_its_time_remaining() {
        let audio = parse_audio(r#"{"present":true,"volume_percent":45,"muted":false}"#).unwrap();
        let backlight = parse_backlight(
            r#"{"present":true,"percent":40,"device":"intel_backlight","raw":7680,
                "max_raw":19200}"#,
        )
        .unwrap();
        let battery = parse_battery(
            r#"{"present":true,"percent":72,"status":"discharging","on_mains":false,
                "time_to_empty_minutes":143,"time_to_full_minutes":null}"#,
        )
        .unwrap();

        let rendered = render_all(&audio, &backlight, &battery);
        assert!(rendered.contains("volume     45%"));
        assert!(!rendered.contains("muted"));
        assert!(rendered.contains("intel_backlight"));
        assert!(rendered.contains("7680 of 19200 steps"));
        assert!(rendered.contains("72%"));
        assert!(rendered.contains("2 h 23 min remaining"), "{rendered}");
    }

    #[test]
    fn a_desktop_says_what_it_lacks_rather_than_printing_zeroes() {
        let audio = parse_audio(r#"{"present":false}"#).unwrap();
        let backlight = parse_backlight(r#"{"present":false}"#).unwrap();
        let battery =
            parse_battery(r#"{"present":false,"status":"unknown","on_mains":true}"#).unwrap();

        let rendered = render_all(&audio, &backlight, &battery);
        assert!(rendered.contains("no audio control"));
        assert!(rendered.contains("no display backlight"));
        assert!(rendered.contains("no battery"));
        assert!(rendered.contains("on mains"));
        assert!(
            !rendered.contains("0%"),
            "absent hardware must never be rendered as a zero: {rendered}"
        );
    }

    #[test]
    fn a_muted_sink_says_so_and_still_shows_the_level_it_will_return_to() {
        let audio = parse_audio(r#"{"present":true,"volume_percent":30,"muted":true}"#).unwrap();
        let rendered = render_audio(&audio);
        assert!(rendered.contains("30%"));
        assert!(rendered.contains("(muted)"));
    }

    #[test]
    fn a_charging_battery_reads_as_charging_and_not_as_plugged_in_twice() {
        let battery = parse_battery(
            r#"{"present":true,"percent":40,"status":"charging","on_mains":true,
                "time_to_empty_minutes":null,"time_to_full_minutes":90}"#,
        )
        .unwrap();
        let rendered = render_battery(&battery);
        assert!(rendered.contains("charging"));
        assert!(rendered.contains("1 h 30 min to full"));
        assert!(
            !rendered.contains("(on mains)"),
            "charging already says it is plugged in: {rendered}"
        );
    }

    #[test]
    fn a_full_battery_on_mains_gets_no_invented_estimate() {
        let battery = parse_battery(
            r#"{"present":true,"percent":100,"status":"full","on_mains":true,
                "time_to_empty_minutes":null,"time_to_full_minutes":null}"#,
        )
        .unwrap();
        let rendered = render_battery(&battery);
        assert!(rendered.contains("100%"));
        assert!(rendered.contains("full"));
        assert!(rendered.contains("(on mains)"));
        assert!(!rendered.contains("remaining"));
    }
}
