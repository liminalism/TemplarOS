//! `cargo xtask build-set` — build all thirteen packages as one identified set.
//!
//! A *build set* is the whole of LegeOS built at one moment and labelled with one string, the
//! [`BuildSetId`](lege_build::build_set_id), which is also the Debian version of every package in
//! it. That single string is what makes the rest of the deployment tooling possible: coherence is
//! a string comparison, and rollback is `apt-get install --allow-downgrades pkg=<previous>`.
//!
//! Two failures this command exists to prevent:
//!
//! * **A mixed set.** Building the packages one at a time — which is what the justfile used to do
//!   and what the reinstall script still allowed — produces an install where the shell is four
//!   days newer than sessiond. That happened on this project's own development host, and every
//!   symptom looked like an application bug. Here, either all thirteen packages carry the same
//!   version or the command fails and publishes nothing.
//! * **A stale build script.** Cargo caches build-script output, so a binary can keep asserting a
//!   build id from a week ago. After the build, every binary that writes a runtime stamp is
//!   checked for the id it was supposed to have been given.

use anyhow::{Context, Result, bail};
use lege_build::{BuildSet, BuiltPackage, components::Matrix};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command as Process,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Options {
    /// Print the plan and exit without building.
    pub dry_run: bool,
    /// Print the id this invocation would use and exit.
    pub print_id: bool,
    /// Override the epoch — reproducible builds and tests.
    pub epoch: Option<u64>,
}

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let matrix = Matrix::read(&root.join("distro/deploy/components.toml"))
        .context("reading the component matrix")?;
    let version = workspace_version(root)?;
    let git_sha =
        git(root, &["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "0000000".into());
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.trim().is_empty());
    let epoch = options.epoch.unwrap_or_else(now_secs);
    let id = lege_build::build_set_id(&version, epoch, &git_sha, dirty);

    if options.print_id {
        println!("{id}");
        return Ok(());
    }

    println!("build set {id}");
    if dirty {
        println!(
            "  the working tree has uncommitted changes; the id says so, and so will every package"
        );
    }
    println!("  {} package(s):", matrix.components.len());
    for component in &matrix.components {
        println!(
            "    cargo deb -p {} --deb-version {id}",
            component.crate_name
        );
    }
    if options.dry_run {
        println!("dry run: nothing was built");
        return Ok(());
    }

    let deb_dir = root.join("target/debian");
    fs::create_dir_all(&deb_dir).context("creating target/debian")?;
    let packages = build_packages(root, &deb_dir, &matrix, &id)?;
    reject_mixed_set(&matrix, &packages, &id)?;
    reject_stale_build_scripts(root, &matrix, &id)?;

    // The compatibility facts travel with the set: at deployment time there is no source
    // checkout to consult, and the gate that decides whether these daemons can be restarted
    // under the clients the previous set left running has to be able to answer without one.
    let set = BuildSet {
        id: id.clone(),
        git_sha,
        dirty,
        epoch,
        packages,
        interfaces: crate::signatures::parse_tree(root)?,
        schemas: [(
            "session".to_owned(),
            lege_build::StoreSchema {
                version: lege_state::schema_version(),
                min_reader_version: lege_state::min_reader_version(),
            },
        )]
        .into_iter()
        .collect(),
    };
    let manifest = deb_dir.join("build-set.json");
    fs::write(&manifest, serde_json::to_vec_pretty(&set)?).context("writing build-set.json")?;

    println!(
        "\n{} package(s) at version {id}\n{}",
        set.packages.len(),
        manifest.display()
    );
    Ok(())
}

/// `cargo deb` for every component, in matrix order.
fn build_packages(
    root: &Path,
    deb_dir: &Path,
    matrix: &Matrix,
    id: &str,
) -> Result<Vec<BuiltPackage>> {
    let mut packages = Vec::new();
    for component in &matrix.components {
        println!("== {} ({})", component.package, component.crate_name);
        let status = Process::new("cargo")
            .args(["deb", "-p", &component.crate_name, "--deb-version", id])
            .current_dir(root)
            .env("LEGEOS_BUILD_SET_ID", id)
            .status()
            .with_context(|| format!("running cargo deb -p {}", component.crate_name))?;
        if !status.success() {
            bail!(
                "cargo deb -p {} failed; nothing was published",
                component.crate_name
            );
        }
        let file = newest_deb(deb_dir, &component.package, id).with_context(|| {
            format!(
                "cargo deb -p {} reported success but produced no {}_{id}_*.deb",
                component.crate_name, component.package
            )
        })?;
        let size_bytes = fs::metadata(&file).map(|m| m.len()).unwrap_or_default();
        packages.push(BuiltPackage {
            component: component.name.clone(),
            package: component.package.clone(),
            crate_name: component.crate_name.clone(),
            version: id.to_owned(),
            file: file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            size_bytes,
        });
    }
    Ok(packages)
}

/// Stated as an assertion rather than trusted: every package that should be in the set is, and
/// every one of them carries the same version.
fn reject_mixed_set(matrix: &Matrix, packages: &[BuiltPackage], id: &str) -> Result<()> {
    let missing: Vec<&str> = matrix
        .components
        .iter()
        .filter(|c| !packages.iter().any(|p| p.package == c.package))
        .map(|c| c.package.as_str())
        .collect();
    if !missing.is_empty() {
        bail!(
            "the set is incomplete; no package for: {}",
            missing.join(", ")
        );
    }
    if let Some(odd) = packages.iter().find(|p| p.version != id) {
        bail!(
            "mixed set: {} is version {} but the set is {id}",
            odd.package,
            odd.version
        );
    }
    Ok(())
}

/// A component that writes a runtime stamp and does not contain the id it was built with would
/// report a build that is not the one running — which is precisely the question this whole
/// machinery exists to answer, so it is checked rather than assumed.
fn reject_stale_build_scripts(root: &Path, matrix: &Matrix, id: &str) -> Result<()> {
    let mut stale = Vec::new();
    for component in &matrix.components {
        if component.stamp.is_none() {
            continue;
        }
        let binary = root.join("target/release").join(&component.crate_name);
        if !binary.is_file() {
            continue;
        }
        if !file_contains(&binary, id.as_bytes())? {
            stale.push(component.crate_name.clone());
        }
    }
    if stale.is_empty() {
        Ok(())
    } else {
        bail!(
            "these binaries do not carry the build id they were built with, so their build script \
             output was cached: {}. Run `cargo clean -p lege-build` and build again.",
            stale.join(", ")
        )
    }
}

/// The newest `<package>_<version>_<arch>.deb` in `dir`, so a directory full of earlier sets does
/// not confuse the answer.
fn newest_deb(dir: &Path, package: &str, version: &str) -> Option<PathBuf> {
    // Debian file names escape the version's `~` and `+` as-is but replace `:` — none of which
    // appear in a build-set id — so a literal prefix match is exact.
    let prefix = format!("{package}_{version}_");
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) || entry.path().extension().is_none_or(|e| e != "deb") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if best.as_ref().is_none_or(|(time, _)| modified >= *time) {
            best = Some((modified, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

fn file_contains(path: &Path, needle: &[u8]) -> Result<bool> {
    let body = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(body.windows(needle.len()).any(|window| window == needle))
}

fn workspace_version(root: &Path) -> Result<String> {
    let text = fs::read_to_string(root.join("Cargo.toml")).context("reading Cargo.toml")?;
    text.lines()
        .skip_while(|line| line.trim() != "[workspace.package]")
        .find_map(|line| line.trim().strip_prefix("version = "))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .context("no [workspace.package] version in Cargo.toml")
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Process::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
