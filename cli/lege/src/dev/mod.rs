//! `lege dev` — the developer's deployment tooling.
//!
//! Building LegeOS and running LegeOS are two different things, and the gap between them is where
//! this project has actually lost time: a partial reinstall left the shell four days newer than
//! sessiond, and every failure that followed read as an application bug. `lege dev status` is the
//! answer to "is what I built what is running?", asked of the machine rather than of memory.

pub mod converge;
pub mod report;

use anyhow::{Context, Result};
use converge::Observation;
use lege_build::components::Matrix;
use std::{collections::BTreeMap, path::Path};

/// Loads the component matrix.
///
/// The repository copy wins when there is one, so a developer editing `components.toml` sees the
/// edit without reinstalling; then the installed copy; then the copy compiled into this binary,
/// which is never absent and is what makes `lege dev status` work from anywhere.
#[must_use]
pub fn load_matrix(root: &Path) -> Matrix {
    for candidate in [
        std::env::var_os("LEGEOS_COMPONENTS").map(std::path::PathBuf::from),
        repository_matrix(),
        Some(root.join(lege_build::components::INSTALLED_PATH.trim_start_matches('/'))),
    ]
    .into_iter()
    .flatten()
    {
        if let Ok(matrix) = Matrix::read(&candidate) {
            return matrix;
        }
    }
    Matrix::embedded()
}

/// `distro/deploy/components.toml` if this process is being run from a source checkout.
fn repository_matrix() -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("distro/deploy/components.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// What the machine looks like: dpkg's view of what is installed, and the stamps of what is
/// running.
///
/// # Errors
/// Never — a missing dpkg status or an unreadable stamp directory is an empty observation, which
/// the report then describes accurately, rather than an error that hides the parts that did read.
pub fn observe(root: &Path, matrix: &Matrix) -> Observation {
    let status_path = root.join("var/lib/dpkg/status");
    let installed = std::fs::read_to_string(&status_path)
        .map(|text| installed_versions(&text, &matrix.packages()))
        .unwrap_or_default();
    Observation {
        installed,
        running: lege_build::read_stamps(),
    }
}

/// The installed version of each of `packages`, from a dpkg `status` file.
///
/// Only packages dpkg marks as actually installed count: a `deinstall`/`config-files` stanza
/// still carries a `Version:`, and treating that as installed would report a removed component as
/// a stale one.
#[must_use]
pub fn installed_versions(status_text: &str, packages: &[&str]) -> BTreeMap<String, String> {
    status_text
        .split("\n\n")
        .filter_map(|stanza| {
            let name = stanza
                .lines()
                .find_map(|line| line.strip_prefix("Package: "))?
                .trim();
            if !packages.contains(&name) {
                return None;
            }
            if !stanza
                .lines()
                .any(|line| line.starts_with("Status:") && line.contains("installed"))
            {
                return None;
            }
            let version = stanza
                .lines()
                .find_map(|line| line.strip_prefix("Version: "))?
                .trim();
            Some((name.to_owned(), version.to_owned()))
        })
        .collect()
}

/// `lege dev status`.
///
/// # Errors
/// If the convergence cannot be serialised for `--json`.
pub fn status(root: &Path, target: Option<&str>, json: bool) -> Result<String> {
    let matrix = load_matrix(root);
    let observation = observe(root, &matrix);
    let convergence = converge::converge(&matrix, &observation, target);
    if json {
        serde_json::to_string_pretty(&report::as_json(&convergence))
            .context("rendering the convergence as JSON")
    } else {
        Ok(report::render(&convergence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Package: legeos-shell\n\
        Status: install ok installed\n\
        Version: 0.1.0~dev2000+gaaaaaaa\n\
        \n\
        Package: legeos-filed\n\
        Status: deinstall ok config-files\n\
        Version: 0.1.0~dev1000+gbbbbbbb\n\
        \n\
        Package: unrelated\n\
        Status: install ok installed\n\
        Version: 9\n";

    #[test]
    fn only_installed_packages_from_the_matrix_are_read() {
        let versions = installed_versions(STATUS, &["legeos-shell", "legeos-filed"]);
        assert_eq!(versions.len(), 1);
        assert_eq!(versions["legeos-shell"], "0.1.0~dev2000+gaaaaaaa");
    }

    #[test]
    fn a_removed_package_is_not_an_installed_one() {
        // `deinstall ok config-files` still carries a Version:; reading it would report a
        // component that is gone as one that is merely out of date.
        assert!(!installed_versions(STATUS, &["legeos-filed"]).contains_key("legeos-filed"));
    }
}
