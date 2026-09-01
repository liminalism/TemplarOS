//! The component matrix: what a build set is made of, and what it takes to make each part run.
//!
//! Parsed from `distro/deploy/components.toml`, which is installed as
//! `/usr/share/legeos/components.toml`. Four copies of the package list existed before this — the
//! justfile's `deb` recipe, `lege doctor`'s `LEGEOS_PACKAGES`, the reinstall script's
//! `expected_packages`, and the deployment tooling — and a list that lives in four places is a
//! list that is wrong in at least one.

use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf};

/// The matrix as shipped in this source tree. Compiled in so that `lege dev` and `cargo xtask`
/// answer identically from a checkout, from an installed system, and from a unit test with no
/// filesystem at all.
const EMBEDDED: &str = include_str!("../../../distro/deploy/components.toml");

/// Where the matrix is installed.
pub const INSTALLED_PATH: &str = "/usr/share/legeos/components.toml";

/// How a newly installed component becomes the one that is running.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Activation {
    /// Nothing to do: the next invocation is the new binary.
    Immediate,
    /// Restart its unit, then wait for it to come back healthy.
    Restart,
    /// Running instances keep the old binary; the next launch gets the new one.
    Relaunch,
    /// Restartable in principle, but not by the tool — it prints what to do instead.
    Manual,
    /// Log out and back in.
    NeedsSession,
    /// Reboot.
    NeedsReboot,
}

impl Activation {
    /// The one-line explanation shown in a convergence report.
    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Immediate => "the next command you run is the new one",
            Self::Restart => "restarted in place",
            Self::Relaunch => "open windows keep the old build; the next launch gets the new one",
            Self::Manual => "restart it yourself when convenient",
            Self::NeedsSession => "log out and back in",
            Self::NeedsReboot => "takes effect at the next boot",
        }
    }
}

/// Which systemd manager owns the component's unit, if any.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    User,
    System,
    /// No unit: an application, a CLI, or a data-only package.
    None,
}

/// One row of the matrix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Component {
    /// Matrix row key — short, stable, what a person types.
    pub name: String,
    /// Cargo package name (`cargo deb -p`).
    #[serde(rename = "crate")]
    pub crate_name: String,
    /// Debian package name.
    pub package: String,
    /// Basename of the runtime stamp this component writes, when it writes one. A component with
    /// no stamp can still be *installed* coherently; it just cannot be asked what it is running.
    #[serde(default)]
    pub stamp: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    pub scope: Scope,
    pub activation: Activation,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub summary: String,
}

/// The whole matrix.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Matrix {
    pub version: u32,
    #[serde(rename = "component")]
    pub components: Vec<Component>,
}

/// Why a matrix could not be used.
#[derive(Debug)]
pub enum MatrixError {
    Parse(toml::de::Error),
    Read {
        path: PathBuf,
        error: std::io::Error,
    },
    Invalid(Vec<String>),
}

impl fmt::Display for MatrixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "parsing the component matrix: {error}"),
            Self::Read { path, error } => {
                write!(f, "reading {}: {error}", path.display())
            }
            Self::Invalid(problems) => {
                write!(f, "the component matrix is inconsistent:")?;
                for problem in problems {
                    write!(f, "\n  {problem}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for MatrixError {}

impl Matrix {
    /// The matrix compiled into this binary.
    ///
    /// # Panics
    /// Never in a built binary: the embedded text is validated by this crate's own tests, so a
    /// malformed matrix fails `just check` rather than reaching anyone.
    #[must_use]
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("the embedded component matrix must be valid")
    }

    /// Parses and validates a matrix.
    ///
    /// # Errors
    /// If the TOML is malformed, or the rows contradict each other (duplicate names or packages,
    /// a `depends_on` naming nothing, a unit-less component claiming a systemd scope).
    pub fn parse(text: &str) -> Result<Self, MatrixError> {
        let matrix: Self = toml::from_str(text).map_err(MatrixError::Parse)?;
        let problems = matrix.problems();
        if problems.is_empty() {
            Ok(matrix)
        } else {
            Err(MatrixError::Invalid(problems))
        }
    }

    /// Reads a matrix from a path.
    ///
    /// # Errors
    /// If the file cannot be read, or [`Matrix::parse`] rejects it.
    pub fn read(path: &std::path::Path) -> Result<Self, MatrixError> {
        let text = std::fs::read_to_string(path).map_err(|error| MatrixError::Read {
            path: path.to_path_buf(),
            error,
        })?;
        Self::parse(&text)
    }

    /// Everything wrong with this matrix, in one pass so a person fixing it sees all of it.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.version != 1 {
            problems.push(format!(
                "version = {} is not a version this build understands (expected 1)",
                self.version
            ));
        }
        if self.components.is_empty() {
            problems.push("no [[component]] rows".to_owned());
        }
        let mut seen_names = std::collections::BTreeSet::new();
        let mut seen_packages = std::collections::BTreeSet::new();
        let mut seen_stamps = std::collections::BTreeSet::new();
        for component in &self.components {
            let name = &component.name;
            if !seen_names.insert(name.clone()) {
                problems.push(format!("two components are called {name:?}"));
            }
            if !seen_packages.insert(component.package.clone()) {
                problems.push(format!(
                    "two components ship the package {:?}",
                    component.package
                ));
            }
            if let Some(stamp) = &component.stamp
                && !seen_stamps.insert(stamp.clone())
            {
                problems.push(format!("two components write the stamp {stamp:?}"));
            }
            match (component.scope, component.unit.as_ref()) {
                (Scope::None, Some(unit)) => problems.push(format!(
                    "{name} has scope \"none\" but names the unit {unit:?}"
                )),
                (Scope::User | Scope::System, None) => {
                    problems.push(format!("{name} claims a systemd scope but names no unit"));
                }
                _ => {}
            }
            if component.activation == Activation::Restart && component.unit.is_none() {
                problems.push(format!("{name} is activated by restart but has no unit"));
            }
            for dependency in &component.depends_on {
                if !self.components.iter().any(|c| &c.name == dependency) {
                    problems.push(format!(
                        "{name} depends on {dependency:?}, which is not a component"
                    ));
                }
            }
        }
        problems
    }

    /// A component by matrix name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Component> {
        self.components.iter().find(|c| c.name == name)
    }

    /// A component by the name of the stamp it writes.
    #[must_use]
    pub fn by_stamp(&self, stamp: &str) -> Option<&Component> {
        self.components
            .iter()
            .find(|c| c.stamp.as_deref() == Some(stamp))
    }

    /// Every Debian package in a build set, in matrix order.
    #[must_use]
    pub fn packages(&self) -> Vec<&str> {
        self.components.iter().map(|c| c.package.as_str()).collect()
    }

    /// Every cargo package to build, in matrix order.
    #[must_use]
    pub fn crates(&self) -> Vec<&str> {
        self.components
            .iter()
            .map(|c| c.crate_name.as_str())
            .collect()
    }

    /// Component names in dependency order: a component never precedes one it depends on.
    ///
    /// Providers before dependents is what keeps an activation from restarting the shell into a
    /// sessiond that has not come back yet. Cycles cannot occur — [`Matrix::problems`] would have
    /// to have passed — but if one somehow did, the remaining rows are appended in matrix order
    /// rather than dropped, because a report that silently omits a component is worse than one
    /// with a debatable order.
    #[must_use]
    pub fn activation_order(&self) -> Vec<&str> {
        let mut ordered: Vec<&str> = Vec::with_capacity(self.components.len());
        let mut remaining: Vec<&Component> = self.components.iter().collect();
        while !remaining.is_empty() {
            let ready: Vec<&Component> = remaining
                .iter()
                .copied()
                .filter(|c| {
                    c.depends_on
                        .iter()
                        .all(|d| ordered.contains(&d.as_str()) || self.get(d).is_none())
                })
                .collect();
            if ready.is_empty() {
                ordered.extend(remaining.iter().map(|c| c.name.as_str()));
                break;
            }
            for component in ready {
                ordered.push(component.name.as_str());
            }
            remaining.retain(|c| !ordered.contains(&c.name.as_str()));
        }
        ordered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_matrix_is_valid() {
        let matrix = Matrix::parse(EMBEDDED).expect("distro/deploy/components.toml must parse");
        assert_eq!(matrix.version, 1);
        assert_eq!(
            matrix.components.len(),
            13,
            "the build set is thirteen packages"
        );
    }

    #[test]
    fn the_shipped_matrix_names_the_packages_the_archive_does() {
        let matrix = Matrix::embedded();
        for package in [
            "legeos-sessiond",
            "legeos-resourced",
            "legeos-filed",
            "legeos-hostd",
            "legeos-boot-health",
            "legeos-session",
            "legeos-comp",
            "legeos-greeter",
            "legeos-shell",
            "legeos-settings",
            "lege-files",
            "legeos-portal",
            "legeos-cli",
        ] {
            assert!(matrix.packages().contains(&package), "missing {package}");
        }
    }

    #[test]
    fn the_daemons_that_must_not_be_restarted_yet_say_so() {
        // The honesty this file exists for: restarting these disconnects every proxy the shell
        // holds, so they stay manual until the composed-session restart test exists.
        let matrix = Matrix::embedded();
        for name in ["sessiond", "filed", "portal"] {
            assert_eq!(
                matrix.get(name).unwrap().activation,
                Activation::Manual,
                "{name} must ship as manual"
            );
        }
        assert_eq!(
            matrix.get("comp").unwrap().activation,
            Activation::NeedsSession,
            "the compositor is Restart=no by design"
        );
    }

    #[test]
    fn providers_come_before_dependents() {
        let matrix = Matrix::embedded();
        let order = matrix.activation_order();
        assert_eq!(order.len(), matrix.components.len());
        for component in &matrix.components {
            let own = order.iter().position(|n| *n == component.name).unwrap();
            for dependency in &component.depends_on {
                let theirs = order.iter().position(|n| n == dependency).unwrap();
                assert!(theirs < own, "{dependency} must precede {}", component.name);
            }
        }
    }

    #[test]
    fn a_dependency_on_nothing_is_a_problem() {
        let text = r#"
version = 1
[[component]]
name = "a"
crate = "a"
package = "a"
scope = "none"
activation = "immediate"
depends_on = ["ghost"]
summary = "s"
"#;
        let error = Matrix::parse(text).unwrap_err();
        assert!(format!("{error}").contains("ghost"), "{error}");
    }

    #[test]
    fn a_unit_less_component_may_not_claim_a_scope() {
        let text = r#"
version = 1
[[component]]
name = "a"
crate = "a"
package = "a"
scope = "user"
activation = "manual"
summary = "s"
"#;
        let error = Matrix::parse(text).unwrap_err();
        assert!(format!("{error}").contains("names no unit"), "{error}");
    }

    #[test]
    fn two_components_may_not_share_a_package() {
        let text = r#"
version = 1
[[component]]
name = "a"
crate = "a"
package = "same"
scope = "none"
activation = "immediate"
summary = "s"
[[component]]
name = "b"
crate = "b"
package = "same"
scope = "none"
activation = "immediate"
summary = "s"
"#;
        let error = Matrix::parse(text).unwrap_err();
        assert!(format!("{error}").contains("\"same\""), "{error}");
    }
}
