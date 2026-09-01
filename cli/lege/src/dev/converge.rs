//! Convergence: the difference between what is installed and what is running.
//!
//! Pure. Everything ambient — the dpkg status file, the runtime stamp directory, the clock — is
//! read by the caller and passed in, so every verdict in this file is reproducible from a fixture
//! and the report can be trusted to be about the machine rather than about the tool's mood.
//!
//! The shape of the answer is deliberately per-component rather than one overall yes/no. "The
//! deployment did not converge" is useless; "the shell is running the new build, the compositor
//! needs a logout, and hostd is still on last week's set" is something a person can act on.

use lege_build::{
    RunningStamp,
    components::{Activation, Component, Matrix},
};
use std::collections::BTreeMap;

/// Where one component stands relative to the target build set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Installed at the target set, and running it (or nothing needs to run).
    Converged,
    /// Installed at the target set; an older build is still running.
    NeedsActivation,
    /// Installed at the target set; expected to be running, and is not running at all.
    NotRunning,
    /// Installed at the target set, but nothing here can confirm what is running.
    Unverifiable,
    /// Installed at some other version — the mixed install that used to go unnoticed.
    OtherVersion,
    /// Not installed at all.
    NotInstalled,
}

impl Verdict {
    /// One glyph, in a column, so the shape of the answer is visible before any of it is read.
    #[must_use]
    pub fn glyph(self, activation: Activation) -> &'static str {
        match self {
            Self::Converged => "✓",
            Self::NeedsActivation => match activation {
                Activation::NeedsSession => "⟳",
                Activation::NeedsReboot => "⏻",
                _ => "↻",
            },
            Self::NotRunning => "·",
            Self::Unverifiable => "?",
            Self::OtherVersion | Self::NotInstalled => "✗",
        }
    }

    /// Whether this verdict means the deployment is not finished.
    #[must_use]
    pub fn is_outstanding(self) -> bool {
        !matches!(self, Self::Converged)
    }
}

/// One component's standing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub component: String,
    pub summary: String,
    pub activation: Activation,
    pub installed: Option<String>,
    pub running: Option<String>,
    pub verdict: Verdict,
}

impl Row {
    /// The calm one-line explanation printed after the component name.
    #[must_use]
    pub fn explanation(&self) -> String {
        match self.verdict {
            Verdict::Converged => "running this build".to_owned(),
            Verdict::NeedsActivation => format!(
                "installed, but the old build is still running — {}",
                self.activation.explanation()
            ),
            Verdict::NotRunning => "installed; not running".to_owned(),
            Verdict::Unverifiable => format!("installed — {}", self.activation.explanation()),
            Verdict::OtherVersion => format!(
                "installed at {}, which is not this set",
                self.installed.as_deref().unwrap_or("?")
            ),
            Verdict::NotInstalled => "not installed".to_owned(),
        }
    }
}

/// The whole answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Convergence {
    /// The build set everything is being compared against, if one could be determined.
    pub target: Option<String>,
    pub rows: Vec<Row>,
}

impl Convergence {
    #[must_use]
    pub fn converged(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.verdict == Verdict::Converged)
            .count()
    }

    #[must_use]
    pub fn is_converged(&self) -> bool {
        self.target.is_some() && self.rows.iter().all(|row| !row.verdict.is_outstanding())
    }

    /// Every distinct installed version, so a mixed install can be named as such.
    #[must_use]
    pub fn installed_versions(&self) -> Vec<&str> {
        let mut versions: Vec<&str> = self
            .rows
            .iter()
            .filter_map(|row| row.installed.as_deref())
            .collect();
        versions.sort_unstable();
        versions.dedup();
        versions
    }
}

/// What the machine looks like, gathered by the caller.
#[derive(Debug, Default)]
pub struct Observation {
    /// Debian package name → installed version.
    pub installed: BTreeMap<String, String>,
    /// Stamp name → what that process says it is running.
    pub running: BTreeMap<String, RunningStamp>,
}

/// Compare the matrix against the machine.
///
/// `target` is the build set everything should be at. Passing `None` asks this function to infer
/// it, which it does by taking the *newest* installed set by epoch rather than the most common
/// one: after a deployment that half-failed, the newest is what was being deployed, and the
/// laggards are what still needs attention. Naming the majority instead would report a
/// half-finished upgrade as a mostly-fine install with two odd packages.
#[must_use]
pub fn converge(matrix: &Matrix, observation: &Observation, target: Option<&str>) -> Convergence {
    let target = target
        .map(ToOwned::to_owned)
        .or_else(|| newest_installed_set(matrix, observation));
    let rows = matrix
        .components
        .iter()
        .map(|component| row_for(component, observation, target.as_deref()))
        .collect();
    Convergence { target, rows }
}

fn row_for(component: &Component, observation: &Observation, target: Option<&str>) -> Row {
    let installed = observation.installed.get(&component.package).cloned();
    let running = component
        .stamp
        .as_ref()
        .and_then(|stamp| observation.running.get(stamp))
        .map(|stamp| stamp.build_set.clone());
    let verdict = verdict_for(component, installed.as_deref(), running.as_deref(), target);
    Row {
        component: component.name.clone(),
        summary: component.summary.clone(),
        activation: component.activation,
        installed,
        running,
        verdict,
    }
}

fn verdict_for(
    component: &Component,
    installed: Option<&str>,
    running: Option<&str>,
    target: Option<&str>,
) -> Verdict {
    let Some(installed) = installed else {
        return Verdict::NotInstalled;
    };
    let Some(target) = target else {
        // Nothing to compare against: on a source checkout with no LegeOS installed at all this
        // function is not reached, so this is the odd case of packages present with versions
        // nothing here can order.
        return Verdict::Unverifiable;
    };
    if installed != target {
        return Verdict::OtherVersion;
    }
    match (component.stamp.as_ref(), running) {
        // No stamp: installing it is as far as this can be observed, and that is enough.
        //
        // For an application or the CLI it is literally enough — the next launch is the new
        // binary. For the session units and the greeter it is an inference, but a sound one: the
        // compositor does write a stamp and needs the same new session, so a session started
        // before the install shows up on the `comp` row. Reporting these two as permanently
        // outstanding instead would put a line in every report that never goes away, and a
        // report with a permanent complaint in it is one people stop reading.
        (None, _) => Verdict::Converged,
        (Some(_), None) => Verdict::NotRunning,
        (Some(_), Some(running)) if running == target => Verdict::Converged,
        (Some(_), Some(_)) => Verdict::NeedsActivation,
    }
}

/// The newest installed build set by epoch, ignoring versions that are not build-set ids.
fn newest_installed_set(matrix: &Matrix, observation: &Observation) -> Option<String> {
    matrix
        .components
        .iter()
        .filter_map(|component| observation.installed.get(&component.package))
        .filter_map(|version| {
            lege_build::parse_build_set_id(version).map(|parsed| (parsed.epoch, version.clone()))
        })
        .max_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)))
        .map(|(_, version)| version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const OLDER: &str = "0.1.0~dev1000+gbbbbbbb";

    fn stamp(build_set: &str) -> RunningStamp {
        RunningStamp {
            component: String::new(),
            build_set: build_set.to_owned(),
            git_sha: "aaaaaaa".to_owned(),
            dirty: false,
            pid: 1,
            started_at_ms: 0,
            schema_versions: BTreeMap::new(),
        }
    }

    /// A machine with everything installed at `set` and every stamping component running it.
    fn fully_converged(set: &str) -> Observation {
        let matrix = Matrix::embedded();
        let mut observation = Observation::default();
        for component in &matrix.components {
            observation
                .installed
                .insert(component.package.clone(), set.to_owned());
            if let Some(name) = &component.stamp {
                observation.running.insert(name.clone(), stamp(set));
            }
        }
        observation
    }

    fn verdict(convergence: &Convergence, component: &str) -> Verdict {
        convergence
            .rows
            .iter()
            .find(|row| row.component == component)
            .unwrap_or_else(|| panic!("no row for {component}"))
            .verdict
    }

    #[test]
    fn a_machine_at_one_set_with_everything_running_it_has_converged() {
        let matrix = Matrix::embedded();
        let convergence = converge(&matrix, &fully_converged(SET), None);
        assert_eq!(convergence.target.as_deref(), Some(SET));
        assert!(convergence.is_converged(), "{:#?}", convergence.rows);
        assert_eq!(convergence.converged(), matrix.components.len());
    }

    #[test]
    fn a_component_still_running_the_old_build_needs_activation() {
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(SET);
        observation
            .running
            .insert("lege-shell".to_owned(), stamp(OLDER));
        let convergence = converge(&matrix, &observation, None);
        assert_eq!(verdict(&convergence, "shell"), Verdict::NeedsActivation);
        assert!(!convergence.is_converged());
        let row = convergence
            .rows
            .iter()
            .find(|row| row.component == "shell")
            .unwrap();
        assert_eq!(row.glyph_for_test(), "↻");
        assert!(row.explanation().contains("restarted in place"));
    }

    #[test]
    fn the_compositor_needing_a_logout_says_so_rather_than_offering_a_restart() {
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(SET);
        observation
            .running
            .insert("lege-comp".to_owned(), stamp(OLDER));
        let convergence = converge(&matrix, &observation, None);
        let row = convergence
            .rows
            .iter()
            .find(|row| row.component == "comp")
            .unwrap();
        assert_eq!(row.verdict, Verdict::NeedsActivation);
        assert_eq!(row.glyph_for_test(), "⟳");
        assert!(
            row.explanation().contains("log out"),
            "{}",
            row.explanation()
        );
    }

    #[test]
    fn a_package_left_behind_at_an_older_version_is_named() {
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(SET);
        observation
            .installed
            .insert("legeos-hostd".to_owned(), OLDER.to_owned());
        let convergence = converge(&matrix, &observation, None);
        assert_eq!(convergence.target.as_deref(), Some(SET));
        assert_eq!(verdict(&convergence, "hostd"), Verdict::OtherVersion);
        assert_eq!(convergence.installed_versions().len(), 2);
    }

    #[test]
    fn the_target_is_the_newest_set_not_the_most_common_one() {
        // A deployment that only got two packages in. The two new ones are what was being
        // deployed; reporting the eleven old ones as the target would call a half-finished
        // upgrade a healthy install.
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(OLDER);
        observation
            .installed
            .insert("legeos-shell".to_owned(), SET.to_owned());
        observation
            .installed
            .insert("legeos-cli".to_owned(), SET.to_owned());
        let convergence = converge(&matrix, &observation, None);
        assert_eq!(convergence.target.as_deref(), Some(SET));
        assert_eq!(verdict(&convergence, "sessiond"), Verdict::OtherVersion);
    }

    #[test]
    fn a_missing_package_is_not_installed_rather_than_stale() {
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(SET);
        observation.installed.remove("legeos-filed");
        let convergence = converge(&matrix, &observation, None);
        assert_eq!(verdict(&convergence, "filed"), Verdict::NotInstalled);
    }

    #[test]
    fn a_stamping_component_that_is_not_running_reads_as_not_running() {
        let matrix = Matrix::embedded();
        let mut observation = fully_converged(SET);
        observation.running.remove("lege-shell");
        let convergence = converge(&matrix, &observation, None);
        assert_eq!(verdict(&convergence, "shell"), Verdict::NotRunning);
    }

    #[test]
    fn an_application_is_converged_once_it_is_installed() {
        // Settings and lege-files write no stamp: nothing needs restarting, the next launch is
        // the new binary, and the report must not manufacture an outstanding item.
        let matrix = Matrix::embedded();
        let convergence = converge(&matrix, &fully_converged(SET), None);
        assert_eq!(verdict(&convergence, "settings"), Verdict::Converged);
        assert_eq!(verdict(&convergence, "files"), Verdict::Converged);
        assert_eq!(verdict(&convergence, "cli"), Verdict::Converged);
    }

    #[test]
    fn a_stale_session_shows_up_on_the_compositor_row_that_can_prove_it() {
        // The session units and the greeter write no stamp, so they read as converged once
        // installed. They are not silently excused: the compositor needs the same new session and
        // does write a stamp, so an old session is reported — once, by the row that can prove it.
        let matrix = Matrix::embedded();
        let convergence = converge(&matrix, &fully_converged(SET), None);
        assert_eq!(verdict(&convergence, "session"), Verdict::Converged);
        assert_eq!(verdict(&convergence, "greeter"), Verdict::Converged);

        let mut observation = fully_converged(SET);
        observation
            .running
            .insert("lege-comp".to_owned(), stamp(OLDER));
        let convergence = converge(&matrix, &observation, None);
        assert!(!convergence.is_converged());
        assert_eq!(verdict(&convergence, "comp"), Verdict::NeedsActivation);
    }

    #[test]
    fn an_explicit_target_overrides_what_is_installed() {
        let matrix = Matrix::embedded();
        let convergence = converge(&matrix, &fully_converged(OLDER), Some(SET));
        assert_eq!(convergence.target.as_deref(), Some(SET));
        assert_eq!(convergence.converged(), 0);
    }

    #[test]
    fn a_machine_with_nothing_installed_has_no_target() {
        let matrix = Matrix::embedded();
        let convergence = converge(&matrix, &Observation::default(), None);
        assert_eq!(convergence.target, None);
        assert!(
            convergence
                .rows
                .iter()
                .all(|row| row.verdict == Verdict::NotInstalled)
        );
    }

    impl Row {
        fn glyph_for_test(&self) -> &'static str {
            self.verdict.glyph(self.activation)
        }
    }
}
