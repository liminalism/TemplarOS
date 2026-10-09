//! `templar power` — the stored idle and lid policy per power source
//! (`crates/templar-session-core/src/power.rs`).
//!
//! Stored profiles are printed separately from `Power1.Inspect` native readback, capabilities,
//! armed policy and errors. Brightness/battery remain `templar status` hardware readings.

use crate::humanize::duration_secs;
use anyhow::{Context, Result};
use templar_session_core::power::{LidAction, PanelPolicy, PowerProfile};

pub fn parse_profile(profile_json: &str) -> Result<PowerProfile> {
    serde_json::from_str(profile_json).context("parsing Power1 profile JSON")
}

fn render_after(label: &str, after: Option<std::time::Duration>) -> String {
    match after {
        Some(d) => format!("{label}: after {}", duration_secs(d.as_secs())),
        None => format!("{label}: never"),
    }
}
fn lid_action(action: LidAction) -> &'static str {
    match action {
        LidAction::Ignore => "ignore",
        LidAction::Lock => "lock",
        LidAction::Suspend => "suspend",
        LidAction::Hibernate => "hibernate",
    }
}

fn panel_policy(policy: PanelPolicy) -> &'static str {
    match policy {
        PanelPolicy::Keep => "keep",
        PanelPolicy::ExternalOnly => "external-only",
        PanelPolicy::AllowHeadless => "allow-headless",
    }
}

pub fn render_profile(source: &str, profile: &PowerProfile) -> String {
    format!(
        "stored {source}\n  {}\n  {}\n  {}\n  {}\n  lid closed: {} · panel {}\n  lid closed (docked): {} · panel {}",
        render_after("dim", profile.dim_after),
        render_after("screen off", profile.screen_off_after),
        render_after("lock", profile.lock_after),
        render_after("suspend", profile.suspend_after),
        lid_action(profile.lid_closed),
        panel_policy(profile.lid_panel),
        lid_action(profile.lid_closed_docked),
        panel_policy(profile.lid_panel_docked),
    )
}
