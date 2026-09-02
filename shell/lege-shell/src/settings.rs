//! The shell's own settings, read from `org.legeos.Settings1` — never from a private file.
//!
//! Planning doc 01 §7 replaces "settings are scattered private implementation details" with one
//! typed schema that has a GUI, a CLI and a change history. The shell is a client of that schema
//! like anything else: `shell.dock.grouping` already exists in
//! `crates/lege-session-core/src/settings.rs`'s baseline schema, with exactly the three choices
//! `lege_window::shell::DockGrouping` has.
//!
//! A missing or unreadable setting is not an error here. The shell is decorative and restartable;
//! it must render *something* sensible when sessiond is not up yet, so every lookup falls back to
//! the schema's own default rather than refusing to draw a dock.

use anyhow::{Context, Result};
use async_trait::async_trait;
use lege_client::proxies::SettingsProxy;
use lege_window::DockGrouping;

/// The key the dock's grouping policy lives under.
pub const DOCK_GROUPING_KEY: &str = "shell.dock.grouping";
/// Logical pixels per Home Field grid cell.
pub const TILE_SIZE_KEY: &str = "shell.home-field.tile-size";
/// The throw experiment gate (`off` / `full`).
pub const THROW_KEY: &str = "shell.home-field.throw";

/// `shell.home-field.previews`.
pub const PREVIEWS_KEY: &str = "shell.home-field.previews";
/// `shell.home-field.preview-rate`, frames per second.
pub const PREVIEW_RATE_KEY: &str = "shell.home-field.preview-rate";
/// `shell.home-field.reduced-motion`.
pub const REDUCED_MOTION_KEY: &str = "shell.home-field.reduced-motion";
/// Recently launched applications, newest first. Not a Home Field layout key: it is launch
/// history for the search strip, and lives under `shell.launcher.*` so the Settings Home Field
/// page does not offer it as a control.
pub const RECENT_APPS_KEY: &str = "shell.launcher.recent-apps";

/// What a parked-session tile shows of its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewMode {
    /// Icon and title only.
    Off,
    /// One frame when the window is parked (and one on the first sight of it).
    Static,
    /// Refreshed at the preview rate while the tile is visible.
    Live,
}

/// The Home Field's own settings, with the schema's defaults when unreadable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HomeFieldSettings {
    pub tile_size: i32,
    pub throw: bool,
    pub previews: PreviewMode,
    /// Frames per second for [`PreviewMode::Live`].
    pub preview_rate: u32,
    /// Keep the design's state changes but remove projected/expanding motion.
    pub reduced_motion: bool,
}

impl Default for HomeFieldSettings {
    fn default() -> Self {
        Self {
            tile_size: crate::field_model::TILE_SIZE,
            throw: false,
            previews: PreviewMode::Static,
            preview_rate: 1,
            reduced_motion: false,
        }
    }
}

/// The `value` of a `SettingValue` JSON payload as an integer.
pub fn parse_int(value_json: &str) -> Option<i64> {
    let value: serde_json::Value = serde_json::from_str(value_json).ok()?;
    value.get("value")?.as_i64()
}

/// The `value` of a `SettingValue` JSON payload as a choice string.
pub fn parse_choice(value_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value_json).ok()?;
    Some(value.get("value")?.as_str()?.to_owned())
}

/// The `value` of a `SettingValue` JSON payload as a boolean.
pub fn parse_bool(value_json: &str) -> Option<bool> {
    let value: serde_json::Value = serde_json::from_str(value_json).ok()?;
    value.get("value")?.as_bool()
}

/// The `value` of a `SettingValue` JSON payload as a list of strings.
pub fn parse_str_list(value_json: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(value_json).ok()?;
    let list = value.get("value")?.as_array()?;
    list.iter()
        .map(|entry| entry.as_str().map(str::to_owned))
        .collect()
}

/// The Home Field's settings, each falling back to its schema default on its own.
pub async fn home_field(client: &impl SettingsClient) -> HomeFieldSettings {
    let defaults = HomeFieldSettings::default();
    let tile_size = client
        .get(TILE_SIZE_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_int)
        .map_or(defaults.tile_size, |size| size.clamp(40, 400) as i32);
    let throw = client
        .get(THROW_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .is_some_and(|choice| choice == "full");
    let previews = match client
        .get(PREVIEWS_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .as_deref()
    {
        Some("off") => PreviewMode::Off,
        Some("live") => PreviewMode::Live,
        _ => PreviewMode::Static,
    };
    let preview_rate = client
        .get(PREVIEW_RATE_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_int)
        .map_or(defaults.preview_rate, |rate| rate.clamp(1, 4) as u32);
    let reduced_motion = client
        .get(REDUCED_MOTION_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(defaults.reduced_motion);
    HomeFieldSettings {
        tile_size,
        throw,
        previews,
        preview_rate,
        reduced_motion,
    }
}

/// What the shell needs from `Settings1`, behind a trait so tests never touch a session bus.
#[async_trait]
pub trait SettingsClient: Send + Sync {
    /// The effective value as `SettingValue` JSON, e.g. `{"type":"choice","value":"by-instance"}`.
    async fn get(&self, key: &str) -> Result<String>;
}

#[async_trait]
impl SettingsClient for SettingsProxy<'_> {
    async fn get(&self, key: &str) -> Result<String> {
        self.get(key).await.context("Settings1.Get")
    }
}

/// Decode a `SettingValue`'s payload into a [`DockGrouping`].
///
/// The wire form is `lege_session_core::settings::SettingValue` — `{"type":..,"value":..}` — and
/// the three payloads are exactly `DockGrouping`'s own kebab-case serde names, so this decodes
/// the inner string through `DockGrouping`'s `Deserialize` rather than restating the mapping.
pub fn parse_dock_grouping(value_json: &str) -> Option<DockGrouping> {
    let value: serde_json::Value = serde_json::from_str(value_json).ok()?;
    let choice = value.get("value")?.as_str()?;
    serde_json::from_value(serde_json::Value::String(choice.to_owned())).ok()
}

/// The dock's grouping policy, defaulting to [`DockGrouping::ByApplication`] whenever the
/// setting cannot be read or understood — grouping by canonical `AppId` is the schema default
/// and the behaviour the design is built around.
pub async fn dock_grouping(client: &impl SettingsClient) -> DockGrouping {
    client
        .get(DOCK_GROUPING_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_dock_grouping)
        .unwrap_or(DockGrouping::ByApplication)
}

/// The recent-apps list, newest first. An unreadable setting is an empty list — a cold session,
/// which is what the strip itself shows.
pub async fn load_recent_apps(client: &impl SettingsClient) -> crate::recents::RecentApps {
    let ids = client
        .get(RECENT_APPS_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_str_list)
        .unwrap_or_default();
    crate::recents::RecentApps::from_ids(ids)
}

/// The `SettingValue` JSON `Settings1.Set` expects for the recent-apps list.
pub fn recent_apps_json(ids: &[String]) -> String {
    serde_json::json!({ "type": "str-list", "value": ids }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Result<String, ()>);

    #[async_trait]
    impl SettingsClient for Fake {
        async fn get(&self, _key: &str) -> Result<String> {
            match &self.0 {
                Ok(value) => Ok(value.clone()),
                Err(()) => anyhow::bail!("no session bus"),
            }
        }
    }

    #[test]
    fn every_schema_choice_decodes_to_its_policy() {
        for (choice, expected) in [
            ("by-application", DockGrouping::ByApplication),
            ("by-instance", DockGrouping::ByInstance),
            ("separate-windows", DockGrouping::SeparateWindows),
        ] {
            let json = format!(r#"{{"type":"choice","value":"{choice}"}}"#);
            assert_eq!(parse_dock_grouping(&json), Some(expected));
        }
    }

    #[test]
    fn an_unknown_or_malformed_value_decodes_to_nothing_rather_than_a_wrong_policy() {
        assert_eq!(
            parse_dock_grouping(r#"{"type":"choice","value":"by-phase-of-moon"}"#),
            None
        );
        assert_eq!(parse_dock_grouping("not json"), None);
        assert_eq!(parse_dock_grouping(r#"{"type":"bool","value":true}"#), None);
    }

    #[test]
    fn reduced_motion_uses_the_typed_boolean_payload() {
        assert_eq!(parse_bool(r#"{"type":"bool","value":true}"#), Some(true));
        assert_eq!(parse_bool(r#"{"type":"choice","value":"true"}"#), None);
    }

    #[tokio::test]
    async fn a_configured_policy_is_honoured() {
        let fake = Fake(Ok(r#"{"type":"choice","value":"by-instance"}"#.to_owned()));
        assert_eq!(dock_grouping(&fake).await, DockGrouping::ByInstance);
    }

    #[tokio::test]
    async fn an_unreachable_settings_service_still_yields_a_usable_dock() {
        // The shell must draw something when sessiond is not up: the schema default, not an error.
        let fake = Fake(Err(()));
        assert_eq!(dock_grouping(&fake).await, DockGrouping::ByApplication);
    }

    #[test]
    fn a_str_list_payload_round_trips_the_recent_ids() {
        let json = recent_apps_json(&[
            "org.legeos.Files".to_owned(),
            "org.legeos.Terminal".to_owned(),
        ]);
        assert_eq!(
            parse_str_list(&json),
            Some(vec![
                "org.legeos.Files".to_owned(),
                "org.legeos.Terminal".to_owned()
            ])
        );
    }

    #[tokio::test]
    async fn missing_recents_are_an_empty_list_not_an_error() {
        let fake = Fake(Err(()));
        assert!(load_recent_apps(&fake).await.ids().is_empty());
    }
}
