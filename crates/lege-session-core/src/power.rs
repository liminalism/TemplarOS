//! Power policy: idle, lid, brightness and battery via logind/upower. The exclusive
//! `power-manager` role; indicators only display state.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LidAction {
    Ignore,
    Lock,
    Suspend,
    Hibernate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerSource {
    Battery,
    Mains,
}

/// Idle/lid policy for one power source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerProfile {
    pub dim_after: Option<Duration>,
    pub screen_off_after: Option<Duration>,
    pub lock_after: Option<Duration>,
    pub suspend_after: Option<Duration>,
    pub lid_closed: LidAction,
    /// External output attached: keep running with the lid closed.
    pub lid_closed_docked: LidAction,
}

impl PowerProfile {
    pub fn default_for(source: PowerSource) -> Self {
        match source {
            PowerSource::Battery => Self {
                dim_after: Some(Duration::from_secs(120)),
                screen_off_after: Some(Duration::from_secs(300)),
                lock_after: Some(Duration::from_secs(300)),
                suspend_after: Some(Duration::from_secs(900)),
                lid_closed: LidAction::Suspend,
                lid_closed_docked: LidAction::Lock,
            },
            PowerSource::Mains => Self {
                dim_after: Some(Duration::from_secs(600)),
                screen_off_after: Some(Duration::from_secs(900)),
                lock_after: Some(Duration::from_secs(900)),
                suspend_after: None,
                lid_closed: LidAction::Suspend,
                lid_closed_docked: LidAction::Ignore,
            },
        }
    }
}
