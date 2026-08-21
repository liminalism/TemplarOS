//! `lege power` — idle/lid/brightness policy per power source (`docs/architecture/session.md`,
//! `crates/lege-session-core/src/power.rs`), the exclusive `power-manager` role.
//!
//! Every D-Bus call goes through [`PowerClient`] so the rendering below is testable with a
//! fixture, never a live `org.legeos.Power1`.

use crate::humanize::duration_secs;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What `lege power` needs from `org.legeos.Power1`.
#[async_trait]
pub trait PowerClient {
    async fn get_profile(&self, source: &str) -> Result<String>;
    async fn set_profile(&self, source: &str, profile_json: &str) -> Result<()>;
    async fn reset_profile(&self, source: &str) -> Result<()>;
}

#[async_trait]
impl PowerClient for lege_client::proxies::PowerProxy<'_> {
    async fn get_profile(&self, source: &str) -> Result<String> {
        self.get_profile(source).await.context("Power1.GetProfile")
    }
    async fn set_profile(&self, source: &str, profile_json: &str) -> Result<()> {
        self.set_profile(source, profile_json)
            .await
            .context("Power1.SetProfile")
    }
    async fn reset_profile(&self, source: &str) -> Result<()> {
        self.reset_profile(source)
            .await
            .context("Power1.ResetProfile")
    }
}

/// Mirrors `lege_session_core::power::LidAction`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LidAction {
    Ignore,
    Lock,
    Suspend,
    Hibernate,
}

impl std::fmt::Display for LidAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Ignore => "ignore",
            Self::Lock => "lock",
            Self::Suspend => "suspend",
            Self::Hibernate => "hibernate",
        };
        formatter.write_str(text)
    }
}

/// Mirrors `lege_session_core::power::PowerProfile` exactly, including its use of
/// `std::time::Duration` (serde's built-in `{"secs":N,"nanos":N}` encoding), so this struct
/// decodes whatever `Power1.GetProfile` sends with no format of its own to keep in sync.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PowerProfile {
    pub dim_after: Option<std::time::Duration>,
    pub screen_off_after: Option<std::time::Duration>,
    pub lock_after: Option<std::time::Duration>,
    pub suspend_after: Option<std::time::Duration>,
    pub lid_closed: LidAction,
    pub lid_closed_docked: LidAction,
}

pub fn parse_profile(profile_json: &str) -> Result<PowerProfile> {
    serde_json::from_str(profile_json).context("parsing Power1 profile JSON")
}

fn render_after(label: &str, after: Option<std::time::Duration>) -> String {
    match after {
        Some(d) => format!("{label}: after {}", duration_secs(d.as_secs())),
        None => format!("{label}: never"),
    }
}

pub fn render_profile(source: &str, profile: &PowerProfile) -> String {
    format!(
        "{source}\n  {}\n  {}\n  {}\n  {}\n  lid closed: {}\n  lid closed (docked): {}",
        render_after("dim", profile.dim_after),
        render_after("screen off", profile.screen_off_after),
        render_after("lock", profile.lock_after),
        render_after("suspend", profile.suspend_after),
        profile.lid_closed,
        profile.lid_closed_docked,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "dim_after": {"secs":120,"nanos":0},
        "screen_off_after": {"secs":300,"nanos":0},
        "lock_after": {"secs":300,"nanos":0},
        "suspend_after": {"secs":900,"nanos":0},
        "lid_closed": "suspend",
        "lid_closed_docked": "lock"
    }"#;

    #[test]
    fn profile_parses_and_renders() {
        let profile = parse_profile(SAMPLE).unwrap();
        assert_eq!(profile.lid_closed, LidAction::Suspend);
        let rendered = render_profile("battery", &profile);
        assert!(rendered.contains("battery"));
        assert!(rendered.contains("dim: after 2m"));
        assert!(rendered.contains("lid closed: suspend"));
        assert!(rendered.contains("lid closed (docked): lock"));
    }

    #[test]
    fn never_is_rendered_for_a_null_duration() {
        let mut profile = parse_profile(SAMPLE).unwrap();
        profile.suspend_after = None;
        assert!(render_profile("mains", &profile).contains("suspend: never"));
    }
}
