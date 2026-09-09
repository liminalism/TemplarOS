//! The shell's own settings, read from `org.templaros.Settings1` — never from a private file.
//!
//! Planning doc 01 §7 replaces "settings are scattered private implementation details" with one
//! typed schema that has a GUI, a CLI and a change history. The shell is a client of that schema
//! like anything else (`crates/templar-session-core/src/settings.rs` holds the baseline schema).
//!
//! A missing or unreadable setting is not an error here. The shell is decorative and restartable;
//! it must render *something* sensible when sessiond is not up yet, so every lookup falls back to
//! the schema's own default rather than refusing to draw.

use anyhow::{Context, Result};
use async_trait::async_trait;
use templar_client::proxies::SettingsProxy;

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
/// `shell.appearance.theme` — `light` or `dark`.
pub const THEME_KEY: &str = "shell.appearance.theme";
/// `shell.appearance.accent` — a bundled accent's name, or `default`.
pub const ACCENT_KEY: &str = "shell.appearance.accent";
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

/// The bundled accents, by the name the schema offers. Names rather than free hex: an accent has
/// to keep enough contrast against the shell it sits on for text drawn in it to be readable, and
/// a text field where a person can type `#fefefe` is a text field where they can make their own
/// desktop illegible. `"default"` (and anything unrecognised) keeps the palette's own.
const ACCENTS: &[(&str, u32)] = &[
    ("teal", 0x000e_7482),
    ("blue", 0x001d_5fb8),
    ("violet", 0x006a_3fb5),
    ("green", 0x001f_7a4a),
    ("amber", 0x008a_5a09),
    ("red", 0x00a8_2f28),
];

/// The palette the shell should draw with, from `shell.appearance.*`.
///
/// Unreadable settings are the light palette, which is the schema's default and the look this
/// shell has always had: a session whose daemon has not started yet must still draw.
pub async fn appearance(client: &impl SettingsClient) -> crate::surface::design::Palette {
    let palette = match client
        .get(THEME_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .as_deref()
    {
        Some("dark") => crate::surface::design::DARK,
        _ => crate::surface::design::LIGHT,
    };
    let accent = client
        .get(ACCENT_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .and_then(|name| {
            ACCENTS
                .iter()
                .find_map(|(known, colour)| (*known == name).then_some(*colour))
        });
    match accent {
        Some(accent) => crate::surface::design::with_accent(palette, accent),
        None => palette,
    }
}

/// The palette from `Settings1`, or the light one when the daemon is not answering.
///
/// Read once, before the first frame. The palette is fixed for the life of the process because
/// changing it means invalidating every cached widget bitmap, and the shell restarts in about
/// half a second — the compositor is the only thing that cannot.
pub async fn load_appearance() -> crate::surface::design::Palette {
    match crate::data::session().await {
        Ok(connection) => match SettingsProxy::new(&connection).await {
            Ok(proxy) => appearance(&proxy).await,
            Err(_) => crate::surface::design::LIGHT,
        },
        Err(_) => crate::surface::design::LIGHT,
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

    /// A per-key fake, because the appearance read asks two different questions and a client
    /// that answers both with the same string cannot tell them apart.
    struct Keyed(std::collections::HashMap<&'static str, String>);

    #[async_trait]
    impl SettingsClient for Keyed {
        async fn get(&self, key: &str) -> Result<String> {
            self.0
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unset"))
        }
    }

    fn keyed(pairs: &[(&'static str, &str)]) -> Keyed {
        Keyed(
            pairs
                .iter()
                .map(|(key, value)| {
                    (
                        *key,
                        serde_json::json!({"type": "choice", "value": value}).to_string(),
                    )
                })
                .collect(),
        )
    }

    /// The shell must draw before sessiond is up, so an unreadable setting is the light palette
    /// — the schema's default, and the look this shell has always had.
    #[tokio::test]
    async fn appearance_defaults_to_light_when_unreadable() {
        assert_eq!(
            appearance(&Fake(Err(()))).await,
            crate::surface::design::LIGHT
        );
        assert_eq!(appearance(&keyed(&[])).await, crate::surface::design::LIGHT);
        // A value the schema does not offer is not a reason to draw nothing.
        assert_eq!(
            appearance(&keyed(&[(THEME_KEY, "solarized")])).await,
            crate::surface::design::LIGHT
        );
    }

    #[tokio::test]
    async fn appearance_reads_the_theme_and_the_named_accent() {
        assert_eq!(
            appearance(&keyed(&[(THEME_KEY, "dark")])).await,
            crate::surface::design::DARK
        );
        // "default" keeps the palette's own accent; a named one replaces it and takes the soft
        // accent with it, so the two can never disagree.
        assert_eq!(
            appearance(&keyed(&[(THEME_KEY, "dark"), (ACCENT_KEY, "default")])).await,
            crate::surface::design::DARK
        );
        let violet = appearance(&keyed(&[(ACCENT_KEY, "violet")])).await;
        assert_eq!(violet.accent, 0x006a_3fb5);
        assert_ne!(
            violet.accent_soft,
            crate::surface::design::LIGHT.accent_soft
        );
        assert_eq!(violet.ground, crate::surface::design::LIGHT.ground);
        // Every accent the schema offers has to resolve to its own colour, or a Settings row
        // does nothing. Teal is the light palette's own accent, which is the point of it being
        // the default rather than a reason to leave it out of the list.
        for (name, colour) in ACCENTS {
            let palette = appearance(&keyed(&[(ACCENT_KEY, name)])).await;
            assert_eq!(palette.accent, *colour, "the {name} accent did not resolve");
        }
        assert_eq!(
            ACCENTS
                .iter()
                .find_map(|(name, colour)| (*name == "teal").then_some(*colour)),
            Some(crate::surface::design::LIGHT.accent),
            "teal is the default accent, so the two must be the same colour"
        );
    }

    #[test]
    fn reduced_motion_uses_the_typed_boolean_payload() {
        assert_eq!(parse_bool(r#"{"type":"bool","value":true}"#), Some(true));
        assert_eq!(parse_bool(r#"{"type":"choice","value":"true"}"#), None);
    }

    #[test]
    fn a_str_list_payload_round_trips_the_recent_ids() {
        let json = recent_apps_json(&[
            "org.templaros.Files".to_owned(),
            "org.templaros.Terminal".to_owned(),
        ]);
        assert_eq!(
            parse_str_list(&json),
            Some(vec![
                "org.templaros.Files".to_owned(),
                "org.templaros.Terminal".to_owned()
            ])
        );
    }

    #[tokio::test]
    async fn missing_recents_are_an_empty_list_not_an_error() {
        let fake = Fake(Err(()));
        assert!(load_recent_apps(&fake).await.ids().is_empty());
    }
}
