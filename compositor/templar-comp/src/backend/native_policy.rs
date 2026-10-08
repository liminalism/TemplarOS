//! Deterministic, hardware-independent policy for native output ownership.
use smithay::reexports::drm::control::{Mode, ModeFlags, ModeTypeFlags};
use std::time::{Duration, Instant};
use templar_types::display::{Display, DisplayId};
pub const CONNECTOR_SETTLE: Duration = Duration::from_millis(750);
pub const LATE_EDID_REPROBE: Duration = Duration::from_millis(1000);
pub const FENCE_TIMEOUT: Duration = Duration::from_millis(50);
pub const WORKER_TIMEOUT: Duration = Duration::from_secs(5);

/// Logical primary follows live mapped heads, not the GPU that owns rendering. A valid
/// explicit choice wins; otherwise stable display identity makes failover order-independent.
pub fn logical_primary<'a>(
    outputs: impl Iterator<Item = (&'a Display, bool)>,
) -> Option<&'a DisplayId> {
    outputs
        .filter(|(display, mapped)| *mapped && display.connected && display.current.enabled)
        .min_by(|(left, _), (right, _)| {
            (!left.current.primary, &left.id).cmp(&(!right.current.primary, &right.id))
        })
        .map(|(display, _)| &display.id)
}

/// Project logical failover without changing the owner's exact scanout configuration.
/// In particular, worker frame admission still matches the configuration it actually applied.
pub fn project_logical_primary(inventory: &mut [Display], primary: Option<&DisplayId>) {
    for display in inventory {
        display.current.primary = primary == Some(&display.id);
    }
}

pub fn select_mode(modes: &[Mode], tv: bool) -> Option<Mode> {
    choose_mode(
        modes,
        tv,
        |mode| {
            mode.size() == (1920, 1080)
                && !mode.flags().contains(ModeFlags::INTERLACE)
                && (59_000..=61_000).contains(&smithay::output::Mode::from(*mode).refresh)
        },
        |mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED),
    )
}
pub fn choose_mode<M: Copy>(
    modes: &[M],
    tv: bool,
    safe: impl Fn(&M) -> bool,
    preferred: impl Fn(&M) -> bool,
) -> Option<M> {
    (if tv {
        modes.iter().find(|mode| safe(mode))
    } else {
        None
    })
    .or_else(|| modes.iter().find(|mode| preferred(mode)))
    .or_else(|| modes.first())
    .copied()
}
/// Identity retains every timing field for persistence. Readback deliberately tolerates
/// rounding and non-signal metadata; interlace/doublescan remain signal-critical.
pub fn same_signal(requested: &Mode, actual: &Mode) -> bool {
    same_signal_values(
        (
            requested.size(),
            requested.flags(),
            smithay::output::Mode::from(*requested).refresh,
        ),
        (
            actual.size(),
            actual.flags(),
            smithay::output::Mode::from(*actual).refresh,
        ),
    )
}
fn same_signal_values(
    requested: ((u16, u16), ModeFlags, i32),
    actual: ((u16, u16), ModeFlags, i32),
) -> bool {
    let critical = ModeFlags::INTERLACE | ModeFlags::DBLSCAN;
    requested.0 == actual.0
        && (requested.1 & critical) == (actual.1 & critical)
        && requested.2.abs_diff(actual.2) <= 1000
}
#[derive(Debug, Default)]
pub struct Settle {
    generation: u64,
    deadline: Option<Instant>,
}
impl Settle {
    pub fn changed(&mut self, now: Instant) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.deadline = Some(now + CONNECTOR_SETTLE);
        self.generation
    }
    pub fn remaining(&self, generation: u64, now: Instant) -> Option<Duration> {
        (generation == self.generation)
            .then(|| {
                self.deadline
                    .map(|deadline| deadline.saturating_duration_since(now))
            })
            .flatten()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Epoch(pub u64);
/// A result may release resources or attest scanout only within the device lifetime that
/// submitted it. Remove/pause supersedes even a successful driver result.
pub fn accepts(current: Epoch, result: Epoch, active: bool) -> bool {
    current == result && active
}
pub fn physical_proof(enabled: bool, actual_enabled: Option<bool>, presented: bool) -> bool {
    match actual_enabled {
        Some(false) => !enabled,
        Some(true) => enabled && presented,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_types::{
        display::{Insets, OutputConfiguration, PhysicalState, Rotation},
        settings::Capability,
    };

    fn display(name: &str, primary: bool) -> Display {
        let id = DisplayId(name.into());
        let capability = Capability::Available {
            requires_restart: false,
        };
        Display {
            id: id.clone(),
            gpu: name.split('/').next().unwrap().into(),
            connector: name.into(),
            make: String::new(),
            model: String::new(),
            serial: None,
            edid_sha256: None,
            connected: true,
            internal: name.contains("eDP"),
            modes: Vec::new(),
            current: OutputConfiguration {
                id,
                enabled: true,
                mode: Some("advertised-mode".into()),
                scale: 1.0,
                rotation: Rotation::Normal,
                x: 0,
                y: 0,
                mirror_of: None,
                primary,
                insets: Insets::default(),
            },
            physical: PhysicalState::NativeEnabled {
                mode: "advertised-mode".into(),
            },
            persistence: capability.clone(),
            configure: capability.clone(),
            hardware_underscan: capability.clone(),
            composition_inset: capability,
        }
    }

    #[test]
    fn removed_primary_promotes_existing_secondary_without_changing_owner_configuration() {
        let panel = display("primary/eDP-1", true);
        let tv = display("secondary/HDMI-A-1", false);
        assert_eq!(
            logical_primary([(&panel, true), (&tv, true)].into_iter()),
            Some(&panel.id)
        );
        // Even a stale connected/enabled/primary catalog entry cannot beat a mapped head.
        let selected = logical_primary([(&panel, false), (&tv, true)].into_iter());
        assert_eq!(selected, Some(&tv.id));
        let mut inventory = vec![panel.clone(), tv.clone()];
        project_logical_primary(&mut inventory, selected);
        let mut expected_panel = panel.clone();
        expected_panel.current.primary = false;
        let mut expected_tv = tv.clone();
        expected_tv.current.primary = true;
        assert_eq!(inventory, [expected_panel, expected_tv]);
        assert!(!tv.current.primary);
        assert!(panel.current.primary);
        // Device removal can remove the old catalog altogether; the result is the same.
        assert_eq!(logical_primary([(&tv, true)].into_iter()), Some(&tv.id));
    }

    #[test]
    fn stale_disconnected_disabled_or_unmapped_primary_cannot_select_home_or_capture() {
        let tv = display("secondary/HDMI-A-1", false);
        let original = display("primary/eDP-1", true);
        for (connected, enabled, mapped) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            let mut stale = original.clone();
            stale.connected = connected;
            stale.current.enabled = enabled;
            assert_eq!(
                logical_primary([(&stale, mapped), (&tv, true)].into_iter()),
                Some(&tv.id)
            );
        }
        let mut inventory = vec![original];
        project_logical_primary(&mut inventory, logical_primary(std::iter::empty()));
        assert!(!inventory[0].current.primary);
    }

    #[test]
    fn logical_primary_preserves_valid_explicit_choice_and_has_stable_fallback() {
        let first = display("gpu-a/DP-1", false);
        let second = display("gpu-z/HDMI-A-1", true);
        for outputs in [
            [(&first, true), (&second, true)],
            [(&second, true), (&first, true)],
        ] {
            assert_eq!(logical_primary(outputs.into_iter()), Some(&second.id));
        }
        let mut second = second;
        second.current.primary = false;
        for outputs in [
            [(&first, true), (&second, true)],
            [(&second, true), (&first, true)],
        ] {
            assert_eq!(logical_primary(outputs.into_iter()), Some(&first.id));
        }
        assert_eq!(
            logical_primary([(&first, false), (&second, false)].into_iter()),
            None
        );
    }

    #[test]
    fn tv_uses_advertised_1080p60_before_preferred_4k30() {
        assert_eq!(
            choose_mode(&[30, 60, 120], true, |mode| *mode == 60, |mode| *mode == 30),
            Some(60)
        );
        assert_eq!(
            choose_mode(&[30, 120], true, |mode| *mode == 60, |mode| *mode == 30),
            Some(30)
        );
        assert_eq!(
            choose_mode(&[30, 60], false, |mode| *mode == 60, |mode| *mode == 30),
            Some(30)
        );
        assert_eq!(choose_mode::<u32>(&[], true, |_| true, |_| true), None);
    }
    #[test]
    fn connector_bounce_extends_one_settle_deadline() {
        let now = Instant::now();
        let mut settle = Settle::default();
        let old = settle.changed(now);
        let latest = settle.changed(now + Duration::from_millis(200));
        assert_eq!(settle.remaining(old, now), None);
        assert_eq!(
            settle.remaining(latest, now + CONNECTOR_SETTLE),
            Some(Duration::from_millis(200))
        );
    }
    #[test]
    fn stale_worker_success_cannot_attest_new_device_or_paused_session() {
        assert!(accepts(Epoch(4), Epoch(4), true));
        assert!(!accepts(Epoch(5), Epoch(4), true));
        assert!(!accepts(Epoch(4), Epoch(4), false));
    }
    #[test]
    fn readback_tolerates_rounding_and_non_signal_flags_not_a_different_signal() {
        let requested = ((1920, 1080), ModeFlags::PHSYNC, 60_000);
        assert!(same_signal_values(
            requested,
            ((1920, 1080), ModeFlags::empty(), 59_940)
        ));
        assert!(!same_signal_values(
            requested,
            ((3840, 2160), ModeFlags::empty(), 60_000)
        ));
        assert!(!same_signal_values(
            requested,
            ((1920, 1080), ModeFlags::empty(), 30_000)
        ));
        assert!(!same_signal_values(
            requested,
            ((1920, 1080), ModeFlags::INTERLACE, 60_000)
        ));
        assert!(!same_signal_values(
            requested,
            ((1920, 1080), ModeFlags::DBLSCAN, 60_000)
        ));
    }
    #[test]
    fn disabled_logical_output_cannot_attest_a_still_lit_physical_output() {
        assert!(physical_proof(false, Some(false), false));
        assert!(physical_proof(true, Some(true), true));
        assert!(!physical_proof(false, Some(true), false));
        assert!(!physical_proof(true, Some(false), true));
        assert!(!physical_proof(true, Some(true), false));
        assert!(!physical_proof(false, None, false));
    }
}
