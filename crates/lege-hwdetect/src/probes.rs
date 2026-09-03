//! Bounded, read-only hardware probes — the one implementation shared by `lege-boot-health`'s
//! boot verdict and `lege doctor --hardware`'s qualification report.
//!
//! Three rules hold for everything in this module, because both callers run on a machine the user
//! is actually using:
//!
//! 1. **Read-only.** A probe reads sysfs/procfs/dpkg or runs a tool that only reports. Nothing here
//!    changes GPU mode, loads or unloads a module, suspends, reboots or writes anything.
//! 2. **Bounded.** Every external command runs under [`run_with_timeout`], which kills the child on
//!    the deadline. A hung `nvidia-smi` costs the caller its timeout, not its boot.
//! 3. **Honest about the difference between four outcomes.** A missing tool is
//!    [`ProbeOutcome::Skipped`], never `Pass`; a probe that could not be run at all — it timed out,
//!    or it is a manual step nobody executed — is [`ProbeOutcome::NotExecuted`], never `Pass` and
//!    never a silent omission. Only an observation that actually happened may be `Pass` or `Fail`.

use crate::{GpuVendor, classify, dpkg, lrm_present, scan_outputs, scan_sysfs};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// What a probe found. The two non-verdicts are deliberately distinct: `Skipped` means "this
/// machine cannot answer the question" (no such tool, no such hardware), `NotExecuted` means
/// "nobody asked the question" (timed out, or a manual step that only a human can run).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeOutcome {
    Pass,
    Fail,
    Skipped,
    NotExecuted,
}

/// One probe's result, with whatever the probe actually saw.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub id: &'static str,
    pub outcome: ProbeOutcome,
    /// One line: what was observed.
    pub summary: String,
    /// Captured evidence — the tool's own output, or the path that was read. May be empty.
    pub output: String,
}

impl Probe {
    pub fn pass(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, ProbeOutcome::Pass, summary)
    }
    pub fn fail(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, ProbeOutcome::Fail, summary)
    }
    pub fn skipped(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, ProbeOutcome::Skipped, summary)
    }
    pub fn not_executed(id: &'static str, summary: impl Into<String>) -> Self {
        Self::new(id, ProbeOutcome::NotExecuted, summary)
    }

    fn new(id: &'static str, outcome: ProbeOutcome, summary: impl Into<String>) -> Self {
        Self {
            id,
            outcome,
            summary: summary.into(),
            output: String::new(),
        }
    }

    /// Attach captured evidence, truncated to [`MAX_OUTPUT`] characters.
    #[must_use]
    pub fn with_output(mut self, output: impl Into<String>) -> Self {
        self.output = truncate(&output.into());
        self
    }
}

/// The default budget for one external command. Long enough for `nvidia-smi -q` on a cold driver,
/// short enough that a wedged tool cannot hold up a boot verdict.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// How much captured output a probe keeps. `nvidia-smi -q` alone prints several hundred lines;
/// the report is evidence, not a log file.
const MAX_OUTPUT: usize = 2000;

fn truncate(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= MAX_OUTPUT {
        return text.to_owned();
    }
    let kept: String = text.chars().take(MAX_OUTPUT).collect();
    format!("{kept}… (truncated)")
}

/// Run `work` on a worker thread and give up at `timeout`, returning `on_timeout()` instead.
///
/// The work itself keeps running on its thread — there is no safe way to interrupt arbitrary code
/// — but the caller is released, which is the property both `lege-boot-health` (must not block a
/// boot) and `lege doctor` (must not hang a terminal) need.
pub fn bounded<T: Send + 'static>(
    timeout: Duration,
    work: impl FnOnce() -> T + Send + 'static,
    on_timeout: impl FnOnce() -> T,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(timeout).unwrap_or_else(|_| on_timeout())
}

/// Spawn `program`, capture its output, and kill it if it outlives `timeout`.
///
/// A missing binary surfaces as [`std::io::ErrorKind::NotFound`] (callers turn that into
/// `Skipped`); a hang surfaces as [`std::io::ErrorKind::TimedOut`] (callers turn that into
/// `NotExecuted`). stdin is `/dev/null` so nothing can ever block waiting for a human.
pub fn run_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            use std::io::Read;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_end(&mut stdout);
            }
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_end(&mut stderr);
            }
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{program} timed out after {timeout:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Run one reporting tool and classify the three ways it can fail to produce a verdict.
///
/// `interpret` sees the successful output and decides `Pass`/`Fail`. Everything else is handled
/// here so that no probe can accidentally report a missing tool as a working one.
fn tool(
    id: &'static str,
    program: &str,
    args: &[&str],
    timeout: Duration,
    interpret: impl FnOnce(&str) -> Probe,
) -> Probe {
    match run_with_timeout(program, args, timeout) {
        Ok(output) if output.status.success() => {
            interpret(&String::from_utf8_lossy(&output.stdout))
        }
        Ok(output) => Probe::fail(id, format!("{program} exited with {}", output.status))
            .with_output(String::from_utf8_lossy(&output.stderr).into_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Probe::skipped(id, format!("{program} is not installed"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
            Probe::not_executed(id, format!("{program} timed out after {timeout:?}"))
        }
        Err(error) => Probe::fail(id, format!("{program}: {error}")),
    }
}

/// The first line of a tool's output, for a one-line summary.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_owned()
}

/// True when a display controller from NVIDIA is present under this sysfs root.
fn has_nvidia(root: &Path) -> bool {
    scan_sysfs(&root.join("sys"))
        .unwrap_or_default()
        .iter()
        .any(|gpu| gpu.vendor == GpuVendor::Nvidia)
}

// ---------------------------------------------------------------------------------------------
// Filesystem probes. These read the tree under `root` and never execute anything, so they are
// meaningful against an unpacked image root as well as against `/`.
// ---------------------------------------------------------------------------------------------

/// The running kernel release, from `proc/sys/kernel/osrelease`.
pub fn kernel_release(root: &Path, _timeout: Duration) -> Probe {
    let path = root.join("proc/sys/kernel/osrelease");
    match fs::read_to_string(&path) {
        Ok(release) => Probe::pass("kernel-release", format!("kernel {}", release.trim()))
            .with_output(path.display().to_string()),
        Err(error) => Probe::skipped(
            "kernel-release",
            format!("{} is unreadable: {error}", path.display()),
        ),
    }
}

/// At least one DRM connector reports `connected`.
pub fn drm_connected_output(root: &Path, _timeout: Duration) -> Probe {
    match scan_outputs(&root.join("sys")) {
        Ok(outputs) => {
            let connected: Vec<&str> = outputs
                .iter()
                .filter(|o| o.connected)
                .map(|o| o.connector.as_str())
                .collect();
            if connected.is_empty() {
                Probe::fail("drm-connected-output", "no connected DRM output")
            } else {
                Probe::pass(
                    "drm-connected-output",
                    format!("{} connected", connected.len()),
                )
                .with_output(connected.join(", "))
            }
        }
        Err(error) => Probe::fail("drm-connected-output", error.to_string()),
    }
}

/// A `cardN` device exists and `nomodeset` is not on the kernel command line.
pub fn drm_modesetting_initialized(root: &Path, _timeout: Duration) -> Probe {
    let drm = root.join("sys/class/drm");
    let has_card = fs::read_dir(&drm).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("card") && !name.contains('-')
        })
    });
    if !has_card {
        return Probe::fail(
            "drm-modesetting",
            format!("no cardN entries under {}", drm.display()),
        );
    }
    let cmdline = fs::read_to_string(root.join("proc/cmdline")).unwrap_or_default();
    if cmdline.split_whitespace().any(|word| word == "nomodeset") {
        return Probe::fail(
            "drm-modesetting",
            "nomodeset present on the kernel command line",
        );
    }
    Probe::pass("drm-modesetting", "card present, nomodeset absent")
}

/// The loaded NVIDIA kernel module and the installed NVIDIA userspace belong to the same driver
/// branch — the failure ADR-0003's one driver authority exists to prevent (kernel and NVIDIA must
/// move as a pair). No NVIDIA GPU is a `Pass`: there is nothing to be incoherent about.
pub fn nvidia_module_state(root: &Path, _timeout: Duration) -> Probe {
    let gpus = match scan_sysfs(&root.join("sys")) {
        Ok(gpus) => gpus,
        Err(error) => return Probe::fail("nvidia-module", error.to_string()),
    };
    let profile = classify(gpus);
    let has_nvidia = profile
        .gpus
        .iter()
        .any(|gpu| gpu.vendor == GpuVendor::Nvidia);
    if !has_nvidia {
        return Probe::pass("nvidia-module", "no NVIDIA GPU present");
    }

    let module_version = fs::read_to_string(root.join("sys/module/nvidia/version"))
        .ok()
        .map(|s| s.trim().to_owned());
    let Some(module_version) = module_version else {
        return Probe::fail(
            "nvidia-module",
            "NVIDIA GPU present but the nvidia module is not loaded",
        );
    };

    let packages = dpkg::scan_status(&root.join("var/lib/dpkg/status"));
    let userspace = dpkg::find_installed(&packages, "nvidia-driver-");
    let Some(userspace) = userspace.first() else {
        return Probe::fail(
            "nvidia-module",
            format!(
                "module {module_version} loaded but no nvidia-driver-* userspace package is installed"
            ),
        );
    };

    if branch_of(&module_version) == branch_of(&userspace.version) {
        Probe::pass(
            "nvidia-module",
            format!(
                "module {module_version} matches userspace {}",
                userspace.version
            ),
        )
        .with_output(format!("{} {}", userspace.name, userspace.version))
    } else {
        Probe::fail(
            "nvidia-module",
            format!(
                "module {module_version} does not match userspace {} — kernel and NVIDIA did not move as a pair",
                userspace.version
            ),
        )
        .with_output(format!("{} {}", userspace.name, userspace.version))
    }
}

/// The leading numeric component of a driver version string (`"580.65.06"` → `"580"`), the same
/// coarse comparison `lege-hostd`'s transaction verification uses.
fn branch_of(version: &str) -> &str {
    version.split(['.', '-']).next().unwrap_or(version)
}

/// Whether NVIDIA render offload is *available* — a hybrid machine with the NVIDIA kernel driver
/// bound and `prime-select` present. Availability only: this probe never selects a mode, never
/// sets `__NV_PRIME_RENDER_OFFLOAD` for anything, and never runs `prime-select`. Actually
/// rendering a frame on the dGPU is the manual `intel-desktop-nvidia-offload` case.
pub fn prime_offload_available(root: &Path, _timeout: Duration) -> Probe {
    let profile = classify(scan_sysfs(&root.join("sys")).unwrap_or_default());
    if !profile.hybrid {
        return Probe::skipped(
            "prime-offload",
            "not a hybrid-graphics machine: render offload does not apply",
        );
    }
    let bound = profile
        .gpus
        .iter()
        .find(|gpu| gpu.vendor == GpuVendor::Nvidia)
        .and_then(|gpu| gpu.driver.clone());
    let selector = root.join("usr/bin/prime-select").exists();
    match bound.as_deref() {
        Some("nvidia") => Probe::pass(
            "prime-offload",
            format!(
                "hybrid, nvidia driver bound, prime-select {}",
                if selector { "present" } else { "absent" }
            ),
        )
        .with_output("offload is invoked as __NV_PRIME_RENDER_OFFLOAD=1 (not exercised here)"),
        Some(other) => Probe::fail(
            "prime-offload",
            format!("hybrid machine but the NVIDIA GPU is bound to {other}, not nvidia"),
        ),
        None => Probe::fail(
            "prime-offload",
            "hybrid machine but no kernel driver is bound to the NVIDIA GPU",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Tool probes. These execute a reporting binary on the *live* machine; `root` is consulted only
// to decide whether the question applies at all. A caller inspecting an image root must not run
// them — see `lege doctor --hardware`, which marks them NotExecuted unless the root is `/`.
// ---------------------------------------------------------------------------------------------

/// Who signed the loaded `nvidia` module, and whether it came from the Canonical-signed LRM.
///
/// LegeOS ships prebuilt restricted modules and never DKMS (planning doc 01 §4): a module signed
/// by a locally enrolled MOK with no `linux-modules-nvidia-*` package installed is a DKMS build,
/// which is a genuine coherence failure however well it happens to work today.
pub fn nvidia_module_signer(root: &Path, timeout: Duration) -> Probe {
    if !has_nvidia(root) {
        return Probe::skipped("nvidia-module-signer", "no NVIDIA GPU present");
    }
    tool(
        "nvidia-module-signer",
        "modinfo",
        &["-F", "signer", "nvidia"],
        timeout,
        |stdout| {
            let signer = first_line(stdout);
            if signer.is_empty() {
                return Probe::fail(
                    "nvidia-module-signer",
                    "the nvidia module reports no signer (unsigned build)",
                );
            }
            let packages = dpkg::scan_status(&root.join("var/lib/dpkg/status"));
            if lrm_present(&packages) {
                Probe::pass("nvidia-module-signer", format!("signed by {signer}"))
                    .with_output(stdout.to_owned())
            } else {
                Probe::fail(
                    "nvidia-module-signer",
                    format!(
                        "signed by {signer}, but no linux-modules-nvidia-* package is installed — this module was not built from the Canonical-signed LRM"
                    ),
                )
                .with_output(stdout.to_owned())
            }
        },
    )
}

/// `nvidia-smi -q`: the driver's own report of itself.
pub fn nvidia_smi(root: &Path, timeout: Duration) -> Probe {
    if !has_nvidia(root) {
        return Probe::skipped("nvidia-smi", "no NVIDIA GPU present");
    }
    tool("nvidia-smi", "nvidia-smi", &["-q"], timeout, |stdout| {
        let field = |name: &str| {
            stdout
                .lines()
                .find(|line| line.trim_start().starts_with(name))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
        };
        let driver = field("Driver Version").unwrap_or_else(|| "unknown".to_owned());
        let name = field("Product Name").unwrap_or_else(|| "unknown GPU".to_owned());
        Probe::pass("nvidia-smi", format!("{name}, driver {driver}")).with_output(stdout.to_owned())
    })
}

/// `vulkaninfo --summary`: which Vulkan devices the loader can actually see.
pub fn vulkan_devices(_root: &Path, timeout: Duration) -> Probe {
    tool(
        "vulkan-devices",
        "vulkaninfo",
        &["--summary"],
        timeout,
        |stdout| {
            let devices: Vec<String> = stdout
                .lines()
                .filter_map(|line| line.trim().strip_prefix("deviceName"))
                .filter_map(|rest| rest.split_once('=').map(|(_, v)| v.trim().to_owned()))
                .collect();
            if devices.is_empty() {
                Probe::fail("vulkan-devices", "vulkaninfo reported no Vulkan device")
                    .with_output(stdout.to_owned())
            } else {
                Probe::pass(
                    "vulkan-devices",
                    format!("{} Vulkan device(s): {}", devices.len(), devices.join(", ")),
                )
                .with_output(stdout.to_owned())
            }
        },
    )
}

/// Vulkan first, then OpenGL — degrade gracefully (`Skipped`, never `Pass`) if neither probe tool
/// is installed. This is `lege-boot-health`'s liveness check: *something* can talk to the GPU.
pub fn graphics_probe(_root: &Path, timeout: Duration) -> Probe {
    graphics_probe_with_candidates(&[("vulkaninfo", &["--summary"]), ("eglinfo", &[])], timeout)
}

/// [`graphics_probe`] with an explicit candidate list, so tests can exercise every branch without
/// depending on what happens to be installed.
pub fn graphics_probe_with_candidates(candidates: &[(&str, &[&str])], timeout: Duration) -> Probe {
    for &(program, args) in candidates {
        let probe = tool("graphics-probe", program, args, timeout, |stdout| {
            Probe::pass(
                "graphics-probe",
                format!("{program}: {}", first_line(stdout)),
            )
            .with_output(stdout.to_owned())
        });
        if probe.outcome != ProbeOutcome::Skipped {
            return probe;
        }
    }
    Probe::skipped(
        "graphics-probe",
        "neither vulkaninfo nor eglinfo is installed",
    )
}

/// CUDA's `deviceQuery` sample, if the machine has it. Ubuntu does not package the CUDA samples,
/// so absence is the normal case and must read as `Skipped`, not as a CUDA failure.
pub fn cuda_device_query(root: &Path, timeout: Duration) -> Probe {
    if !has_nvidia(root) {
        return Probe::skipped("cuda-device-query", "no NVIDIA GPU present");
    }
    tool("cuda-device-query", "deviceQuery", &[], timeout, |stdout| {
        let result = stdout
            .lines()
            .find(|line| line.contains("Result ="))
            .unwrap_or_else(|| stdout.lines().next().unwrap_or_default())
            .trim()
            .to_owned();
        Probe::pass("cuda-device-query", result).with_output(stdout.to_owned())
    })
}

/// `vainfo`: which VA-API profiles the installed media driver offers.
pub fn vaapi(_root: &Path, timeout: Duration) -> Probe {
    tool("vaapi", "vainfo", &[], timeout, |stdout| {
        let driver = stdout
            .lines()
            .find(|line| line.contains("Driver version"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let profiles = stdout
            .lines()
            .filter(|line| line.contains("VAProfile"))
            .count();
        if profiles == 0 {
            Probe::fail("vaapi", "vainfo reported no VA-API profile").with_output(stdout.to_owned())
        } else {
            Probe::pass("vaapi", format!("{profiles} VA-API profiles; {driver}"))
                .with_output(stdout.to_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_that_times_out_is_not_executed_rather_than_failed() {
        // A hung tool is not evidence of broken hardware. Reporting it as Fail would make a busy
        // machine look unqualified; reporting it as Pass would be a lie. It is NotExecuted.
        let probe = tool("slow", "sleep", &["5"], Duration::from_millis(80), |_| {
            Probe::pass("slow", "unreachable")
        });
        assert_eq!(probe.outcome, ProbeOutcome::NotExecuted);
        assert!(probe.summary.contains("timed out"));

        // The same rule for a check that hangs without being a subprocess.
        let bounded_probe = bounded(
            Duration::from_millis(50),
            || {
                std::thread::sleep(Duration::from_secs(5));
                Probe::pass("slow", "unreachable")
            },
            || Probe::not_executed("slow", "timed out"),
        );
        assert_eq!(bounded_probe.outcome, ProbeOutcome::NotExecuted);
    }

    #[test]
    fn a_missing_binary_is_skipped_not_failed() {
        // The distinction the whole report rests on: "vulkaninfo is not installed" must never be
        // reported as a Vulkan failure, and must never be reported as a pass either.
        let probe = tool(
            "absent",
            "lege-hwdetect-test-binary-that-cannot-exist",
            &[],
            Duration::from_secs(2),
            |_| Probe::pass("absent", "unreachable"),
        );
        assert_eq!(probe.outcome, ProbeOutcome::Skipped);
        assert!(probe.summary.contains("not installed"));

        let candidates: &[(&str, &[&str])] = &[
            ("lege-hwdetect-test-missing-vulkaninfo", &[]),
            ("lege-hwdetect-test-missing-eglinfo", &[]),
        ];
        assert_eq!(
            graphics_probe_with_candidates(candidates, Duration::from_secs(2)).outcome,
            ProbeOutcome::Skipped
        );

        // A tool that is present and fails is a genuine Fail, not a skip.
        let present: &[(&str, &[&str])] = &[("false", &[])];
        assert_eq!(
            graphics_probe_with_candidates(present, Duration::from_secs(2)).outcome,
            ProbeOutcome::Fail
        );
    }
}
