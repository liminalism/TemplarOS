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
/// `shell.home-garden.theme` — how grown Places are drawn (`surface::place_styles::themes`).
pub const GARDEN_THEME_KEY: &str = "shell.home-garden.theme";
/// `shell.appearance.theme` — `light` or `dark`.
pub const THEME_KEY: &str = "shell.appearance.theme";
/// `shell.appearance.accent` — a bundled accent's name, or `default`.
pub const ACCENT_KEY: &str = "shell.appearance.accent";
/// Recently launched applications, newest first. Not a Home Field layout key: it is launch
/// history for the search strip, and lives under `shell.launcher.*` so the Settings Home Field
/// page does not offer it as a control.
pub const RECENT_APPS_KEY: &str = "shell.launcher.recent-apps";

/// `shell.rails.mode` — `gesture` (the bottom-centre gate reveals the rails) or `bar` (the
/// classic always-there island and cluster).
pub const RAILS_MODE_KEY: &str = "shell.rails.mode";
/// `shell.rails.swap` — `false` puts running applications on the left rail, the system on the
/// right; `true` exchanges them.
pub const RAILS_SWAP_KEY: &str = "shell.rails.swap";
/// `shell.rails.lanes` — `off`, `contiguous` or `rows`: how the running-apps strip groups
/// windows by kind of program (`crate::taskbar::LaneMode`).
pub const RAILS_LANES_KEY: &str = "shell.rails.lanes";
/// `shell.rails.gate-width`, logical px.
pub const RAILS_GATE_WIDTH_KEY: &str = "shell.rails.gate-width";
/// `shell.rails.gate-height`, logical px.
pub const RAILS_GATE_HEIGHT_KEY: &str = "shell.rails.gate-height";
/// `shell.rails.learning` — the compositor outlines the gate.
pub const RAILS_LEARNING_KEY: &str = "shell.rails.learning";
/// `shell.rails.exceptions` — app ids under whose windows the gate never arms.
pub const RAILS_EXCEPTIONS_KEY: &str = "shell.rails.exceptions";

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
    /// The Place theme's name, already resolved against the themes this build ships: an unknown
    /// name has become the default here, so everything downstream draws a theme that exists.
    pub garden_theme: &'static str,
}

impl Default for HomeFieldSettings {
    fn default() -> Self {
        Self {
            tile_size: crate::field_model::TILE_SIZE,
            throw: false,
            previews: PreviewMode::Static,
            preview_rate: 1,
            reduced_motion: false,
            garden_theme: crate::surface::place_styles::DEFAULT_THEME,
        }
    }
}

/// How the bottom of the screen is reached: gesture rails, or the classic bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RailSettings {
    /// `true` for gesture rails (`shell.rails.mode` = `gesture`), `false` for the classic bar.
    pub gesture: bool,
    pub swap: bool,
    pub gate_width: u32,
    pub gate_height: u32,
    pub learning: bool,
    pub exceptions: Vec<String>,
}

impl Default for RailSettings {
    fn default() -> Self {
        Self {
            gesture: true,
            swap: false,
            gate_width: 240,
            gate_height: 96,
            learning: false,
            exceptions: Vec::new(),
        }
    }
}

/// The rails' settings, each falling back to its schema default on its own.
pub async fn rails(client: &impl SettingsClient) -> RailSettings {
    let defaults = RailSettings::default();
    let read = |key: &'static str| async move { client.get(key).await.ok() };
    let gesture = read(RAILS_MODE_KEY)
        .await
        .as_deref()
        .and_then(parse_choice)
        .map_or(defaults.gesture, |mode| mode != "bar");
    let swap = read(RAILS_SWAP_KEY)
        .await
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(defaults.swap);
    let gate_width = read(RAILS_GATE_WIDTH_KEY)
        .await
        .as_deref()
        .and_then(parse_int)
        .map_or(defaults.gate_width, |width| width.clamp(120, 800) as u32);
    let gate_height = read(RAILS_GATE_HEIGHT_KEY)
        .await
        .as_deref()
        .and_then(parse_int)
        .map_or(defaults.gate_height, |height| height.clamp(40, 300) as u32);
    let learning = read(RAILS_LEARNING_KEY)
        .await
        .as_deref()
        .and_then(parse_bool)
        .unwrap_or(defaults.learning);
    let exceptions = read(RAILS_EXCEPTIONS_KEY)
        .await
        .as_deref()
        .and_then(parse_str_list)
        .unwrap_or_default()
        .into_iter()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .collect();
    RailSettings {
        gesture,
        swap,
        gate_width,
        gate_height,
        learning,
        exceptions,
    }
}

/// The activity lane mode, falling back to the schema default when unreadable or unknown.
pub async fn lanes(client: &impl SettingsClient) -> crate::taskbar::LaneMode {
    client
        .get(RAILS_LANES_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .and_then(|choice| crate::taskbar::LaneMode::parse(&choice))
        .unwrap_or_default()
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
    let garden_theme = client
        .get(GARDEN_THEME_KEY)
        .await
        .ok()
        .as_deref()
        .and_then(parse_choice)
        .map_or(defaults.garden_theme, |name| {
            crate::surface::place_styles::theme_named(&name).name
        });
    HomeFieldSettings {
        tile_size,
        throw,
        previews,
        preview_rate,
        reduced_motion,
        garden_theme,
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

    /// An unreadable schema is the schema's defaults: gesture rails, apps on the left.
    #[tokio::test]
    async fn rail_settings_fall_back_to_the_schema_defaults() {
        assert_eq!(rails(&Fake(Err(()))).await, RailSettings::default());
        assert!(RailSettings::default().gesture);
    }

    #[tokio::test]
    async fn rail_settings_read_the_mode_and_clamp_the_gate() {
        let mut pairs = std::collections::HashMap::new();
        pairs.insert(
            RAILS_MODE_KEY,
            r#"{"type":"choice","value":"bar"}"#.to_owned(),
        );
        pairs.insert(
            RAILS_GATE_WIDTH_KEY,
            r#"{"type":"int","value":5000}"#.to_owned(),
        );
        pairs.insert(
            RAILS_EXCEPTIONS_KEY,
            r#"{"type":"str-list","value":[" org.mpv.Mpv ",""]}"#.to_owned(),
        );
        let settings = rails(&Keyed(pairs)).await;
        assert!(!settings.gesture);
        assert_eq!(settings.gate_width, 800);
        assert_eq!(settings.gate_height, 96);
        assert_eq!(settings.exceptions, vec!["org.mpv.Mpv".to_owned()]);
    }

    #[tokio::test]
    async fn lanes_read_the_choice_and_fall_back_on_anything_else() {
        use crate::taskbar::LaneMode;
        let with = |value: &str| {
            let mut pairs = std::collections::HashMap::new();
            pairs.insert(
                RAILS_LANES_KEY,
                format!(r#"{{"type":"choice","value":"{value}"}}"#),
            );
            Keyed(pairs)
        };
        assert_eq!(lanes(&with("rows")).await, LaneMode::Rows);
        assert_eq!(lanes(&with("off")).await, LaneMode::Off);
        assert_eq!(lanes(&with("sideways")).await, LaneMode::Contiguous);
        assert_eq!(lanes(&Fake(Err(()))).await, LaneMode::Contiguous);
    }

    #[tokio::test]
    async fn missing_recents_are_an_empty_list_not_an_error() {
        let fake = Fake(Err(()));
        assert!(load_recent_apps(&fake).await.ids().is_empty());
    }
}
