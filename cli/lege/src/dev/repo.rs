//! `lege dev repo` — the local package archive a development machine installs from.
//!
//! **Why an archive at all**, when `dpkg -i` is right there: because `dpkg -i` is not
//! transactional. `scripts/reinstall-legeos-desktop.sh` installs thirteen packages, and when one
//! fails halfway it calls `apt-get -f install` to repair whatever state that left — which is the
//! non-atomic install that produced the mixed sets this whole roadmap exists to end. One
//! `apt-get install pkg=version …` against a real archive either takes the whole set or takes
//! none of it, and can be pointed at the *previous* set just as easily, which is what makes
//! rollback a normal operation rather than a rebuild.
//!
//! **Where.** `/var/lib/legeos/devrepo`, world-readable. Not the developer's home directory: apt
//! drops to the `_apt` user to fetch, and `_apt` cannot read `/home/<user>` on a normal machine,
//! which produces a confusing permission failure people usually work around by disabling the
//! sandbox. `/var/lib` avoids the trap instead of teaching everyone the workaround.
//!
//! **Layout.** A flat repository (ADR-0009 stage 1, roadmap decision D4): the `.deb` files live
//! under `pool/`, and one `Packages`/`Release` pair at the root indexes them, with `Filename:`
//! fields relative to the root. That is why every command here is run with the repository root as
//! its working directory. `aptly` and a `dists/` tree remain the Stage-2 archive.legeos.org
//! design; this is deliberately the smaller thing.
//!
//! **Signing.** A per-machine key, generated on first `init`, described in its own user id as
//! "built on this machine". It asserts exactly one thing — that these packages came off this
//! developer's disk and were not modified in the pool since — and deliberately not provenance,
//! which is the archive key's job.

use super::cmd::CommandRunner;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// Where the archive lives.
pub const DEFAULT_ROOT: &str = "/var/lib/legeos/devrepo";
/// The signing key's user id.
pub const KEY_UID: &str =
    "LegeOS development archive (built on this machine) <legeos-dev@localhost>";
/// Where the exported public key is installed for apt.
pub const KEYRING_PATH: &str = "/etc/apt/keyrings/legeos-dev.gpg";
/// The deb822 source apt reads.
pub const SOURCE_PATH: &str = "/etc/apt/sources.list.d/legeos-dev.sources";
/// How many published sets are kept.
pub const DEFAULT_KEEP: usize = 3;

/// One published build set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SetManifest {
    /// The build-set id, which is every package's version.
    pub build_set: String,
    pub published_at_ms: u64,
    pub packages: Vec<PublishedPackage>,
    /// The set's shipped D-Bus signatures, carried so a later deployment can compare against
    /// them without a source checkout. Absent for a set published before this was recorded.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub interfaces: std::collections::BTreeMap<String, lege_build::interfaces::InterfaceSignatures>,
    /// What the set's binaries make of each persistent store.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub schemas: std::collections::BTreeMap<String, lege_build::StoreSchema>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublishedPackage {
    pub package: String,
    pub version: String,
    /// Path within the repository, e.g. `pool/legeos-shell_0.1.0~dev1+gabc_amd64.deb`.
    pub file: String,
    pub size_bytes: u64,
}

impl SetManifest {
    /// `pkg=version` for every package, which is exactly what `apt-get install` wants.
    #[must_use]
    pub fn pinned_packages(&self) -> Vec<String> {
        self.packages
            .iter()
            .map(|package| format!("{}={}", package.package, package.version))
            .collect()
    }
}

/// The compatibility facts a build set carries into the archive.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Compatibility {
    pub interfaces: std::collections::BTreeMap<String, lege_build::interfaces::InterfaceSignatures>,
    pub schemas: std::collections::BTreeMap<String, lege_build::StoreSchema>,
}

/// The archive.
#[derive(Debug)]
pub struct Repo {
    pub root: PathBuf,
    pub runner: Arc<dyn CommandRunner>,
}

impl Repo {
    #[must_use]
    pub fn new(root: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self { root, runner }
    }

    fn pool(&self) -> PathBuf {
        self.root.join("pool")
    }

    fn sets(&self) -> PathBuf {
        self.root.join("sets")
    }

    fn gnupg(&self) -> PathBuf {
        self.root.join("gnupg")
    }

    fn public_key(&self) -> PathBuf {
        self.root.join("legeos-dev.gpg")
    }

    /// The archive's own copy of its apt source, so `apt-get update` can be pointed at this
    /// archive alone instead of refreshing every source on the machine.
    #[must_use]
    pub fn apt_dir(&self) -> PathBuf {
        self.root.join("apt")
    }

    /// Creates the archive and its signing key if they are not there already. Idempotent.
    ///
    /// # Errors
    /// If a directory cannot be created, or `gpg` cannot generate or export the key.
    pub fn init(&self) -> Result<String> {
        for dir in [self.root.clone(), self.pool(), self.sets(), self.apt_dir()] {
            fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            // 0755: `_apt` has to be able to read every one of these. See the module comment.
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o755));
        }
        fs::create_dir_all(self.gnupg()).context("creating the archive keyring directory")?;
        // gpg refuses to use a home directory anyone else can read, and is right to.
        fs::set_permissions(self.gnupg(), fs::Permissions::from_mode(0o700))
            .context("securing the archive keyring directory")?;

        let created = self.ensure_key()?;
        self.export_key()?;
        write_public(
            &self.apt_dir().join("legeos-dev.sources"),
            self.source_stanza(&self.public_key()).as_bytes(),
        )?;
        Ok(if created {
            format!("{} — created, with a new signing key", self.root.display())
        } else {
            format!("{} — ready", self.root.display())
        })
    }

    /// Whether a secret key for [`KEY_UID`] exists, generating one if not. Returns whether it
    /// generated.
    fn ensure_key(&self) -> Result<bool> {
        let homedir = self.gnupg().display().to_string();
        let listed = self.runner.run(
            &self.root,
            "gpg",
            &["--homedir", &homedir, "--list-secret-keys", KEY_UID],
        );
        if listed.is_ok_and(|output| output.status.success()) {
            return Ok(false);
        }
        // No passphrase: this key signs a local development pool, and a passphrase would put a
        // pinentry prompt in the middle of every deployment. It is protected by the 0700
        // directory, and it asserts nothing beyond "this machine built these".
        let output = self
            .runner
            .run(
                &self.root,
                "gpg",
                &[
                    "--homedir",
                    &homedir,
                    "--batch",
                    "--passphrase",
                    "",
                    "--quick-generate-key",
                    KEY_UID,
                    "default",
                    "default",
                    "never",
                ],
            )
            .context("generating the archive signing key")?;
        if !output.status.success() {
            bail!(
                "gpg could not generate the archive signing key: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(true)
    }

    fn export_key(&self) -> Result<()> {
        let homedir = self.gnupg().display().to_string();
        let target = self.public_key().display().to_string();
        let output = self
            .runner
            .run(
                &self.root,
                "gpg",
                &[
                    "--homedir",
                    &homedir,
                    "--batch",
                    "--yes",
                    "--output",
                    &target,
                    "--export",
                    KEY_UID,
                ],
            )
            .context("exporting the archive public key")?;
        if !output.status.success() {
            bail!(
                "gpg could not export the archive public key: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let _ = fs::set_permissions(self.public_key(), fs::Permissions::from_mode(0o644));
        Ok(())
    }

    /// The deb822 source and keyring apt needs, written under `sysroot`.
    ///
    /// Separate from [`Repo::init`] because it writes outside the archive, into `/etc`, which
    /// needs root — and a tool that quietly needs root halfway through is a tool people run with
    /// `sudo` out of habit. This step says what it is.
    ///
    /// # Errors
    /// If `/etc/apt` cannot be written — almost always "run this one step as root".
    pub fn install_source(&self, sysroot: &Path) -> Result<Vec<PathBuf>> {
        let keyring = sysroot.join(KEYRING_PATH.trim_start_matches('/'));
        let source = sysroot.join(SOURCE_PATH.trim_start_matches('/'));
        for path in [&keyring, &source] {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {} (needs root)", parent.display()))?;
            }
        }
        fs::copy(self.public_key(), &keyring).with_context(|| {
            format!(
                "installing the archive key to {} (needs root)",
                keyring.display()
            )
        })?;
        let _ = fs::set_permissions(&keyring, fs::Permissions::from_mode(0o644));
        fs::write(&source, self.source_stanza(&keyring))
            .with_context(|| format!("writing {} (needs root)", source.display()))?;
        let _ = fs::set_permissions(&source, fs::Permissions::from_mode(0o644));
        Ok(vec![keyring, source])
    }

    /// Deliberately no `Trusted:` field. It reads like "trust only if `Signed-By` checks out",
    /// but apt's actual semantics are the opposite of that: `Trusted: no` *forces* every package
    /// from the source to be flagged unauthenticated, unconditionally — even when the signature
    /// verifies cleanly. Leaving it unset is what makes `Signed-By` alone the gate: a good
    /// signature authenticates the source, a missing or bad one does not.
    fn source_stanza(&self, keyring: &Path) -> String {
        format!(
            "# The local LegeOS development archive. Written by `lege dev repo init`.\n\
             Types: deb\n\
             URIs: file://{}\n\
             Suites: ./\n\
             Signed-By: {}\n",
            self.root.display(),
            keyring.display(),
        )
    }

    /// Publishes a build set: copies its packages into the pool, records the set, regenerates and
    /// signs the index, moves the current/previous markers, and prunes.
    ///
    /// # Errors
    /// If a package file is missing, or any of the archive tools fail. On failure the markers are
    /// left alone, so a half-published set never becomes the current one.
    pub fn publish(
        &self,
        packages: &[(String, String, PathBuf)],
        build_set: &str,
    ) -> Result<SetManifest> {
        self.publish_with(packages, build_set, &Compatibility::default())
    }

    /// [`Repo::publish`], also recording the set's compatibility facts.
    ///
    /// # Errors
    /// As [`Repo::publish`].
    pub fn publish_with(
        &self,
        packages: &[(String, String, PathBuf)],
        build_set: &str,
        compatibility: &Compatibility,
    ) -> Result<SetManifest> {
        if packages.is_empty() {
            bail!("refusing to publish an empty set");
        }
        if let Some((package, version, _)) =
            packages.iter().find(|(_, version, _)| version != build_set)
        {
            bail!("{package} is version {version}, not {build_set}: this is not one set");
        }
        self.init()?;

        let mut published = Vec::new();
        for (package, version, source) in packages {
            let name = source
                .file_name()
                .with_context(|| format!("{} has no file name", source.display()))?;
            let target = self.pool().join(name);
            fs::copy(source, &target)
                .with_context(|| format!("copying {} into the pool", source.display()))?;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o644));
            published.push(PublishedPackage {
                package: package.clone(),
                version: version.clone(),
                file: format!("pool/{}", name.to_string_lossy()),
                size_bytes: fs::metadata(&target).map(|m| m.len()).unwrap_or_default(),
            });
        }

        let manifest = SetManifest {
            build_set: build_set.to_owned(),
            published_at_ms: now_ms(),
            packages: published,
            interfaces: compatibility.interfaces.clone(),
            schemas: compatibility.schemas.clone(),
        };
        let set_dir = self.sets().join(build_set);
        fs::create_dir_all(&set_dir).context("creating the set directory")?;
        let _ = fs::set_permissions(&set_dir, fs::Permissions::from_mode(0o755));
        write_public(
            &set_dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;

        self.reindex()?;
        self.advance_markers(build_set)?;
        self.prune(DEFAULT_KEEP)?;
        Ok(manifest)
    }

    /// Regenerates `Packages` and `Release`, and signs the latter.
    fn reindex(&self) -> Result<()> {
        let packages = self
            .runner
            .run(&self.root, "apt-ftparchive", &["packages", "pool"])
            .context("running apt-ftparchive packages")?;
        if !packages.status.success() {
            bail!(
                "apt-ftparchive could not index the pool: {}",
                String::from_utf8_lossy(&packages.stderr)
            );
        }
        write_public(&self.root.join("Packages"), &packages.stdout)?;

        let release = self
            .runner
            .run(
                &self.root,
                "apt-ftparchive",
                &[
                    "-o",
                    "APT::FTPArchive::Release::Origin=LegeOS",
                    "-o",
                    "APT::FTPArchive::Release::Label=legeos-dev",
                    "-o",
                    "APT::FTPArchive::Release::Suite=./",
                    "-o",
                    "APT::FTPArchive::Release::Components=main",
                    "release",
                    ".",
                ],
            )
            .context("running apt-ftparchive release")?;
        if !release.status.success() {
            bail!(
                "apt-ftparchive could not write a Release: {}",
                String::from_utf8_lossy(&release.stderr)
            );
        }
        write_public(&self.root.join("Release"), &release.stdout)?;

        let homedir = self.gnupg().display().to_string();
        // Both forms: InRelease is what modern apt prefers, Release.gpg is what an older or
        // differently configured one falls back to. Writing only one of them produces a
        // repository that works on this machine and mysteriously does not on the next.
        for (flag, output) in [
            ("--clearsign", "InRelease"),
            ("--detach-sign", "Release.gpg"),
        ] {
            let target = self.root.join(output).display().to_string();
            let signed = self
                .runner
                .run(
                    &self.root,
                    "gpg",
                    &[
                        "--homedir",
                        &homedir,
                        "--batch",
                        "--yes",
                        "--armor",
                        "--local-user",
                        KEY_UID,
                        "--output",
                        &target,
                        flag,
                        "Release",
                    ],
                )
                .with_context(|| format!("signing {output}"))?;
            if !signed.status.success() {
                bail!(
                    "gpg could not sign {output}: {}",
                    String::from_utf8_lossy(&signed.stderr)
                );
            }
            let _ = fs::set_permissions(self.root.join(output), fs::Permissions::from_mode(0o644));
        }
        Ok(())
    }

    /// `previous` takes the old `current`, and `current` takes the new set.
    ///
    /// Republishing the same set leaves `previous` alone: it would otherwise be overwritten with
    /// the set being deployed, and the one thing to roll back *to* would be gone.
    fn advance_markers(&self, build_set: &str) -> Result<()> {
        if let Some(current) = self.current()?
            && current != build_set
        {
            write_public(&self.root.join("previous"), current.as_bytes())?;
        }
        write_public(&self.root.join("current"), build_set.as_bytes())
    }

    /// The set most recently published.
    ///
    /// # Errors
    /// Never for a missing marker — that is `None`, not a failure.
    pub fn current(&self) -> Result<Option<String>> {
        read_marker(&self.root.join("current"))
    }

    /// The set to roll back to.
    ///
    /// # Errors
    /// Never for a missing marker.
    pub fn previous(&self) -> Result<Option<String>> {
        read_marker(&self.root.join("previous"))
    }

    /// Every published set, newest first.
    ///
    /// # Errors
    /// If the sets directory cannot be read.
    pub fn published_sets(&self) -> Result<Vec<String>> {
        let Ok(entries) = fs::read_dir(self.sets()) else {
            return Ok(Vec::new());
        };
        let mut sets: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        sets.sort_by_key(|set| {
            std::cmp::Reverse(lege_build::parse_build_set_id(set).map_or(0, |parsed| parsed.epoch))
        });
        Ok(sets)
    }

    /// Reads a published set's manifest.
    ///
    /// # Errors
    /// If the set was never published here, or its manifest is unreadable.
    pub fn manifest(&self, build_set: &str) -> Result<SetManifest> {
        let path = self.sets().join(build_set).join("manifest.json");
        let body = fs::read(&path)
            .with_context(|| format!("{build_set} was never published to this archive"))?;
        serde_json::from_slice(&body).with_context(|| format!("parsing {}", path.display()))
    }

    /// Drops all but the newest `keep` sets, and the packages only they referenced.
    ///
    /// # Errors
    /// If the archive cannot be read, or a file cannot be removed.
    pub fn prune(&self, keep: usize) -> Result<Vec<String>> {
        let sets = self.published_sets()?;
        let protected: Vec<String> = [self.current()?, self.previous()?]
            .into_iter()
            .flatten()
            .collect();
        let doomed = sets_to_drop(&sets, &protected, keep);
        for set in &doomed {
            let _ = fs::remove_dir_all(self.sets().join(set));
        }
        self.sweep_pool()?;
        Ok(doomed)
    }

    /// Removes pool files no surviving set's manifest names.
    fn sweep_pool(&self) -> Result<()> {
        let mut wanted = std::collections::BTreeSet::new();
        for set in self.published_sets()? {
            if let Ok(manifest) = self.manifest(&set) {
                for package in manifest.packages {
                    wanted.insert(package.file);
                }
            }
        }
        let Ok(entries) = fs::read_dir(self.pool()) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let name = format!("pool/{}", entry.file_name().to_string_lossy());
            if !wanted.contains(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

/// Which sets to drop: everything past the newest `keep`, minus anything currently deployed or
/// held for rollback.
///
/// Pure, and separate, because getting this wrong deletes the thing a rollback needs. `current`
/// and `previous` are never dropped no matter how far down the list they have fallen — a
/// developer who published four sets and then rolled back two is exactly the person who most
/// needs the old one still to be there.
#[must_use]
pub fn sets_to_drop(newest_first: &[String], protected: &[String], keep: usize) -> Vec<String> {
    newest_first
        .iter()
        .skip(keep)
        .filter(|set| !protected.contains(set))
        .cloned()
        .collect()
}

fn read_marker(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let trimmed = text.trim().to_owned();
            Ok((!trimmed.is_empty()).then_some(trimmed))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes a file `_apt` can read.
fn write_public(path: &Path, body: &[u8]) -> Result<()> {
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::super::cmd::FakeCommandRunner;
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lege-devrepo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A repository under a scratch directory, plus the fake that records what it would have run.
    fn repo(name: &str) -> (Repo, Arc<FakeCommandRunner>, PathBuf) {
        let scratch = scratch(name);
        let runner = Arc::new(FakeCommandRunner::default());
        let repo = Repo::new(scratch.join("devrepo"), runner.clone());
        (repo, runner, scratch)
    }

    /// A build set on disk, ready to publish.
    fn build_set(scratch: &Path, version: &str) -> Vec<(String, String, PathBuf)> {
        let built = scratch.join(format!("built-{version}"));
        fs::create_dir_all(&built).unwrap();
        lege_build::components::Matrix::embedded()
            .packages()
            .into_iter()
            .map(|package| {
                let file = built.join(format!("{package}_{version}_amd64.deb"));
                fs::write(&file, b"deb").unwrap();
                (package.to_owned(), version.to_owned(), file)
            })
            .collect()
    }

    const SET: &str = "0.1.0~dev2000+gaaaaaaa";
    const NEWER: &str = "0.1.0~dev3000+gccccccc";

    #[test]
    fn init_creates_a_world_readable_archive_and_a_private_keyring() {
        let (repo, _runner, scratch) = repo("init");
        repo.init().unwrap();
        for dir in ["", "pool", "sets"] {
            let mode = fs::metadata(repo.root.join(dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755, "{dir} must be readable by _apt");
        }
        let mode = fs::metadata(repo.gnupg()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "the signing key must not be");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn init_generates_a_key_once_and_then_leaves_it_alone() {
        let (repo, runner, scratch) = repo("init-key");
        runner.fail("gpg-missing-key-marker"); // no failures: --list-secret-keys succeeds
        repo.init().unwrap();
        let generated = runner
            .lines()
            .iter()
            .filter(|line| line.contains("--quick-generate-key"))
            .count();
        assert_eq!(generated, 0, "an existing key must not be regenerated");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn a_missing_key_is_generated_with_no_passphrase_prompt() {
        let (repo, runner, scratch) = repo("init-newkey");
        // The fake fails every gpg call, including the listing, which is how "no key yet" looks.
        runner.fail("gpg");
        let error = repo.init().unwrap_err();
        let generate = runner
            .lines()
            .into_iter()
            .find(|line| line.contains("--quick-generate-key"))
            .expect("a missing key must be generated");
        assert!(generate.contains("--batch"), "{generate}");
        assert!(generate.contains(KEY_UID), "{generate}");
        assert!(format!("{error:#}").contains("signing key"), "{error:#}");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn publishing_a_set_indexes_and_signs_the_archive() {
        let (repo, runner, scratch) = repo("publish");
        let manifest = repo.publish(&build_set(&scratch, SET), SET).unwrap();
        assert_eq!(manifest.packages.len(), 13);
        assert_eq!(repo.current().unwrap().as_deref(), Some(SET));
        assert!(repo.root.join("Packages").is_file());
        assert!(repo.root.join("Release").is_file());

        let lines = runner.lines();
        assert!(
            lines
                .iter()
                .any(|line| line == "apt-ftparchive packages pool"),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("release .")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("--clearsign")),
            "InRelease must be written: {lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("--detach-sign")),
            "Release.gpg must be written too: {lines:#?}"
        );
        // Every archive tool must run *in* the archive, or the index points nowhere.
        for call in runner.calls() {
            if call.program == "apt-ftparchive" {
                assert_eq!(call.dir, repo.root, "{call:?}");
            }
        }
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn the_manifest_pins_every_package_the_way_apt_wants() {
        let (repo, _runner, scratch) = repo("pins");
        let manifest = repo.publish(&build_set(&scratch, SET), SET).unwrap();
        let pinned = manifest.pinned_packages();
        assert_eq!(pinned.len(), 13);
        assert!(pinned.contains(&format!("legeos-shell={SET}")));
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn a_mixed_set_is_refused_before_anything_is_copied() {
        let (repo, _runner, scratch) = repo("mixed");
        let mut packages = build_set(&scratch, SET);
        packages[0].1 = NEWER.to_owned();
        let error = repo.publish(&packages, SET).unwrap_err();
        assert!(format!("{error}").contains("not one set"), "{error}");
        assert!(!repo.pool().exists(), "nothing may be copied");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn publishing_a_second_set_makes_the_first_the_rollback_target() {
        let (repo, _runner, scratch) = repo("markers");
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        repo.publish(&build_set(&scratch, NEWER), NEWER).unwrap();
        assert_eq!(repo.current().unwrap().as_deref(), Some(NEWER));
        assert_eq!(repo.previous().unwrap().as_deref(), Some(SET));
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn republishing_the_same_set_does_not_lose_the_rollback_target() {
        let (repo, _runner, scratch) = repo("republish");
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        repo.publish(&build_set(&scratch, NEWER), NEWER).unwrap();
        repo.publish(&build_set(&scratch, NEWER), NEWER).unwrap();
        assert_eq!(
            repo.previous().unwrap().as_deref(),
            Some(SET),
            "the set to roll back to must survive a republish"
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn pruning_keeps_three_sets_and_removes_their_packages() {
        let (repo, _runner, scratch) = repo("prune");
        for epoch in 1..=5 {
            let set = format!("0.1.0~dev{epoch}000+gaaaaaa{epoch}");
            repo.publish(&build_set(&scratch, &set), &set).unwrap();
        }
        let sets = repo.published_sets().unwrap();
        assert_eq!(sets.len(), 3, "{sets:?}");
        let pooled = fs::read_dir(repo.pool()).unwrap().count();
        assert_eq!(pooled, 3 * 13, "the pool must not keep orphans");
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn pruning_never_drops_the_set_a_rollback_would_need() {
        // Four sets published, then a rollback to the oldest. It has fallen well past `keep`,
        // and it is the one file on the machine that must not be deleted.
        let sets: Vec<String> = (1..=5)
            .rev()
            .map(|epoch| format!("0.1.0~dev{epoch}000+ga"))
            .collect();
        let protected = vec![sets[4].clone(), sets[0].clone()];
        let dropped = sets_to_drop(&sets, &protected, 3);
        assert!(!dropped.contains(&sets[4]), "{dropped:?}");
        assert_eq!(dropped, vec![sets[3].clone()]);
    }

    #[test]
    fn the_deb822_source_points_at_the_archive_and_its_key() {
        let (repo, _runner, scratch) = repo("source");
        repo.init().unwrap();
        // The fake gpg records its argv but writes no file, so stand in for the export.
        fs::write(repo.public_key(), b"fake key").unwrap();
        let written = repo.install_source(&scratch.join("sysroot")).unwrap();
        let text = fs::read_to_string(&written[1]).unwrap();
        assert!(
            text.contains(&format!("URIs: file://{}", repo.root.display())),
            "{text}"
        );
        assert!(text.contains("Suites: ./"), "{text}");
        assert!(text.contains("Signed-By:"), "{text}");
        assert!(
            !text.contains("Trusted:"),
            "Trusted: no forces every package unauthenticated even with a good signature; \
             Signed-By alone must gate trust: {text}"
        );
        let _ = fs::remove_dir_all(scratch);
    }
}
