//! Per-user file-view preferences: how folders and shortcut panes present themselves — the
//! sort descriptor and list/thumbnail mode of `lege_types::file_sort` — as one global default
//! plus per-path overrides. Stored as a single JSON settings value
//! (`files.view-preferences`), so it rides the existing settings store, history and reset
//! machinery rather than growing a second persistence path.
//!
//! Global default and overrides reset independently: clearing an override falls back to the
//! default, and changing the default never rewrites overrides.

use lege_types::file_sort::{SortDescriptor, ViewMode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The settings key the encoded preferences live under.
pub const FILE_VIEW_SETTING: &str = "files.view-preferences";

/// One surface's presentation choice.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewPreference {
    pub sort: SortDescriptor,
    pub view: ViewMode,
}

/// Every stored choice: the default, and exact-path overrides for folders and panes.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileViewPreferences {
    #[serde(default)]
    pub default: ViewPreference,
    #[serde(default)]
    pub overrides: BTreeMap<PathBuf, ViewPreference>,
}

impl FileViewPreferences {
    /// The preference in effect for `path`: its exact override, else the global default.
    #[must_use]
    pub fn effective(&self, path: &Path) -> ViewPreference {
        self.overrides.get(path).copied().unwrap_or(self.default)
    }

    /// Whether `path` has its own override (as opposed to following the default).
    #[must_use]
    pub fn has_override(&self, path: &Path) -> bool {
        self.overrides.contains_key(path)
    }

    pub fn set_default(&mut self, preference: ViewPreference) {
        self.default = preference;
    }

    pub fn set_override(&mut self, path: &Path, preference: ViewPreference) {
        self.overrides.insert(path.to_path_buf(), preference);
    }

    /// Remove `path`'s override so it follows the default again. Returns whether one existed.
    pub fn clear_override(&mut self, path: &Path) -> bool {
        self.overrides.remove(path).is_some()
    }

    /// Reset the global default without touching any override.
    pub fn reset_default(&mut self) {
        self.default = ViewPreference::default();
    }

    /// The JSON this round-trips through the settings store as.
    ///
    /// # Errors
    /// Serialization of this plain data model cannot practically fail; the result type exists
    /// for the caller's uniform error handling.
    pub fn encode(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// Decode a stored value. An empty or absent value is everything-default, so a fresh
    /// session needs no migration step.
    #[must_use]
    pub fn decode(stored: &str) -> Self {
        if stored.trim().is_empty() {
            return Self::default();
        }
        serde_json::from_str(stored).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::file_sort::SortField;

    fn thumbnails_by_size() -> ViewPreference {
        ViewPreference {
            sort: SortDescriptor {
                field: SortField::Size,
                ascending: false,
                folders_first: true,
            },
            view: ViewMode::Thumbnails,
        }
    }

    /// Global default plus per-path overrides round-trip through the stored encoding — the
    /// same "survives restart" path the settings store gives every value.
    #[test]
    fn file_view_preferences_round_trip_and_survive_a_restart() {
        let mut preferences = FileViewPreferences::default();
        preferences.set_default(ViewPreference {
            sort: SortDescriptor::default(),
            view: ViewMode::List,
        });
        preferences.set_override(Path::new("/home/a/Desktop"), thumbnails_by_size());

        let stored = preferences.encode().expect("encode");
        let restarted = FileViewPreferences::decode(&stored);
        assert_eq!(restarted, preferences, "exactly what was stored comes back");
        assert_eq!(
            restarted.effective(Path::new("/home/a/Desktop")),
            thumbnails_by_size()
        );
        assert_eq!(
            restarted.effective(Path::new("/home/a/Documents")),
            restarted.default,
            "paths without an override follow the default"
        );

        // Absent or empty storage is a fresh default, never an error.
        assert_eq!(
            FileViewPreferences::decode(""),
            FileViewPreferences::default()
        );
        assert_eq!(
            FileViewPreferences::decode("not json"),
            FileViewPreferences::default()
        );
    }

    /// The default and each override reset independently.
    #[test]
    fn file_view_preferences_reset_default_and_overrides_independently() {
        let mut preferences = FileViewPreferences::default();
        preferences.set_default(thumbnails_by_size());
        preferences.set_override(Path::new("/home/a/Desktop"), ViewPreference::default());

        preferences.reset_default();
        assert_eq!(preferences.default, ViewPreference::default());
        assert!(
            preferences.has_override(Path::new("/home/a/Desktop")),
            "resetting the default leaves overrides alone"
        );

        assert!(preferences.clear_override(Path::new("/home/a/Desktop")));
        assert!(!preferences.has_override(Path::new("/home/a/Desktop")));
        assert!(
            !preferences.clear_override(Path::new("/home/a/Desktop")),
            "clearing twice reports nothing to clear"
        );
        assert_eq!(
            preferences.effective(Path::new("/home/a/Desktop")),
            preferences.default,
            "a cleared override falls back to the default"
        );
    }
}
