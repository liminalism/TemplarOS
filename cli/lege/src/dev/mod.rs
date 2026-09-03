//! `lege dev` — the developer's deployment tooling.
//!
//! Building LegeOS and running LegeOS are two different things, and the gap between them is where
//! this project has actually lost time: a partial reinstall left the shell four days newer than
//! sessiond, and every failure that followed read as an application bug. `lege dev status` is the
//! answer to "is what I built what is running?", asked of the machine rather than of memory.

pub mod activate;
pub mod cmd;
pub mod converge;
pub mod gates;
pub mod install;
pub mod repo;
pub mod report;
pub mod systemd;

use anyhow::{Context, Result, bail};
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
    let published = repo.publish_with(
        &packages,
        &set.id,
        &repo::Compatibility {
            interfaces: set.interfaces.clone(),
            schemas: set.schemas.clone(),
        },
    )?;
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

/// `lege dev install` — put a published build set on this machine.
///
/// `build_set` defaults to the archive's current set, which is what `lege dev repo publish` just
/// wrote, so the ordinary sequence is `cargo xtask build-set` → publish → install.
///
/// # Errors
/// If the archive holds no such set, or apt refuses it, or the machine is not at that set
/// afterwards.
pub fn install_set(
    repo_root: PathBuf,
    root: &Path,
    build_set: Option<&str>,
    dry_run: bool,
) -> Result<String> {
    let installer = open_installer(repo_root, root);
    let target = match build_set {
        Some(set) => set.to_owned(),
        None => installer
            .repo
            .current()?
            .context("the archive holds no set; run `lege dev repo publish` first")?,
    };
    run_install(&installer, &target, dry_run)
}

/// `lege dev rollback` — put the previous set back.
///
/// The same code path as an install, with the previous set instead of the current one. That is
/// deliberate: a rollback that took a different path from an install would be the one path never
/// exercised until the moment it has to work.
///
/// # Errors
/// If there is no previous set, or the install fails.
pub fn rollback(repo_root: PathBuf, root: &Path, dry_run: bool) -> Result<String> {
    let installer = open_installer(repo_root, root);
    let previous = installer.repo.previous()?.context(
        "there is no previous set to roll back to; this archive has only ever published one",
    )?;
    run_install(&installer, &previous, dry_run)
}

fn open_installer(repo_root: PathBuf, root: &Path) -> install::Installer {
    let runner = Arc::new(cmd::RealCommandRunner::default());
    install::Installer::new(
        repo::Repo::new(repo_root, runner.clone()),
        runner,
        root.to_path_buf(),
    )
}

fn run_install(installer: &install::Installer, target: &str, dry_run: bool) -> Result<String> {
    let plan = installer.plan(target)?;
    let mut out = format!(
        "build set {target}
"
    );
    let _ = writeln!(out, "  {}", plan.command_line());
    if dry_run {
        out.push_str(
            "
dry run: nothing was installed
",
        );
        return Ok(out);
    }
    if !install::is_root(&installer.root) {
        bail!("installing a build set changes package state; run this as root");
    }
    installer.install(&plan)?;
    let _ = writeln!(
        out,
        "
installed {} package(s)",
        plan.pinned.len()
    );
    if let Some(previous) = &plan.rollback_to {
        let _ = writeln!(out, "  previous set retained: {previous}");
        let _ = writeln!(out, "  roll back with: sudo lege dev rollback");
    }
    out.push_str("  `lege dev status` says what still needs restarting\n");
    Ok(out)
}

/// What a deployment run should do.
#[derive(Clone, Debug)]
pub struct DeployOptions {
    /// The manifest `cargo xtask build-set` wrote.
    pub build_set: PathBuf,
    /// Print the whole pipeline and change nothing.
    pub dry_run: bool,
    /// On a failed health gate, put the previous set back without asking.
    pub auto_rollback: bool,
    /// Copy the state stores before installing a set that could not otherwise be rolled back.
    pub snapshot_state: bool,
}

/// `lege dev deploy` — publish, install, activate, report.
///
/// Deliberately **not** the build step. `cargo xtask build-set` is run by the developer as
/// themselves; a deployment needs root for apt, and building as root leaves a target directory
/// nobody can write to afterwards and a cargo cache in the wrong home. So this starts from the
/// manifest that build produced, and says so when it is missing.
///
/// # Errors
/// If the set was never built, apt refuses it, or a component fails its health gate — in which
/// case the rollback command is printed, or run for you with `--auto-rollback`.
pub fn deploy(repo_root: PathBuf, root: &Path, options: &DeployOptions) -> Result<String> {
    let (set, _) = lege_build::BuildSet::read(&options.build_set).with_context(|| {
        format!(
            "reading {} — run `cargo xtask build-set` first",
            options.build_set.display()
        )
    })?;
    let matrix = load_matrix(root);
    let mut out = format!("deploying build set {}\n", set.id);

    if options.dry_run {
        return Ok(deploy_plan(
            &repo_root,
            root,
            &set,
            &matrix,
            options.snapshot_state,
            out,
        ));
    }

    if !install::is_root(root) {
        bail!("deploying changes package state and restarts units; run this as root");
    }

    // The gates run before the install, not after: refusing a set that could not be rolled back
    // is only useful while it is still true that nothing has changed.
    let deployed = gates::deployed_set(&open_repo(repo_root.clone()))?;
    if options.snapshot_state {
        for path in gates::snapshot_stores(root, &set.id)? {
            let _ = writeln!(out, "snapshot: {}", path.display());
        }
    }
    let gate = gates::run(
        &matrix,
        &set,
        deployed.as_ref(),
        gates::hostd_busy(root),
        options.snapshot_state,
    )?;
    for note in &gate.notes {
        let _ = writeln!(out, "  {note}");
    }

    let published = repo_publish(repo_root.clone(), &options.build_set)?;
    out.push_str(&published);

    let installer = open_installer(repo_root.clone(), root);
    let install_plan = installer.plan(&set.id)?;
    installer.install(&install_plan)?;
    let _ = writeln!(out, "installed {} package(s)", install_plan.pinned.len());

    let convergence = converge::converge(&matrix, &observe(root, &matrix), Some(&set.id));
    let plan = activate::plan(&matrix, &convergence, &gate.conditions);
    let runner: Arc<dyn cmd::CommandRunner> = Arc::new(cmd::RealCommandRunner::default());
    let target_user = systemd::sudo_target_user();
    let systemctl = systemd::RealSystemctl {
        runner: runner.clone(),
        dir: root.to_path_buf(),
        target_user: target_user.clone(),
    };
    let bus = systemd::RealBusProbe {
        runner,
        dir: root.to_path_buf(),
        target_user: target_user.clone(),
    };
    let stamps = systemd::RealStamps { target_user };
    let activation = activate::execute(
        &plan,
        &matrix,
        &activate::Effects {
            systemctl: &systemctl,
            bus: &bus,
            clock: &systemd::RealClock,
            stamps: &stamps,
        },
    );
    out.push_str(&report::render_activation(&activation));

    let failures = activation.failures();
    if !failures.is_empty() {
        // Every failure, on its own line. The rest of the list was attempted anyway, so more than
        // one of them can be broken, and a summary that named only the first would quietly hide
        // the others behind a fix that does not apply to them.
        out.push('\n');
        for (component, reason) in &failures {
            let _ = writeln!(out, "{component} did not come back: {reason}");
        }
        let names: Vec<&str> = failures.iter().map(|(component, _)| *component).collect();
        let names = names.join(", ");
        if options.auto_rollback {
            let previous = rollback(repo_root, root, false)?;
            out.push_str(&previous);
            // A one-line-prefixed error swallowing a long multi-line report reads exactly like a
            // success dump unless it says so in the very first word: this is a failed deployment
            // start to finish, not a log of steps that merely happened to end early.
            bail!("DEPLOY FAILED — {names} did not activate; the set was rolled back\n\n{out}");
        }
        let _ = writeln!(
            out,
            "\neverything else was activated; roll the whole set back with: sudo lege dev rollback"
        );
        bail!(
            "DEPLOY FAILED — {names} did not activate; roll back with: sudo lege dev rollback\n\n{out}"
        );
    }

    let final_convergence = converge::converge(&matrix, &observe(root, &matrix), Some(&set.id));
    out.push('\n');
    out.push_str(&report::render(&final_convergence));
    out.push_str("\ndeploy succeeded\n");
    if let Some(previous) = open_repo(repo_root).previous()? {
        let _ = writeln!(out, "\nprevious set retained: {previous}");
        out.push_str("roll back with: sudo lege dev rollback\n");
    }
    Ok(out)
}

/// `--dry-run`: the whole pipeline, stated, with nothing changed.
fn deploy_plan(
    repo_root: &Path,
    root: &Path,
    set: &lege_build::BuildSet,
    matrix: &lege_build::components::Matrix,
    snapshot_state: bool,
    mut out: String,
) -> String {
    let installer = open_installer(repo_root.to_path_buf(), root);
    let _ = writeln!(out, "\n1. publish {} package(s)", set.packages.len());
    match installer.plan(&set.id) {
        Ok(plan) => {
            let _ = writeln!(out, "2. {}", plan.command_line());
        }
        Err(_) => {
            let _ = writeln!(
                out,
                "2. apt-get install … (the set is not in the archive yet)"
            );
        }
    }
    // Against the machine *as it would be after the install*, not as it is now: before the
    // install every package reads as not installed, and an activation plan of thirteen
    // "the set is incomplete" lines answers a question nobody asked.
    let mut observation = observe(root, matrix);
    for package in &set.packages {
        observation
            .installed
            .insert(package.package.clone(), set.id.clone());
    }
    let convergence = converge::converge(matrix, &observation, Some(&set.id));
    let repo = open_repo(repo_root.to_path_buf());
    let deployed = gates::deployed_set(&repo).unwrap_or(None);
    let gate = gates::run(
        matrix,
        set,
        deployed.as_ref(),
        gates::hostd_busy(root),
        snapshot_state,
    );
    match &gate {
        Ok(report) => {
            for note in &report.notes {
                let _ = writeln!(out, "   {note}");
            }
        }
        Err(error) => {
            let _ = writeln!(out, "   refused: {error}");
        }
    }
    let conditions = gate.map(|report| report.conditions).unwrap_or_default();
    let plan = activate::plan(matrix, &convergence, &conditions);
    out.push_str("3. activate:\n");
    out.push_str(&report::render_activation_plan(&plan));
    out.push_str("\ndry run: nothing was published, installed or restarted\n");
    out
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
