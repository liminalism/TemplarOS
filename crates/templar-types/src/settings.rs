//! Canonical destinations shared by Settings, launchers and diagnostics.
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum SettingsPage {
    System,
    Sound,
    Files,
    Appearance,
    Homegarden,
    Rails,
    HomeField,
    Associations,
    Background,
    Providers,
    Resources,
    History,
    Hardware,
    Network,
    Bluetooth,
    Displays,
    Input,
    Windows,
    DateTime,
}

impl SettingsPage {
    /// Navigation order, independent of the stable page discriminants used by view storage.
    pub const ALL: [Self; 19] = [
        Self::System,
        Self::DateTime,
        Self::Sound,
        Self::Network,
        Self::Bluetooth,
        Self::Displays,
        Self::Input,
        Self::Appearance,
        Self::Files,
        Self::HomeField,
        Self::Homegarden,
        Self::Rails,
        Self::Windows,
        Self::Associations,
        Self::Background,
        Self::Providers,
        Self::Resources,
        Self::History,
        Self::Hardware,
    ];
    pub const fn slug(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Appearance => "appearance",
            Self::Sound => "sound",
            Self::Files => "files",
            Self::Network => "network",
            Self::Bluetooth => "bluetooth",
            Self::Displays => "displays",
            Self::Input => "input",
            Self::Homegarden => "homegarden",
            Self::Rails => "rails",
            Self::HomeField => "home-field",
            Self::Associations => "associations",
            Self::Background => "background",
            Self::Providers => "providers",
            Self::Resources => "resources",
            Self::History => "history",
            Self::Hardware => "hardware",
            Self::Windows => "windows",
            Self::DateTime => "date-time",
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Appearance => "Appearance",
            Self::Sound => "Sound",
            Self::Files => "Files",
            Self::Network => "Network",
            Self::Bluetooth => "Bluetooth",
            Self::Displays => "Displays",
            Self::Input => "Input",
            Self::Homegarden => "Homegarden",
            Self::Rails => "Rails",
            Self::HomeField => "Home Field",
            Self::Associations => "Associations",
            Self::Background => "Background",
            Self::Providers => "Providers",
            Self::Resources => "Resources",
            Self::History => "History",
            Self::Hardware => "Hardware",
            Self::Windows => "Windows",
            Self::DateTime => "Date & Time",
        }
    }
    pub const fn search_labels(self) -> &'static [&'static str] {
        match self {
            Self::System => &["battery", "volume", "brightness", "power", "screen off"],
            Self::Sound => &["audio", "microphone", "speakers", "headphones", "volume"],
            Self::Files => &["index", "search", "roots", "monitoring", "bookmarks"],
            Self::Network => &["wifi", "wi-fi", "ethernet", "connections"],
            Self::Bluetooth => &["pairing", "wireless devices", "bluetooth"],
            Self::Displays => &[
                "monitor",
                "outputs",
                "resolution",
                "scale",
                "orientation",
                "television",
            ],
            Self::Input => &[
                "touchpad",
                "mouse",
                "keyboard",
                "tap",
                "scroll",
                "acceleration",
            ],
            Self::Appearance => &[
                "theme",
                "accent",
                "light",
                "dark",
                "wallpaper",
                "background image",
            ],
            Self::Homegarden => &["folders", "work", "ignore"],
            Self::Rails => &["windows", "titlebar", "exceptions"],
            Self::HomeField => &["home", "widgets", "layout"],
            Self::Associations => &["default apps", "open with", "file types"],
            Self::Background => &["background apps", "permissions"],
            Self::Providers => &["file manager", "file chooser", "network provider"],
            Self::Resources => &["processes", "memory", "cpu"],
            Self::History => &["events", "undo"],
            Self::Hardware => &["drivers", "devices"],
            Self::Windows => &["title tabs", "window corners", "hover controls"],
            Self::DateTime => &[
                "clock",
                "timezone",
                "date",
                "time",
                "locale",
                "automatic time",
            ],
        }
    }
}
impl fmt::Display for SettingsPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}
impl FromStr for SettingsPage {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        const ALIASES: &[(&str, &str)] = &[
            ("home", "home-field"),
            ("home_field", "home-field"),
            ("home-garden", "homegarden"),
            ("defaults", "associations"),
            ("power", "system"),
            ("audio", "sound"),
            ("volume", "sound"),
            ("index", "files"),
            ("search", "files"),
            ("display", "displays"),
            ("monitor", "displays"),
            ("outputs", "displays"),
            ("touchpad", "input"),
            ("mouse", "input"),
            ("keyboard", "input"),
        ];
        let slug = ALIASES
            .iter()
            .find_map(|(alias, slug)| alias.eq_ignore_ascii_case(value).then_some(*slug))
            .unwrap_or(value);
        Self::ALL
            .into_iter()
            .find(|page| page.slug().eq_ignore_ascii_case(slug))
            .or_else(|| {
                Self::ALL.into_iter().find(|page| {
                    page.label().eq_ignore_ascii_case(value)
                        || page
                            .search_labels()
                            .iter()
                            .any(|label| label.eq_ignore_ascii_case(value))
                })
            })
            .ok_or_else(|| format!("unknown Settings page {value:?}"))
    }
}

/// Whether an owned operation is usable now; absence always carries an actionable reason.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum Capability {
    Available { requires_restart: bool },
    Unsupported { reason: String },
    Unavailable { reason: String },
}

/// Generations are ordered only within one service-owner lifetime.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Revision {
    pub owner_epoch: uuid::Uuid,
    pub generation: u64,
}
impl Revision {
    pub fn new_owner() -> Self {
        Self {
            owner_epoch: uuid::Uuid::new_v4(),
            generation: 0,
        }
    }
    #[must_use]
    pub fn at_generation(self, generation: u64) -> Self {
        Self { generation, ..self }
    }
    pub fn supersedes(self, previous: Self) -> bool {
        self.owner_epoch != previous.owner_epoch || self.generation >= previous.generation
    }
}

/// Monotonic revision and values read together by the owning service.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Observation<T> {
    pub revision: Revision,
    pub value: T,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SettingObservation<T> {
    pub key: String,
    pub capability: Capability,
    pub value: T,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

/// Failed writes may still have a readable applied value. Neither field contains desired state.
#[derive(Clone, Debug)]
pub struct OperationResult<T> {
    pub observed: Result<T, OperationError>,
    pub write_error: Option<OperationError>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_audio_alias_wins_over_system_volume_search_label() {
        for alias in ["volume", "Volume", "VOLUME", "audio"] {
            assert_eq!(alias.parse::<SettingsPage>().unwrap(), SettingsPage::Sound);
        }
        assert_eq!(
            "battery".parse::<SettingsPage>().unwrap(),
            SettingsPage::System
        );
        assert_eq!(
            "file chooser".parse::<SettingsPage>().unwrap(),
            SettingsPage::Providers
        );
        assert!("unknown owned destination".parse::<SettingsPage>().is_err());
    }
}
