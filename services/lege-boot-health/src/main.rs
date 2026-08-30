//! `lege-boot-health` — runs as a **system** service ordered before `boot-complete.target`.
//! `FailureAction=reboot-force` in the unit bounds a genuine black-screen case, and the boot
//! counter bounds the number of tries — but both of those are only correct to arm when there is
//! something to roll back *to*. On an ordinary boot (no kernel/driver update on trial) there is
//! no candidate deployment and nothing a forced reboot could improve; it can only loop. So this
//! binary separates two questions that must never be collapsed into one:
//!
//!   1. Is the machine healthy right now? (`Report::healthy`, from the six checks below —
//!      computed and printed on *every* boot, because `lege doctor` wants it regardless.)
//!   2. Is this boot a trial of a candidate deployment? (`Report::trial`, queried from
//!      `lege-hostd`'s `Updates1.Current` — phase `"trial"` means "yes".)
//!
//! Only `trial && !healthy` exits non-zero and arms the reboot ([`should_force_reboot`]). An
//! unhealthy *ordinary* boot still prints its full report and exits 0: there is nothing to roll
//! back to, and forcing a reboot on every boot of every machine — including a perfectly healthy
//! first boot before anything has ever been wired up — is strictly worse than not checking at
//! all. (An earlier version of this file did not make that distinction and would have reboot-
//! looped any machine on which the two markers below were never written — which, before the
//! session-daemon track wires its side, is every machine.)
//!
//! Every check is bounded by a timeout (a hung probe must not hang boot forever) and reports one
//! of three states: `Pass`, `Fail`, or `Skipped` — a skip (a probe tool that is not installed) is
//! never silently treated as a pass; it shows up in both the JSON report and the human summary so
//! "we didn't check" is never confused with "it's fine". The two marker checks use that same
//! vocabulary to distinguish "nobody has ever been able to report this fact" from "somebody
//! reported it unhealthy": see [`SESSION_HEALTHY_REPORT_WIRED`].
//!
//! Two checks read a marker file under `/run/legeos/` rather than querying the session directly,
//! because a system-scope oneshot cannot reach into a user session bus to ask "is
//! `lege-comp.service` active". `/run/legeos/` is `lege-hostd`'s own `RuntimeDirectory`
//! (`session/units/system/lege-hostd.service`), so both markers are **written by hostd**, not by
//! the session directly — `lege-sessiond` cannot write into a root-owned runtime directory, so it
//! reports over the new `org.legeos.Host.Session1` interface
//! (`interfaces/dbus/org.legeos.Host.Session1.xml`) and hostd is the one that persists the fact:
//!
//!   - `/run/legeos/compositor-active` — hostd writes this when `Session1.ReportCompositorActive`
//!     is called, once the compositor's Wayland socket is accepting connections.
//!   - `/run/legeos/session-healthy` — hostd writes this when `Session1.ReportSessionHealthy` is
//!     called, once sessiond has loaded policy and claimed its D-Bus name.
//!
//! `lege-sessiond` reports session health at readiness and probes the public Wayland socket before
//! reporting compositor activity. The `*_REPORT_WIRED` switches below keep those claims explicit.

use anyhow::Result;
use clap::Parser;
use lege_hwdetect::{GpuVendor, classify, dpkg, scan_outputs, scan_sysfs};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Whether `lege-sessiond` (or whatever ends up calling it) is actually calling
/// `Session1.ReportCompositorActive`/`ReportSessionHealthy` yet. This is a fact about the
/// session-daemon track's code, not something inferable at runtime from a single boot — a marker
/// being absent looks identical whether nobody has ever been able to write it or whether the
/// session genuinely failed to come up. So it is a compile-time switch, not a guess:
///
///   - `false` (today): an absent marker means "nothing can write this yet" → [`Status::Skipped`].
///     A skip never arms a reboot (see the module docs), so this is safe by construction even
///     though it is fail-*open* for this one fact.
///   - `true` (once the session-daemon track wires the call, tracked in
///     `session/units/system/README.md`): an absent marker means the session genuinely did not
///     report healthy → [`Status::Fail`], which — combined with `Report::trial` — can once again
///     arm a rollback the way the module doc describes.
///
/// Flipping one of these is a one-line, reviewable change, and [`Report`] always states which
/// mode each marker is in, so nobody re-arms a fail-closed check without noticing which side of
/// the switch they are on.
///
/// They are deliberately *separate*: the two markers are wired by different evidence and became
/// available at different times. Collapsing them into one switch would mean arming a check whose
/// writer does not exist in order to arm one whose writer does — the exact confusion between
/// "unwired" and "unhealthy" this vocabulary exists to prevent.
///
/// `lege-sessiond` reports session health at the same moment it signals systemd readiness: the
/// policy is loaded and the bus name is claimed.
const SESSION_HEALTHY_REPORT_WIRED: bool = true;

/// `lege-sessiond` now waits for the public Wayland socket to accept a connection before asking
/// hostd to persist this marker.
const COMPOSITOR_ACTIVE_REPORT_WIRED: bool = true;

/// Boot health probe.
#[derive(Debug, Parser)]
#[command(name = "lege-boot-health", version, about)]
struct Cli {
    /// Print the report and exit 0 regardless of health (for `lege doctor`).
    #[arg(long)]
    report_only: bool,
    /// Root of the system to probe (tests and `--report-only` on a dev machine point this at a
    /// fixture tree; production always uses `/`).
    #[arg(long, default_value = "/")]
    root: PathBuf,
    /// Wall-clock budget per check, in seconds.
    #[arg(long, default_value_t = 10)]
    timeout_secs: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    Pass,
    Fail,
    /// The probe could not run (a tool is missing) — distinct from `Pass` so a missing
    /// `vulkaninfo` is never confused with a working Vulkan stack.
    Skipped,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Pass,
            detail: detail.into(),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            detail: detail.into(),
        }
    }
    fn skipped(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skipped,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    healthy: bool,
    /// Whether a candidate deployment is actually on trial this boot — see the module docs.
    /// Only `trial && !healthy` may arm a forced reboot.
    trial: bool,
    /// Mirrors [`SESSION_HEALTHY_REPORT_WIRED`] and [`COMPOSITOR_ACTIVE_REPORT_WIRED`], so a
    /// reader of the JSON report can tell whether an absent marker was treated as "not wired yet"
    /// or "genuinely unhealthy" without having to know the binary's source.
    session_healthy_report_wired: bool,
    compositor_active_report_wired: bool,
    checks: Vec<Check>,
}

/// Whether the process should ask systemd to force a reboot
/// (`FailureAction=reboot-force` in `lege-boot-health.service`). Deliberately not just
/// `!healthy`: an ordinary boot has no candidate deployment and nothing to roll back to, so
/// forcing a reboot then cannot improve anything and can only loop forever. Only a boot that is
/// genuinely trialling a candidate — and found it unhealthy — may arm the reboot.
fn should_force_reboot(report: &Report) -> bool {
    report.trial && !report.healthy
}

/// Runs every check against `root` and assembles the report. `trial` is resolved by the caller
/// (a D-Bus round trip in production) so this stays a plain, fixture-driven function to test.
fn evaluate(root: &Path, timeout: Duration, trial: bool) -> Report {
    let checks = run_checks(root, timeout);
    let healthy = checks.iter().all(|check| check.status != Status::Fail);
    Report {
        healthy,
        trial,
        session_healthy_report_wired: SESSION_HEALTHY_REPORT_WIRED,
        compositor_active_report_wired: COMPOSITOR_ACTIVE_REPORT_WIRED,
        checks,
    }
}

fn run_checks(root: &Path, timeout: Duration) -> Vec<Check> {
    vec![
        bounded("drm-connected-output", timeout, {
            let root = root.to_path_buf();
            move || drm_connected_output(&root)
        }),
        bounded("nvidia-module", timeout, {
            let root = root.to_path_buf();
            move || nvidia_module_state(&root)
        }),
        bounded("drm-modesetting", timeout, {
            let root = root.to_path_buf();
            move || drm_modesetting_initialized(&root)
        }),
        bounded("compositor-active", timeout, {
            let root = root.to_path_buf();
            move || {
                marker_check(
                    "compositor-active",
                    &root,
                    "run/legeos/compositor-active",
                    COMPOSITOR_ACTIVE_REPORT_WIRED,
                )
            }
        }),
        bounded("graphics-probe", timeout, move || graphics_probe(timeout)),
        bounded("session-healthy", timeout, {
            let root = root.to_path_buf();
            move || {
                marker_check(
                    "session-healthy",
                    &root,
                    "run/legeos/session-healthy",
                    SESSION_HEALTHY_REPORT_WIRED,
                )
            }
        }),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();
    let timeout = Duration::from_secs(cli.timeout_secs.max(1));

    let trial = query_is_trial_boot(timeout).await;
    let report = evaluate(&cli.root, timeout, trial);
    print_human_summary(&report);
    println!("{}", serde_json::to_string_pretty(&report)?);

    if cli.report_only || !should_force_reboot(&report) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

/// Asks `lege-hostd` whether the current transaction is in `Phase::Trial`. Bounded by `timeout`
/// like every other check; any failure to connect, call or parse (hostd not running yet, no bus,
/// malformed reply) resolves to `false` — the fail-safe direction, since `false` can only ever
/// suppress a reboot, never force one.
async fn query_is_trial_boot(timeout: Duration) -> bool {
    match tokio::time::timeout(timeout, query_transaction_phase()).await {
        Ok(Some(phase)) => phase == "trial",
        _ => false,
    }
}

async fn query_transaction_phase() -> Option<String> {
    let connection = zbus::Connection::system().await.ok()?;
    let reply = connection
        .call_method(
            Some("org.legeos.Host"),
            "/org/legeos/Host",
            Some("org.legeos.Host.Updates1"),
            "Current",
            &(),
        )
        .await
        .ok()?;
    let json: String = reply.body().deserialize().ok()?;
    phase_from_transaction_json(&json)
}

/// The `phase` field of a `Transaction` JSON blob (`Updates1.Current`'s return value), or `None`
/// for the empty string `Current` returns when no transaction exists. Split out from
/// [`query_transaction_phase`] so the parsing logic is testable without a bus connection.
fn phase_from_transaction_json(json: &str) -> Option<String> {
    if json.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("phase")?.as_str().map(str::to_owned)
}

/// Runs `check` on a worker thread so a hang cannot block boot past `timeout`.
fn bounded(
    name: &'static str,
    timeout: Duration,
    check: impl FnOnce() -> Check + Send + 'static,
) -> Check {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check());
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Check::fail(name, format!("timed out after {timeout:?}")))
}

fn print_human_summary(report: &Report) {
    eprintln!(
        "lege-boot-health: {} (trial={}, session-healthy-wired={}, compositor-active-wired={}, \
         force-reboot={})",
        if report.healthy {
            "HEALTHY"
        } else {
            "UNHEALTHY"
        },
        report.trial,
        report.session_healthy_report_wired,
        report.compositor_active_report_wired,
        should_force_reboot(report),
    );
    for check in &report.checks {
        let marker = match check.status {
            Status::Pass => "OK  ",
            Status::Fail => "FAIL",
            Status::Skipped => "SKIP",
        };
        eprintln!("  [{marker}] {:<20} {}", check.name, check.detail);
    }
}

fn drm_connected_output(root: &Path) -> Check {
    match scan_outputs(&root.join("sys")) {
        Ok(outputs) => {
            let connected = outputs.iter().filter(|o| o.connected).count();
            if connected > 0 {
                Check::pass("drm-connected-output", format!("{connected} connected"))
            } else {
                Check::fail("drm-connected-output", "no connected DRM output")
            }
        }
        Err(error) => Check::fail("drm-connected-output", error.to_string()),
    }
}

fn nvidia_module_state(root: &Path) -> Check {
    let gpus = match scan_sysfs(&root.join("sys")) {
        Ok(gpus) => gpus,
        Err(error) => return Check::fail("nvidia-module", error.to_string()),
    };
    let profile = classify(gpus);
    let has_nvidia = profile
        .gpus
        .iter()
        .any(|gpu| gpu.vendor == GpuVendor::Nvidia);
    if !has_nvidia {
        return Check::pass("nvidia-module", "no NVIDIA GPU present");
    }

    let module_version = fs::read_to_string(root.join("sys/module/nvidia/version"))
        .ok()
        .map(|s| s.trim().to_owned());
    let Some(module_version) = module_version else {
        return Check::fail(
            "nvidia-module",
            "NVIDIA GPU present but the nvidia module is not loaded",
        );
    };

    let packages = dpkg::scan_status(&root.join("var/lib/dpkg/status"));
    let userspace = dpkg::find_installed(&packages, "nvidia-driver-");
    let Some(userspace) = userspace.first() else {
        return Check::fail(
            "nvidia-module",
            format!(
                "module {module_version} loaded but no nvidia-driver-* userspace package is installed"
            ),
        );
    };

    if branch_of(&module_version) == branch_of(&userspace.version) {
        Check::pass(
            "nvidia-module",
            format!(
                "module {module_version} matches userspace {}",
                userspace.version
            ),
        )
    } else {
        Check::fail(
            "nvidia-module",
            format!(
                "module {module_version} does not match userspace {} — kernel and NVIDIA did not move as a pair",
                userspace.version
            ),
        )
    }
}

/// The leading numeric component of a driver version string (`"580.65.06"` → `"580"`), the same
/// coarse comparison `lege-hostd`'s transaction verification uses.
fn branch_of(version: &str) -> &str {
    version.split(['.', '-']).next().unwrap_or(version)
}

fn drm_modesetting_initialized(root: &Path) -> Check {
    let drm = root.join("sys/class/drm");
    let has_card = fs::read_dir(&drm).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("card") && !name.contains('-')
        })
    });
    if !has_card {
        return Check::fail(
            "drm-modesetting",
            format!("no cardN entries under {}", drm.display()),
        );
    }
    let cmdline = fs::read_to_string(root.join("proc/cmdline")).unwrap_or_default();
    if cmdline.split_whitespace().any(|word| word == "nomodeset") {
        return Check::fail(
            "drm-modesetting",
            "nomodeset present on the kernel command line",
        );
    }
    Check::pass("drm-modesetting", "card present, nomodeset absent")
}

/// `writer_wired` is [`SESSION_HEALTHY_REPORT_WIRED`] at every real call site; a parameter here purely so
/// tests can exercise both branches without touching the constant.
fn marker_check(name: &'static str, root: &Path, relative: &str, writer_wired: bool) -> Check {
    let path = root.join(relative);
    match fs::read_to_string(&path) {
        Ok(content) => Check::pass(name, format!("marker present: {}", content.trim())),
        Err(_) if writer_wired => Check::fail(
            name,
            format!(
                "marker absent at {} (writer is wired: this means the session did not report healthy)",
                path.display()
            ),
        ),
        Err(_) => Check::skipped(
            name,
            format!(
                "marker absent at {} (no writer for this fact yet; see the *_REPORT_WIRED switches)",
                path.display()
            ),
        ),
    }
}

/// Vulkan first, then OpenGL — degrade gracefully (report `Skipped`, not `Pass`) if neither probe
/// tool is installed.
fn graphics_probe(timeout: Duration) -> Check {
    graphics_probe_with_candidates(&[("vulkaninfo", &["--summary"]), ("eglinfo", &[])], timeout)
}

fn graphics_probe_with_candidates(candidates: &[(&str, &[&str])], timeout: Duration) -> Check {
    for &(tool, args) in candidates {
        match run_with_timeout(tool, args, timeout) {
            Ok(output) if output.status.success() => {
                let first_line = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                return Check::pass("graphics-probe", format!("{tool}: {first_line}"));
            }
            Ok(output) => {
                return Check::fail(
                    "graphics-probe",
                    format!("{tool} exited with {}", output.status),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Check::fail("graphics-probe", format!("{tool}: {error}")),
        }
    }
    Check::skipped(
        "graphics-probe",
        "neither vulkaninfo nor eglinfo is installed",
    )
}

fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> std::io::Result<Output> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn drm_connected_output_fails_with_nothing_connected() {
        let dir = fixture();
        let card = dir.path().join("sys/class/drm/card1-HDMI-A-1");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("status"), "disconnected\n").unwrap();
        assert_eq!(drm_connected_output(dir.path()).status, Status::Fail);
    }

    #[test]
    fn drm_connected_output_passes_with_one_connected() {
        let dir = fixture();
        let card = dir.path().join("sys/class/drm/card1-eDP-1");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("status"), "connected\n").unwrap();
        assert_eq!(drm_connected_output(dir.path()).status, Status::Pass);
    }

    #[test]
    fn nvidia_module_passes_without_an_nvidia_gpu() {
        let dir = fixture();
        assert_eq!(nvidia_module_state(dir.path()).status, Status::Pass);
    }

    fn write_nvidia_gpu(root: &Path) {
        let dev = root.join("sys/bus/pci/devices/0000:01:00.0");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("class"), "0x030000\n").unwrap();
        fs::write(dev.join("vendor"), "0x10de\n").unwrap();
        fs::write(dev.join("device"), "0x28e0\n").unwrap();
        fs::write(dev.join("boot_vga"), "0\n").unwrap();
    }

    #[test]
    fn nvidia_module_fails_when_gpu_present_but_module_unloaded() {
        let dir = fixture();
        write_nvidia_gpu(dir.path());
        assert_eq!(nvidia_module_state(dir.path()).status, Status::Fail);
    }

    #[test]
    fn nvidia_module_fails_on_version_mismatch_and_passes_on_match() {
        let dir = fixture();
        write_nvidia_gpu(dir.path());
        fs::create_dir_all(dir.path().join("sys/module/nvidia")).unwrap();
        fs::write(dir.path().join("sys/module/nvidia/version"), "580.65.06\n").unwrap();
        let dpkg_status = dir.path().join("var/lib/dpkg/status");
        fs::create_dir_all(dpkg_status.parent().unwrap()).unwrap();
        fs::write(
            &dpkg_status,
            "Package: nvidia-driver-575-open\nStatus: install ok installed\nVersion: 575.1\n\n",
        )
        .unwrap();
        assert_eq!(nvidia_module_state(dir.path()).status, Status::Fail);

        fs::write(
            &dpkg_status,
            "Package: nvidia-driver-580-open\nStatus: install ok installed\nVersion: 580.65.06\n\n",
        )
        .unwrap();
        assert_eq!(nvidia_module_state(dir.path()).status, Status::Pass);
    }

    #[test]
    fn drm_modesetting_fails_without_a_card_and_with_nomodeset() {
        let dir = fixture();
        assert_eq!(drm_modesetting_initialized(dir.path()).status, Status::Fail);

        fs::create_dir_all(dir.path().join("sys/class/drm/card0")).unwrap();
        assert_eq!(drm_modesetting_initialized(dir.path()).status, Status::Pass);

        fs::create_dir_all(dir.path().join("proc")).unwrap();
        fs::write(dir.path().join("proc/cmdline"), "quiet nomodeset\n").unwrap();
        assert_eq!(drm_modesetting_initialized(dir.path()).status, Status::Fail);
    }

    #[test]
    fn marker_check_is_skipped_not_failed_while_the_writer_is_unwired() {
        let dir = fixture();
        // writer_wired = false (matches COMPOSITOR_ACTIVE_REPORT_WIRED today): absence means "nobody can
        // write this yet", not "the session is unhealthy" — must never be Fail, or every machine
        // that has not wired the session-daemon side yet would look permanently unhealthy.
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/legeos/session-healthy",
            false,
        );
        assert_eq!(check.status, Status::Skipped);

        fs::create_dir_all(dir.path().join("run/legeos")).unwrap();
        fs::write(dir.path().join("run/legeos/session-healthy"), "ok\n").unwrap();
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/legeos/session-healthy",
            false,
        );
        assert_eq!(check.status, Status::Pass);
    }

    #[test]
    fn marker_check_fails_closed_once_the_writer_is_wired() {
        let dir = fixture();
        // writer_wired = true: now absence genuinely means the session did not report healthy.
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/legeos/session-healthy",
            true,
        );
        assert_eq!(check.status, Status::Fail);

        fs::create_dir_all(dir.path().join("run/legeos")).unwrap();
        fs::write(dir.path().join("run/legeos/session-healthy"), "ok\n").unwrap();
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/legeos/session-healthy",
            true,
        );
        assert_eq!(check.status, Status::Pass);
    }

    #[test]
    fn phase_from_transaction_json_handles_empty_and_malformed_input() {
        assert_eq!(phase_from_transaction_json(""), None);
        assert_eq!(phase_from_transaction_json("not json"), None);
        assert_eq!(phase_from_transaction_json(r#"{"no_phase_field":1}"#), None);
        assert_eq!(
            phase_from_transaction_json(r#"{"id":"txn-1","phase":"trial","log":[]}"#),
            Some("trial".to_owned())
        );
        assert_eq!(
            phase_from_transaction_json(r#"{"phase":"awaiting-reboot"}"#),
            Some("awaiting-reboot".to_owned())
        );
    }

    #[test]
    fn should_force_reboot_only_when_trial_and_unhealthy() {
        let report = |healthy: bool, trial: bool| Report {
            healthy,
            trial,
            session_healthy_report_wired: true,
            compositor_active_report_wired: true,
            checks: vec![],
        };
        assert!(!should_force_reboot(&report(true, true)));
        assert!(!should_force_reboot(&report(true, false)));
        assert!(!should_force_reboot(&report(false, false)));
        assert!(should_force_reboot(&report(false, true)));
    }

    #[test]
    fn an_unhealthy_ordinary_boot_never_forces_a_reboot() {
        // The bug this whole change fixes: an empty fixture root fails drm-connected-output (no
        // outputs at all), so `healthy` is false — but `trial` is false too (an ordinary boot),
        // so nothing should be armed.
        let dir = fixture();
        let report = evaluate(dir.path(), Duration::from_secs(2), false);
        assert!(
            !report.healthy,
            "expected at least one failing check on an empty fixture root: {:?}",
            report.checks
        );
        assert!(!report.trial);
        assert!(!should_force_reboot(&report));
    }

    #[test]
    fn a_trial_boot_with_a_genuinely_failed_check_still_forces_a_reboot() {
        let dir = fixture();
        let report = evaluate(dir.path(), Duration::from_secs(2), true);
        assert!(!report.healthy);
        assert!(report.trial);
        assert!(should_force_reboot(&report));
    }

    #[test]
    fn a_healthy_trial_boot_does_not_force_a_reboot() {
        let dir = fixture();
        let card = dir.path().join("sys/class/drm/card0-eDP-1");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("status"), "connected\n").unwrap();
        fs::create_dir_all(dir.path().join("sys/class/drm/card0")).unwrap();
        // The session-healthy writer is wired (`SESSION_HEALTHY_REPORT_WIRED`), so a genuinely
        // healthy boot is one where the session actually reported: an absent marker now means
        // "the session did not come up", which is exactly the case a trial boot must fail on.
        fs::create_dir_all(dir.path().join("run/legeos")).unwrap();
        fs::write(dir.path().join("run/legeos/session-healthy"), "ok\n").unwrap();
        fs::write(dir.path().join("run/legeos/compositor-active"), "ok\n").unwrap();

        let report = evaluate(dir.path(), Duration::from_secs(5), true);
        assert!(report.healthy, "{:?}", report.checks);
        assert!(!should_force_reboot(&report));
    }

    #[test]
    fn a_trial_boot_where_the_session_never_reported_is_unhealthy() {
        // The case the whole rollback design exists for: the graphics stack came up, but the
        // session never appeared. Before the writer was wired this was indistinguishable from
        // "nobody can report this", and a black-screened trial boot looked healthy.
        let dir = fixture();
        let card = dir.path().join("sys/class/drm/card0-eDP-1");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("status"), "connected\n").unwrap();
        fs::create_dir_all(dir.path().join("sys/class/drm/card0")).unwrap();

        let report = evaluate(dir.path(), Duration::from_secs(5), true);
        assert!(!report.healthy);
        assert!(should_force_reboot(&report), "{:?}", report.checks);
    }

    #[test]
    fn graphics_probe_skips_when_no_tool_is_installed() {
        // Nonexistent binary names rather than touching process-wide PATH, so this test is safe
        // to run in parallel with everything else in this binary.
        let candidates: &[(&str, &[&str])] = &[
            ("lege-boot-health-test-missing-vulkaninfo", &[]),
            ("lege-boot-health-test-missing-eglinfo", &[]),
        ];
        let check = graphics_probe_with_candidates(candidates, Duration::from_secs(2));
        assert_eq!(check.status, Status::Skipped);
    }

    #[test]
    fn graphics_probe_passes_when_the_first_candidate_succeeds() {
        let candidates: &[(&str, &[&str])] = &[("printf", &["Vulkan Instance Version: 1.3\n"])];
        let check = graphics_probe_with_candidates(candidates, Duration::from_secs(2));
        assert_eq!(check.status, Status::Pass);
    }

    #[test]
    fn graphics_probe_fails_when_a_present_tool_exits_nonzero() {
        let candidates: &[(&str, &[&str])] = &[("false", &[])];
        let check = graphics_probe_with_candidates(candidates, Duration::from_secs(2));
        assert_eq!(check.status, Status::Fail);
    }

    #[test]
    fn run_with_timeout_kills_a_hung_process() {
        let result = run_with_timeout("sleep", &["5"], Duration::from_millis(100));
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn bounded_reports_a_failure_when_the_check_itself_hangs() {
        let check = bounded("slow", Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(5));
            Check::pass("slow", "")
        });
        assert_eq!(check.status, Status::Fail);
        assert!(check.detail.contains("timed out"));
    }
}
