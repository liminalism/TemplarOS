//! The power policy a session *records*: what should happen after an idle interval, and what a
//! closing lid should do, for each power source.
//!
//! Pure data. Nothing in *this crate* enforces any of it, and only one field is enforced
//! anywhere: [`PowerProfile::screen_off_after`] is read by the shell and relayed to the
//! compositor, which turns the outputs off after that much idle time and back on at the next
//! input (`compositor/lege-comp/src/idle.rs`). The compositor is the only process that can see
//! the seat go quiet, so that is where it had to live.
//!
//! Every other field is still a value stored and not a thing that happens: there is no lid
//! listener and no call to logind or upower anywhere behind this, and setting `suspend_after` has
//! never suspended anything. Treating those as a description of what the machine will do is the
//! one mistake this module invites, and the reason the sentence is here.
//!
//! The rest is deliberate remaining scope. `dim_after` needs a backlight ramp (now
//! `org.legeos.Backlight1`), `lock_after` needs a lock surface to drive — and
//! `Session.Lifecycle1.Lock` is still a stub for want of a Wayland lock-manager client. See
//! `services/lege-sessiond/src/apis/power.rs`.
//!
//! Battery *state* is not here and never was: that is `org.legeos.Battery1`, a hardware reading
//! rather than stored policy.

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

/// The idle and lid policy stored for one power source.
///
/// `screen_off_after` is enforced (module doc). Every other interval here is a value the session
/// has been told to keep, not a timer that is running: a field being `Some` means "this is what
/// should happen", never "this is scheduled".
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
