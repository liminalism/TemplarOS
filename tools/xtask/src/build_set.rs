//! `cargo xtask build-set` — build all thirteen packages as one identified set.
//!
//! A *build set* is the whole of TemplarOS built at one moment and labelled with one string, the
//! [`BuildSetId`](templar_build::build_set_id), which is also the Debian version of every package in
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
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command as Process,
    time::{SystemTime, UNIX_EPOCH},
};
use templar_build::{BuildSet, BuiltPackage, components::Matrix};

pub struct Options {
    /// Print the plan and exit without building.
    pub dry_run: bool,
    /// Print the id this invocation would use and exit.
    pub print_id: bool,
    /// Override the epoch — reproducible builds and tests.
    pub epoch: Option<u64>,
    /// D1's package-closure report, whose observation time and verdict travel with the set.
    pub closure_report: Option<PathBuf>,
}

/// The build-set id this checkout would produce right now, and the epoch inside it.
///
/// Shared with `cargo xtask archive changelog`, which must derive the changelog head entry from
/// *this* id and no other — one authority means one function.
///
/// # Errors
/// If the workspace version cannot be read from `Cargo.toml`.
pub fn current_id(root: &Path, epoch: Option<u64>) -> Result<(String, u64)> {
    let version = workspace_version(root)?;
    let git_sha =
        git(root, &["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "0000000".into());
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.trim().is_empty());
    let epoch = epoch.unwrap_or_else(now_secs);
    Ok((
        templar_build::build_set_id(&version, epoch, &git_sha, dirty),
        epoch,
    ))
}

pub fn run(root: &Path, options: &Options) -> Result<()> {
    let matrix = Matrix::read(&root.join("distro/deploy/components.toml"))
        .context("reading the component matrix")?;
    let git_sha =
        git(root, &["rev-parse", "--short=7", "HEAD"]).unwrap_or_else(|| "0000000".into());
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.trim().is_empty());
    let (id, epoch) = current_id(root, options.epoch)?;

    if options.print_id {
        println!("{id}");
        return Ok(());
    }

    let provenance = crate::archive::provenance(root, options.closure_report.as_deref());
    println!("build set {id}");
    println!(
        "  changelog head: {}",
        templar_build::changelog_entry(
            &id,
            crate::archive::DISTRIBUTION,
            crate::archive::MAINTAINER,
            epoch
        )
        .lines()
        .next()
        .unwrap_or_default()
    );
    if !provenance.rustc.is_empty() || !provenance.host.is_empty() {
        println!(
            "  provenance: {} on {} ({})",
            provenance.rustc, provenance.host, provenance.arch
        );
    }
    if let Some(resolution) = &provenance.ubuntu_index_resolution {
        println!(
            "  ubuntu index: {resolution}{}",
            provenance
                .ubuntu_index_observed_at
                .as_ref()
                .map_or_else(String::new, |at| format!(", observed {at}"))
        );
    }
    if dirty {
        println!(
            "  the working tree has uncommitted changes; the id says so, and so will every package"
        );
    }
    println!("  {} package(s):", matrix.components.len());
    for component in &matrix.components {
        println!(
            "    cargo deb -p {} --deb-version {id} --output target/debian",
            component.crate_name
        );
    }
    let metapackages = metapackage_dirs(root)?;
    println!("  {} metapackage(s):", metapackages.len());
    for dir in &metapackages {
        println!(
            "    dpkg-deb --build {} (from control.in)",
            dir.file_name().unwrap_or_default().to_string_lossy()
        );
    }
    if options.dry_run {
        println!("dry run: nothing was built");
        return Ok(());
    }

    let deb_dir = root.join("target/debian");
    fs::create_dir_all(&deb_dir).context("creating target/debian")?;
    let mut packages = build_packages(root, &deb_dir, &matrix, &id)?;
    packages.extend(build_metapackages(root, &deb_dir, &metapackages, &id)?);
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
            templar_build::StoreSchema {
                version: templar_state::schema_version(),
                min_reader_version: templar_state::min_reader_version(),
            },
        )]
        .into_iter()
        .collect(),
        provenance,
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
            .arg("--output")
            .arg(deb_dir)
            .current_dir(root)
            .env("TEMPLAROS_BUILD_SET_ID", id)
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

/// Every `distro/metapackages/*/` holding a rendered `control.in`, sorted.
fn metapackage_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let dir = root.join("distro/metapackages");
    let mut dirs: Vec<PathBuf> = fs::read_dir(&dir)
        .context("reading distro/metapackages")?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("control.in").is_file())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// The metapackages: each `control.in` (rendered by `cargo xtask seeds`) becomes one `.deb` at the
/// set's version, built with `dpkg-deb`. They carry no files except the archive pins, so there is
/// no debhelper run here: `${misc:Depends}` is dropped and the `@…@` templates are filled from
/// `distro/base.toml`, which is the one place the resolved kernel and driver versions live.
fn build_metapackages(
    root: &Path,
    deb_dir: &Path,
    dirs: &[PathBuf],
    id: &str,
) -> Result<Vec<BuiltPackage>> {
    let base =
        fs::read_to_string(root.join("distro/base.toml")).context("reading distro/base.toml")?;
    let substitutions = [
        ("@KERNEL_MIN@", base_value(&base, "min_version")?),
        ("@NVIDIA_BRANCH@", base_value(&base, "branch")?),
        ("@NVIDIA_LRM_VERSION@", base_value(&base, "lrm_version")?),
        (
            "@NVIDIA_USERSPACE_VERSION@",
            base_value(&base, "userspace_version")?,
        ),
    ];
    let mut packages = Vec::new();
    for dir in dirs {
        let template = fs::read_to_string(dir.join("control.in"))
            .with_context(|| format!("reading {}", dir.join("control.in").display()))?;
        let control = render_metapackage_control(&template, id, &substitutions)?;
        let package = control_field(&control, "Package")
            .with_context(|| format!("{} declares no Package", dir.display()))?;
        let arch = control_field(&control, "Architecture").unwrap_or_else(|| "all".to_owned());
        println!("== {package} (metapackage)");

        // Staged under the system temp dir, not `target/`: a checkout on NTFS (this project's own
        // development host) reports 0777 for everything, which dpkg-deb refuses.
        let stage = std::env::temp_dir()
            .join("templaros-metapackages")
            .join(&package);
        let _ = fs::remove_dir_all(&stage);
        fs::create_dir_all(stage.join("DEBIAN"))
            .context("creating the metapackage staging tree")?;
        // dpkg-deb refuses a control directory outside 0755..0775, whatever the umask made it.
        for dir in [&stage, &stage.join("DEBIAN")] {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
        }
        fs::write(stage.join("DEBIAN/control"), &control)?;
        if package == "templaros-archive-keyring" {
            // The archive pins ship with the keyring package; the key itself and the source stanza
            // are written by the provisioner from the archive it installs from.
            let pins = stage.join("etc/apt/preferences.d");
            fs::create_dir_all(&pins)?;
            for dir in [stage.join("etc"), stage.join("etc/apt"), pins.clone()] {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
            }
            for entry in fs::read_dir(root.join("distro/apt/preferences.d"))
                .context("reading distro/apt/preferences.d")?
            {
                let entry = entry?;
                let pin = pins.join(entry.file_name());
                fs::copy(entry.path(), &pin)?;
                fs::set_permissions(&pin, fs::Permissions::from_mode(0o644))?;
            }
        }
        let file = format!("{package}_{id}_{arch}.deb");
        let status = Process::new("dpkg-deb")
            .args(["--root-owner-group", "--build"])
            .arg(&stage)
            .arg(deb_dir.join(&file))
            .status()
            .with_context(|| format!("running dpkg-deb --build for {package}"))?;
        if !status.success() {
            bail!("dpkg-deb --build {package} failed; nothing was published");
        }
        let size_bytes = fs::metadata(deb_dir.join(&file))
            .map(|m| m.len())
            .unwrap_or_default();
        packages.push(BuiltPackage {
            component: package.clone(),
            package,
            crate_name: String::new(),
            version: id.to_owned(),
            file,
            size_bytes,
        });
    }
    Ok(packages)
}

/// One `key = "value"` from `distro/base.toml`, by key alone: the four keys the templates need are
/// unique in that file, and this refuses if that ever stops being true.
fn base_value(base: &str, key: &str) -> Result<String> {
    let mut values = base.lines().filter_map(|line| {
        let (name, rest) = line.split_once('=')?;
        (name.trim() == key).then(|| {
            rest.split('#')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches('"')
                .to_owned()
        })
    });
    let value = values
        .next()
        .with_context(|| format!("distro/base.toml declares no `{key}`"))?;
    if values.next().is_some() {
        bail!("distro/base.toml declares `{key}` more than once");
    }
    if value.is_empty() {
        bail!("distro/base.toml leaves `{key}` empty; run the closure and fill it in");
    }
    Ok(value)
}

/// A `control.in` as `DEBIAN/control`: comment lines gone, `${misc:Depends}` gone (nothing here is
/// built by debhelper), templates substituted, and the fields dpkg-deb insists on added.
fn render_metapackage_control(
    template: &str,
    version: &str,
    substitutions: &[(&str, String)],
) -> Result<String> {
    let mut out = String::new();
    for line in template.lines().filter(|line| !line.starts_with('#')) {
        let mut line = line
            .replace(", ${misc:Depends}", "")
            .replace("${misc:Depends}", "");
        for (from, to) in substitutions {
            line = line.replace(from, to);
        }
        // `${misc:Depends}` may have been the whole of a folded continuation line, or the whole
        // field: dropping it must not leave a blank continuation or a trailing comma behind.
        if line.trim() == "Depends:" || line.trim().is_empty() {
            while out.ends_with(",\n") || out.ends_with(", \n") {
                out.truncate(out.trim_end().trim_end_matches(',').len());
                out.push('\n');
            }
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
    if let Some(at) = out.find('@') {
        bail!(
            "an unsubstituted template remains in the metapackage control: {}",
            out[at..].lines().next().unwrap_or_default()
        );
    }
    let package = control_field(&out, "Package").context("control.in declares no Package")?;
    let head = format!("Package: {package}\n");
    let rest = out
        .strip_prefix(&head)
        .with_context(|| format!("{package}: `Package:` must be the first field"))?;
    Ok(format!(
        "{head}Version: {version}\nSection: metapackages\nPriority: optional\nMaintainer: {}\n{rest}",
        crate::archive::MAINTAINER
    ))
}

fn control_field(control: &str, field: &str) -> Option<String> {
    control
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}:")))
        .map(|value| value.trim().to_owned())
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
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map_or_else(|| root.join("target"), |path| root.join(path));
        let binary = target.join("release").join(&component.crate_name);
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
             output was cached: {}. Run `cargo clean -p templar-build` and build again.",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn metapackage_control_is_rendered_for_dpkg_deb() {
        let template = "# GENERATED\nPackage: templaros-kernel\nArchitecture: amd64\nDepends: linux-generic (>= @KERNEL_MIN@), ${misc:Depends}\nDescription: k\n body\n";
        let control =
            render_metapackage_control(template, "0.1.0~dev1", &[("@KERNEL_MIN@", "7.0".into())])
                .unwrap();
        assert!(control.starts_with("Package: templaros-kernel\nVersion: 0.1.0~dev1\n"));
        assert!(control.contains("Depends: linux-generic (>= 7.0)\n"));
        assert!(!control.contains("misc:Depends") && !control.contains('#'));
        let folded = "Package: p\nDepends: a, b,\n ${misc:Depends}\nDescription: d\n";
        let control = render_metapackage_control(folded, "1", &[]).unwrap();
        assert!(
            control.contains("Depends: a, b\nDescription: d\n"),
            "{control}"
        );
        assert!(
            render_metapackage_control(template, "1", &[]).is_err(),
            "unsubstituted @…@"
        );
    }

    #[test]
    fn every_rendered_metapackage_builds_with_dpkg_deb() {
        if Process::new("dpkg-deb").arg("--version").output().is_err() {
            eprintln!("NOT EXECUTED: dpkg-deb is not installed");
            return;
        }
        let root = repo();
        let out = root.join("target/metapackages/test-debs");
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out).unwrap();
        let dirs = metapackage_dirs(&root).unwrap();
        assert_eq!(dirs.len(), 8, "eight metapackages are declared");
        let built = build_metapackages(&root, &out, &dirs, "0.0.0~test").unwrap();
        assert_eq!(built.len(), 8);
        for package in &built {
            assert!(out.join(&package.file).is_file(), "{}", package.file);
            assert_eq!(package.version, "0.0.0~test");
        }
        assert!(
            built
                .iter()
                .any(|p| p.file == "templaros-kernel_0.0.0~test_amd64.deb")
        );
        assert!(
            built
                .iter()
                .any(|p| p.file == "templaros-desktop_0.0.0~test_all.deb")
        );
    }
}
