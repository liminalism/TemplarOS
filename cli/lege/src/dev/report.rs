//! The convergence report.
//!
//! This is a user-facing design surface, not debug output. Someone reads it at the moment they
//! are least patient — right after a deployment, wanting to know whether it is safe to keep
//! working — so it has to be true at a glance: one glyph per component in a fixed column, one
//! calm sentence saying what is left, and every version string spelled out exactly once at the
//! top rather than repeated thirteen times down the page.

use super::converge::{Convergence, Row, Verdict};
use lege_build::components::Activation;
use std::fmt::Write as _;

/// The whole report, as printed.
#[must_use]
pub fn render(convergence: &Convergence) -> String {
    let Some(target) = &convergence.target else {
        return no_build_set(convergence);
    };

    let mut out = String::new();
    let total = convergence.rows.len();
    let converged = convergence.converged();
    let _ = writeln!(out, "build set {target}");
    let _ = writeln!(out, "{converged} of {total} component(s) converged\n");

    let width = convergence
        .rows
        .iter()
        .map(|row| row.component.chars().count())
        .max()
        .unwrap_or(0);
    for row in &convergence.rows {
        let _ = writeln!(
            out,
            "  {glyph}  {name:<width$}  {explanation}",
            glyph = row.verdict.glyph(row.activation),
            name = row.component,
            explanation = row.explanation(),
        );
    }

    let outstanding: Vec<&Row> = convergence
        .rows
        .iter()
        .filter(|row| row.verdict.is_outstanding())
        .collect();
    if outstanding.is_empty() {
        out.push_str("\nEverything is running the build you installed.\n");
        return out;
    }

    out.push('\n');
    for line in outstanding_summary(&outstanding) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// What to say when no build set could be determined.
///
/// Two very different situations, and conflating them would be a lie in one direction or the
/// other: a source checkout with nothing installed, and a machine running LegeOS packages that
/// predate build identity — every one of them `0.1.0-1`, which is exactly the state this whole
/// mechanism was built to end.
fn no_build_set(convergence: &Convergence) -> String {
    let installed: Vec<&Row> = convergence
        .rows
        .iter()
        .filter(|row| row.installed.is_some())
        .collect();
    if installed.is_empty() {
        return "No LegeOS packages are installed here.\n".to_owned();
    }
    let mut out = String::from(
        "The installed packages carry no build-set id, so what is running cannot be compared \
         with what is installed.\n\n",
    );
    let width = installed
        .iter()
        .map(|row| row.component.chars().count())
        .max()
        .unwrap_or(0);
    for row in &installed {
        let _ = writeln!(
            out,
            "  ?  {name:<width$}  {version}",
            name = row.component,
            version = row.installed.as_deref().unwrap_or("?"),
        );
    }
    out.push_str(
        "\nBuild them with `cargo xtask build-set` and install that set; from then on this \
         report can answer.\n",
    );
    out
}

/// The closing paragraph: what is left, grouped by what it would take, in the order a person
/// would do it — the things the tool can do, then the logout, then the reboot, then the faults.
fn outstanding_summary(outstanding: &[&Row]) -> Vec<String> {
    let mut lines = Vec::new();
    let named = |rows: Vec<&Row>| -> String {
        rows.iter()
            .map(|row| row.component.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let restartable: Vec<&Row> = outstanding
        .iter()
        .copied()
        .filter(|row| {
            row.verdict == Verdict::NeedsActivation
                && matches!(row.activation, Activation::Restart | Activation::Manual)
        })
        .collect();
    if !restartable.is_empty() {
        lines.push(format!(
            "Still running an older build: {}. `lege dev deploy` restarts these.",
            named(restartable)
        ));
    }

    let session: Vec<&Row> = outstanding
        .iter()
        .copied()
        .filter(|row| {
            row.activation == Activation::NeedsSession && row.verdict != Verdict::Converged
        })
        .collect();
    if !session.is_empty() {
        lines.push(format!(
            "Waiting for a new session: {}. Log out and back in when you are ready.",
            named(session)
        ));
    }

    let reboot: Vec<&Row> = outstanding
        .iter()
        .copied()
        .filter(|row| row.activation == Activation::NeedsReboot)
        .collect();
    if !reboot.is_empty() {
        lines.push(format!("Waiting for a reboot: {}.", named(reboot)));
    }

    let wrong: Vec<&Row> = outstanding
        .iter()
        .copied()
        .filter(|row| matches!(row.verdict, Verdict::OtherVersion | Verdict::NotInstalled))
        .collect();
    if !wrong.is_empty() {
        lines.push(format!(
            "Not part of this set: {}. The install is mixed; deploy the set again.",
            named(wrong)
        ));
    }

    let idle: Vec<&Row> = outstanding
        .iter()
        .copied()
        .filter(|row| row.verdict == Verdict::NotRunning)
        .collect();
    if !idle.is_empty() {
        lines.push(format!(
            "Installed but not running: {}. That is expected outside a LegeOS session.",
            named(idle)
        ));
    }
    lines
}

/// The same answer as data, for `--json`.
#[must_use]
pub fn as_json(convergence: &Convergence) -> serde_json::Value {
    serde_json::json!({
        "build_set": convergence.target,
        "converged": convergence.is_converged(),
        "components": convergence
            .rows
            .iter()
            .map(|row| serde_json::json!({
                "component": row.component,
                "verdict": format!("{:?}", row.verdict),
                "activation": format!("{:?}", row.activation),
                "installed": row.installed,
                "running": row.running,
                "explanation": row.explanation(),
            }))
            .collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::converge::{Observation, converge};
    use super::*;
    use lege_build::{RunningStamp, components::Matrix};
    use std::collections::BTreeMap;

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const OLDER: &str = "0.1.0~dev1000+gbbbbbbb";

    fn observation(set: &str) -> Observation {
        let matrix = Matrix::embedded();
        let mut observation = Observation::default();
        for component in &matrix.components {
            observation
                .installed
                .insert(component.package.clone(), set.to_owned());
            if let Some(name) = &component.stamp {
                observation.running.insert(
                    name.clone(),
                    RunningStamp {
                        component: name.clone(),
                        build_set: set.to_owned(),
                        git_sha: "aaaaaaa".to_owned(),
                        dirty: false,
                        pid: 1,
                        started_at_ms: 0,
                        schema_versions: BTreeMap::new(),
                    },
                );
            }
        }
        observation
    }

    #[test]
    fn a_machine_with_nothing_installed_says_so_in_one_sentence() {
        let text = render(&converge(
            &Matrix::embedded(),
            &Observation::default(),
            None,
        ));
        assert_eq!(text, "No LegeOS packages are installed here.\n");
    }

    #[test]
    fn packages_predating_build_identity_are_a_different_sentence_from_no_packages() {
        // The state of every LegeOS install before this work: thirteen packages, all 0.1.0-1,
        // and no way to tell a fresh one from a four-day-old one. Saying "nothing is installed"
        // here would be false, and saying "converged" would be worse.
        let mut observed = observation(SET);
        for version in observed.installed.values_mut() {
            *version = "0.1.0-1".to_owned();
        }
        let text = render(&converge(&Matrix::embedded(), &observed, None));
        assert!(text.contains("carry no build-set id"), "{text}");
        assert!(text.contains("0.1.0-1"), "{text}");
        assert!(text.contains("cargo xtask build-set"), "{text}");
    }

    #[test]
    fn the_component_column_is_aligned() {
        let convergence = converge(&Matrix::embedded(), &observation(SET), None);
        let text = render(&convergence);
        let columns: Vec<usize> = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .filter_map(|line| line.find("running this build"))
            .collect();
        assert!(!columns.is_empty());
        assert!(
            columns.windows(2).all(|pair| pair[0] == pair[1]),
            "explanations must start in one column:\n{text}"
        );
    }

    #[test]
    fn a_converged_machine_ends_with_the_sentence_someone_wants_to_read() {
        let convergence = converge(&Matrix::embedded(), &observation(SET), None);
        let text = render(&convergence);
        assert!(text.contains("build set 0.1.0~dev2000+gaaaaaaa"), "{text}");
        assert!(
            text.ends_with("Everything is running the build you installed.\n"),
            "{text}"
        );
    }

    #[test]
    fn an_outstanding_restart_and_an_outstanding_logout_are_reported_separately() {
        let mut observed = observation(SET);
        for (stamp, name) in [("lege-shell", "shell"), ("lege-comp", "comp")] {
            let entry = observed.running.get_mut(stamp).unwrap();
            entry.build_set = OLDER.to_owned();
            assert_eq!(entry.component, stamp, "fixture names {name}");
        }
        let text = render(&converge(&Matrix::embedded(), &observed, None));
        assert!(
            text.contains("Still running an older build: shell."),
            "{text}"
        );
        assert!(text.contains("Waiting for a new session: comp."), "{text}");
    }

    #[test]
    fn a_mixed_install_says_the_install_is_mixed() {
        let mut observed = observation(SET);
        observed
            .installed
            .insert("legeos-hostd".to_owned(), OLDER.to_owned());
        let text = render(&converge(&Matrix::embedded(), &observed, None));
        assert!(text.contains("Not part of this set: hostd."), "{text}");
        assert!(text.contains("The install is mixed"), "{text}");
    }

    #[test]
    fn the_json_form_carries_every_component() {
        let convergence = converge(&Matrix::embedded(), &observation(SET), None);
        let value = as_json(&convergence);
        assert_eq!(value["converged"], serde_json::Value::Bool(true));
        assert_eq!(
            value["components"].as_array().unwrap().len(),
            convergence.rows.len()
        );
    }
}
