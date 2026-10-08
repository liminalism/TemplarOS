//! Installing a build set, and putting the previous one back.
//!
//! Native TemplarOS sends the complete pinned set and signed public archive to hostd.
//! Installation happens in a separate candidate root; success here means queued, not
//! installed, promoted, or running. Status/reboot/boot-health finish the transition.
//!
//! The explicit legacy overlay lane still invokes one apt operation with pinned versions.
//! Apt may partially configure a failing set there; snapshot/rollback policy belongs to
//! that lane and is not an atomic-install guarantee.

use super::{cmd::CommandRunner, repo::Repo};
use anyhow::{Context, Result, bail};
use std::{path::Path, sync::Arc};

/// What an install would do, or did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallPlan {
    pub build_set: String,
    /// `package=version` for every package in the set.
    pub pinned: Vec<String>,
    /// The set that would remain available to roll back to.
    pub rollback_to: Option<String>,
}

impl InstallPlan {
    /// The `apt-get` invocation, as a printable line.
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut parts = vec!["apt-get".to_owned()];
        parts.extend(apt_install_args().iter().map(ToString::to_string));
        parts.extend(self.pinned.clone());
        parts.join(" ")
    }
}

/// The fixed part of the install invocation.
///
/// `--allow-downgrades` because a rollback is an install of older versions.
/// `--no-install-recommends` because a deployment must install the set and nothing else; a
/// recommends pulled in halfway through a rollback is a change nobody asked for at the moment
/// they least want one.
fn apt_install_args() -> Vec<&'static str> {
    vec![
        "install",
        "--yes",
        "--allow-downgrades",
        "--no-install-recommends",
    ]
}

/// Installs whole build sets from a local archive.
#[derive(Debug)]
pub struct Installer {
    pub repo: Repo,
    pub runner: Arc<dyn CommandRunner>,
    /// Filesystem root, so the post-install verification can read a fixture's dpkg status.
    pub root: std::path::PathBuf,
}

impl Installer {
    #[must_use]
    pub fn new(repo: Repo, runner: Arc<dyn CommandRunner>, root: std::path::PathBuf) -> Self {
        Self { repo, runner, root }
    }

    /// What installing `build_set` would do.
    ///
    /// Only the component matrix's packages are pinned. A set also carries the archive's
    /// metapackages — `templaros-desktop`, `templaros-kernel`, `templaros-dev-*` and the two NVIDIA rows —
    /// and those describe an *image*, not a development host: their dependencies resolve inside a
    /// TemplarOS rootfs and nowhere else, and `templaros-nvidia-open` conflicts with
    /// `templaros-nvidia-proprietary` by design, so a plan that pinned the whole set would be
    /// unsatisfiable on any machine. They stay in the archive, which is where the image build
    /// resolves against them. This is also what [`Installer::verify`] already checks afterwards.
    ///
    /// # Errors
    /// If the set was never published to this archive.
    pub fn plan(&self, build_set: &str) -> Result<InstallPlan> {
        let manifest = self.repo.manifest(build_set)?;
        let matrix = super::load_matrix(&self.root);
        let components = matrix.packages();
        let rollback_to = self
            .repo
            .current()?
            .filter(|current| current != build_set)
            .or(self.repo.previous()?);
        Ok(InstallPlan {
            build_set: build_set.to_owned(),
            pinned: manifest
                .packages
                .iter()
                .filter(|package| components.contains(&package.package.as_str()))
                .map(|package| format!("{}={}", package.package, package.version))
                .collect(),
            rollback_to,
        })
    }
    /// Native updates are queued in hostd; no running-root apt or unit restart occurs.
    pub fn queue_candidate(&self, plan: &InstallPlan) -> Result<String> {
        if !is_native_root(&self.root)? {
            bail!("candidate queue requires native TemplarOS");
        }
        let capability = self.runner.run(
            &self.root,
            "busctl",
            &[
                "--system",
                "--timeout=30",
                "introspect",
                "org.templaros.Host",
                "/org/templaros/Host",
                "org.templaros.Host.Accounts1",
            ],
        )?;
        if !capability.status.success()
            || !String::from_utf8_lossy(&capability.stdout).contains("ChangeSelfPassword")
        {
            bail!(
                "running hostd lacks the owned candidate/account contract; migrate the legacy installation offline before native deployment"
            );
        }
        let changes = serde_json::json!({
            "kernel": null, "nvidia_modules": null, "nvidia_userspace": null,
            "firmware": [], "other_packages": plan.pinned,
            "archive": self.repo.root.canonicalize()?.display().to_string(),
        })
        .to_string();
        let output = self
            .runner
            .run(
                &self.root,
                "busctl",
                &[
                    "--system",
                    "--timeout=30",
                    "--json=short",
                    "call",
                    "org.templaros.Host",
                    "/org/templaros/Host",
                    "org.templaros.Host.Updates1",
                    "Plan",
                    "s",
                    &changes,
                ],
            )
            .context("queuing the native hostd candidate")?;
        if !output.status.success() {
            bail!(
                "hostd refused the candidate: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let reply: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("decoding hostd Plan reply")?;
        let id = reply["data"][0]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("hostd returned no transaction id")?;
        Ok(id.to_owned())
    }

    /// Installs a published build set, then checks that it actually took.
    ///
    /// # Errors
    /// If the set is unknown, apt refuses it, or the machine afterwards is not at that set. The
    /// last of those matters: apt can exit zero having resolved its way to something other than
    /// what was asked for, and a deployment that trusts the exit code would then activate
    /// components against packages that are not there.
    pub fn install(&self, plan: &InstallPlan) -> Result<()> {
        if is_native_root(&self.root)? {
            self.queue_candidate(plan)?;
            return Ok(());
        }
        self.refresh_index()?;

        let mut args: Vec<&str> = apt_install_args();
        let pinned: Vec<&str> = plan.pinned.iter().map(String::as_str).collect();
        args.extend(pinned);
        let output = self
            .runner
            .run(&self.root, "apt-get", &args)
            .context("running apt-get install")?;
        if !output.status.success() {
            // apt's own diagnosis of *which* packages it would not authenticate — the detail
            // that actually explains a refusal like this — goes to stdout, not stderr; the final
            // "E: ..." line on stderr alone never says why.
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "apt-get refused the set; nothing was changed:\n{}{}{}",
                stdout.trim(),
                if stdout.trim().is_empty() { "" } else { "\n" },
                stderr.trim()
            );
        }
        self.verify(&plan.build_set)
    }

    /// Refreshes apt's index for this archive only.
    ///
    /// A plain `apt-get update` would go to the network and refresh every source on the machine,
    /// which is a slow and unrelated thing to do in the middle of a deployment. Pointing apt at
    /// the archive's own copy of its source file updates exactly what is about to be installed.
    ///
    /// `--allow-releaseinfo-change`: every publish regenerates this archive's `Release` from
    /// scratch (origin/label/suite are recomputed, not preserved), so apt sometimes sees a
    /// "repository changed its Origin/Label" mismatch against what it cached last time and
    /// refuses to refresh — leaving a stale index in place that the following `apt-get install`
    /// then reports as unauthenticated. There is exactly one publisher of this repository (this
    /// machine), so accepting that change here is always correct, not a hijacked-mirror risk.
    fn refresh_index(&self) -> Result<()> {
        let sourceparts = self.repo.apt_dir().display().to_string();
        let output = self
            .runner
            .run(
                &self.root,
                "apt-get",
                &[
                    "update",
                    "--allow-releaseinfo-change",
                    "-o",
                    "Dir::Etc::SourceList=/dev/null",
                    "-o",
                    &format!("Dir::Etc::SourceParts={sourceparts}"),
                ],
            )
            .context("refreshing the local archive index")?;
        if !output.status.success() {
            bail!(
                "apt could not read the local archive:\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Every package in the set is installed, at the set's version.
    fn verify(&self, build_set: &str) -> Result<()> {
        let matrix = super::load_matrix(&self.root);
        let status = std::fs::read_to_string(self.root.join("var/lib/dpkg/status"))
            .context("reading dpkg status after the install")?;
        let installed = super::installed_versions(&status, &matrix.packages());
        let wrong: Vec<String> = matrix
            .packages()
            .into_iter()
            .filter_map(|package| match installed.get(package) {
                Some(version) if version == build_set => None,
                Some(version) => Some(format!("{package} is {version}")),
                None => Some(format!("{package} is not installed")),
            })
            .collect();
        if wrong.is_empty() {
            Ok(())
        } else {
            bail!(
                "apt reported success but the machine is not at {build_set}: {}",
                wrong.join(", ")
            )
        }
    }
}

pub fn is_native_root(root: &Path) -> Result<bool> {
    let text = match std::fs::read_to_string(root.join("etc/os-release")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && root != Path::new("/") => {
            return Ok(false);
        }
        Err(error) => {
            return Err(error)
                .context("reading operating-system identity; refusing a guessed live-root route");
        }
    };
    let native = text
        .lines()
        .filter_map(|line| line.trim().strip_prefix("ID="))
        .map(|id| id.trim_matches(['\"', '\'']))
        .any(|id| id == "templaros");
    Ok(native)
}
/// Whether this process can change package state.
///
/// Read from `/proc/self/status` rather than through libc, which the CLI does not otherwise link.
/// A guess is fine here — it only chooses between a clear sentence up front and apt's own less
/// clear one two seconds later.
#[must_use]
pub fn is_root(root: &Path) -> bool {
    std::fs::read_to_string(root.join("proc/self/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|line| line.split_whitespace().nth(1).map(ToOwned::to_owned))
        })
        .is_some_and(|effective| effective == "0")
}

#[cfg(test)]
mod tests {
    use super::super::cmd::FakeCommandRunner;
    use super::*;
    use std::{fs, path::PathBuf};

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const OLDER: &str = "0.1.0~dev1000+gbbbbbbb";
    const METAPACKAGES: &[&str] = &[
        "templaros-desktop",
        "templaros-kernel",
        "templaros-nvidia-open",
        "templaros-nvidia-proprietary",
    ];

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("templar-install-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The matrix's packages plus the archive metapackages a real set also carries — including
    /// the two NVIDIA rows, which conflict with each other and so can never be installed together.
    fn build_set(scratch: &Path, version: &str) -> Vec<(String, String, PathBuf)> {
        let built = scratch.join(format!("built-{version}"));
        fs::create_dir_all(&built).unwrap();
        templar_build::components::Matrix::embedded()
            .packages()
            .into_iter()
            .chain(METAPACKAGES.iter().copied())
            .map(|package| {
                let file = built.join(format!("{package}_{version}_amd64.deb"));
                fs::write(&file, b"deb").unwrap();
                (package.to_owned(), version.to_owned(), file)
            })
            .collect()
    }

    /// Write a dpkg status file saying the whole matrix is installed at `version`.
    fn record_installed(root: &Path, version: &str) {
        use std::fmt::Write as _;
        let mut status = String::new();
        for package in templar_build::components::Matrix::embedded().packages() {
            let _ = write!(
                status,
                "Package: {package}\nStatus: install ok installed\nVersion: {version}\n\n"
            );
        }
        let path = root.join("var/lib/dpkg/status");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, status).unwrap();
    }

    fn installer(name: &str) -> (Installer, Arc<FakeCommandRunner>, PathBuf) {
        let scratch = scratch(name);
        let runner = Arc::new(FakeCommandRunner::default());
        let repo = Repo::new(scratch.join("devrepo"), runner.clone());
        repo.publish(&build_set(&scratch, OLDER), OLDER).unwrap();
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        let root = scratch.join("root");
        fs::create_dir_all(&root).unwrap();
        (Installer::new(repo, runner.clone(), root), runner, scratch)
    }

    #[test]
    fn the_archive_metapackages_are_never_part_of_an_install() {
        // A set carries the image's metapackages so the image build can resolve against them.
        // Pinning them on a host asks apt for an impossible set — the two NVIDIA rows conflict —
        // and apt then refuses the whole deployment.
        let (installer, _runner, scratch) = installer("metapackages");
        let plan = installer.plan(SET).unwrap();
        for meta in METAPACKAGES {
            assert!(
                !plan.pinned.iter().any(|pin| pin.starts_with(meta)),
                "{meta} must not be pinned: {:?}",
                plan.pinned
            );
        }
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn the_index_refresh_touches_only_the_local_archive() {
        // A plain `apt-get update` in the middle of a deployment goes to the network and
        // refreshes every unrelated source on the machine.
        let (installer, runner, scratch) = installer("scoped-update");
        record_installed(&installer.root, SET);
        let plan = installer.plan(SET).unwrap();
        installer.install(&plan).unwrap();
        let update = runner
            .lines()
            .into_iter()
            .find(|line| line.starts_with("apt-get update"))
            .expect("an index refresh");
        assert!(
            update.contains("--allow-releaseinfo-change"),
            "every publish regenerates Release from scratch; apt must not refuse the refresh \
             and silently keep a stale, mismatched index: {update}"
        );
        assert!(
            update.contains("Dir::Etc::SourceList=/dev/null"),
            "{update}"
        );
        assert!(update.contains("devrepo/apt"), "{update}");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn an_apt_success_that_did_not_take_is_still_a_failure() {
        // apt can exit zero having resolved its way somewhere else. A deployment that trusted
        // the exit code would then activate components against packages that are not there.
        let (installer, _runner, scratch) = installer("verify");
        record_installed(&installer.root, OLDER);
        let plan = installer.plan(SET).unwrap();
        let error = installer.install(&plan).unwrap_err();
        assert!(format!("{error}").contains("is not at"), "{error}");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn a_refused_set_changes_nothing_and_says_so() {
        let (installer, runner, scratch) = installer("refused");
        record_installed(&installer.root, OLDER);
        runner.fail("apt-get");
        let plan = installer.plan(SET).unwrap();
        let error = installer.install(&plan).unwrap_err();
        assert!(format!("{error:#}").contains("local archive"), "{error:#}");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn the_plan_names_what_a_rollback_would_go_back_to() {
        let (installer, _runner, scratch) = installer("rollback-target");
        let plan = installer.plan(SET).unwrap();
        assert_eq!(plan.rollback_to.as_deref(), Some(OLDER));
        assert!(plan.command_line().starts_with("apt-get install --yes"));
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn rolling_back_is_the_same_path_with_the_previous_set() {
        let (installer, runner, scratch) = installer("rollback");
        record_installed(&installer.root, OLDER);
        let plan = installer.plan(OLDER).unwrap();
        installer.install(&plan).unwrap();
        let install = runner
            .lines()
            .into_iter()
            .find(|line| line.starts_with("apt-get install"))
            .unwrap();
        assert!(
            install.contains(&format!("templaros-comp={OLDER}")),
            "{install}"
        );
        assert!(
            install.contains("--allow-downgrades"),
            "a rollback is a downgrade, and must be allowed to be one: {install}"
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn a_set_that_was_never_published_cannot_be_installed() {
        let (installer, _runner, scratch) = installer("unknown");
        let error = installer.plan("0.1.0~dev9999+gzzzzzzz").unwrap_err();
        assert!(format!("{error}").contains("never published"), "{error}");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn native_install_queues_pinned_candidate_without_live_apt() {
        let (installer, runner, scratch) = installer("native-queue");
        fs::create_dir_all(installer.root.join("etc")).unwrap();
        fs::write(installer.root.join("etc/os-release"), "ID=templaros\n").unwrap();
        // Each output is interpreted by its own busctl call; this records the
        // required capability token and typed Plan payload without a live bus.
        runner.say(
            "busctl",
            r#"{"type":"s","data":["txn-native"],"contract":"ChangeSelfPassword"}"#,
        );
        let plan = installer.plan(SET).unwrap();
        assert_eq!(installer.queue_candidate(&plan).unwrap(), "txn-native");
        let calls = runner.calls();
        assert!(
            !calls
                .iter()
                .any(|call| call.program == "apt-get" || call.program == "systemctl")
        );
        let queue = calls
            .iter()
            .find(|call| call.program == "busctl" && call.args.iter().any(|arg| arg == "Plan"))
            .unwrap();
        let changes: serde_json::Value = serde_json::from_str(queue.args.last().unwrap()).unwrap();
        assert_eq!(changes["other_packages"], serde_json::json!(plan.pinned));
        assert_eq!(
            changes["archive"],
            installer
                .repo
                .root
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn native_install_refuses_old_hostd_before_plan_or_live_apt() {
        let (installer, runner, scratch) = installer("native-old-hostd");
        fs::create_dir_all(installer.root.join("etc")).unwrap();
        fs::write(installer.root.join("etc/os-release"), "ID=templaros\n").unwrap();
        let error = installer
            .queue_candidate(&installer.plan(SET).unwrap())
            .unwrap_err();
        assert!(format!("{error}").contains("migrate the legacy installation offline"));
        assert!(
            !runner
                .calls()
                .iter()
                .any(|call| call.program == "apt-get" || call.args.iter().any(|arg| arg == "Plan"))
        );
        let _ = fs::remove_dir_all(scratch);
    }
}
