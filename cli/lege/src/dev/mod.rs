//! `lege dev` — the developer's deployment tooling.
//!
//! Building LegeOS and running LegeOS are two different things, and the gap between them is where
//! this project has actually lost time: a partial reinstall left the shell four days newer than
//! sessiond, and every failure that followed read as an application bug. `lege dev status` is the
//! answer to "is what I built what is running?", asked of the machine rather than of memory.

pub mod cmd;
pub mod converge;
pub mod repo;
pub mod report;

use anyhow::{Context, Result};
use converge::Observation;
use lege_build::components::Matrix;
use std::{collections::BTreeMap, fmt::Write as _, path::Path, path::PathBuf, sync::Arc};

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

/// Opens the local archive with a real command runner.
#[must_use]
pub fn open_repo(root: PathBuf) -> repo::Repo {
    repo::Repo::new(root, Arc::new(cmd::RealCommandRunner::default()))
}

/// `lege dev repo init`.
///
/// # Errors
/// If the archive cannot be created or its key generated; and, with `install_source`, if `/etc`
/// cannot be written — which means "run this step as root", and says so.
pub fn repo_init(root: PathBuf, sysroot: &Path, install_source: bool) -> Result<String> {
    let repo = open_repo(root);
    let mut out = repo.init()?;
    out.push('\n');
    if install_source {
        let written = repo.install_source(sysroot)?;
        for path in written {
            let _ = writeln!(out, "  wrote {}", path.display());
        }
    } else {
        let _ = writeln!(
            out,
            "  apt is not yet pointed at it; run `sudo lege dev repo init --install-source`"
        );
    }
    Ok(out)
}

/// `lege dev repo publish`: takes a `build-set.json` and puts that whole set in the archive.
///
/// # Errors
/// If the manifest cannot be read, a `.deb` it names is missing, or an archive tool fails.
pub fn repo_publish(root: PathBuf, manifest_path: &Path) -> Result<String> {
    let (set, dir) = lege_build::BuildSet::read(manifest_path).with_context(|| {
        format!(
            "reading {} — run `cargo xtask build-set` first",
            manifest_path.display()
        )
    })?;
    let packages: Vec<(String, String, PathBuf)> = set
        .packages
        .iter()
        .map(|package| {
            (
                package.package.clone(),
                package.version.clone(),
                dir.join(&package.file),
            )
        })
        .collect();
    let repo = open_repo(root);
    let published = repo.publish(&packages, &set.id)?;
    let mut out = format!(
        "published {} package(s) as build set {}\n",
        published.packages.len(),
        published.build_set
    );
    if let Some(previous) = repo.previous()? {
        let _ = writeln!(out, "  previous set retained: {previous}");
        let _ = writeln!(out, "  roll back with: lege dev rollback");
    }
    Ok(out)
}

/// `lege dev repo status`.
///
/// # Errors
/// If the archive cannot be read.
pub fn repo_status(root: PathBuf) -> Result<String> {
    let repo = open_repo(root);
    let mut out = format!("{}\n", repo.root.display());
    let _ = writeln!(
        out,
        "  current   {}",
        repo.current()?.unwrap_or_else(|| "(none)".to_owned())
    );
    let _ = writeln!(
        out,
        "  previous  {}",
        repo.previous()?.unwrap_or_else(|| "(none)".to_owned())
    );
    let sets = repo.published_sets()?;
    let _ = writeln!(out, "  retained  {} set(s)", sets.len());
    for set in &sets {
        let _ = writeln!(out, "    {set}");
    }
    Ok(out)
}

/// `lege dev repo prune`.
///
/// # Errors
/// If the archive cannot be read or a file cannot be removed.
pub fn repo_prune(root: PathBuf, keep: usize) -> Result<String> {
    let dropped = open_repo(root).prune(keep)?;
    Ok(if dropped.is_empty() {
        format!("nothing to prune; keeping {keep} set(s)\n")
    } else {
        format!(
            "removed {} old set(s): {}\n",
            dropped.len(),
            dropped.join(", ")
        )
    })
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
