//! The real-hardware matrix as an executable, deliberately non-destructive plan.
//!
//! `tests/hardware/*.toml` contains actions such as rebooting, changing GPU mode and suspending.
//! `lege doctor --hardware` must therefore never run a case merely because it was listed. It
//! validates and selects the checked-in data, reports the exact steps and expectations, and leaves
//! execution/evidence capture to an explicitly supervised hardware run.

use super::{Context, Finding, Outcome, Severity};
use lege_hwdetect::{GpuMode, classify, scan_sysfs};
use serde::Deserialize;
use std::{collections::BTreeSet, fs};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HardwareOptions {
    pub check: Option<String>,
    pub profile: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HardwareCase {
    id: String,
    profiles: Vec<String>,
    steps: Vec<String>,
    expect: Vec<String>,
}

struct EmbeddedCase {
    id: &'static str,
    source: &'static str,
}

const CASES: &[EmbeddedCase] = &[
    EmbeddedCase {
        id: "cuda-and-vulkan-where-installed",
        source: include_str!("../../../../tests/hardware/cuda-and-vulkan-where-installed.toml"),
    },
    EmbeddedCase {
        id: "driver-update-and-rollback",
        source: include_str!("../../../../tests/hardware/driver-update-and-rollback.toml"),
    },
    EmbeddedCase {
        id: "external-monitor-each-output-path",
        source: include_str!("../../../../tests/hardware/output-hotplug.toml"),
    },
    EmbeddedCase {
        id: "intel-desktop-nvidia-offload",
        source: include_str!("../../../../tests/hardware/intel-desktop-nvidia-offload.toml"),
    },
    EmbeddedCase {
        id: "kernel-update-and-rollback",
        source: include_str!("../../../../tests/hardware/kernel-update-and-rollback.toml"),
    },
    EmbeddedCase {
        id: "nvidia-desktop",
        source: include_str!("../../../../tests/hardware/nvidia-desktop.toml"),
    },
    EmbeddedCase {
        id: "suspend-resume",
        source: include_str!("../../../../tests/hardware/suspend-resume.toml"),
    },
    EmbeddedCase {
        id: "wayland-and-xwayland-apps",
        source: include_str!("../../../../tests/hardware/wayland-and-xwayland-apps.toml"),
    },
];

pub fn findings(context: &Context, options: &HardwareOptions) -> Vec<Finding> {
    let profile = options
        .profile
        .clone()
        .unwrap_or_else(|| detect_profile(context));
    let mut findings = vec![Finding {
        id: "hardware-profile",
        severity: Severity::Info,
        outcome: Outcome::Pass,
        summary: format!("hardware matrix profile: {profile}"),
        detail: if options.profile.is_some() {
            "selected explicitly with --profile".to_owned()
        } else {
            "derived from the GPUs visible under --root".to_owned()
        },
    }];

    if options.check.as_deref() == Some("gpu-mode") {
        findings.push(gpu_mode(context, &profile));
        return findings;
    }

    let mut known = BTreeSet::from(["gpu-mode"]);
    let mut matched = 0usize;
    for embedded in CASES {
        known.insert(embedded.id);
        if options
            .check
            .as_deref()
            .is_some_and(|requested| requested != embedded.id)
        {
            continue;
        }
        match parse_case(embedded) {
            Ok(case) => {
                if !case.profiles.iter().any(|candidate| candidate == &profile) {
                    if options.check.is_some() {
                        findings.push(Finding {
                            id: embedded.id,
                            severity: Severity::Info,
                            outcome: Outcome::Skipped,
                            summary: format!("hardware case does not apply to profile {profile}"),
                            detail: format!("applies to: {}", case.profiles.join(", ")),
                        });
                    }
                    continue;
                }
                matched += 1;
                findings.push(plan_finding(embedded.id, &case));
            }
            Err(error) => findings.push(Finding {
                id: embedded.id,
                severity: Severity::Error,
                outcome: Outcome::Fail,
                summary: "hardware matrix case is invalid".to_owned(),
                detail: error,
            }),
        }
    }

    if let Some(requested) = options.check.as_deref()
        && !known.contains(requested)
    {
        findings.push(Finding {
            id: "hardware-check",
            severity: Severity::Error,
            outcome: Outcome::Fail,
            summary: format!("unknown hardware check {requested:?}"),
            detail: format!(
                "known checks: {}",
                known.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    } else if options.check.is_none() && matched == 0 {
        findings.push(Finding {
            id: "hardware-matrix",
            severity: Severity::Info,
            outcome: Outcome::Skipped,
            summary: format!("no matrix cases apply to profile {profile}"),
            detail: "use --profile to review another profile without executing it".to_owned(),
        });
    }

    findings
}

fn parse_case(embedded: &EmbeddedCase) -> Result<HardwareCase, String> {
    let case: HardwareCase = toml::from_str(embedded.source)
        .map_err(|error| format!("parsing {}: {error}", embedded.id))?;
    if case.id != embedded.id {
        return Err(format!(
            "embedded id {:?} does not match file id {:?}",
            embedded.id, case.id
        ));
    }
    if case.profiles.is_empty() || case.steps.is_empty() || case.expect.is_empty() {
        return Err("profiles, steps and expect must all be non-empty".to_owned());
    }
    Ok(case)
}

fn plan_finding(id: &'static str, case: &HardwareCase) -> Finding {
    let steps = case
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| format!("{}. {step}", index + 1))
        .collect::<Vec<_>>()
        .join(" | ");
    let expectations = case
        .expect
        .iter()
        .enumerate()
        .map(|(index, expectation)| format!("{}. {expectation}", index + 1))
        .collect::<Vec<_>>()
        .join(" | ");
    Finding {
        id,
        severity: Severity::Info,
        outcome: Outcome::Skipped,
        summary: format!(
            "manual hardware case ready ({} steps, {} expectations)",
            case.steps.len(),
            case.expect.len()
        ),
        detail: format!("steps: {steps}; expect: {expectations}"),
    }
}

fn detect_profile(context: &Context) -> String {
    let gpus = scan_sysfs(&context.path("sys")).unwrap_or_default();
    let profile = classify(gpus);
    if profile.hybrid {
        "hybrid-intel-nvidia".to_owned()
    } else if profile.is_discrete_only() {
        "nvidia-only".to_owned()
    } else if profile.gpus.is_empty() {
        "unknown".to_owned()
    } else {
        "integrated-only".to_owned()
    }
}

fn gpu_mode(context: &Context, profile: &str) -> Finding {
    let path = context.path("etc/legeos/hardware/gpu-mode");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Finding {
            id: "gpu-mode",
            severity: Severity::Error,
            outcome: Outcome::Fail,
            summary: "declarative GPU mode is missing".to_owned(),
            detail: path.display().to_string(),
        };
    };
    let mode = match contents.trim() {
        "hybrid" => GpuMode::Hybrid,
        "integrated" => GpuMode::Integrated,
        "discrete" => GpuMode::Discrete,
        other => {
            return Finding {
                id: "gpu-mode",
                severity: Severity::Error,
                outcome: Outcome::Fail,
                summary: format!("unrecognised GPU mode {other:?}"),
                detail: path.display().to_string(),
            };
        }
    };
    let allowed = match profile {
        "hybrid-intel-nvidia" => true,
        "nvidia-only" => mode == GpuMode::Discrete,
        "integrated-only" => mode == GpuMode::Integrated,
        _ => false,
    };
    Finding {
        id: "gpu-mode",
        severity: Severity::Error,
        outcome: if allowed {
            Outcome::Pass
        } else {
            Outcome::Fail
        },
        summary: if allowed {
            format!("GPU mode {} is valid for {profile}", contents.trim())
        } else {
            format!("GPU mode {} is not valid for {profile}", contents.trim())
        },
        detail: path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(root: &std::path::Path, hardware: bool) -> Context {
        Context {
            root: root.to_path_buf(),
            hardware,
        }
    }

    #[test]
    fn every_embedded_case_parses_and_has_a_unique_id() {
        let mut ids = BTreeSet::new();
        for embedded in CASES {
            let case = parse_case(embedded).expect(embedded.id);
            assert!(ids.insert(case.id));
        }
    }

    #[test]
    fn an_explicit_profile_selects_the_matching_non_destructive_plan() {
        let dir = tempfile::tempdir().unwrap();
        let options = HardwareOptions {
            check: Some("suspend-resume".to_owned()),
            profile: Some("hybrid-intel-nvidia".to_owned()),
        };
        let findings = findings(&context(dir.path(), true), &options);
        let selected = findings
            .iter()
            .find(|finding| finding.id == "suspend-resume")
            .expect("selected case");
        assert_eq!(selected.outcome, Outcome::Skipped);
        assert!(selected.detail.contains("systemctl suspend"));
        assert!(selected.detail.contains("all windows recoverable"));
    }

    #[test]
    fn an_unknown_check_fails_and_lists_known_ids() {
        let dir = tempfile::tempdir().unwrap();
        let options = HardwareOptions {
            check: Some("not-a-case".to_owned()),
            profile: Some("hybrid-intel-nvidia".to_owned()),
        };
        let findings = findings(&context(dir.path(), true), &options);
        let failure = findings
            .iter()
            .find(|finding| finding.id == "hardware-check")
            .expect("unknown check failure");
        assert_eq!(failure.outcome, Outcome::Fail);
        assert!(failure.detail.contains("suspend-resume"));
    }

    #[test]
    fn gpu_mode_checks_the_declared_mode_against_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let mode = dir.path().join("etc/legeos/hardware");
        fs::create_dir_all(&mode).unwrap();
        fs::write(mode.join("gpu-mode"), "hybrid\n").unwrap();
        let finding = gpu_mode(&context(dir.path(), true), "hybrid-intel-nvidia");
        assert_eq!(finding.outcome, Outcome::Pass);
        let finding = gpu_mode(&context(dir.path(), true), "integrated-only");
        assert_eq!(finding.outcome, Outcome::Fail);
    }
}
