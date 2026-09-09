//! The real-hardware qualification report: bounded read-only probes, plus the matrix of cases only
//! a human can run.
//!
//! Two halves, and the whole point is that they never blur into each other.
//!
//! **The probes** ([`templar_hwdetect::probes`]) observe what this machine can be asked without
//! changing it: kernel and NVIDIA module versions, who signed the module, `nvidia-smi`, the Vulkan
//! devices the loader can see, whether render offload is available, CUDA's `deviceQuery` if it
//! happens to be installed, and the VA-API profiles. Each is timeout-bounded and read-only, and
//! each reports `Pass`, `Fail`, `Skipped` (this machine cannot answer) or `NotExecuted` (nobody
//! asked — it timed out, or the root under inspection is not this machine).
//!
//! **The matrix** (`tests/hardware/*.toml`) contains actions such as rebooting, changing GPU mode,
//! suspending and unplugging cables. `templar doctor --hardware` must never run a case merely because
//! it was listed: it validates and selects the checked-in data, reports the exact steps and
//! expectations, and emits every one of them as [`Outcome::NotExecuted`]. A case in this report is
//! a plan for a supervised hardware run, never a result of one.

use super::{Context, Finding, Outcome, Severity};
use serde::Deserialize;
use std::{collections::BTreeSet, fs, path::Path, time::Duration};
use templar_hwdetect::{
    GpuMode, classify,
    probes::{self, Probe, ProbeOutcome},
    scan_sysfs,
};

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
    // From X1's daily-driver capability matrix (C6).
    EmbeddedCase {
        id: "hardware-video-decode",
        source: include_str!("../../../../tests/hardware/hardware-video-decode.toml"),
    },
    EmbeddedCase {
        id: "brightness-and-audio-keys",
        source: include_str!("../../../../tests/hardware/brightness-and-audio-keys.toml"),
    },
    EmbeddedCase {
        id: "removable-storage",
        source: include_str!("../../../../tests/hardware/removable-storage.toml"),
    },
];

/// One automated probe in the qualification report.
///
/// `severity` decides what a failure costs: the graphics stack coming up at all is an `Error` (the
/// machine is not qualified without it), while a reporting tool's verdict is a `Warning` — a dev
/// box without CUDA or VA-API is not a broken reference laptop.
struct ProbeSpec {
    /// The id the probe reports under; `--check <id>` selects it.
    id: &'static str,
    severity: Severity,
    /// `true` when the probe runs an external binary, and therefore observes the *live* machine
    /// whatever `--root` says. Those are marked `NotExecuted` unless the root is `/`.
    executes: bool,
    run: fn(&Path, Duration) -> Probe,
}

/// The safe subset: everything here is read-only and bounded. Nothing that changes GPU mode,
/// suspends, reboots or touches a cable is a probe — those are matrix cases, and stay manual.
const PROBES: &[ProbeSpec] = &[
    ProbeSpec {
        id: "kernel-release",
        severity: Severity::Info,
        executes: false,
        run: probes::kernel_release,
    },
    ProbeSpec {
        id: "drm-connected-output",
        severity: Severity::Error,
        executes: false,
        run: probes::drm_connected_output,
    },
    ProbeSpec {
        id: "drm-modesetting",
        severity: Severity::Error,
        executes: false,
        run: probes::drm_modesetting_initialized,
    },
    ProbeSpec {
        id: "nvidia-module",
        severity: Severity::Error,
        executes: false,
        run: probes::nvidia_module_state,
    },
    ProbeSpec {
        id: "nvidia-module-signer",
        severity: Severity::Error,
        executes: true,
        run: probes::nvidia_module_signer,
    },
    ProbeSpec {
        id: "nvidia-smi",
        severity: Severity::Warning,
        executes: true,
        run: probes::nvidia_smi,
    },
    ProbeSpec {
        id: "vulkan-devices",
        severity: Severity::Warning,
        executes: true,
        run: probes::vulkan_devices,
    },
    ProbeSpec {
        id: "prime-offload",
        severity: Severity::Warning,
        executes: false,
        run: probes::prime_offload_available,
    },
    ProbeSpec {
        id: "cuda-device-query",
        severity: Severity::Warning,
        executes: true,
        run: probes::cuda_device_query,
    },
    ProbeSpec {
        id: "vaapi",
        severity: Severity::Warning,
        executes: true,
        run: probes::vaapi,
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

    // The live preflight comes first: on the installation medium it is the section that decides
    // whether anything should be written to a disk at all, and `templar doctor --preflight` runs it
    // on its own (main.rs) so an image check failing on a live medium cannot block an install.
    let preflight = super::preflight::findings(context);
    let preflight_ids: Vec<&'static str> = preflight.iter().map(|f| f.id).collect();
    findings.extend(
        preflight
            .into_iter()
            .filter(|f| options.check.as_deref().is_none_or(|id| id == f.id)),
    );

    findings.extend(probe_findings(context, options.check.as_deref()));

    let mut known = BTreeSet::from(["gpu-mode"]);
    known.extend(preflight_ids);
    known.extend(PROBES.iter().map(|spec| spec.id));
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

/// Run the safe probe subset — or, when `--root` points somewhere that is not this machine, say
/// plainly that the tool probes did not run instead of running them against the wrong system.
fn probe_findings(context: &Context, requested: Option<&str>) -> Vec<Finding> {
    let live = context.root == Path::new("/");
    PROBES
        .iter()
        .filter(|spec| requested.is_none_or(|id| id == spec.id))
        .map(|spec| {
            if spec.executes && !live {
                return Finding {
                    id: spec.id,
                    severity: Severity::Info,
                    outcome: Outcome::NotExecuted,
                    summary: "probe runs a tool on the live machine".to_owned(),
                    detail: format!(
                        "not executed against --root {}: the answer would describe this host, not that root",
                        context.root.display()
                    ),
                };
            }
            finding_from_probe(spec, &(spec.run)(&context.root, probes::DEFAULT_TIMEOUT))
        })
        .collect()
}

/// A [`Probe`] as a doctor [`Finding`]. The four outcomes map across one-for-one: nothing is
/// promoted to a pass, and a probe's captured output becomes the finding's detail so `--json`
/// carries the evidence.
fn finding_from_probe(spec: &ProbeSpec, probe: &Probe) -> Finding {
    Finding {
        id: spec.id,
        severity: spec.severity,
        outcome: match probe.outcome {
            ProbeOutcome::Pass => Outcome::Pass,
            ProbeOutcome::Fail => Outcome::Fail,
            ProbeOutcome::Skipped => Outcome::Skipped,
            ProbeOutcome::NotExecuted => Outcome::NotExecuted,
        },
        summary: probe.summary.clone(),
        detail: probe.output.clone(),
    }
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
        // NotExecuted, not Skipped: this machine could answer the question, and the runner
        // deliberately did not ask it. Only a supervised hardware run can turn this into a
        // pass or a fail, and its evidence is attached to the AKR record by hand.
        outcome: Outcome::NotExecuted,
        summary: format!(
            "manual hardware case ready, not executed ({} steps, {} expectations)",
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
    let path = context.path("etc/templaros/hardware/gpu-mode");
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
        assert_eq!(selected.outcome, Outcome::NotExecuted);
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
    fn the_report_distinguishes_all_four_outcomes_and_never_passes_a_manual_case() {
        // The one property this whole lane exists for. A report that can only say pass/fail/skip
        // has to lie about a manual case (calling a plan a "skip" reads as "we looked and there
        // was nothing to check") and about a probe that never ran. So one fixture is arranged to
        // produce every outcome at once, and the manual cases are checked to be none of them but
        // NotExecuted.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Two GPUs: hybrid, so the reference profile is detected and prime-offload applies.
        for (slot, vendor, device) in [
            ("0000:00:02.0", "0x8086", "0xa7a0"),
            ("0000:01:00.0", "0x10de", "0x28e0"),
        ] {
            let device_dir = root.join("sys/bus/pci/devices").join(slot);
            fs::create_dir_all(&device_dir).unwrap();
            fs::write(device_dir.join("class"), "0x030000\n").unwrap();
            fs::write(device_dir.join("vendor"), format!("{vendor}\n")).unwrap();
            fs::write(device_dir.join("device"), format!("{device}\n")).unwrap();
            fs::write(device_dir.join("boot_vga"), "0\n").unwrap();
        }
        // A connected output and a modesetting card: two probes that can pass.
        fs::create_dir_all(root.join("sys/class/drm/card0")).unwrap();
        let connector = root.join("sys/class/drm/card0-eDP-1");
        fs::create_dir_all(&connector).unwrap();
        fs::write(connector.join("status"), "connected\n").unwrap();
        // A loaded module whose branch matches the installed userspace: nvidia-module passes.
        fs::create_dir_all(root.join("sys/module/nvidia")).unwrap();
        fs::write(root.join("sys/module/nvidia/version"), "580.65.06\n").unwrap();
        let status = root.join("var/lib/dpkg/status");
        fs::create_dir_all(status.parent().unwrap()).unwrap();
        fs::write(
            &status,
            "Package: nvidia-driver-580-open\nStatus: install ok installed\nVersion: 580.65.06\n\n",
        )
        .unwrap();
        // Nothing writes proc/sys/kernel/osrelease here, so kernel-release is Skipped, and no
        // kernel driver is bound to the NVIDIA GPU, so prime-offload fails.

        let findings = findings(&context(root, true), &HardwareOptions::default());
        let outcomes: Vec<Outcome> = findings.iter().map(|f| f.outcome).collect();
        for expected in [
            Outcome::Pass,
            Outcome::Fail,
            Outcome::Skipped,
            Outcome::NotExecuted,
        ] {
            assert!(
                outcomes.contains(&expected),
                "the report never produced {expected:?}: {findings:#?}"
            );
        }

        let manual: Vec<&Finding> = findings
            .iter()
            .filter(|f| CASES.iter().any(|case| case.id == f.id))
            .collect();
        assert_eq!(manual.len(), CASES.len(), "every case should be reported");
        for case in manual {
            assert_eq!(
                case.outcome,
                Outcome::NotExecuted,
                "manual case {} must never be reported as executed",
                case.id
            );
        }
    }

    #[test]
    fn gpu_mode_checks_the_declared_mode_against_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let mode = dir.path().join("etc/templaros/hardware");
        fs::create_dir_all(&mode).unwrap();
        fs::write(mode.join("gpu-mode"), "hybrid\n").unwrap();
        let finding = gpu_mode(&context(dir.path(), true), "hybrid-intel-nvidia");
        assert_eq!(finding.outcome, Outcome::Pass);
        let finding = gpu_mode(&context(dir.path(), true), "integrated-only");
        assert_eq!(finding.outcome, Outcome::Fail);
    }
}
