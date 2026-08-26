//! `lege doctor`: the package-level integration test as a runtime tool (planning doc 01 §7).
//!
//! Every check has an ID, a severity and a human explanation. The same engine runs
//! against `/` at runtime, against an image root with `--root`, and against the hardware matrix.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod checks;

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
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub outcome: Outcome,
    pub summary: String,
    pub detail: String,
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
    checks::all()
        .iter()
        .map(|check| check.run(context))
        .collect()
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
/// the failed ids, which is what a caller like `scripts/reinstall-legeos-desktop.sh` has just
/// told the reader to go and fix.
pub fn summarise(findings: &[Finding]) -> String {
    let count = |outcome| findings.iter().filter(|f| f.outcome == outcome).count();
    let failed: Vec<&str> = findings
        .iter()
        .filter(|f| f.outcome == Outcome::Fail)
        .map(|f| f.id)
        .collect();
    let tally = format!(
        "{} checks: {} passed, {} failed, {} skipped",
        findings.len(),
        count(Outcome::Pass),
        failed.len(),
        count(Outcome::Skipped),
    );
    if failed.is_empty() {
        tally
    } else {
        format!("{tally}\nfailed: {}", failed.join(", "))
    }
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
    fn summarise_tallies_and_names_the_failures() {
        let findings = vec![
            finding_of("os-release", Outcome::Skipped, ""),
            finding_of("session-units", Outcome::Pass, ""),
            finding_of("mime-handlers", Outcome::Fail, ""),
        ];
        assert_eq!(
            summarise(&findings),
            "3 checks: 1 passed, 1 failed, 1 skipped\nfailed: mime-handlers"
        );

        let clean = vec![finding_of("session-units", Outcome::Pass, "")];
        assert_eq!(summarise(&clean), "1 checks: 1 passed, 0 failed, 0 skipped");
    }
}
