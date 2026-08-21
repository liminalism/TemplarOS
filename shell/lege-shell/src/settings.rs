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
}
