//! Stored per-source power policy and truthful runtime observations.
//! Sessiond schedules from compositor-owned idle samples; hardware owners perform effects.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use templar_types::{
    compositor_control::ActivityObservation,
    settings::{Capability, Revision},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LidAction {
    Ignore,
    Lock,
    Suspend,
    Hibernate,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelPolicy {
    #[default]
    Keep,
    ExternalOnly,
    AllowHeadless,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PowerSource {
    Battery,
    Mains,
}

/// Stored policy is not applied state. [`PowerObservation`] names the selected source,
/// actual native readback, capabilities and armed policy separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerProfile {
    pub dim_after: Option<Duration>,
    pub screen_off_after: Option<Duration>,
    pub lock_after: Option<Duration>,
    pub suspend_after: Option<Duration>,
    pub lid_closed: LidAction,
    /// Action when actual connected and enabled external outputs are available.
    pub lid_closed_docked: LidAction,
    /// Additive fields keep the original lid-action JSON readable by retained binaries.
    /// Applied only for awake Ignore/Lock actions, never before a sleep coverage barrier.
    #[serde(default)]
    pub lid_panel: PanelPolicy,
    #[serde(default)]
    pub lid_panel_docked: PanelPolicy,
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
                lid_panel: PanelPolicy::Keep,
                lid_panel_docked: PanelPolicy::Keep,
            },
            PowerSource::Mains => Self {
                dim_after: Some(Duration::from_secs(600)),
                screen_off_after: Some(Duration::from_secs(900)),
                lock_after: Some(Duration::from_secs(900)),
                suspend_after: None,
                lid_closed: LidAction::Suspend,
                lid_closed_docked: LidAction::Ignore,
                lid_panel: PanelPolicy::Keep,
                lid_panel_docked: PanelPolicy::Keep,
            },
        }
    }
}

impl PowerSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Battery => "battery",
            Self::Mains => "mains",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerFeature {
    pub capability: Capability,
    /// The serialized policy owner has armed this supported selected policy, not performed it.
    pub armed: bool,
    pub error: Option<String>,
}
impl PowerFeature {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            capability: Capability::Unavailable {
                reason: reason.into(),
            },
            armed: false,
            error: None,
        }
    }
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            capability: Capability::Unsupported {
                reason: reason.into(),
            },
            armed: false,
            error: None,
        }
    }
    pub fn available(armed: bool) -> Self {
        Self {
            capability: Capability::Available {
                requires_restart: false,
            },
            armed,
            error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerObservation {
    pub revision: Revision,
    pub battery: Option<PowerProfile>,
    pub mains: Option<PowerProfile>,
    pub selected_source: Option<PowerSource>,
    pub selected_profile: Option<PowerProfile>,
    pub session_active: Option<bool>,
    pub inhibited: Option<bool>,
    pub protected_covered: Option<bool>,
    /// Meaningful only when screen_off_observed is true; None then means disabled.
    pub screen_off_after_ms: Option<u64>,
    pub screen_off_observed: bool,
    pub screen_off: PowerFeature,
    pub dim: PowerFeature,
    pub lock: PowerFeature,
    pub suspend: PowerFeature,
    pub hibernate: PowerFeature,
    pub lid: PowerFeature,
    pub lid_present: Option<bool>,
    pub lid_closed: Option<bool>,
    pub docked: Option<bool>,
    pub lid_inhibitor_owned: bool,
    pub sleep_delay_owned: bool,
    pub dimmed_percent: Option<u32>,
    pub sleep_pending: Option<String>,
    pub errors: Vec<String>,
}

/// No saved Locked edge, desired profile or failed inventory can authorize sleep.
pub fn protected_sleep_ready(activity: &ActivityObservation) -> bool {
    activity.session_active
        && activity.locked
        && activity.lock_generation.is_some()
        && activity.protected_covered
        && matches!(activity.idle_control, Capability::Available { .. })
        && matches!(activity.display_control, Capability::Available { .. })
}

/// One-shot policy deadlines; callers use only a fresh compositor elapsed sample.
pub fn deadline_after(
    interval: Option<Duration>,
    elapsed: Duration,
    completed: bool,
) -> Option<Duration> {
    if completed {
        None
    } else {
        interval.map(|interval| interval.saturating_sub(elapsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deadlines_do_not_poll_or_rearm_completed_actions() {
        assert_eq!(
            deadline_after(Some(Duration::from_secs(5)), Duration::from_secs(2), false),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            deadline_after(Some(Duration::from_secs(5)), Duration::from_secs(6), false),
            Some(Duration::ZERO)
        );
        assert_eq!(deadline_after(None, Duration::from_secs(6), false), None);
        assert_eq!(
            deadline_after(Some(Duration::from_secs(5)), Duration::ZERO, true),
            None
        );
    }
    #[test]
    fn sleep_needs_current_native_generation_and_coverage() {
        let revision = Revision {
            owner_epoch: uuid::Uuid::nil(),
            generation: 0,
        };
        let mut activity = ActivityObservation {
            revision,
            display_revision: revision,
            input_revision: revision,
            activity_serial: 1,
            idle_elapsed_ms: 500,
            session_active: true,
            locked: true,
            lock_generation: Some(1),
            protected_covered: true,
            idle: false,
            blanked: false,
            inhibited: false,
            idle_timeout_ms: None,
            outputs: Vec::new(),
            idle_control: Capability::Available {
                requires_restart: false,
            },
            input_control: Capability::Available {
                requires_restart: false,
            },
            display_control: Capability::Available {
                requires_restart: false,
            },
        };
        assert!(protected_sleep_ready(&activity));
        activity.protected_covered = false;
        assert!(!protected_sleep_ready(&activity));
        activity.protected_covered = true;
        activity.lock_generation = None;
        assert!(!protected_sleep_ready(&activity));
        activity.lock_generation = Some(2);
        activity.session_active = false;
        assert!(!protected_sleep_ready(&activity));
        activity.session_active = true;
        activity.display_control = Capability::Unavailable {
            reason: "actual readback inaccessible".into(),
        };
        assert!(!protected_sleep_ready(&activity));
    }
    #[test]
    fn additive_panel_fields_keep_the_original_lid_reader_compatible() {
        #[derive(Deserialize)]
        struct OriginalLidReader {
            lid_closed: LidAction,
            lid_closed_docked: LidAction,
        }
        let mut original =
            serde_json::to_value(PowerProfile::default_for(PowerSource::Mains)).unwrap();
        original.as_object_mut().unwrap().remove("lid_panel");
        original.as_object_mut().unwrap().remove("lid_panel_docked");
        let mut current: PowerProfile = serde_json::from_value(original).unwrap();
        assert_eq!(current.lid_panel, PanelPolicy::Keep);
        current.lid_closed = LidAction::Ignore;
        current.lid_panel = PanelPolicy::ExternalOnly;
        let old: OriginalLidReader =
            serde_json::from_value(serde_json::to_value(current).unwrap()).unwrap();
        assert_eq!(old.lid_closed, LidAction::Ignore);
        assert_eq!(old.lid_closed_docked, LidAction::Ignore);
    }
}
