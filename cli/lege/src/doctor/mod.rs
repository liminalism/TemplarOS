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
