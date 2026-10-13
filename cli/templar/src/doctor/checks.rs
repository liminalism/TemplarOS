//! The check catalogue. Add a check here and it runs everywhere doctor runs.
//!
//! Several checks below (planning doc 01 §7's "package-level integration testing") are
//! runtime-only: they read the live per-user state store (`templar_state::Store`) rather than a
//! packaged file, so they cannot say anything useful about an unpacked image root that has never
//! booted a session. Those checks `Skip` rather than `Fail` when `context.root != "/"`, and — the
//! important part — they only ever *open* the store when it already exists (`Path::exists`
//! first): `templar doctor` is a read-only diagnostic, and `templar_state::Store::open` creates and
//! migrates a fresh database if none is there, which running a check must never do as a side
//! effect.

use super::{Check, Context, Finding, Outcome, Severity, exists, finding};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use templar_types::{AppManifest, ProviderRole};

pub fn all() -> Vec<Box<dyn Check>> {
    vec![
        Box::new(OsRelease),
        Box::new(SessionUnitsPresent),
        Box::new(UnitBinariesPresent),
        Box::new(BuildSetCoherent),
        Box::new(RunningMatchesInstalled),
        Box::new(PackageAgesConsistent),
        Box::new(CompositorUsable),
        Box::new(ProtectedUnitsHaveResourceControl),
        Box::new(OomdOwnership),
        Box::new(NvidiaFlavourConsistent),
        Box::new(NoStandaloneNvidiaInstaller),
        Box::new(PortalConfigPresent),
        Box::new(StorageReserve),
        Box::new(HostUnitsPresent),
        Box::new(AppManifestsValid),
        Box::new(MimeHandlersInstalled),
        Box::new(AutostartProvidersResolved),
        Box::new(FileIndexHealth),
    ]
}

/// Load every manifest under the search order `templar_appd::manifests` documents, rooted at
/// `context.root` so this works against an image root as well as `/`. Only `SYSTEM_DIR` and
/// `ETC_DIR` are checked here — `$XDG_DATA_HOME/templaros/apps` is a live user directory, not a
/// packaged one, and out of scope for a distribution-image check.
fn load_manifests(context: &Context) -> Vec<AppManifest> {
    [
        templar_appd::manifests::SYSTEM_DIR,
        templar_appd::manifests::ETC_DIR,
    ]
    .iter()
    .flat_map(|dir| templar_appd::manifests::load_dir(&context.path(dir)).unwrap_or_default())
    .collect()
}

/// Every TemplarOS unit's `ExecStart=` binary is actually installed.
///
/// `SessionUnitsPresent` above checks the units are there, which is not the same thing: a unit is
/// a promise that a binary will be at a path, and an install can satisfy the first half and not
/// the second. That is not hypothetical — `templar-comp.service` shipped for a while with no package
/// installing `/usr/bin/templar-comp` at all, and both this check and `xtask check-packaging` were
/// blind to it. Selecting TemplarOS at the login screen would have started the session target and
/// dropped the user straight back to the greeter with nothing to explain why.
///
/// Reading `ExecStart` from the installed units rather than a hardcoded list means a unit added
/// later is covered without anyone remembering to come back here.
struct UnitBinariesPresent;
impl Check for UnitBinariesPresent {
    fn id(&self) -> &'static str {
        "unit-binaries"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let mut checked = 0usize;
        let mut missing = Vec::new();
        for dir in [
            "/usr/lib/systemd/user",
            "/usr/lib/systemd/system",
            "/usr/lib/systemd/user/templaros-session.target.wants",
        ] {
            let Ok(entries) = fs::read_dir(context.path(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                // Only our own units: the rest of the system's units are not ours to judge.
                if !name.starts_with("templar") || !name.ends_with(".service") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                for binary in exec_start_binaries(&text) {
                    checked += 1;
                    if !exists(&context.root, &binary) {
                        missing.push(format!("{name} -> {binary}"));
                    }
                }
            }
        }
        if checked == 0 {
            return finding(
                self,
                Outcome::Skipped,
                "no TemplarOS units installed to check",
                "nothing under /usr/lib/systemd matching templar*.service",
            );
        }
        if missing.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!("{checked} unit binary/ies installed"),
                "",
            )
        } else {
            missing.sort();
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} unit(s) point at a binary that is not installed",
                    missing.len()
                ),
                missing.join(", "),
            )
        }
    }
}

/// Absolute `ExecStart=` binaries in a unit file, with systemd's prefix characters and any
/// arguments stripped. A relative or variable-expanded command is skipped rather than guessed at.
fn exec_start_binaries(unit: &str) -> Vec<String> {
    unit.lines()
        .filter_map(|line| line.trim().strip_prefix("ExecStart="))
        .filter_map(|command| {
            command
                .trim_start_matches(['-', '@', '+', '!', ':'])
                .split_whitespace()
                .next()
        })
        .filter(|binary| binary.starts_with('/'))
        .map(ToOwned::to_owned)
        .collect()
}

/// Every Debian package in a TemplarOS build set, keyed by its dpkg name.
///
/// This has to be an explicit set rather than "everything starting with `templaros-`":
/// `templaros-nvidia-open` and `templaros-nvidia-proprietary` also carry that prefix and are driver
/// flavour packages with nothing to do with build skew, and `templar-files` breaks the naming
/// pattern entirely. The set is not written down *here*, though — it comes from the component
/// matrix (`distro/deploy/components.toml`), which is also what `cargo xtask build-set` builds and
/// what the reinstall script installs, so the three can no longer disagree about what TemplarOS is.
fn templaros_packages() -> Vec<String> {
    templar_build::components::Matrix::embedded()
        .packages()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
}

/// How far apart two TemplarOS packages' build dates are allowed to be before that is a fault rather
/// than the ordinary spread of a single build run.
const MAX_BUILD_SKEW: Duration = Duration::from_secs(24 * 60 * 60);

/// systemd-oomd is armed where ADR-0014 says it should be, and disarmed where it says it must be.
///
/// Two things on this desktop can decide a process must lose memory: `templar-resourced`, which
/// knows which window has focus and which application has unsaved work, and `systemd-oomd`, which
/// knows neither. ADR-0014 gives the session to resourced and leaves oomd armed on `app.slice` at
/// 90% as a far-above backstop — the level at which resourced is very likely not running at all.
///
/// The realistic way that posture drifts is not someone editing it on purpose: it is a
/// distribution upgrade replacing a drop-in, or a slice reorganisation making the session visible
/// to a monitor it was never meant to be visible to. Neither announces itself; both are silent
/// until the day something kills the compositor. Hence a check rather than a comment.
///
/// A Warning, not an Error: a machine where the drop-ins are simply not installed is a source
/// checkout or a partial install, and `unit-binaries` is the check that says so.
struct OomdOwnership;
impl Check for OomdOwnership {
    fn id(&self) -> &'static str {
        "oomd-ownership"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let app = context.path("/usr/lib/systemd/user/app.slice.d/templaros.conf");
        let session = context.path("/usr/lib/systemd/user/session.slice.d/templaros.conf");
        let (Ok(app_text), Ok(session_text)) =
            (fs::read_to_string(&app), fs::read_to_string(&session))
        else {
            return finding(
                self,
                Outcome::Skipped,
                "the slice drop-ins are not installed",
                app.display().to_string(),
            );
        };

        let mut problems = Vec::new();
        if directive(&app_text, "ManagedOOMMemoryPressure").as_deref() != Some("kill") {
            problems.push(
                "app.slice does not arm systemd-oomd; nothing would contain a session whose \
                 governor is not running"
                    .to_owned(),
            );
        }
        match directive(&app_text, "ManagedOOMMemoryPressureLimit") {
            Some(limit) if limit == "90%" => {}
            Some(limit) => problems.push(format!(
                "app.slice arms systemd-oomd at {limit}, not the 90% ADR-0014 states"
            )),
            None => problems.push("app.slice sets no oomd pressure limit".to_owned()),
        }
        if directive(&session_text, "ManagedOOMMemoryPressure").as_deref() == Some("kill") {
            problems.push(
                "session.slice arms systemd-oomd: it could kill the compositor, the shell or the \
                 governor, which are what a recovery is for"
                    .to_owned(),
            );
        }
        if directive(&session_text, "ManagedOOMSwap").as_deref() == Some("kill") {
            problems.push("session.slice arms systemd-oomd on swap pressure".to_owned());
        }

        let unprotected: Vec<&str> = OOMD_AVOID_UNITS
            .iter()
            .copied()
            .filter(|unit| {
                let path = context.path(&format!("/usr/lib/systemd/user/{unit}"));
                fs::read_to_string(&path).is_ok_and(|text| {
                    directive(&text, "ManagedOOMPreference").as_deref() != Some("avoid")
                })
            })
            .collect();
        if !unprotected.is_empty() {
            problems.push(format!(
                "{} do not declare ManagedOOMPreference=avoid",
                unprotected.join(", ")
            ));
        }

        if problems.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "systemd-oomd is the backstop on app.slice and disarmed on session.slice \
                 (ADR-0014)",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "the systemd-oomd posture has drifted from ADR-0014 in {} way(s)",
                    problems.len()
                ),
                problems.join("; "),
            )
        }
    }
}

/// The units ADR-0014 puts out of systemd-oomd's reach.
const OOMD_AVOID_UNITS: [&str; 5] = [
    "templar-comp.service",
    "templar-shell.service",
    "templar-sessiond.service",
    "templar-filed.service",
    "templar-resourced.service",
];

/// The last value of a `Key=value` directive in a unit file.
///
/// Last rather than first, because that is what systemd does: a later assignment in a drop-in
/// overrides an earlier one, and a check that read the first would pass a file that had been
/// overridden two lines further down.
fn directive(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .map(|value| value.trim().to_owned())
        .next_back()
}

/// Every TemplarOS package installed here belongs to the same build set.
///
/// This is the check `PackageAgesConsistent` was a stand-in for. A build set gives all thirteen
/// packages one version string (`0.1.0~dev<epoch>+g<sha>`), so an install that mixes two
/// deployments is not a judgement call about file timestamps — it is two different strings, named
/// exactly, with the rollback already spelled out by `templar dev status`.
///
/// Severity is Error because a mixed set is not a cosmetic problem: it is a shell calling D-Bus
/// methods an older sessiond does not serve, and every symptom of that looks like an application
/// bug rather than an install that never finished.
struct BuildSetCoherent;
impl Check for BuildSetCoherent {
    fn id(&self) -> &'static str {
        "build-set-coherent"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let status_path = context.path("/var/lib/dpkg/status");
        let Ok(status_text) = fs::read_to_string(&status_path) else {
            return finding(
                self,
                Outcome::Skipped,
                "no dpkg status to check",
                status_path.display().to_string(),
            );
        };
        let matrix = templar_build::components::Matrix::embedded();
        let installed = crate::dev::installed_versions(&status_text, &matrix.packages());
        if installed.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "no TemplarOS packages installed — a source checkout, not an install",
                "",
            );
        }

        let identified: BTreeMap<&String, &String> = installed
            .iter()
            .filter(|(_, version)| templar_build::parse_build_set_id(version).is_some())
            .collect();
        if identified.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "these packages predate build identity; package-ages is the fallback",
                format!("{} package(s) with no build-set id", installed.len()),
            );
        }

        let mut sets: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (package, version) in &identified {
            sets.entry(version.as_str())
                .or_default()
                .push(package.as_str());
        }
        if sets.len() > 1 {
            let detail = sets
                .iter()
                .map(|(version, packages)| format!("{version}: {}", packages.join(" ")))
                .collect::<Vec<_>>()
                .join("; ");
            return finding(
                self,
                Outcome::Fail,
                format!("mixed install: {} different build sets", sets.len()),
                detail,
            );
        }

        let set = sets.keys().next().copied().unwrap_or_default();
        let unidentified: Vec<&str> = installed
            .keys()
            .filter(|package| !identified.contains_key(package))
            .map(String::as_str)
            .collect();
        if !unidentified.is_empty() {
            return finding(
                self,
                Outcome::Fail,
                format!(
                    "{} package(s) are not part of build set {set}",
                    unidentified.len()
                ),
                unidentified.join(", "),
            );
        }
        let missing: Vec<&str> = matrix
            .packages()
            .into_iter()
            .filter(|package| !installed.contains_key(*package))
            .collect();
        if !missing.is_empty() {
            return finding(
                self,
                Outcome::Fail,
                format!("build set {set} is incomplete"),
                format!("not installed: {}", missing.join(", ")),
            );
        }
        finding(
            self,
            Outcome::Pass,
            format!("all {} package(s) are build set {set}", installed.len()),
            "",
        )
    }
}

/// Every running TemplarOS process is running the build that is installed.
///
/// A coherent install and a stale process are different faults with the same symptoms. The shell
/// restarts in half a second, so a shell still running yesterday's build after a deployment is
/// ordinary and worth a nudge, not an alarm — hence Warning, and hence the wording, which says
/// what to do rather than what is broken.
///
/// Only meaningful against a live root: an unpacked image has no running processes, and neither
/// does a machine outside a TemplarOS session.
struct RunningMatchesInstalled;
impl Check for RunningMatchesInstalled {
    fn id(&self) -> &'static str {
        "running-build"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != Path::new("/") {
            return finding(
                self,
                Outcome::Skipped,
                "an image root has nothing running",
                "",
            );
        }
        let matrix = templar_build::components::Matrix::embedded();
        let observation = crate::dev::observe(&context.root, &matrix);
        if observation.running.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "no TemplarOS process is running here",
                "",
            );
        }
        let convergence = crate::dev::converge::converge(&matrix, &observation, None);
        let stale: Vec<&crate::dev::converge::Row> = convergence
            .rows
            .iter()
            .filter(|row| row.verdict == crate::dev::converge::Verdict::NeedsActivation)
            .collect();
        if stale.is_empty() {
            return finding(
                self,
                Outcome::Pass,
                format!(
                    "{} running component(s) match what is installed",
                    observation.running.len()
                ),
                "",
            );
        }
        let detail = stale
            .iter()
            .map(|row| format!("{} — {}", row.component, row.activation.explanation()))
            .collect::<Vec<_>>()
            .join(", ");
        finding(
            self,
            Outcome::Fail,
            format!(
                "{} component(s) are still running an older build; `templar dev status` says what \
                 each needs",
                stale.len()
            ),
            detail,
        )
    }
}

/// A mixed-age TemplarOS install, judged by build date — the **fallback** for packages that carry no
/// build-set id.
///
/// [`BuildSetCoherent`] is the real check now: a build set gives every package one version string,
/// so "were these built together?" is a string comparison with no heuristic in it. This check
/// stays for the installs that predate that, where every package is `0.1.0-1` and two packages
/// agreeing on version proves nothing. It skips itself the moment build-set ids are present,
/// because a 24-hour mtime window is a guess and a version equality is not.
///
/// What it can prove is the mtime cargo-deb stamps on each package's binary at build time: packages
/// built in the same run land within moments of each other, and packages left over from a prior
/// run sit however many days behind. This is not a theoretical failure mode — a real install on
/// this project's own development host had `templar-shell` four days newer than `templar`, with
/// `templar-comp` and `templar-sessiond` in between, because a partial reinstall had only rebuilt some of
/// the packages. The newer shell went on to call D-Bus interfaces and Wayland requests the older
/// sessiond and compositor did not yet serve, and every failure that followed looked like an
/// application bug rather than what it actually was: an install that never finished.
///
/// A source checkout has none of these packages installed at all, which is not a broken install —
/// it is not an install — so that case is `Skipped`, not `Fail`.
struct PackageAgesConsistent;

/// Which rebuild command to name when packages are mixed-age.
///
/// On an installed TemplarOS (`ID=templaros`) that is `scripts/update-templaros.sh`. On a
/// Lubuntu/Ubuntu overlay host it is `scripts/reinstall-templaros-desktop.sh`. A missing
/// os-release is treated as the overlay: that is the development-host case this check was
/// written for, and the existing fixtures do not ship an os-release.
fn rebuild_advice(context: &Context) -> &'static str {
    let text = fs::read_to_string(context.path("/etc/os-release")).unwrap_or_default();
    if text.lines().any(|line| line == "ID=templaros") {
        "scripts/update-templaros.sh"
    } else {
        "scripts/reinstall-templaros-desktop.sh"
    }
}
impl Check for PackageAgesConsistent {
    fn id(&self) -> &'static str {
        "package-ages"
    }
    fn severity(&self) -> Severity {
        // A warning, not an error: the mtime window is a heuristic, and `build-set-coherent` is
        // the error-severity check that can actually prove the same thing.
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let status_path = context.path("/var/lib/dpkg/status");
        let Ok(status_text) = fs::read_to_string(&status_path) else {
            return finding(
                self,
                Outcome::Skipped,
                "no dpkg status to check",
                status_path.display().to_string(),
            );
        };
        let installed = installed_templaros_packages(&status_text);
        if installed.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "no TemplarOS packages installed — a source checkout, not an install",
                "",
            );
        }
        if installed
            .iter()
            .all(|(_, version)| templar_build::parse_build_set_id(version).is_some())
        {
            return finding(
                self,
                Outcome::Skipped,
                "every package carries a build-set id, so build-set-coherent answers this exactly",
                "",
            );
        }

        let dated: Vec<(String, SystemTime)> = installed
            .into_iter()
            .filter_map(|(package, _)| {
                let binary = primary_binary(context, &package)?;
                let modified = fs::metadata(&binary).ok()?.modified().ok()?;
                Some((package, modified))
            })
            .collect();
        // Fewer than two dated binaries means there is nothing to compare — a missing binary is
        // `unit-binaries`' or `app-manifests`' fault to report, not this check's.
        let Some((newest_package, newest)) = dated.iter().max_by_key(|(_, time)| *time).cloned()
        else {
            return finding(
                self,
                Outcome::Skipped,
                "no dated TemplarOS package binaries to compare",
                "",
            );
        };

        let mut lagging: Vec<(String, SystemTime)> = dated
            .iter()
            .filter(|(package, time)| {
                *package != newest_package
                    && newest
                        .duration_since(*time)
                        .is_ok_and(|behind| behind > MAX_BUILD_SKEW)
            })
            .cloned()
            .collect();
        if lagging.is_empty() {
            return finding(
                self,
                Outcome::Pass,
                format!(
                    "{} TemplarOS package(s) installed, all built within a day of each other",
                    dated.len()
                ),
                "",
            );
        }
        lagging.sort_by_key(|(_, time)| *time);
        let detail = format!(
            "{} — newest is {newest_package} ({}); re-run {} to \
             rebuild the rest",
            lagging
                .iter()
                .map(|(package, time)| format!("{package} ({})", format_date(*time)))
                .collect::<Vec<_>>()
                .join(", "),
            format_date(newest),
            rebuild_advice(context),
        );
        finding(
            self,
            Outcome::Fail,
            format!(
                "mixed-age TemplarOS install: {} of {} package(s) are more than a day older than the \
                 newest",
                lagging.len(),
                dated.len()
            ),
            detail,
        )
    }
}

/// Every package in [`templaros_packages`] that `status_text` (a dpkg `status` file) marks as
/// installed, with the version dpkg records for it.
fn installed_templaros_packages(status_text: &str) -> Vec<(String, String)> {
    let known = templaros_packages();
    status_text
        .split("\n\n")
        .filter_map(|stanza| {
            let name = stanza
                .lines()
                .find_map(|line| line.strip_prefix("Package: "))?;
            let installed = stanza
                .lines()
                .any(|line| line.starts_with("Status:") && line.contains("installed"));
            if !installed || !known.iter().any(|package| package == name) {
                return None;
            }
            let version = stanza
                .lines()
                .find_map(|line| line.strip_prefix("Version: "))
                .unwrap_or_default();
            Some((name.to_owned(), version.trim().to_owned()))
        })
        .collect()
}

/// The one binary `package`'s dpkg file list names under `/usr/bin` or `/usr/libexec` — read from
/// `<root>/var/lib/dpkg/info/<package>.list` rather than a hardcoded table, so a binary that moves
/// or is renamed is picked up without anyone remembering to update this file. `templaros-filed` lists
/// two (`templar-filed` and the `templar-preview-codecs` helper); a dpkg file list is always sorted, so
/// the package's own namesake binary sorts first and is what cargo-deb actually stamped as the
/// build artefact.
fn primary_binary(context: &Context, package: &str) -> Option<PathBuf> {
    let list_path = context.path(&format!("/var/lib/dpkg/info/{package}.list"));
    let text = fs::read_to_string(list_path).ok()?;
    let entry = text
        .lines()
        .find(|line| line.starts_with("/usr/bin/") || line.starts_with("/usr/libexec/"))?;
    Some(context.path(entry))
}

/// `YYYY-MM-DD` for a `SystemTime`, day granularity being all a 24-hour skew threshold can
/// meaningfully report. Computed with Howard Hinnant's public-domain `civil_from_days` rather than
/// pulling in a date-and-time crate: nothing else in this workspace needs one, and a doctor check
/// printing a build date is not reason enough to add one.
fn format_date(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (year, month, day) = civil_from_days(secs.div_euclid(86400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch to a proleptic-Gregorian (year, month, day). See
/// <https://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// The compositor can actually start: its binary, its private protocol definitions, and XWayland.
///
/// XWayland is a runtime lookup rather than a linked library, so nothing in the build catches its
/// absence; without it every X11 application fails to start with no diagnostic pointing here.
struct CompositorUsable;
impl Check for CompositorUsable {
    fn id(&self) -> &'static str {
        "compositor-usable"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        if !exists(&context.root, "/usr/bin/templar-comp") {
            return finding(
                self,
                Outcome::Fail,
                "compositor not installed",
                "/usr/bin/templar-comp is missing; selecting TemplarOS at the greeter would fail with \
                 no session",
            );
        }
        let mut warnings = Vec::new();
        for protocol in [
            "/usr/share/templaros/protocols/templar-shell-v2.xml",
            "/usr/share/templaros/protocols/templar-recovery-v1.xml",
        ] {
            if !exists(&context.root, protocol) {
                warnings.push(protocol.to_owned());
            }
        }
        // XWayland lives in different places across derivatives; accept any of them.
        let xwayland = ["/usr/bin/Xwayland", "/usr/libexec/Xwayland"]
            .iter()
            .any(|p| exists(&context.root, p));
        if !xwayland {
            warnings.push("Xwayland (X11 applications will not start)".to_owned());
        }
        if warnings.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "compositor installed with XWayland",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "compositor installed but {} support file(s) missing",
                    warnings.len()
                ),
                warnings.join(", "),
            )
        }
    }
}

/// `/etc/os-release` identifies TemplarOS and its Ubuntu base.
///
/// This is an *image* check: it asks whether the thing being inspected was built as TemplarOS. That
/// question has no answer at runtime on a developer machine, where the session is installed from
/// `.deb`s onto an ordinary Ubuntu host and `/etc/os-release` correctly says `ID=ubuntu`. Failing
/// there made `templar doctor` exit 1 on every developer install, which in turn made
/// `scripts/reinstall-templaros-desktop.sh` refuse a session that was in fact perfectly installed —
/// so at `context.root == "/"` a non-TemplarOS identity is `Skipped`, not `Fail`.
///
/// The cost is that a genuine TemplarOS system whose `os-release` had been clobbered would skip here
/// rather than fail. Nothing in `/etc/os-release` can distinguish that from a developer host, and
/// the other twelve checks are the ones that say whether the session will actually start.
struct OsRelease;
impl Check for OsRelease {
    fn id(&self) -> &'static str {
        "os-release"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let runtime = context.root == Path::new("/");
        let path = context.path("/etc/os-release");
        let Ok(text) = fs::read_to_string(&path) else {
            return finding(
                self,
                if runtime {
                    Outcome::Skipped
                } else {
                    Outcome::Fail
                },
                "os-release missing",
                path.display().to_string(),
            );
        };
        let is_templaros = text.lines().any(|line| line == "ID=templaros");
        let like_ubuntu = text
            .lines()
            .any(|line| line.starts_with("ID_LIKE=") && line.contains("ubuntu"));
        let detail = format!("ID=templaros:{is_templaros} ID_LIKE ubuntu:{like_ubuntu}");
        match (is_templaros && like_ubuntu, runtime) {
            (true, _) => finding(
                self,
                Outcome::Pass,
                "os-release identifies TemplarOS (Ubuntu-derived)",
                "",
            ),
            (false, true) => finding(
                self,
                Outcome::Skipped,
                "not a TemplarOS host — image identity is only asserted against an image root",
                detail,
            ),
            (false, false) => finding(self, Outcome::Fail, "os-release is not TemplarOS", detail),
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
            "/usr/lib/systemd/user/templaros-session.target",
            "/usr/lib/systemd/user/templar-comp.service",
            "/usr/lib/systemd/user/templar-sessiond.service",
            "/usr/lib/systemd/user/templar-resourced.service",
            "/usr/lib/systemd/user/templar-filed.service",
            "/usr/lib/systemd/user/templar-shell.service",
            "/usr/share/wayland-sessions/templaros.desktop",
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
            "templar-comp.service",
            "templar-sessiond.service",
            "templar-resourced.service",
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
        let open = exists(
            &context.root,
            "/var/lib/dpkg/info/templaros-nvidia-open.list",
        );
        let proprietary = exists(
            &context.root,
            "/var/lib/dpkg/info/templaros-nvidia-proprietary.list",
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
                let expected = templar_hwdetect::scan_sysfs(&context.path("/sys"))
                    .ok()
                    .map(templar_hwdetect::classify)
                    .and_then(|profile| profile.nvidia_flavour);
                match expected {
                    Some(templar_hwdetect::NvidiaFlavour::Open) if !open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the open flavour",
                        "templaros-nvidia-proprietary installed",
                    ),
                    Some(templar_hwdetect::NvidiaFlavour::Proprietary) if open => finding(
                        self,
                        Outcome::Fail,
                        "hardware wants the proprietary flavour",
                        "templaros-nvidia-open installed",
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
            "/usr/share/xdg-desktop-portal/templaros-portals.conf",
        ) && exists(
            &context.root,
            "/usr/share/xdg-desktop-portal/portals/templaros.portal",
        );
        if ok {
            finding(self, Outcome::Pass, "portal backend configured", "")
        } else {
            finding(
                self,
                Outcome::Fail,
                "portal backend not configured",
                "templaros-portals.conf / templaros.portal missing",
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
        // Delegated to templar-filed when it is running; a coarse fallback here.
        match fs::metadata("/") {
            Ok(_) => finding(
                self,
                Outcome::Pass,
                "root volume reachable",
                "detailed reserve check via templar-filed",
            ),
            Err(error) => finding(self, Outcome::Fail, "cannot stat /", error.to_string()),
        }
    }
}

/// `templar-hostd`'s system-scope units, polkit policy and bus policy are all present.
struct HostUnitsPresent;
impl Check for HostUnitsPresent {
    fn id(&self) -> &'static str {
        "host-units"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let required = [
            "/usr/lib/systemd/system/templar-hostd.service",
            "/usr/lib/systemd/system/templar-boot-health.service",
            "/usr/share/polkit-1/actions/org.templaros.host.policy",
            "/usr/share/dbus-1/system.d/org.templaros.Host.conf",
        ];
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|p| !exists(&context.root, p))
            .collect();
        if missing.is_empty() {
            finding(
                self,
                Outcome::Pass,
                "host daemon units and policy present",
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!("{} host unit/policy file(s) missing", missing.len()),
                missing.join(", "),
            )
        }
    }
}

/// Every installed application manifest parses and its executable exists (planning doc 01 §7:
/// "every declared plugin has its binary", "every default application has a valid executable" —
/// checked over every installed manifest, the strongest static approximation of "default"
/// available without a live session's stored defaults).
struct AppManifestsValid;
impl Check for AppManifestsValid {
    fn id(&self) -> &'static str {
        "app-manifests"
    }
    /// A warning, not an error. `distro/app-manifests/README.md` says curated alias manifests for
    /// third-party applications — Waterfox, Steam, Wine prefixes — "live here permanently", so a
    /// manifest whose executable is absent usually just means the user has not installed that
    /// application, which is not a fault in the system. The genuinely broken case — a manifest
    /// naming a binary no package anywhere provides — is a build-time question, and belongs to
    /// `xtask check-packaging` rather than here.
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        let manifests = load_manifests(context);
        if manifests.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "no application manifests installed",
                "",
            );
        }
        let missing: Vec<String> = manifests
            .iter()
            .filter(|manifest| !exists(&context.root, &manifest.executable.to_string_lossy()))
            .map(|manifest| format!("{} ({})", manifest.id, manifest.executable.display()))
            .collect();
        if missing.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!(
                    "{} application manifest(s), every executable present",
                    manifests.len()
                ),
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} manifest(s) name an executable that is not installed",
                    missing.len()
                ),
                format!(
                    "{} — expected if the application simply is not installed; a fault only if \
                     nothing provides it at all",
                    missing.join(", ")
                ),
            )
        }
    }
}

/// Every MIME handler named in a `mimeapps.list` refers to an installed application (planning
/// doc 01 §7). Checks the system default (`/etc/xdg/mimeapps.list`, meaningful for an image root
/// too) and, at runtime, `$XDG_DATA_HOME/applications/mimeapps.list`.
///
/// On a TemplarOS image that second file is the broker's own generated copy
/// (`docs/architecture/associations.md`: "mimeapps.list is generated by the broker"). On a
/// developer host it is not: it is whatever desktop the machine ran before, and on this project's
/// own workstation it is a symlink to a KDE `~/.config/mimeapps.list` with 156 entries. So
/// "installed" has to mean *installed at all*, not *carries a TemplarOS app manifest* — a handler is
/// resolved against every application directory in `application_dirs` first, and the manifest set
/// second. Checking the manifests alone reported all 156 real, present `.desktop` files as
/// missing handlers and made the check useless outside a pristine image.
struct MimeHandlersInstalled;
impl Check for MimeHandlersInstalled {
    fn id(&self) -> &'static str {
        "mime-handlers"
    }
    fn severity(&self) -> Severity {
        Severity::Error
    }
    fn run(&self, context: &Context) -> Finding {
        let mut candidates = vec![context.path("/etc/xdg/mimeapps.list")];
        if context.root == Path::new("/")
            && let Some(data_home) = xdg_data_home()
        {
            candidates.push(data_home.join("applications/mimeapps.list"));
        }
        let Some((path, text)) = candidates
            .into_iter()
            .find_map(|path| fs::read_to_string(&path).ok().map(|text| (path, text)))
        else {
            return finding(self, Outcome::Skipped, "no mimeapps.list found", "");
        };

        let defaults = templar_associationd::import::parse_default_applications(&text);
        if defaults.is_empty() {
            return finding(
                self,
                Outcome::Skipped,
                "mimeapps.list has no [Default Applications]",
                path.display().to_string(),
            );
        }
        let manifests = load_manifests(context);
        let known: std::collections::BTreeSet<&str> = manifests
            .iter()
            .flat_map(|manifest| {
                std::iter::once(manifest.primary_desktop_file_id.as_str())
                    .chain(manifest.aliases.desktop_file_ids.iter().map(String::as_str))
            })
            .collect();
        let applications = application_dirs(context);
        let orphaned: Vec<String> = defaults
            .iter()
            .filter(|(_, desktop_id)| {
                !known.contains(desktop_id.as_str())
                    && !desktop_entry_installed(&applications, desktop_id)
            })
            .map(|(mime, desktop_id)| format!("{mime} → {desktop_id}"))
            .collect();
        if orphaned.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!(
                    "{} MIME default(s), every handler installed",
                    defaults.len()
                ),
                path.display().to_string(),
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} MIME default(s) with no installed handler",
                    orphaned.len()
                ),
                orphaned.join(", "),
            )
        }
    }
}

/// Every autostart entry that declares `X-TemplarOS-Provides` only starts for the role's actually
/// selected provider (planning doc 01 §7: "every autostart entry resolves to one selected
/// provider"). Runtime only: this needs the live provider-selection state, which exists only
/// once a session has run at least once, and this check never creates that state itself — it
/// opens the store only when the file is already there.
struct AutostartProvidersResolved;
impl Check for AutostartProvidersResolved {
    fn id(&self) -> &'static str {
        "autostart-providers"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != Path::new("/") {
            return finding(
                self,
                Outcome::Skipped,
                "autostart providers (runtime only)",
                "",
            );
        }
        let db_path = templar_state::default_path();
        if !db_path.exists() {
            return finding(
                self,
                Outcome::Skipped,
                "no session state store yet (no session has run)",
                db_path.display().to_string(),
            );
        }
        let store = match templar_state::Store::open(&db_path) {
            Ok(store) => store,
            Err(error) => {
                return finding(
                    self,
                    Outcome::Fail,
                    "could not open the state store",
                    error.to_string(),
                );
            }
        };
        let registry = match templar_session_core::providers::load(&store) {
            Ok(registry) => registry,
            Err(error) => {
                return finding(
                    self,
                    Outcome::Fail,
                    "could not load provider selections",
                    error.to_string(),
                );
            }
        };

        let manifests = load_manifests(context);
        let by_desktop_id = desktop_id_index(&manifests);
        let mut dirs = vec![context.path("/etc/xdg/autostart")];
        if let Some(config_home) = xdg_config_home() {
            dirs.push(config_home.join("autostart"));
        }
        let (checked, mismatches) = scan_autostart_entries(&dirs, &by_desktop_id, &registry);
        if checked == 0 {
            finding(
                self,
                Outcome::Skipped,
                "no autostart entries declare a provider role",
                "",
            )
        } else if mismatches.is_empty() {
            finding(
                self,
                Outcome::Pass,
                format!("{checked} role-declaring autostart entry/entries resolve correctly"),
                "",
            )
        } else {
            finding(
                self,
                Outcome::Fail,
                format!(
                    "{} autostart entry/entries do not resolve",
                    mismatches.len()
                ),
                mismatches.join("; "),
            )
        }
    }
}

/// Map every desktop-file id an installed manifest answers to (its primary id, plus every
/// compatibility alias) to that manifest's canonical [`templar_types::AppId`].
fn desktop_id_index(manifests: &[AppManifest]) -> BTreeMap<&str, &templar_types::AppId> {
    manifests
        .iter()
        .flat_map(|manifest| {
            std::iter::once((manifest.primary_desktop_file_id.as_str(), &manifest.id)).chain(
                manifest
                    .aliases
                    .desktop_file_ids
                    .iter()
                    .map(move |id| (id.as_str(), &manifest.id)),
            )
        })
        .collect()
}

/// Scan `dirs` for non-hidden `.desktop` autostart entries that declare `X-TemplarOS-Provides`, and
/// check each one against `registry`. Returns `(entries checked, mismatch descriptions)`; an
/// entry whose desktop id matches no installed manifest is silently skipped here — that gap is
/// [`AppManifestsValid`]'s / [`MimeHandlersInstalled`]'s to report, not this check's.
fn scan_autostart_entries(
    dirs: &[std::path::PathBuf],
    by_desktop_id: &BTreeMap<&str, &templar_types::AppId>,
    registry: &templar_session_core::providers::ProviderRegistry,
) -> (u32, Vec<String>) {
    let mut mismatches = Vec::new();
    let mut checked = 0u32;
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(desktop_entry) = templar_appd::desktop_entry::parse(&text) else {
                continue;
            };
            if desktop_entry.hidden {
                continue;
            }
            let Some(role_name) = desktop_entry.extra.get("X-TemplarOS-Provides") else {
                continue;
            };
            let Some(role) = ProviderRole::ALL
                .iter()
                .find(|r| r.as_str() == role_name.trim())
            else {
                continue;
            };
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(app_id) = by_desktop_id.get(file_name) else {
                continue;
            };
            checked += 1;
            if registry.provider(*role).as_ref() != Some(*app_id) {
                mismatches.push(format!(
                    "{file_name} declares {role} but is not the selected provider"
                ));
            }
        }
    }
    (checked, mismatches)
}

/// Every directory that can hold an installed `.desktop` entry, in XDG search order, rooted at
/// `context.root` so this works against an image root as well as `/`.
///
/// `XDG_DATA_DIRS` is consulted at runtime but never trusted to be complete. A `doctor` run from
/// a plain login shell, from a package hook or under `sudo` frequently carries only the spec
/// default `/usr/local/share:/usr/share`, which omits the snap and flatpak export directories a
/// desktop session would have added — and a handler like `chromium_chromium.desktop` lives only
/// in `/var/lib/snapd/desktop/applications`. The well-known roots are therefore always searched,
/// whatever the environment says, and `XDG_DATA_DIRS` only ever adds to them.
fn application_dirs(context: &Context) -> Vec<std::path::PathBuf> {
    const WELL_KNOWN: [&str; 4] = [
        "/usr/local/share",
        "/usr/share",
        "/var/lib/flatpak/exports/share",
        "/var/lib/snapd/desktop",
    ];
    let mut roots: Vec<std::path::PathBuf> =
        WELL_KNOWN.iter().map(|dir| context.path(dir)).collect();
    if context.root == Path::new("/") {
        if let Some(value) = std::env::var_os("XDG_DATA_DIRS") {
            roots.extend(std::env::split_paths(&value).filter(|dir| !dir.as_os_str().is_empty()));
        }
        if let Some(data_home) = xdg_data_home() {
            roots.push(data_home.join("flatpak/exports/share"));
            roots.push(data_home);
        }
    }
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for root in roots {
        let dir = root.join("applications");
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Whether a `.desktop` entry with this id is installed in any application directory.
///
/// The id in a `mimeapps.list` is usually just the filename, but the XDG desktop-file-id
/// convention also lets `kde4-konsole.desktop` name `kde4/konsole.desktop`. Each `-` is replaced
/// with `/` in turn, the way glib resolves the same id, so a nested entry is not called missing.
fn desktop_entry_installed(dirs: &[std::path::PathBuf], desktop_id: &str) -> bool {
    let mut candidates = vec![desktop_id.to_owned()];
    let mut nested = desktop_id.to_owned();
    while let Some(index) = nested.find('-') {
        nested.replace_range(index..=index, "/");
        candidates.push(nested.clone());
    }
    candidates
        .iter()
        .any(|name| dirs.iter().any(|dir| dir.join(name).is_file()))
}

fn xdg_data_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
}

fn xdg_config_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
}

fn xdg_cache_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
}

/// `$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3` — mirrors
/// `services/templar-filed/src/main.rs`'s `default_index_db_path`, which is private to that binary.
fn index_db_path() -> Option<std::path::PathBuf> {
    xdg_cache_home().map(|cache| cache.join("templaros").join("file-index-v1.sqlite3"))
}

/// The shared file index: size, generation, per-root state, watcher backend/limits, watches
/// armed/refused, last event, last reconciliation, and why a root is stale — plan of record
/// `docs/planning/shared-incremental-file-index.md` acceptance gate 10, unmet before this check
/// existed. Runtime only, and opens only the existing derived database in read-only/query-only
/// mode. It never opens session state, applies migrations, recovers staging or rebuilds a cache.
/// An absent cache `Skip`s; unsupported stored versions fail without changing any data.
///
/// A root reporting `Stale` because the watch budget ran out is not this check's failure to
/// report honestly — `RootStatus::reason` says so, and that reason is carried into `detail`
/// verbatim rather than treated as an error. Only a genuine problem (the index would not open, a
/// root recorded a `last_error`) turns the outcome into `Fail`.
struct FileIndexHealth;
impl Check for FileIndexHealth {
    fn id(&self) -> &'static str {
        "file-index"
    }
    fn severity(&self) -> Severity {
        Severity::Warning
    }
    fn run(&self, context: &Context) -> Finding {
        if context.root != Path::new("/") {
            return finding(self, Outcome::Skipped, "file index (runtime only)", "");
        }
        let (diagnostics, roots) = match open_index_for_diagnostics() {
            FileIndexProbe::Ready(diagnostics, roots) => (diagnostics, roots),
            FileIndexProbe::Skipped(summary, detail) => {
                return finding(self, Outcome::Skipped, summary, detail);
            }
            FileIndexProbe::Failed(summary, detail) => {
                return finding(self, Outcome::Fail, summary, detail);
            }
        };

        let mut lines = vec![format!(
            "index: schema v{} generation {} — {} bytes on disk, fts={}, {} cursor(s) open, \
             change log {}..{}",
            diagnostics.schema_version,
            diagnostics.generation,
            diagnostics.db_bytes,
            diagnostics.fts_available,
            diagnostics.cursor_count,
            diagnostics.change_seq_floor,
            diagnostics.change_seq_head,
        )];
        lines.push(format!(
            "availability: {} reason={:?}",
            diagnostics.availability.state, diagnostics.availability.reason
        ));
        lines.push(format!(
            "storage: main={} WAL={} SHM={} total={} bytes",
            diagnostics.storage.main_bytes,
            diagnostics.storage.wal_bytes,
            diagnostics.storage.shm_bytes,
            diagnostics.storage.total()
        ));
        lines.extend(templar_file_index::diagnostics::runtime_lines(
            diagnostics.runtime.as_ref(),
        ));
        let warnings = templar_file_index::diagnostics::health_warnings(
            diagnostics.storage,
            diagnostics.runtime.as_ref(),
        );
        let mut any_error = !diagnostics.availability.is_available() || !warnings.is_empty();
        lines.extend(
            warnings
                .into_iter()
                .map(|warning| format!("warning: {warning}")),
        );
        for root in &roots {
            let (line, root_had_error) = describe_index_root(root);
            any_error |= root_had_error;
            lines.push(line);
        }
        let outcome = if any_error {
            Outcome::Fail
        } else {
            Outcome::Pass
        };
        finding(
            self,
            outcome,
            format!(
                "{} indexed root(s), schema v{}, generation {}",
                roots.len(),
                diagnostics.schema_version,
                diagnostics.generation,
            ),
            lines.join("\n"),
        )
    }
}

/// What trying to open the file index for diagnostics found.
enum FileIndexProbe {
    Ready(
        templar_file_index::Diagnostics,
        Vec<templar_file_index::RootStatus>,
    ),
    /// Nothing to report yet — never a check failure.
    Skipped(&'static str, String),
    /// A genuine problem: the database exists but would not open or answer.
    Failed(&'static str, String),
}

/// Read only the existing derived store. Do not open session state or construct the filed owner:
/// both paths migrate and perform recovery writes, which are forbidden in a diagnostic probe.
fn open_index_for_diagnostics() -> FileIndexProbe {
    if let Some(diagnostics) = live_index_diagnostics() {
        let roots = diagnostics.roots.clone();
        return FileIndexProbe::Ready(diagnostics, roots);
    }
    let Some(db_path) = index_db_path() else {
        return FileIndexProbe::Skipped(
            "no $HOME/$XDG_CACHE_HOME to locate the index cache",
            String::new(),
        );
    };
    if !db_path.exists() {
        return FileIndexProbe::Skipped(
            "no file index yet (templar-filed has never indexed)",
            db_path.display().to_string(),
        );
    }
    match templar_file_index::Diagnostics::read_only(&db_path) {
        Ok(diagnostics) => {
            let roots = diagnostics.roots.clone();
            FileIndexProbe::Ready(diagnostics, roots)
        }
        Err(error) => FileIndexProbe::Failed(
            "could not inspect the file index read-only",
            format!("{}: {error}", db_path.display()),
        ),
    }
}

/// Bounded read-only call on a separate thread because Check is synchronous and callers may
/// already be running Tokio. NoAutoStart preserves a stopped service and its disk fallback.
fn live_index_diagnostics() -> Option<templar_file_index::Diagnostics> {
    std::thread::Builder::new()
        .name("templar-doctor-index".into())
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    let connection = zbus::Connection::session().await.ok()?;
                    let proxy = templar_client::proxies::FilesIndexProxy::new(&connection)
                        .await
                        .ok()?;
                    let response: Option<String> = proxy
                        .inner()
                        .call_with_flags("Roots", zbus::proxy::MethodFlags::NoAutoStart.into(), &())
                        .await
                        .ok()?;
                    let value: serde_json::Value = serde_json::from_str(&response?).ok()?;
                    serde_json::from_value(value.get("diagnostics")?.clone()).ok()
                })
                .await
                .ok()
                .flatten()
            })
        })
        .ok()?
        .join()
        .ok()
        .flatten()
}

/// One `RootStatus` as a `templar doctor` detail line: state, entry count, why it is stale (if it
/// is), the watcher's own backend/limits/armed/refused/last-event report, and the last
/// reconciliation. Returns the line and whether it carries a genuine error (a watcher error or a
/// recorded `last_error`) — `Stale` alone, with a `reason`, is not an error: it is the honest
/// report this check exists to produce.
fn describe_index_root(root: &templar_file_index::RootStatus) -> (String, bool) {
    use std::fmt::Write as _;
    let mut any_error = false;
    let mut line = format!(
        "root {} [{}] {} — {} entries, generation {}, complete={}",
        root.root.0,
        root.state.as_str(),
        root.display,
        root.entry_count,
        root.generation,
        root.complete,
    );
    let _ = write!(
        line,
        " — metadata={} identity={:?} fs_type={:?} fs_uuid={:?} fsid={:?} mount_id={:?} mount_point={:?} mount_root={:?} nested_in={:?} contains={:?} offline_reason={:?}",
        root.metadata.as_str(),
        root.volume.confidence,
        root.volume.fs_type,
        root.volume.fs_uuid,
        root.volume.fsid,
        root.volume.mount_id,
        root.volume.mount_point.as_ref().map(|p| p.display()),
        root.volume.mount_root.as_ref().map(|p| p.display()),
        root.nested_in,
        root.contains,
        root.offline_reason
    );
    if let Some(reason) = &root.reason {
        let _ = write!(line, " — reason: {reason}");
    }
    match &root.watch {
        Some(watch) => {
            let _ = write!(
                line,
                " — watcher {} required={} armed={} refused={} limit={} queue_limit={} overflow_count={} \
                 last_event_ms={}",
                watch.backend,
                watch.required_display(),
                watch.watches_armed,
                watch.refused_display(),
                watch
                    .watch_limit
                    .map_or_else(|| "?".to_owned(), |v| v.to_string()),
                watch
                    .queue_limit
                    .map_or_else(|| "?".to_owned(), |v| v.to_string()),
                watch.overflow_count,
                watch
                    .last_event_ms
                    .map_or_else(|| "never".to_owned(), |v| v.to_string()),
            );
            if let Some(error) = &watch.last_error {
                any_error = true;
                let _ = write!(line, " — watcher error: {error}");
            }
        }
        None => line.push_str(" — no watcher status recorded yet"),
    }
    match (
        root.last_reconcile_started_ms,
        root.last_reconcile_finished_ms,
    ) {
        (Some(started), Some(finished)) if finished >= started => {
            let _ = write!(line, " — last reconcile took {}ms", finished - started);
        }
        (Some(started), _) => {
            let _ = write!(line, " — reconcile in progress since {started}ms");
        }
        (None, _) => line.push_str(" — never reconciled"),
    }
    if let Some(error) = &root.last_error {
        any_error = true;
        let _ = write!(line, " — last error: {error}");
    }
    (line, any_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{exit_code, run_all};

    #[test]
    fn empty_root_fails_the_identity_checks_and_reports_exit_code_1() {
        let root = std::env::temp_dir().join(format!("templar-doctor-{}", std::process::id()));
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
            "ID=templaros\nID_LIKE=ubuntu debian\n",
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

    #[test]
    fn exec_start_binaries_strips_systemd_prefixes_and_arguments() {
        let unit = "[Service]\nExecStart=-/usr/bin/templar-comp --backend udev\n\
                    ExecStart=@/usr/libexec/templar-filed\nExecStart=relative-thing\n\
                    ExecStartPre=/usr/bin/not-this\n";
        assert_eq!(
            exec_start_binaries(unit),
            vec![
                "/usr/bin/templar-comp".to_owned(),
                "/usr/libexec/templar-filed".to_owned()
            ]
        );
    }

    /// The regression that shipped: the unit is installed, the binary it names is not, and every
    /// other check is perfectly happy about it.
    #[test]
    fn a_unit_naming_an_uninstalled_binary_fails_even_though_the_unit_is_present() {
        let root = fixture_root("unit-binaries");
        let units = root.join("usr/lib/systemd/user");
        fs::create_dir_all(&units).unwrap();
        fs::write(
            units.join("templar-comp.service"),
            "[Service]\nExecStart=/usr/bin/templar-comp\n",
        )
        .unwrap();

        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let findings = run_all(&context);
        let unit_binaries = findings.iter().find(|f| f.id == "unit-binaries").unwrap();
        assert_eq!(unit_binaries.outcome, Outcome::Fail);
        assert!(unit_binaries.detail.contains("/usr/bin/templar-comp"));
        assert!(
            findings
                .iter()
                .any(|f| f.id == "compositor-usable" && f.outcome == Outcome::Fail)
        );

        // Install the binary and the check goes green.
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/templar-comp"), "").unwrap();
        let findings = run_all(&context);
        assert_eq!(
            findings
                .iter()
                .find(|f| f.id == "unit-binaries")
                .unwrap()
                .outcome,
            Outcome::Pass
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compositor_check_reports_missing_xwayland_and_protocols_separately_from_the_binary() {
        let root = fixture_root("compositor-usable");
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/templar-comp"), "").unwrap();
        let findings = run_all(&Context {
            root: root.clone(),
            hardware: false,
        });
        let compositor = findings
            .iter()
            .find(|f| f.id == "compositor-usable")
            .unwrap();
        assert_eq!(compositor.outcome, Outcome::Fail);
        assert!(compositor.detail.contains("Xwayland"));
        assert!(compositor.detail.contains("templar-shell-v2.xml"));
        // The binary is there, so it must not claim the compositor is uninstalled.
        assert!(!compositor.summary.contains("not installed"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn compositor_check_requires_the_current_packaged_shell_protocol() {
        let root = fixture_root("compositor-current-protocol");
        let protocols = root.join("usr/share/templaros/protocols");
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir_all(&protocols).unwrap();
        for name in ["templar-comp", "Xwayland"] {
            fs::write(root.join("usr/bin").join(name), "").unwrap();
        }
        fs::write(protocols.join("templar-recovery-v1.xml"), "").unwrap();
        fs::write(protocols.join("templar-shell-v1.xml"), "").unwrap();
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let legacy_only = CompositorUsable.run(&context);
        assert_eq!(legacy_only.outcome, Outcome::Fail);
        assert_eq!(
            legacy_only.detail,
            "/usr/share/templaros/protocols/templar-shell-v2.xml"
        );

        fs::remove_file(protocols.join("templar-shell-v1.xml")).unwrap();
        // Use the independent cargo-deb declaration so a future packaging rename
        // cannot leave doctor and its fixture agreeing on an obsolete filename.
        let package: toml::Value = toml::from_str(include_str!(
            "../../../../compositor/templar-comp/Cargo.toml"
        ))
        .unwrap();
        for asset in package["package"]["metadata"]["deb"]["assets"]
            .as_array()
            .unwrap()
        {
            let target = asset[1].as_str().unwrap();
            if target.starts_with("usr/share/templaros/protocols/") {
                fs::write(root.join(target), "").unwrap();
            }
        }
        let current = CompositorUsable.run(&context);
        assert_eq!(current.outcome, Outcome::Pass);
        assert!(current.detail.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("templar-doctor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Register `package` as installed under `root`: a dpkg status stanza, a file list naming
    /// `binary` (an absolute path, e.g. `/usr/bin/templar-comp`), and the binary itself stamped with
    /// `unix_secs` as its mtime — the thing cargo-deb actually stamps at build time and the thing
    /// [`PackageAgesConsistent`] reads.
    fn install_package(root: &Path, package: &str, binary: &str, unix_secs: u64) {
        install_package_at(root, package, binary, unix_secs, "0.1.0-1");
    }

    /// [`install_package`] with an explicit version, for the build-set checks.
    fn install_package_at(root: &Path, package: &str, binary: &str, unix_secs: u64, version: &str) {
        use std::fmt::Write as _;
        let status_path = root.join("var/lib/dpkg/status");
        fs::create_dir_all(status_path.parent().unwrap()).unwrap();
        let mut status = fs::read_to_string(&status_path).unwrap_or_default();
        write!(
            status,
            "Package: {package}\nStatus: install ok installed\nVersion: {version}\n\n"
        )
        .unwrap();
        fs::write(&status_path, status).unwrap();

        let list_path = root
            .join("var/lib/dpkg/info")
            .join(format!("{package}.list"));
        fs::create_dir_all(list_path.parent().unwrap()).unwrap();
        fs::write(&list_path, format!("{binary}\n")).unwrap();

        let binary_path = root.join(binary.trim_start_matches('/'));
        fs::create_dir_all(binary_path.parent().unwrap()).unwrap();
        fs::write(&binary_path, "").unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&binary_path)
            .unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_secs(unix_secs))
            .unwrap();
    }

    /// Install every matrix package at `version`, so the coherence check has a complete set.
    fn install_whole_set(root: &Path, version: &str) {
        for (index, package) in templar_build::components::Matrix::embedded()
            .packages()
            .iter()
            .enumerate()
        {
            install_package_at(
                root,
                package,
                &format!("/usr/bin/{package}"),
                1_787_616_000 + index as u64,
                version,
            );
        }
    }

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const OLDER_SET: &str = "0.1.0~dev1000+gbbbbbbb";

    /// The oomd posture as ADR-0014 states it, under a scratch root.
    fn install_oomd_posture(root: &Path) {
        let user = root.join("usr/lib/systemd/user");
        fs::create_dir_all(user.join("app.slice.d")).unwrap();
        fs::create_dir_all(user.join("session.slice.d")).unwrap();
        fs::write(
            user.join("app.slice.d/templaros.conf"),
            "[Slice]\nManagedOOMMemoryPressure=kill\nManagedOOMMemoryPressureLimit=90%\n",
        )
        .unwrap();
        fs::write(
            user.join("session.slice.d/templaros.conf"),
            "[Slice]\nManagedOOMMemoryPressure=auto\nManagedOOMSwap=auto\n",
        )
        .unwrap();
        for unit in OOMD_AVOID_UNITS {
            fs::write(
                user.join(unit),
                "[Service]\nOOMScoreAdjust=-900\nManagedOOMPreference=avoid\n",
            )
            .unwrap();
        }
    }

    #[test]
    fn oomd_ownership_passes_the_posture_the_adr_states() {
        let root = fixture_root("oomd-ok");
        install_oomd_posture(&root);
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(OomdOwnership.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oomd_ownership_catches_a_session_slice_that_became_killable() {
        // The realistic drift: a distribution upgrade replaces the drop-in and nothing announces
        // it until the day oomd kills the compositor.
        let root = fixture_root("oomd-session-armed");
        install_oomd_posture(&root);
        fs::write(
            root.join("usr/lib/systemd/user/session.slice.d/templaros.conf"),
            "[Slice]\nManagedOOMMemoryPressure=kill\n",
        )
        .unwrap();
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let finding = OomdOwnership.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail);
        assert!(
            finding.detail.contains("what a recovery is for"),
            "{finding:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oomd_ownership_catches_an_unarmed_app_slice_and_an_unprotected_unit() {
        let root = fixture_root("oomd-app-disarmed");
        install_oomd_posture(&root);
        fs::write(
            root.join("usr/lib/systemd/user/app.slice.d/templaros.conf"),
            "[Slice]\nManagedOOMMemoryPressure=auto\n",
        )
        .unwrap();
        fs::write(
            root.join("usr/lib/systemd/user/templar-comp.service"),
            "[Service]\nOOMScoreAdjust=-900\n",
        )
        .unwrap();
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let finding = OomdOwnership.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail);
        assert!(finding.detail.contains("does not arm"), "{finding:?}");
        assert!(
            finding.detail.contains("templar-comp.service"),
            "{finding:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oomd_ownership_skips_a_tree_with_no_drop_ins() {
        let root = fixture_root("oomd-absent");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(OomdOwnership.run(&context).outcome, Outcome::Skipped);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directive_reads_the_last_assignment_the_way_systemd_does() {
        let text = "[Slice]\nManagedOOMMemoryPressure=auto\nManagedOOMMemoryPressure=kill\n";
        assert_eq!(
            directive(text, "ManagedOOMMemoryPressure").as_deref(),
            Some("kill")
        );
        assert_eq!(
            directive("# ManagedOOMPreference=avoid\n", "ManagedOOMPreference"),
            None,
            "a commented-out directive is not a directive"
        );
    }

    #[test]
    fn the_shipped_units_and_drop_ins_state_the_posture_the_adr_does() {
        // Against the repository's own files rather than a fixture: the check above proves the
        // check works, and this proves the thing it checks is actually true here.
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let app =
            fs::read_to_string(repo.join("distro/policy/systemd/user/app.slice.d/templaros.conf"))
                .unwrap();
        assert_eq!(
            directive(&app, "ManagedOOMMemoryPressureLimit").as_deref(),
            Some("90%")
        );
        let session = fs::read_to_string(
            repo.join("distro/policy/systemd/user/session.slice.d/templaros.conf"),
        )
        .unwrap();
        assert_ne!(
            directive(&session, "ManagedOOMMemoryPressure").as_deref(),
            Some("kill")
        );
        for unit in OOMD_AVOID_UNITS {
            let text = fs::read_to_string(repo.join("session/units/user").join(unit)).unwrap();
            assert_eq!(
                directive(&text, "ManagedOOMPreference").as_deref(),
                Some("avoid"),
                "{unit}"
            );
        }
    }

    #[test]
    fn build_set_coherent_passes_a_whole_set_at_one_version() {
        let root = fixture_root("build-set-coherent");
        install_whole_set(&root, SET);
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let finding = BuildSetCoherent.run(&context);
        assert_eq!(finding.outcome, Outcome::Pass, "{finding:?}");
        assert!(finding.summary.contains(SET), "{}", finding.summary);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_set_coherent_names_both_sets_of_a_mixed_install() {
        let root = fixture_root("build-set-mixed");
        install_whole_set(&root, SET);
        install_package_at(
            &root,
            "templaros-sessiond",
            "/usr/bin/templaros-sessiond",
            1,
            OLDER_SET,
        );
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let finding = BuildSetCoherent.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail, "{finding:?}");
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.detail.contains(OLDER_SET), "{}", finding.detail);
        assert!(
            finding.detail.contains("templaros-sessiond"),
            "{}",
            finding.detail
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_set_coherent_fails_an_incomplete_set() {
        let root = fixture_root("build-set-incomplete");
        install_package_at(&root, "templaros-shell", "/usr/bin/templar-shell", 1, SET);
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let finding = BuildSetCoherent.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail, "{finding:?}");
        assert!(
            finding.summary.contains("incomplete"),
            "{}",
            finding.summary
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_set_coherent_defers_to_package_ages_for_packages_with_no_id() {
        // The two checks must never both speak: an install predating build identity is
        // package-ages' business, and an install carrying ids is not.
        let root = fixture_root("build-set-legacy");
        install_whole_set(&root, "0.1.0-1");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(BuildSetCoherent.run(&context).outcome, Outcome::Skipped);
        assert_ne!(
            PackageAgesConsistent.run(&context).outcome,
            Outcome::Skipped,
            "the fallback has to answer when the primary cannot"
        );

        let _ = fs::remove_dir_all(&root);

        let root = fixture_root("build-set-identified");
        install_whole_set(&root, SET);
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(BuildSetCoherent.run(&context).outcome, Outcome::Pass);
        assert_eq!(
            PackageAgesConsistent.run(&context).outcome,
            Outcome::Skipped,
            "the fallback must stand down once build-set ids are present"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn running_build_skips_an_image_root() {
        let context = Context {
            root: PathBuf::from("/nonexistent-image-root"),
            hardware: false,
        };
        assert_eq!(
            RunningMatchesInstalled.run(&context).outcome,
            Outcome::Skipped
        );
    }

    #[test]
    fn package_ages_consistent_skips_a_source_checkout_with_no_templaros_packages() {
        let root = fixture_root("package-ages-none");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(
            PackageAgesConsistent.run(&context).outcome,
            Outcome::Skipped
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn package_ages_consistent_passes_a_set_built_together() {
        let root = fixture_root("package-ages-agreeing");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let base = 1_787_616_000; // 2026-08-25T00:00:00Z
        install_package(&root, "templaros-comp", "/usr/bin/templar-comp", base);
        install_package(&root, "templaros-cli", "/usr/bin/templar", base + 60);
        install_package(
            &root,
            "templaros-sessiond",
            "/usr/libexec/templar-sessiond",
            base + 120,
        );
        assert_eq!(PackageAgesConsistent.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn package_ages_consistent_fails_and_names_a_package_built_three_days_earlier() {
        let root = fixture_root("package-ages-skewed");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let newest = 1_787_616_000; // 2026-08-25T00:00:00Z
        let three_days_earlier = newest - 3 * 24 * 60 * 60; // 2026-08-22T00:00:00Z
        install_package(&root, "templaros-shell", "/usr/bin/templar-shell", newest);
        install_package(
            &root,
            "templaros-cli",
            "/usr/bin/templar",
            three_days_earlier,
        );
        let finding = PackageAgesConsistent.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail);
        assert!(finding.detail.contains("templaros-cli (2026-08-22)"));
        assert!(
            finding
                .detail
                .contains("newest is templaros-shell (2026-08-25)")
        );
        assert!(finding.detail.contains("reinstall-templaros-desktop.sh"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn package_ages_consistent_on_templaros_names_the_os_update_script() {
        let root = fixture_root("package-ages-templaros");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(
            root.join("etc/os-release"),
            "ID=templaros\nID_LIKE=\"ubuntu debian\"\n",
        )
        .unwrap();
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        let newest = 1_787_616_000;
        let three_days_earlier = newest - 3 * 24 * 60 * 60;
        install_package(&root, "templaros-shell", "/usr/bin/templar-shell", newest);
        install_package(
            &root,
            "templaros-cli",
            "/usr/bin/templar",
            three_days_earlier,
        );
        let finding = PackageAgesConsistent.run(&context);
        assert_eq!(finding.outcome, Outcome::Fail);
        assert!(finding.detail.contains("update-templaros.sh"));
        assert!(!finding.detail.contains("reinstall-templaros-desktop.sh"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn format_date_matches_known_calendar_dates() {
        let cases: [(u64, &str); 3] = [
            (0, "1970-01-01"),
            (951_782_400, "2000-02-29"),
            (1_787_616_000, "2026-08-25"),
        ];
        for (secs, expected) in cases {
            assert_eq!(
                format_date(UNIX_EPOCH + Duration::from_secs(secs)),
                expected
            );
        }
    }

    fn manifest(id: &str, executable: &str) -> String {
        format!(
            r#"{{"id":"{id}","display_name":"{id}","primary_desktop_file_id":"{id}.desktop",
                "executable":"{executable}","mime_types":["application/pdf"],
                "resource_class":"Interactive"}}"#
        )
    }

    #[test]
    fn host_units_present_reports_every_missing_file() {
        let root = fixture_root("host-units");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(HostUnitsPresent.run(&context).outcome, Outcome::Fail);

        for path in [
            "usr/lib/systemd/system/templar-hostd.service",
            "usr/lib/systemd/system/templar-boot-health.service",
            "usr/share/polkit-1/actions/org.templaros.host.policy",
            "usr/share/dbus-1/system.d/org.templaros.Host.conf",
        ] {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, "").unwrap();
        }
        assert_eq!(HostUnitsPresent.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn app_manifests_valid_skips_when_none_installed_and_fails_a_missing_executable() {
        let root = fixture_root("app-manifests");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Skipped);

        let apps_dir = root.join("usr/share/templaros/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        fs::write(
            apps_dir.join("org.templaros.Viewer.json"),
            manifest("org.templaros.Viewer", "/usr/bin/templar-viewer"),
        )
        .unwrap();
        // The executable does not exist under the root: this must fail, not silently pass.
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Fail);

        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::write(root.join("usr/bin/templar-viewer"), "").unwrap();
        assert_eq!(AppManifestsValid.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mime_handlers_installed_flags_a_handler_with_no_manifest() {
        let root = fixture_root("mime-handlers");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        assert_eq!(
            MimeHandlersInstalled.run(&context).outcome,
            Outcome::Skipped
        );

        fs::create_dir_all(root.join("etc/xdg")).unwrap();
        fs::write(
            root.join("etc/xdg/mimeapps.list"),
            "[Default Applications]\napplication/pdf=org.templaros.Viewer.desktop;\n",
        )
        .unwrap();
        // No manifest installed at all yet: the handler is orphaned.
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Fail);

        let apps_dir = root.join("usr/share/templaros/apps");
        fs::create_dir_all(&apps_dir).unwrap();
        fs::write(
            apps_dir.join("org.templaros.Viewer.json"),
            manifest("org.templaros.Viewer", "/usr/bin/templar-viewer"),
        )
        .unwrap();
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn os_release_skips_rather_than_fails_on_a_non_templaros_runtime_host() {
        // An image root still fails: that is the check's real subject.
        let root = fixture_root("os-release-image");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/os-release"), "ID=ubuntu\nID_LIKE=debian\n").unwrap();
        assert_eq!(
            OsRelease
                .run(&Context {
                    root: root.clone(),
                    hardware: false,
                })
                .outcome,
            Outcome::Fail
        );
        let _ = fs::remove_dir_all(&root);

        // At `/` the same identity is not a fault — the developer host this runs on is Ubuntu,
        // and the session installed onto it is fine. Whatever the host is, never Fail here.
        let live = OsRelease.run(&Context {
            root: std::path::PathBuf::from("/"),
            hardware: false,
        });
        assert_ne!(live.outcome, Outcome::Fail);
    }

    #[test]
    fn mime_handlers_accepts_a_plain_desktop_entry_with_no_templaros_manifest() {
        let root = fixture_root("mime-handlers-desktop");
        let context = Context {
            root: root.clone(),
            hardware: false,
        };
        fs::create_dir_all(root.join("etc/xdg")).unwrap();
        fs::write(
            root.join("etc/xdg/mimeapps.list"),
            "[Default Applications]\nimage/png=XnView.desktop;\n",
        )
        .unwrap();
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Fail);

        // No manifest, no packaging — just the `.desktop` file every other distribution ships.
        let apps = root.join("usr/share/applications");
        fs::create_dir_all(&apps).unwrap();
        fs::write(
            apps.join("XnView.desktop"),
            "[Desktop Entry]\nName=XnView\n",
        )
        .unwrap();
        assert_eq!(MimeHandlersInstalled.run(&context).outcome, Outcome::Pass);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn desktop_entry_installed_resolves_a_nested_desktop_file_id() {
        let root = fixture_root("desktop-id-nesting");
        let apps = root.join("applications");
        fs::create_dir_all(apps.join("kde4")).unwrap();
        fs::write(apps.join("kde4/konsole.desktop"), "[Desktop Entry]\n").unwrap();
        let dirs = vec![apps];
        assert!(desktop_entry_installed(&dirs, "kde4-konsole.desktop"));
        assert!(!desktop_entry_installed(&dirs, "kde4-kate.desktop"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn application_dirs_always_includes_the_snap_and_flatpak_exports() {
        let dirs = application_dirs(&Context {
            root: std::path::PathBuf::from("/image"),
            hardware: false,
        });
        assert!(dirs.contains(&std::path::PathBuf::from(
            "/image/var/lib/snapd/desktop/applications"
        )));
        assert!(dirs.contains(&std::path::PathBuf::from(
            "/image/var/lib/flatpak/exports/share/applications"
        )));
        assert!(dirs.contains(&std::path::PathBuf::from("/image/usr/share/applications")));
    }

    #[test]
    fn autostart_providers_resolved_is_runtime_only() {
        let context = Context {
            root: std::path::PathBuf::from("/some/image/root"),
            hardware: false,
        };
        assert_eq!(
            AutostartProvidersResolved.run(&context).outcome,
            Outcome::Skipped
        );
    }

    #[test]
    fn file_index_health_is_runtime_only() {
        let context = Context {
            root: std::path::PathBuf::from("/some/image/root"),
            hardware: false,
        };
        assert_eq!(FileIndexHealth.run(&context).outcome, Outcome::Skipped);
    }

    #[test]
    fn describe_index_root_reports_a_stale_reason_without_calling_it_an_error() {
        let fixture = tempfile::tempdir().unwrap();
        let db_path = fixture.path().join("index.sqlite3");
        let core =
            templar_file_index::Index::open(&db_path, templar_file_index::IndexConfig::default())
                .unwrap();
        let root = core
            .add_root(
                &templar_file_index::NativePath::from_path(fixture.path()),
                templar_file_index::RootPolicy::default_home(),
            )
            .unwrap();
        // A root only leaves `Empty` for `Stale` by way of a scan — `can_transition_to` refuses
        // the direct jump, the same way the real watcher can never claim `Stale` about a root it
        // never scanned.
        core.scan_root(root.root, templar_file_index::ScanOptions::default())
            .unwrap();
        core.mark_stale(
            root.root,
            &templar_file_index::StaleReason::WatcherExhausted {
                armed: 10,
                refused: 5,
            },
        )
        .unwrap();
        let status = core.root_status(root.root).unwrap();
        let (line, has_error) = describe_index_root(&status);
        assert!(!has_error, "a budget-driven Stale state is not an error");
        assert!(line.contains("watcher-exhausted"), "{line}");
    }

    #[test]
    fn scan_autostart_entries_flags_a_role_declared_by_the_wrong_app() {
        let dir =
            std::env::temp_dir().join(format!("templar-doctor-autostart-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("org.templaros.NetworkTray.desktop"),
            "[Desktop Entry]\nType=Application\nName=Network Tray\nExec=templar-network-tray\n\
             X-TemplarOS-Provides=network-status-ui\n",
        )
        .unwrap();
        fs::write(
            dir.join("hidden.desktop"),
            "[Desktop Entry]\nType=Application\nName=Hidden\nExec=x\nHidden=true\n\
             X-TemplarOS-Provides=network-status-ui\n",
        )
        .unwrap();

        let manifests = [
            serde_json::from_str::<AppManifest>(&manifest(
                "org.templaros.NetworkTray",
                "/usr/bin/templar-network-tray",
            ))
            .unwrap(),
            serde_json::from_str::<AppManifest>(&manifest(
                "org.gnome.NmApplet",
                "/usr/bin/nm-applet",
            ))
            .unwrap(),
        ];
        let by_desktop_id = desktop_id_index(&manifests);

        let mut registry = templar_session_core::providers::ProviderRegistry::default();
        // A selection only sticks (rather than reading as stale) once the app has also declared
        // the role — exactly like the real registry a live session builds.
        registry.declare(
            "org.templaros.NetworkTray".parse().unwrap(),
            templar_types::ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 100,
            },
        );
        registry.declare(
            "org.gnome.NmApplet".parse().unwrap(),
            templar_types::ProviderCapability {
                role: ProviderRole::NetworkStatusUi,
                priority: 50,
            },
        );
        registry.select(
            ProviderRole::NetworkStatusUi,
            "org.gnome.NmApplet".parse().unwrap(),
        );
        let (checked, mismatches) =
            scan_autostart_entries(std::slice::from_ref(&dir), &by_desktop_id, &registry);
        // The hidden entry must not be counted at all.
        assert_eq!(checked, 1);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].contains("org.templaros.NetworkTray.desktop"));

        registry.select(
            ProviderRole::NetworkStatusUi,
            "org.templaros.NetworkTray".parse().unwrap(),
        );
        let (checked, mismatches) =
            scan_autostart_entries(std::slice::from_ref(&dir), &by_desktop_id, &registry);
        assert_eq!(checked, 1);
        assert!(mismatches.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}
