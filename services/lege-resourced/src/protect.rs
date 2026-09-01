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
//!
//! ## This module used to weaken the protection it exists to provide
//!
//! Its values were "deliberately modest" and smaller than the ones the unit files declare — the
//! shell's units say `MemoryLow=64M`, this said 32 MiB — and it wrote unconditionally, every
//! tick. So systemd would set the real protection at startup and the governor would immediately
//! halve it, then keep halving it for the life of the session. A second line of defence that
//! silently overrides the first is not a second line of defence.
//!
//! Two changes fix it, and both are load-bearing. The values here now match the unit files, so
//! agreeing costs nothing. And a write only happens when it would *raise* the value — so a
//! larger number set by anyone, for any reason, survives. That rule is what makes this genuinely
//! independent: it can only ever add protection, never remove it.

use crate::cgroup::Cgroup;
use std::{collections::HashSet, path::Path};

/// `(unit file name under session.slice, memory.low bytes)`.
///
/// These match `session/units/user/*.service` and `distro/policy/systemd/user/*.d/` — this is a
/// second, independent line of defence that must never contradict the first. Where a unit file
/// declares a value, the same value appears here; where none does, a modest one is chosen and
/// says why.
pub const SESSION_CRITICAL_UNITS: &[(&str, u64)] = &[
    // Compositor core: also the input path (Wayland input is delivered through it).
    // lege-comp.service: MemoryLow=256M.
    ("lege-comp.service", 256 * crate::policy::MIB),
    // The shell hosts the protected pressure/recovery overlay. lege-shell.service: MemoryLow=64M.
    ("lege-shell.service", 64 * crate::policy::MIB),
    // The governor itself: it must not be reclaimed while deciding whom else to reclaim.
    // lege-resourced.service: MemoryLow=96M.
    ("lege-resourced.service", 96 * crate::policy::MIB),
    // Policy: every launch, every association, every setting goes through it, and a session whose
    // sessiond is thrashing cannot open a window. lege-sessiond.service: MemoryLow=128M.
    ("lege-sessiond.service", 128 * crate::policy::MIB),
    // The file service: a stalled filed is a file manager, a portal and a Home surface that all
    // stop answering at once. lege-filed.service: MemoryLow=64M.
    ("lege-filed.service", 64 * crate::policy::MIB),
    // Audio must survive memory pressure just as much as video and input do.
    // pipewire.service.d/legeos.conf: MemoryLow=64M.
    ("pipewire.service", 64 * crate::policy::MIB),
    // No drop-in declares one for the PulseAudio bridge; it is small, and this is a floor.
    ("pipewire-pulse.service", 8 * crate::policy::MIB),
    // wireplumber.service.d/legeos.conf: MemoryLow=64M.
    ("wireplumber.service", 64 * crate::policy::MIB),
];

/// One session-critical unit as found (or not) on this tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectionStatus {
    pub unit: &'static str,
    pub cgroup_present: bool,
    /// Whether this tick wrote `memory.low`.
    pub memory_low_applied: bool,
    /// What was already set, when it could be read. A value at or above the desired one means
    /// nothing was written, which is a success, not a failure — see [`apply`].
    pub existing_bytes: Option<u64>,
}

/// Apply `memory.low` to every present session-critical unit under `session_slice`.
///
/// **Only ever upwards.** If the cgroup already has a `memory.low` at or above the value this
/// module wants, nothing is written. That is what stops the governor from undoing the unit
/// files' own, larger protection every tick — the bug this rule exists to close — and it means a
/// larger value set by anyone, for any reason, survives contact with the governor.
///
/// Absent units (not started yet, or this session simply has no audio) are reported, not treated
/// as errors: re-running this on a later tick is exactly how a unit that starts after the
/// governor still gets protected. Every write is best-effort — a failure here must never stop the
/// sampling loop.
pub fn apply(session_slice: &Path) -> Vec<ProtectionStatus> {
    SESSION_CRITICAL_UNITS
        .iter()
        .map(|&(unit, desired)| {
            let cgroup = Cgroup::from_directory(session_slice.join(unit));
            if !cgroup.exists() {
                return ProtectionStatus {
                    unit,
                    cgroup_present: false,
                    memory_low_applied: false,
                    existing_bytes: None,
                };
            }
            // An unreadable memory.low is treated as no protection at all, so a write is
            // attempted: failing to read is a reason to be careful, not a reason to leave a
            // session-critical unit unprotected.
            let existing = cgroup.memory_low().ok();
            if existing.is_some_and(|current| current >= desired) {
                return ProtectionStatus {
                    unit,
                    cgroup_present: true,
                    memory_low_applied: false,
                    existing_bytes: existing,
                };
            }
            let applied = match cgroup.set_memory_low(desired) {
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
                existing_bytes: existing,
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
        assert_eq!(written.trim(), (256 * crate::policy::MIB).to_string());

        let audio = statuses
            .iter()
            .find(|s| s.unit == "pipewire.service")
            .unwrap();
        assert!(!audio.cgroup_present);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn protect_never_lowers_an_existing_memory_low() {
        // The bug this rule closes. The unit files set the real protection at startup; this
        // module used to write a smaller number over it on every tick, for the life of the
        // session, so the second line of defence quietly demolished the first.
        let dir = std::env::temp_dir().join(format!(
            "lege-resourced-protect-never-lowers-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lege-shell.service")).unwrap();
        let path = dir.join("lege-shell.service/memory.low");
        // Larger than anything this module would choose: someone raised it on purpose.
        fs::write(&path, format!("{}\n", 512 * crate::policy::MIB)).unwrap();

        let statuses = apply(&dir);
        let shell = statuses
            .iter()
            .find(|s| s.unit == "lege-shell.service")
            .unwrap();
        assert!(shell.cgroup_present);
        assert!(
            !shell.memory_low_applied,
            "nothing should have been written"
        );
        assert_eq!(shell.existing_bytes, Some(512 * crate::policy::MIB));
        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            (512 * crate::policy::MIB).to_string(),
            "the larger protection must survive the governor"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unset_memory_low_is_raised_to_the_units_own_value() {
        let dir = std::env::temp_dir().join(format!(
            "lege-resourced-protect-raises-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lege-sessiond.service")).unwrap();
        let path = dir.join("lege-sessiond.service/memory.low");
        fs::write(&path, "0\n").unwrap();

        let statuses = apply(&dir);
        let sessiond = statuses
            .iter()
            .find(|s| s.unit == "lege-sessiond.service")
            .unwrap();
        assert!(sessiond.memory_low_applied);
        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            (128 * crate::policy::MIB).to_string()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unlimited_memory_low_is_never_reduced_to_a_number() {
        // systemd writes `max` for an unbounded value; parsing that as zero would make the
        // governor replace unlimited protection with a few megabytes.
        let dir =
            std::env::temp_dir().join(format!("lege-resourced-protect-max-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lege-comp.service")).unwrap();
        let path = dir.join("lege-comp.service/memory.low");
        fs::write(&path, "max\n").unwrap();

        let statuses = apply(&dir);
        let comp = statuses
            .iter()
            .find(|s| s.unit == "lege-comp.service")
            .unwrap();
        assert!(!comp.memory_low_applied);
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "max");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_protected_unit_agrees_with_the_unit_file_that_declares_it() {
        // The two lines of defence must not contradict each other; where they did, the governor
        // won, every tick, and the unit file's number was decoration.
        let declared: &[(&str, u64)] = &[
            ("lege-comp.service", 256),
            ("lege-shell.service", 64),
            ("lege-resourced.service", 96),
            ("lege-sessiond.service", 128),
            ("lege-filed.service", 64),
        ];
        for (unit, mib) in declared {
            let ours = SESSION_CRITICAL_UNITS
                .iter()
                .find(|(name, _)| name == unit)
                .unwrap_or_else(|| panic!("{unit} is not protected here"))
                .1;
            assert_eq!(
                ours,
                mib * crate::policy::MIB,
                "{unit}: session/units/user says {mib}M"
            );
        }
    }

    #[test]
    fn unit_names_cover_the_documented_set() {
        let names = unit_names();
        assert!(names.contains("lege-comp.service"));
        assert!(names.contains("lege-resourced.service"));
        assert_eq!(names.len(), SESSION_CRITICAL_UNITS.len());
    }
}
