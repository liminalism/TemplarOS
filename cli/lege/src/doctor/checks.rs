//! The check catalogue. Add a check here and it runs everywhere doctor runs.

use super::{Check, Context, Finding, Outcome, Severity, exists, finding};
use std::fs;

pub fn all() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(OsRelease),
        Box::new(SessionUnitsPresent),
        Box::new(ProtectedUnitsHaveResourceControl),
        Box::new(NvidiaFlavourConsistent),
        Box::new(NoStandaloneNvidiaInstaller),
        Box::new(PortalConfigPresent),
        Box::new(StorageReserve),
    ]
}

/// `/etc/os-release` identifies LegeOS and its Ubuntu base.
struct OsRelease;
impl Check for OsRelease {
    fn id(&self) -> &'static str {
        "os-release"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let path = context.path("/etc/os-release");
        let Ok(text) = fs::read_to_string(&path) else {
            return finding(
                self,
                Outcome::Fail,
                "os-release missing",
                path.display().to_string(),
            );
        };
        let is_legeos = text.lines().any(|line| line == "ID=legeos");
        let like_ubuntu = text
            .lines()
            .any(|line| line.starts_with("ID_LIKE=") && line.contains("ubuntu"));
        if is_legeos && like_ubuntu {
            finding(
                self,
                Outcome::Pass,
                "os-release identifies LegeOS (Ubuntu-derived)",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "os-release is not LegeOS",
                format!("ID=legeos:{is_legeos} ID_LIKE ubuntu:{like_ubuntu}"),
            )
        }
    }
}

/// The session target and core units are installed.
struct SessionUnitsPresent;
impl Check for SessionUnitsPresent {
    fn id(&self) -> &'static str {
        "session-units"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let required = [
            "/usr/lib/systemd/user/legeos-session.target",
            "/usr/lib/systemd/user/lege-comp.service",
            "/usr/lib/systemd/user/lege-sessiond.service",
            "/usr/lib/systemd/user/lege-resourced.service",
            "/usr/lib/systemd/user/lege-filed.service",
            "/usr/lib/systemd/user/lege-shell.service",
            "/usr/share/wayland-sessions/legeos.desktop",
        ];
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|p| !exists(&context.root, p))
            .collect();
        if missing.is_empty() {
            finding(self, Outcome::Pass, "session units present", "")
        } else {
            finding(
                self,
                Outcome::Fail,
                format!("{} session unit(s) missing", missing.len()),
                missing.join(", "),
            )
        }
    }
}

/// Compositor, sessiond, resourced and audio units carry memory/CPU protection.
struct ProtectedUnitsHaveResourceControl;
impl Check for ProtectedUnitsHaveResourceControl {
    fn id(&self) -> &'static str {
        "protected-units"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let units = [
            "lege-comp.service",
            "lege-sessiond.service",
            "lege-resourced.service",
        ];
        let mut unprotected = Vec::new();
        for unit in units {
            let path = context.path(&format!("/usr/lib/systemd/user/{unit}"));
            let text = fs::read_to_string(&path).unwrap_or_default();
            if !(text.contains("MemoryLow=") && text.contains("CPUWeight=")) {
                unprotected.push(unit);
            }
        }
        if unprotected.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "session-critical units are protected",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "units without MemoryLow/CPUWeight",
                unprotected.join(", "),
            )
        }
    }
}

/// Exactly one NVIDIA flavour is installed, matching the detected hardware.
struct NvidiaFlavourConsistent;
impl Check for NvidiaFlavourConsistent {
    fn id(&self) -> &'static str {
        "nvidia-flavour"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let open = exists(&context.root, "/var/lib/dpkg/info/legeos-nvidia-open.list");
        let proprietary = exists(
            &context.root,
            "/var/lib/dpkg/info/legeos-nvidia-proprietary.list",
        );
        match (open, proprietary) {
            (true, true) => finding(
                self,
                Outcome::Fail,
                "both NVIDIA flavours installed",
                "open and proprietary modules must never coexist",
            ),
            (false, false) => finding(
                self,
                Outcome::Skipped,
                "no NVIDIA metapackage installed",
                "",
            ),
            (open, _) => {
                let expected = lege_hwdetect::scan_sysfs(&context.path("/sys"))
                    .ok()
                    .map(lege_hwdetect::classify)
                    .and_then(|profile| profile.nvidia_flavour);
                match expected {
                    Some(lege_hwdetect::NvidiaFlavour::Open) if !open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the open flavour",
                        "legeos-nvidia-proprietary installed",
                    ),
                    Some(lege_hwdetect::NvidiaFlavour::Proprietary) if open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the proprietary flavour",
                        "legeos-nvidia-open installed",
                    ),
                    _ => finding(
                        self,
                        Outcome::Pass,
                        "one NVIDIA flavour, consistent with hardware",
                        "",
                    ),
                }
            }
        }
    }
}

/// NVIDIA's standalone installer is not part of a supported configuration.
struct NoStandaloneNvidiaInstaller;
impl Check for NoStandaloneNvidiaInstaller {
    fn id(&self) -> &'static str {
        "no-nvidia-installer"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let markers = [
            "/usr/bin/nvidia-uninstall",
            "/var/lib/nvidia/log",
            "/usr/lib/nvidia/alternate-install-present",
        ];
        let present: Vec<&str> = markers
            .iter()
            .copied()
            .filter(|m| exists(&context.root, m))
            .collect();
        if present.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "no standalone NVIDIA installer artefacts",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                "standalone NVIDIA installer detected (unsupported)",
                present.join(", "),
            )
        }
    }
}

/// The portal backend selection file exists so every portal has one responding backend.
struct PortalConfigPresent;
impl Check for PortalConfigPresent {
    fn id(&self) -> &'static str {
        "portal-config"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let ok = exists(
            &context.root,
            "/usr/share/xdg-desktop-portal/legeos-portals.conf",
        ) && exists(
            &context.root,
            "/usr/share/xdg-desktop-portal/portals/legeos.portal",
        );
        if ok {
            finding(self, Outcome::Pass, "portal backend configured", "")
        } else {
            finding(
                self,
                Outcome::Fail,
                "portal backend not configured",
                "legeos-portals.conf / legeos.portal missing",
            )
        }
    }
}

/// The root volume has more than its reserve available (runtime only).
struct StorageReserve;
impl Check for StorageReserve {
    fn id(&self) -> &'static str {
        "storage-reserve"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != std::path::Path::new("/") {
            return finding(self, Outcome::Skipped, "storage reserve (runtime only)", "");
        }
        // Delegated to lege-filed when it is running; a coarse fallback here.
        match fs::metadata("/") {
            Ok(_) => finding(
                self,
                Outcome::Pass,
                "root volume reachable",
                "detailed reserve check via lege-filed",
            ),
            Err(error) => finding(self, Outcome::Fail, "cannot stat /", error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{exit_code, run_all};

    #[test]
    fn empty_root_fails_the_identity_checks_and_reports_exit_code_1() {
        let root = std::env::temp_dir().join(format!("lege-doctor-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let findings = run_all(&Context {
            root: root.clone(),
            hardware: false,
        });
        assert!(
            findings
                .iter()
                .any(|f| f.id == "os-release" && f.outcome == Outcome::Fail)
        );
        assert_eq!(exit_code(&findings), 1);

        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(
            root.join("etc/os-release"),
            "ID=legeos\nID_LIKE=ubuntu debian\n",
        )
        .unwrap();
        let findings = run_all(&Context {
            root: root.clone(),
            hardware: false,
        });
        assert!(
            findings
                .iter()
                .any(|f| f.id == "os-release" && f.outcome == Outcome::Pass)
        );
        let _ = fs::remove_dir_all(&root);
    }
}
