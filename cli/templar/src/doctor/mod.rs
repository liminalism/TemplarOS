//! `templar doctor`: the package-level integration test as a runtime tool (planning doc 01 §7).
//!
//! Every check has an ID, a severity and a human explanation. The same engine runs
//! against `/` at runtime, against an image root with `--root`, and against the hardware matrix.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod checks;
pub mod freeze;
pub mod hardware;
pub mod preflight;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Pass,
    Fail,
    /// The question does not apply to this machine, or the tool that answers it is not installed
    /// — never a `Pass`.
    Skipped,
    /// Nobody asked the question: a manual hardware case that only a human can run, a probe that
    /// timed out, or a probe that would have had to execute against a root that is not this
    /// machine. Distinct from `Skipped` because "we cannot answer" and "we did not try" are
    /// different admissions, and neither may ever be reported as a pass.
    NotExecuted,
}

impl Outcome {
    /// The lower-case word for terminal output. `{:?}` would render `NotExecuted` as
    /// `notexecuted`; the hyphen is the same spelling `--json` uses.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
            Self::NotExecuted => "not-executed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub outcome: Outcome,
    pub summary: String,
    pub detail: String,
}

impl Finding {
    /// Keep warning failures distinct from blocking errors in terminal output.
    /// The JSON outcome remains `fail`; severity still controls the exit status.
    pub fn terminal_label(&self) -> &'static str {
        match (self.outcome, self.severity) {
            (Outcome::Fail, Severity::Warning) => "warning",
            (Outcome::Fail, Severity::Error) => "error",
            _ => self.outcome.label(),
        }
    }
}

/// Where and what to check.
#[derive(Clone, Debug)]
pub struct Context {
    /// Filesystem root (`/` at runtime, an unpacked image otherwise).
    pub root: PathBuf,
    /// Include hardware-matrix checks (suspend/resume, hotplug…) — only meaningful on hardware.
    pub hardware: bool,
}

impl Context {
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative.trim_start_matches('/'))
    }
}

/// A single check.
pub trait Check: Sync {
    fn id(&self) -> &'static str;
    fn severity(&self) -> Severity;
    fn run(&self, context: &Context) -> Finding;
}

/// Run every registered check.
pub fn run_all(context: &Context) -> Vec<Finding> {
    run_all_with_hardware(context, &hardware::HardwareOptions::default())
}

/// Run one built-in check by id, for `--check`. Returns `None` when the id is not a
/// built-in — the caller then resolves it as a hardware case (with `--hardware`) or
/// reports it unknown.
pub fn run_one_builtin(context: &Context, id: &str) -> Option<Finding> {
    checks::all()
        .iter()
        .find(|check| check.id() == id)
        .map(|check| check.run(context))
}

/// The ids `--check` accepts on its own.
pub fn builtin_ids() -> Vec<&'static str> {
    checks::all().iter().map(|check| check.id()).collect()
}

/// Run the package/image checks and, when requested, append the selected hardware-matrix plan.
pub fn run_all_with_hardware(
    context: &Context,
    hardware_options: &hardware::HardwareOptions,
) -> Vec<Finding> {
    let mut findings: Vec<Finding> = checks::all()
        .iter()
        .map(|check| check.run(context))
        .collect();
    if context.hardware {
        findings.extend(hardware::findings(context, hardware_options));
    }
    findings
}

/// Exit code convention: 0 all pass, 1 any error, 2 warnings only.
pub fn exit_code(findings: &[Finding]) -> i32 {
    let failed = |severity| {
        findings
            .iter()
            .any(|f| f.outcome == Outcome::Fail && f.severity == severity)
    };
    if failed(Severity::Error) {
        1
    } else if failed(Severity::Warning) {
        2
    } else {
        0
    }
}

/// How many comma-separated entries of a `detail` the terminal output keeps, and the hard
/// character ceiling for a detail that is not a list.
const DETAIL_ITEMS: usize = 4;
const DETAIL_CHARS: usize = 160;

/// Shorten a `detail` for terminal output.
///
/// `detail` is unbounded, and `mime-handlers` against a populated `mimeapps.list` emits several
/// kilobytes of it on one line. That line buries every other finding in the scrollback — the
/// `fail` rows the reader is actually looking for included — so the human output keeps the first
/// few entries and says how many it dropped. `--json` still carries the whole thing.
pub fn abbreviate(detail: &str) -> String {
    let items: Vec<&str> = detail.split(", ").collect();
    if items.len() > DETAIL_ITEMS {
        return format!(
            "{} … and {} more",
            items[..DETAIL_ITEMS].join(", "),
            items.len() - DETAIL_ITEMS
        );
    }
    if detail.chars().count() > DETAIL_CHARS {
        return format!("{}…", detail.chars().take(DETAIL_CHARS).collect::<String>());
    }
    detail.to_owned()
}

/// A closing tally that names the failures.
///
/// Findings print in registration order, so a `fail` sits wherever its check happens to be
/// registered and scrolls past between passes. The tally is the last thing on screen and repeats
/// the failed ids, which is what a caller like `scripts/reinstall-templaros-desktop.sh` has just
/// told the reader to go and fix.
pub fn summarise(findings: &[Finding]) -> String {
    let count = |outcome| findings.iter().filter(|f| f.outcome == outcome).count();
    let failed: Vec<&str> = findings
        .iter()
        .filter(|f| f.outcome == Outcome::Fail)
        .map(|f| f.id)
        .collect();
    let tally = format!(
        "{} checks: {} passed, {} failed, {} skipped, {} not executed",
        findings.len(),
        count(Outcome::Pass),
        failed.len(),
        count(Outcome::Skipped),
        count(Outcome::NotExecuted),
    );
    let mut summary = tally;
    for (severity, label) in [
        (Severity::Error, "errors"),
        (Severity::Warning, "warnings"),
        (Severity::Info, "failed information checks"),
    ] {
        let ids: Vec<_> = findings
            .iter()
            .filter(|f| f.outcome == Outcome::Fail && f.severity == severity)
            .map(|f| f.id)
            .collect();
        if !ids.is_empty() {
            summary.push_str(&format!("\n{label}: {}", ids.join(", ")));
        }
    }
    summary
}

pub(crate) fn finding(
    check: &dyn Check,
    outcome: Outcome,
    summary: impl Into<String>,
    detail: impl Into<String>,
) -> Finding {
    Finding {
        id: check.id(),
        severity: check.severity(),
        outcome,
        summary: summary.into(),
        detail: detail.into(),
    }
}

pub(crate) fn exists(root: &Path, relative: &str) -> bool {
    root.join(relative.trim_start_matches('/')).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding_of(id: &'static str, outcome: Outcome, detail: &str) -> Finding {
        Finding {
            id,
            severity: Severity::Error,
            outcome,
            summary: String::new(),
            detail: detail.to_owned(),
        }
    }

    #[test]
    fn abbreviate_keeps_a_short_detail_whole_and_caps_a_long_list() {
        assert_eq!(abbreviate(""), "");
        assert_eq!(abbreviate("a, b, c"), "a, b, c");
        let many: Vec<String> = (0..156).map(|n| format!("item{n}")).collect();
        assert_eq!(
            abbreviate(&many.join(", ")),
            "item0, item1, item2, item3 … and 152 more"
        );
    }

    #[test]
    fn abbreviate_caps_a_long_detail_that_is_not_a_list() {
        let long = "x".repeat(500);
        let short = abbreviate(&long);
        assert_eq!(short.chars().count(), DETAIL_CHARS + 1);
        assert!(short.ends_with('…'));
    }

    #[test]
    fn check_selection_runs_one_builtin_by_id() {
        // `--check file-index` used to require `--hardware` and then fail as unknown
        // hardware; a built-in id now selects exactly that check, no hardware needed.
        let context = Context {
            root: std::path::PathBuf::from("/"),
            hardware: false,
        };
        let finding = run_one_builtin(&context, "file-index").expect("a built-in id resolves");
        assert_eq!(finding.id, "file-index");
        assert!(run_one_builtin(&context, "gpu-mode").is_none());
        assert!(run_one_builtin(&context, "bogus-name").is_none());
        assert!(builtin_ids().contains(&"file-index"));
    }

    #[test]
    fn summarise_tallies_and_names_the_failures() {
        let findings = vec![
            finding_of("os-release", Outcome::Skipped, ""),
            finding_of("session-units", Outcome::Pass, ""),
            finding_of("mime-handlers", Outcome::Fail, ""),
            finding_of("suspend-resume", Outcome::NotExecuted, ""),
        ];
        assert_eq!(
            summarise(&findings),
            "4 checks: 1 passed, 1 failed, 1 skipped, 1 not executed\nerrors: mime-handlers"
        );

        let clean = vec![finding_of("session-units", Outcome::Pass, "")];
        assert_eq!(
            summarise(&clean),
            "1 checks: 1 passed, 0 failed, 0 skipped, 0 not executed"
        );
    }

    #[test]
    fn warning_failures_are_distinct_from_blocking_errors() {
        let error = finding_of("compositor-usable", Outcome::Fail, "");
        let mut warning = finding_of("running-build", Outcome::Fail, "");
        warning.severity = Severity::Warning;
        assert_eq!(error.terminal_label(), "error");
        assert_eq!(warning.terminal_label(), "warning");
        assert_eq!(exit_code(std::slice::from_ref(&warning)), 2);
        let findings = [error, warning];
        assert_eq!(exit_code(&findings), 1);
        assert_eq!(
            summarise(&findings),
            "2 checks: 0 passed, 2 failed, 0 skipped, 0 not executed\nerrors: compositor-usable\nwarnings: running-build"
        );
        let mut passing = findings[1].clone();
        passing.outcome = Outcome::Pass;
        assert_eq!(passing.terminal_label(), "pass");
        assert_eq!(exit_code(&[passing]), 0);
    }
}
