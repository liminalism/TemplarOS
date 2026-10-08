//! `templar-boot-health` — runs as a **system** service ordered before `boot-complete.target`.
//! `FailureAction=reboot-force` in the unit bounds a genuine black-screen case, and the boot
//! counter bounds the number of tries — but both of those are only correct to arm when there is
//! something to roll back *to*. On an ordinary boot (no kernel/driver update on trial) there is
//! no candidate deployment and nothing a forced reboot could improve; it can only loop. So this
//! binary separates two questions that must never be collapsed into one:
//!
//!   1. Is the machine healthy right now? (`Report::healthy`, from the six checks below —
//!      computed and printed on *every* boot, because `templar doctor` wants it regardless.)
//!   2. Is this boot a trial of a candidate deployment? (`Report::trial`, queried from
//!      `templar-hostd`'s `Updates1.Current` — phase `"trial"` means "yes".)
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
//! Two checks read a marker file under `/run/templaros/` rather than querying the session directly,
//! because a system-scope oneshot cannot reach into a user session bus to ask "is
//! `templar-comp.service` active". `/run/templaros/` is `templar-hostd`'s own `RuntimeDirectory`
//! (`session/units/system/templar-hostd.service`), so both markers are **written by hostd**, not by
//! the session directly — `templar-sessiond` cannot write into a root-owned runtime directory, so it
//! reports over the new `org.templaros.Host.Session1` interface
//! (`interfaces/dbus/org.templaros.Host.Session1.xml`) and hostd is the one that persists the fact:
//!
//!   - `/run/templaros/compositor-active` — hostd writes this when `Session1.ReportCompositorActive`
//!     is called, once the compositor's Wayland socket is accepting connections.
//!   - `/run/templaros/session-healthy` — hostd writes this when `Session1.ReportSessionHealthy` is
//!     called, once sessiond has loaded policy and claimed its D-Bus name.
//!
//! `templar-sessiond` reports session health at readiness and probes the public Wayland socket before
//! reporting compositor activity. The `*_REPORT_WIRED` switches below keep those claims explicit.
//!
//! ## Acting on the verdict
//!
//! Reporting was, until now, all this binary did. The promotion loop was open on both boot
//! backends: nothing anywhere called `Deployments1.MarkGood`, so a candidate that booted perfectly
//! stayed unblessed until a human ran the CLI, and nothing called `Deployments1.Rollback`, so a
//! candidate that came up broken was left to `FailureAction=reboot-force` and the boot counter
//! running out. Both halves are closed here: a healthy trial boot blesses, an unhealthy one rolls
//! back to the known-good deployment, and [`PROMOTION_WIRED`] plus `--report-only` keep it
//! possible to run this binary without moving the boot default.

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use templar_hwdetect::probes::{self, Probe, ProbeOutcome};

/// Whether `templar-sessiond` (or whatever ends up calling it) is actually calling
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
/// `templar-sessiond` reports session health at the same moment it signals systemd readiness: the
/// policy is loaded and the bus name is claimed.
const SESSION_HEALTHY_REPORT_WIRED: bool = true;

/// `templar-sessiond` now waits for the public Wayland socket to accept a connection before asking
/// hostd to persist this marker.
const COMPOSITOR_ACTIVE_REPORT_WIRED: bool = true;

/// Boot health probe.
#[derive(Debug, Parser)]
#[command(name = "templar-boot-health", version, about)]
struct Cli {
    /// Print the report and exit 0 regardless of health (for `templar doctor`).
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
    /// Mirrors [`PROMOTION_WIRED`], for the same reason the two above do.
    promotion_wired: bool,
    /// What this run did about the candidate deployment, if anything.
    promotion: Promotion,
    checks: Vec<Check>,
}

/// Whether this binary is allowed to *act* on its verdict, rather than only report it.
///
/// The promotion loop has been open on both boot backends since the beginning: nothing ever
/// called `Deployments1.MarkGood`, so a candidate deployment could boot perfectly and still never
/// be blessed — only a human running `templar update` ever closed it. And nothing ever called
/// `Rollback`, so a candidate that came up broken relied entirely on `FailureAction=reboot-force`
/// and the boot counter running out, which is the blunt instrument, not the plan.
///
/// This is a switch rather than an unconditional call for the same reason
/// [`SESSION_HEALTHY_REPORT_WIRED`] is: acting here changes what the machine boots next time, so
/// which side of the switch the code is on should be readable in one line rather than inferred
/// from a call graph. `--report-only` also suppresses it, unconditionally, so `templar doctor` and
/// anyone debugging can run this binary without moving the boot default.
const PROMOTION_WIRED: bool = true;

/// What this run did about the deployment on trial.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "outcome", content = "detail")]
enum Promotion {
    /// Not a trial boot, or acting is switched off: nothing to decide.
    NotAttempted(&'static str),
    /// The candidate came up healthy and was blessed.
    MarkedGood,
    /// The candidate came up unhealthy and the previous known-good deployment was made default.
    RolledBack(String),
    /// It should have acted and could not — hostd unreachable, no known-good deployment to go
    /// back to, polkit refusing. Reported, never silently swallowed: an unattended probe that
    /// fails quietly is indistinguishable from one that was never wired up.
    Failed(String),
}

/// Whether the process should ask systemd to force a reboot
/// (`FailureAction=reboot-force` in `templar-boot-health.service`). Deliberately not just
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
        promotion_wired: PROMOTION_WIRED,
        promotion: Promotion::NotAttempted("not decided yet"),
        checks,
    }
}

/// What this run should do about the deployment on trial. Pure, so every branch is testable
/// without a bus.
fn intended_promotion(report: &Report, report_only: bool, wired: bool) -> Option<bool> {
    if report_only || !wired || !report.trial {
        return None;
    }
    Some(report.healthy)
}

/// The reason nothing was attempted, for the report.
fn not_attempted(report: &Report, report_only: bool, wired: bool) -> &'static str {
    if report_only {
        "--report-only"
    } else if !wired {
        "acting on the verdict is switched off"
    } else if !report.trial {
        "not a trial boot: no candidate to bless or roll back"
    } else {
        "not decided yet"
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
                    "run/templaros/compositor-active",
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
                    "run/templaros/session-healthy",
                    SESSION_HEALTHY_REPORT_WIRED,
                )
            }
        }),
    ]
}

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays, which
    // greps and JSON consumers cannot read. A terminal keeps its colours.
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();
    let timeout = Duration::from_secs(cli.timeout_secs.max(1));

    let trial = query_is_trial_boot(timeout).await;
    let mut report = evaluate(&cli.root, timeout, trial);

    // Close the loop: a candidate that came up healthy is blessed here rather than waiting for a
    // human to run `templar update mark-good`, and one that came up broken is rolled back here
    // rather than left to the boot counter and a forced reboot.
    report.promotion = match intended_promotion(&report, cli.report_only, PROMOTION_WIRED) {
        None => Promotion::NotAttempted(not_attempted(&report, cli.report_only, PROMOTION_WIRED)),
        Some(healthy) => match tokio::time::timeout(timeout, decide_deployment(healthy)).await {
            Ok(promotion) => promotion,
            Err(_) => Promotion::Failed("templar-hostd did not answer in time".to_owned()),
        },
    };

    print_human_summary(&report);
    println!("{}", serde_json::to_string_pretty(&report)?);

    if cli.report_only || !should_force_reboot(&report) {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

/// Bless or roll back the deployment on trial, through `templar-hostd`.
///
/// Every failure is reported rather than propagated: this runs before `boot-complete.target`, and
/// a probe that returns an error here takes the boot down over a bookkeeping call. If the
/// rollback could not be made, `should_force_reboot` still arms the reboot the way it always did
/// — the blunt instrument is still there, it is just no longer the only one.
async fn decide_deployment(healthy: bool) -> Promotion {
    let Ok(connection) = zbus::Connection::system().await else {
        return Promotion::Failed("no system bus".to_owned());
    };
    if healthy {
        return match call_deployments(&connection, "MarkGood", &()).await {
            Ok(()) => Promotion::MarkedGood,
            Err(error) => Promotion::Failed(format!("MarkGood failed: {error}")),
        };
    }
    let Some(target) = known_good_deployment(&connection).await else {
        return Promotion::Failed(
            "unhealthy trial boot with no known-good deployment to return to".to_owned(),
        );
    };
    match call_deployments(&connection, "Rollback", &(target.as_str(),)).await {
        Ok(()) => Promotion::RolledBack(target),
        Err(error) => Promotion::Failed(format!("Rollback failed: {error}")),
    }
}

async fn call_deployments<A>(
    connection: &zbus::Connection,
    method: &str,
    args: &A,
) -> Result<(), String>
where
    A: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    connection
        .call_method(
            Some("org.templaros.Host"),
            "/org/templaros/Host",
            Some("org.templaros.Host.Deployments1"),
            method,
            args,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The id of the deployment marked known-good, from `Deployments1.List`.
///
/// This is what a rollback returns *to*. There is deliberately no fallback to "whatever is first
/// in the list": rolling back to a deployment nobody has ever confirmed boots would turn one bad
/// boot into two.
async fn known_good_deployment(connection: &zbus::Connection) -> Option<String> {
    let reply = connection
        .call_method(
            Some("org.templaros.Host"),
            "/org/templaros/Host",
            Some("org.templaros.Host.Deployments1"),
            "List",
            &(),
        )
        .await
        .ok()?;
    let json: String = reply.body().deserialize().ok()?;
    known_good_from_deployments_json(&json)
}

/// The known-good deployment's id in a `Deployments1.List` reply. Split out so the choice is
/// testable without a bus.
fn known_good_from_deployments_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .as_array()?
        .iter()
        .find(|deployment| {
            deployment
                .get("known_good")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        })
        .and_then(|deployment| Some(deployment.get("id")?.as_str()?.to_owned()))
}

/// Asks `templar-hostd` whether the current transaction is in `Phase::Trial`. Bounded by `timeout`
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
            Some("org.templaros.Host"),
            "/org/templaros/Host",
            Some("org.templaros.Host.Updates1"),
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
///
/// A boot verdict has no fourth state to report a timeout into: an unproven check at boot must
/// hold the boot back, so a timed-out check is a failure here even though the same timeout is
/// [`ProbeOutcome::NotExecuted`] in `templar doctor`'s report, where nothing is being decided.
fn bounded(
    name: &'static str,
    timeout: Duration,
    check: impl FnOnce() -> Check + Send + 'static,
) -> Check {
    probes::bounded(timeout, check, || {
        Check::fail(name, format!("timed out after {timeout:?}"))
    })
}

/// Map a [`Probe`] onto this binary's three-state boot check.
///
/// `Skipped` stays `Skipped` — a missing `vulkaninfo` must never read as a working Vulkan stack,
/// and must never fail a boot either. `NotExecuted` becomes `Fail` for the reason in [`bounded`].
/// The boot log is one line per check, so only the probe's summary crosses over; the captured
/// output is for `templar doctor`'s report, not for the journal.
fn check_from(probe: &Probe) -> Check {
    let detail = probe.summary.clone();
    match probe.outcome {
        ProbeOutcome::Pass => Check::pass(probe.id, detail),
        ProbeOutcome::Fail | ProbeOutcome::NotExecuted => Check::fail(probe.id, detail),
        ProbeOutcome::Skipped => Check::skipped(probe.id, detail),
    }
}

fn print_human_summary(report: &Report) {
    eprintln!(
        "templar-boot-health: {} (trial={}, session-healthy-wired={}, compositor-active-wired={}, \
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
    // The deployment decision goes on its own line, not in the parenthesised list: it is the one
    // thing in this output that changed the machine.
    match &report.promotion {
        Promotion::NotAttempted(reason) => eprintln!("  deployment: nothing to decide ({reason})"),
        Promotion::MarkedGood => {
            eprintln!("  deployment: blessed — this candidate is now the default");
        }
        Promotion::RolledBack(id) => eprintln!("  deployment: rolled back to {id}"),
        Promotion::Failed(reason) => eprintln!("  deployment: COULD NOT ACT — {reason}"),
    }
}

fn drm_connected_output(root: &Path) -> Check {
    check_from(&probes::drm_connected_output(root, probes::DEFAULT_TIMEOUT))
}

fn nvidia_module_state(root: &Path) -> Check {
    check_from(&probes::nvidia_module_state(root, probes::DEFAULT_TIMEOUT))
}

fn drm_modesetting_initialized(root: &Path) -> Check {
    check_from(&probes::drm_modesetting_initialized(
        root,
        probes::DEFAULT_TIMEOUT,
    ))
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
    check_from(&probes::graphics_probe(Path::new("/"), timeout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_hwdetect::probes::run_with_timeout;

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
            "run/templaros/session-healthy",
            false,
        );
        assert_eq!(check.status, Status::Skipped);

        fs::create_dir_all(dir.path().join("run/templaros")).unwrap();
        fs::write(dir.path().join("run/templaros/session-healthy"), "ok\n").unwrap();
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/templaros/session-healthy",
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
            "run/templaros/session-healthy",
            true,
        );
        assert_eq!(check.status, Status::Fail);

        fs::create_dir_all(dir.path().join("run/templaros")).unwrap();
        fs::write(dir.path().join("run/templaros/session-healthy"), "ok\n").unwrap();
        let check = marker_check(
            "session-healthy",
            dir.path(),
            "run/templaros/session-healthy",
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

    fn report_of(healthy: bool, trial: bool) -> Report {
        Report {
            healthy,
            trial,
            session_healthy_report_wired: true,
            compositor_active_report_wired: true,
            promotion_wired: true,
            promotion: Promotion::NotAttempted("test"),
            checks: vec![],
        }
    }

    #[test]
    fn should_force_reboot_only_when_trial_and_unhealthy() {
        assert!(!should_force_reboot(&report_of(true, true)));
        assert!(!should_force_reboot(&report_of(true, false)));
        assert!(!should_force_reboot(&report_of(false, false)));
        assert!(should_force_reboot(&report_of(false, true)));
    }

    #[test]
    fn a_healthy_trial_boot_blesses_and_an_unhealthy_one_rolls_back() {
        // The loop this closes: nothing in the tree ever called MarkGood, so a candidate could
        // boot perfectly and still never become the default unless a human ran the CLI.
        assert_eq!(
            intended_promotion(&report_of(true, true), false, true),
            Some(true)
        );
        assert_eq!(
            intended_promotion(&report_of(false, true), false, true),
            Some(false)
        );
    }

    #[test]
    fn an_ordinary_boot_never_touches_the_deployment() {
        // Blessing on every boot would mark whatever is running good, which is exactly the
        // property a trial is supposed to establish rather than assume.
        assert_eq!(
            intended_promotion(&report_of(true, false), false, true),
            None
        );
        assert_eq!(
            not_attempted(&report_of(true, false), false, true),
            "not a trial boot: no candidate to bless or roll back"
        );
    }

    #[test]
    fn report_only_and_the_switch_both_suppress_acting() {
        assert_eq!(intended_promotion(&report_of(true, true), true, true), None);
        assert_eq!(
            not_attempted(&report_of(true, true), true, true),
            "--report-only"
        );
        assert_eq!(
            intended_promotion(&report_of(true, true), false, false),
            None
        );
        assert_eq!(
            not_attempted(&report_of(true, true), false, false),
            "acting on the verdict is switched off"
        );
    }

    #[test]
    fn a_rollback_target_is_the_known_good_deployment_and_nothing_else() {
        // Rolling back to a deployment nobody has confirmed boots turns one bad boot into two.
        let json = r#"[
            {"id":"cand2","known_good":false},
            {"id":"base1","known_good":true},
            {"id":"cand1","known_good":false}
        ]"#;
        assert_eq!(
            known_good_from_deployments_json(json).as_deref(),
            Some("base1")
        );
        assert_eq!(
            known_good_from_deployments_json(r#"[{"id":"cand2","known_good":false}]"#),
            None,
            "no known-good deployment means no rollback target, not a guess"
        );
        assert_eq!(known_good_from_deployments_json("[]"), None);
        assert_eq!(known_good_from_deployments_json("not json"), None);
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
        fs::create_dir_all(dir.path().join("run/templaros")).unwrap();
        fs::write(dir.path().join("run/templaros/session-healthy"), "ok\n").unwrap();
        fs::write(dir.path().join("run/templaros/compositor-active"), "ok\n").unwrap();

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
            ("templar-boot-health-test-missing-vulkaninfo", &[]),
            ("templar-boot-health-test-missing-eglinfo", &[]),
        ];
        let check = check_from(&probes::graphics_probe_with_candidates(
            candidates,
            Duration::from_secs(2),
        ));
        assert_eq!(check.status, Status::Skipped);
    }

    #[test]
    fn graphics_probe_passes_when_the_first_candidate_succeeds() {
        let candidates: &[(&str, &[&str])] = &[("printf", &["Vulkan Instance Version: 1.3\n"])];
        let check = check_from(&probes::graphics_probe_with_candidates(
            candidates,
            Duration::from_secs(2),
        ));
        assert_eq!(check.status, Status::Pass);
    }

    #[test]
    fn graphics_probe_fails_when_a_present_tool_exits_nonzero() {
        let candidates: &[(&str, &[&str])] = &[("false", &[])];
        let check = check_from(&probes::graphics_probe_with_candidates(
            candidates,
            Duration::from_secs(2),
        ));
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
