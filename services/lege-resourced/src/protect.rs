//! Protect the interface explicitly (planning doc 01 §5).
//!
//! The compositor, the shell (which hosts the recovery overlay), audio and the governor itself
//! must survive exactly the pressure that makes everything else worth containing. Two
//! independent defences apply:
//!
//! 1. **Structural**: these units live under `session.slice`, not `app.slice`
//!    ([`crate::scan::user_session_slice`]), so [`crate::scan::discover`] never returns them —
//!    they cannot appear in the victim pool at all, regardless of any policy bug.
//! 2. **Kernel-enforced**: this module additionally writes a modest `memory.low` to each one's
//!    cgroup directly, so the kernel's own reclaim resists taking their pages under *global*
//!    memory pressure — independent of whether the governor is running, and independent of
//!    whether the packaged systemd drop-ins (`distro/policy/systemd/user/*.d/`) happen to be
//!    installed. `memory.low` only (never `memory.min`): overcommitting hard protection can
//!    itself force OOM (planning doc 02 §5).
//!
//! Units are identified **by name**, not guessed from behaviour: this list is the single source
//! of truth for "session-critical" on the governor side.

use crate::cgroup::Cgroup;
use std::{collections::HashSet, path::Path};

/// `(unit file name under session.slice, memory.low bytes)`. Deliberately modest — protection
/// values are also set on the units themselves (`session/units/user/*.service`,
/// `distro/policy/systemd/user/*.d/`); this is a second, independent line of defence, not the
/// primary one, so it does not need to match those numbers exactly.
pub const SESSION_CRITICAL_UNITS: &[(&str, u64)] = &[
    // Compositor core: also the input path (Wayland input is delivered through it).
    ("lege-comp.service", 64 * crate::policy::MIB),
    // The shell hosts the protected pressure/recovery overlay.
    ("lege-shell.service", 32 * crate::policy::MIB),
    // The governor itself: it must not be reclaimed while deciding whom else to reclaim.
    ("lege-resourced.service", 32 * crate::policy::MIB),
    // Audio must survive memory pressure just as much as video and input do.
    ("pipewire.service", 16 * crate::policy::MIB),
    ("pipewire-pulse.service", 8 * crate::policy::MIB),
    ("wireplumber.service", 16 * crate::policy::MIB),
];

/// One session-critical unit as found (or not) on this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectionStatus {
    pub unit: &'static str,
    pub cgroup_present: bool,
    pub memory_low_applied: bool,
}

/// Apply `memory.low` to every present session-critical unit under `session_slice`. Absent units
/// (not started yet, or this session simply has no audio) are reported, not treated as errors —
/// re-running this on a later tick is exactly how a unit that starts after the governor still
/// gets protected. Every write is best-effort: a failure here must never stop the sampling loop.
pub fn apply(session_slice: &Path) -> Vec<ProtectionStatus> {
    SESSION_CRITICAL_UNITS
        .iter()
        .map(|&(unit, memory_low_bytes)| {
            let cgroup = Cgroup::from_directory(session_slice.join(unit));
            if !cgroup.exists() {
                return ProtectionStatus {
                    unit,
                    cgroup_present: false,
                    memory_low_applied: false,
                };
            }
            let applied = match cgroup.set_memory_low(memory_low_bytes) {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(unit, %error, "could not apply memory.low protection");
                    false
                }
            };
            ProtectionStatus {
                unit,
                cgroup_present: true,
                memory_low_applied: applied,
            }
        })
        .collect()
}

/// Unit *file* names (`lege-comp.service`, …), for callers that only need the identity set —
/// for example to double-check a discovered `app.slice` entry never collides with one of these.
pub fn unit_names() -> HashSet<&'static str> {
    SESSION_CRITICAL_UNITS
        .iter()
        .map(|&(unit, _)| unit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn absent_units_are_reported_without_error() {
        let dir =
            std::env::temp_dir().join(format!("lege-resourced-protect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let statuses = apply(&dir);
        assert_eq!(statuses.len(), SESSION_CRITICAL_UNITS.len());
        assert!(
            statuses
                .iter()
                .all(|s| !s.cgroup_present && !s.memory_low_applied)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn present_units_get_memory_low_written() {
        let dir = std::env::temp_dir().join(format!(
            "lege-resourced-protect-present-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lege-comp.service")).unwrap();
        // A real cgroup directory always has this file; writes go through it.
        fs::write(dir.join("lege-comp.service/memory.low"), "0\n").unwrap();

        let statuses = apply(&dir);
        let comp = statuses
            .iter()
            .find(|s| s.unit == "lege-comp.service")
            .unwrap();
        assert!(comp.cgroup_present);
        assert!(comp.memory_low_applied);
        let written = fs::read_to_string(dir.join("lege-comp.service/memory.low")).unwrap();
        assert_eq!(written.trim(), (64 * crate::policy::MIB).to_string());

        let audio = statuses
            .iter()
            .find(|s| s.unit == "pipewire.service")
            .unwrap();
        assert!(!audio.cgroup_present);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unit_names_cover_the_documented_set() {
        let names = unit_names();
        assert!(names.contains("lege-comp.service"));
        assert!(names.contains("lege-resourced.service"));
        assert_eq!(names.len(), SESSION_CRITICAL_UNITS.len());
    }
}
