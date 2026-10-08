//! `templar dev repo` — the local package archive a development machine installs from.
//!
//! **Why an archive at all**, when `dpkg -i` is right there: because `dpkg -i` is not
//! transactional. The predecessor's direct reinstall path passed a batch of packages to
//! `dpkg -i`, and when one failed halfway it called `apt-get -f install` to repair that state — the
//! non-atomic install that produced the mixed sets this whole roadmap exists to end. One
//! `apt-get install pkg=version …` against a real archive either takes the whole set or takes
//! none of it, and can be pointed at the *previous* set just as easily, which is what makes
//! rollback a normal operation rather than a rebuild.
//!
//! **Where.** `/var/lib/templaros/devrepo`, world-readable. Not the developer's home directory: apt
//! drops to the `_apt` user to fetch, and `_apt` cannot read `/home/<user>` on a normal machine,
//! which produces a confusing permission failure people usually work around by disabling the
//! sandbox. `/var/lib` avoids the trap instead of teaching everyone the workaround.
//!
//! **Layout.** A flat repository (ADR-0009 stage 1, roadmap decision D4): the `.deb` files live
//! under `pool/`, and one `Packages`/`Release` pair at the root indexes them, with `Filename:`
//! fields relative to the root. That is why every command here is run with the repository root as
//! its working directory. `aptly` and a `dists/` tree remain the Stage-2 archive.templaros.org
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
pub const DEFAULT_ROOT: &str = "/var/lib/templaros/devrepo";
/// The signing key's user id.
pub const KEY_UID: &str =
    "TemplarOS development archive (built on this machine) <templaros-dev@localhost>";
/// Where the exported public key is installed for apt.
pub const KEYRING_PATH: &str = "/etc/apt/keyrings/templaros-dev.gpg";
/// The deb822 source apt reads.
pub const SOURCE_PATH: &str = "/etc/apt/sources.list.d/templaros-dev.sources";
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
    pub interfaces:
        std::collections::BTreeMap<String, templar_build::interfaces::InterfaceSignatures>,
    /// What the set's binaries make of each persistent store.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub schemas: std::collections::BTreeMap<String, templar_build::StoreSchema>,
    /// Which toolchain and host built the set, and which Ubuntu index it resolved against.
    /// Defaulted, so a set published before provenance was recorded still reads.
    #[serde(default, skip_serializing_if = "templar_build::Provenance::is_empty")]
    pub provenance: templar_build::Provenance,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublishedPackage {
    pub package: String,
    pub version: String,
    /// Path within the repository, e.g. `pool/templaros-shell_0.1.0~dev1+gabc_amd64.deb`.
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
    pub interfaces:
        std::collections::BTreeMap<String, templar_build::interfaces::InterfaceSignatures>,
    pub schemas: std::collections::BTreeMap<String, templar_build::StoreSchema>,
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

    /// The archive at `root`, driven by real processes.
    #[must_use]
    pub fn open(root: PathBuf) -> Self {
        Self::new(root, Arc::new(super::cmd::RealCommandRunner::default()))
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
        self.root.join("templaros-dev.gpg")
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
            &self.apt_dir().join("templaros-dev.sources"),
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
        if super::install::is_native_root(sysroot)? {
            bail!(
                "native archive keys/sources are installed only into hostd candidates; publish then use templar dev install/deploy"
            );
        }
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
            "# The local TemplarOS development archive. Written by `templar dev repo init`.\n\
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
        let provenance = provenance_beside(packages, build_set);
        self.publish_with_provenance(packages, build_set, compatibility, &provenance)
    }

    /// [`Repo::publish_with`], stating the set's provenance rather than reading it off the disk
    /// next to the packages.
    ///
    /// # Errors
    /// As [`Repo::publish`].
    pub fn publish_with_provenance(
        &self,
        packages: &[(String, String, PathBuf)],
        build_set: &str,
        compatibility: &Compatibility,
        provenance: &templar_build::Provenance,
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
        // The set is *staged* from here until the markers move. A publish that dies halfway —
        // out of disk, gpg gone, the machine rebooted — therefore leaves an archive that says so,
        // instead of one whose `current` names a set only half of which is on disk.
        write_public(&self.root.join("staged"), build_set.as_bytes())?;

        let set_dir = self.sets().join(build_set);
        let debs_dir = set_dir.join("debs");
        fs::create_dir_all(&debs_dir).context("creating the set directory")?;
        for dir in [&set_dir, &debs_dir] {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        }

        let mut published = Vec::new();
        for (package, version, source) in packages {
            let name = source
                .file_name()
                .with_context(|| format!("{} has no file name", source.display()))?;
            let target = self.pool().join(name);
            fs::copy(source, &target)
                .with_context(|| format!("copying {} into the pool", source.display()))?;
            let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o644));
            // One directory holding exactly this set, so an image build or a provisioner can be
            // pointed at a *named set* rather than at the whole pool, which holds three of them.
            // A hard link costs nothing; a copy is the fallback when the pool is another
            // filesystem.
            let linked = debs_dir.join(name);
            let _ = fs::remove_file(&linked);
            if fs::hard_link(&target, &linked).is_err() {
                fs::copy(&target, &linked).with_context(|| {
                    format!("placing {} in the set directory", name.to_string_lossy())
                })?;
                let _ = fs::set_permissions(&linked, fs::Permissions::from_mode(0o644));
            }
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
            provenance: provenance.clone(),
        };
        write_public(
            &set_dir.join("manifest.json"),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;

        self.reindex()?;
        self.advance_markers(build_set)?;
        // Only now is the set the archive's own: staged means "not yet current", and it is.
        let _ = fs::remove_file(self.root.join("staged"));
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
                    "APT::FTPArchive::Release::Origin=TemplarOS",
                    "-o",
                    "APT::FTPArchive::Release::Label=templaros-dev",
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

    /// A set written into the archive whose promotion to `current` did not complete — a publish
    /// still running, or one that failed.
    ///
    /// Deliberately *not* called `candidate`: that word already means a boot deployment awaiting
    /// assessment (`distro/deploy`), and an archive state and a boot state that share a name are
    /// two things one operator will eventually confuse for each other.
    ///
    /// # Errors
    /// Never for a missing marker.
    pub fn staged(&self) -> Result<Option<String>> {
        read_marker(&self.root.join("staged"))
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
            std::cmp::Reverse(
                templar_build::parse_build_set_id(set).map_or(0, |parsed| parsed.epoch),
            )
        });
        Ok(sets)
    }

    /// Reads a published set's manifest.
    ///
    /// # Errors
    /// If the set was never published here, or its manifest is unreadable.
    pub fn manifest(&self, build_set: &str) -> Result<SetManifest> {
        let path = self.sets().join(build_set).join("manifest.json");
        let no_sets_dir = !self.sets().is_dir();
        let body = fs::read(&path).with_context(|| {
            if no_sets_dir {
                format!(
                    "{build_set} was never published to this archive (this archive predates set \
                     manifests; republish to record one)"
                )
            } else {
                format!("{build_set} was never published to this archive")
            }
        })?;
        serde_json::from_slice(&body).with_context(|| format!("parsing {}", path.display()))
    }

    /// Drops all but the newest `keep` sets, and the packages only they referenced.
    ///
    /// # Errors
    /// If the archive cannot be read, or a file cannot be removed.
    pub fn prune(&self, keep: usize) -> Result<Vec<String>> {
        let sets = self.published_sets()?;
        let protected: Vec<String> = [self.current()?, self.previous()?, self.staged()?]
            .into_iter()
            .flatten()
            .collect();
        let doomed = sets_to_drop(&sets, &protected, keep);
        for set in &doomed {
            let _ = fs::remove_dir_all(self.sets().join(set));
        }
        // The index must never name a file the pool no longer has: apt would 404 on it, and
        // `repo verify` (which regenerates the index) rightly calls the archive incoherent.
        if self.sweep_pool()? {
            self.reindex()?;
        }
        Ok(doomed)
    }

    /// Removes pool files no surviving set's manifest names. True when anything was removed.
    fn sweep_pool(&self) -> Result<bool> {
        let mut wanted = std::collections::BTreeSet::new();
        for set in self.published_sets()? {
            if let Ok(manifest) = self.manifest(&set) {
                for package in manifest.packages {
                    wanted.insert(package.file);
                }
            }
        }
        let Ok(entries) = fs::read_dir(self.pool()) else {
            return Ok(false);
        };
        let mut removed = false;
        for entry in entries.flatten() {
            let name = format!("pool/{}", entry.file_name().to_string_lossy());
            if !wanted.contains(&name) && fs::remove_file(entry.path()).is_ok() {
                removed = true;
            }
        }
        Ok(removed)
    }

    /// The directory holding exactly one published set's `.deb` files.
    ///
    /// What an image build or a provisioner is pointed at when it must consume *a named set*
    /// rather than whatever the pool happens to hold.
    #[must_use]
    pub fn set_debs(&self, build_set: &str) -> PathBuf {
        self.sets().join(build_set).join("debs")
    }

    /// Checks that the archive holds coherent, signed, installable sets — and says so in a report
    /// rather than in an exit code.
    ///
    /// Publishing already refuses a mixed set, but publishing is not the only thing that touches
    /// an archive: pool files are deleted by hand, a set directory is copied off a dying disk, a
    /// signature is made by a key that has since been replaced. Everything here is therefore
    /// re-checked against what is on disk *now*, not against what publish believed then.
    ///
    /// `matrix` is passed in rather than looked up, so this runs from an installed binary with no
    /// source checkout: [`templar_build::components::Matrix::embedded`] is a complete answer.
    ///
    /// # Errors
    /// If the archive cannot be read at all. A *failing* archive is a report with `ok == false`,
    /// not an error: the caller wants to be told everything that is wrong, not the first thing.
    pub fn verify(
        &self,
        matrix: &templar_build::components::Matrix,
        only: Option<&str>,
    ) -> Result<VerifyReport> {
        let current = self.current()?;
        let previous = self.previous()?;
        let staged = self.staged()?;
        let mut problems = Vec::new();

        let scope: Vec<(String, &str)> = match only {
            Some(set) => vec![(
                set.to_owned(),
                role_of(
                    set,
                    current.as_deref(),
                    previous.as_deref(),
                    staged.as_deref(),
                ),
            )],
            None => [
                current.as_ref().map(|set| (set.clone(), "current")),
                previous
                    .as_ref()
                    .filter(|set| Some(*set) != current.as_ref())
                    .map(|set| (set.clone(), "previous")),
                staged.as_ref().map(|set| (set.clone(), "staged")),
            ]
            .into_iter()
            .flatten()
            .collect(),
        };
        if scope.is_empty() {
            problems.push("the archive has published no set".to_owned());
        }
        if let Some(set) = &staged {
            problems.push(format!(
                "{set} is staged but never became current: a publish did not finish"
            ));
        }

        problems.extend(self.index_problems());
        let sets = scope
            .into_iter()
            .map(|(set, role)| self.verdict(matrix, &set, role))
            .collect::<Vec<_>>();

        let ok = problems.is_empty() && sets.iter().all(|verdict| verdict.ok);
        Ok(VerifyReport {
            root: self.root.display().to_string(),
            current,
            previous,
            staged,
            sets,
            problems,
            ok,
        })
    }

    /// The index and its signatures, as they are on disk right now.
    fn index_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for name in ["Packages", "Release", "InRelease", "Release.gpg"] {
            if !self.root.join(name).is_file() {
                problems.push(format!("{name} is missing; the archive is not indexed"));
            }
        }

        // Regenerating the index is the pool's hash check: `Packages` carries a SHA256 line per
        // file, so a `.deb` replaced in the pool since publishing cannot reproduce the same text.
        match self
            .runner
            .run(&self.root, "apt-ftparchive", &["packages", "pool"])
        {
            Err(error) => problems.push(format!("apt-ftparchive could not read the pool: {error}")),
            Ok(output) if !output.status.success() => problems.push(format!(
                "apt-ftparchive could not read the pool: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(output) => match fs::read(self.root.join("Packages")) {
                Ok(on_disk) if on_disk != output.stdout => problems.push(
                    "the pool no longer matches Packages: a file was added, removed or modified \
                     since the index was written"
                        .to_owned(),
                ),
                Ok(_) => {}
                Err(error) => problems.push(format!("Packages cannot be read: {error}")),
            },
        }

        // Signatures are checked against the exported public key, which is what the medium and
        // the target carry. The private home (`gnupg/`, 0700, never copied) is only for signing;
        // the verifier must not depend on it or an installer can never verify what it installs.
        let keyring = self.public_key();
        match fs::File::open(&keyring) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                problems.push(format!(
                    "the public key {} is missing, so the signatures were not checked; \
                     the next publish exports it",
                    keyring.display()
                ));
                return problems;
            }
            Err(error) => {
                // A root-owned archive verified as the user: the key exists but cannot be read,
                // which must not read as a failed signature — that phrasing means tampering.
                problems.push(format!(
                    "the public key {} cannot be read by this user ({error}), so the signatures \
                     were not checked; the archive itself may be fine — re-run with access to \
                     it rather than reading this as tampering",
                    keyring.display()
                ));
                return problems;
            }
        }
        let keyring = keyring.display().to_string();
        for (name, args) in [
            ("InRelease", vec!["InRelease"]),
            ("Release.gpg", vec!["Release.gpg", "Release"]),
        ] {
            let mut argv = vec!["--keyring", keyring.as_str()];
            argv.extend(args);
            match self.runner.run(&self.root, "gpgv", &argv) {
                Err(error) => problems.push(format!("{name} could not be verified: {error}")),
                Ok(output) if !output.status.success() => problems.push(format!(
                    "{name} does not verify against this archive's key: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Ok(_) => {}
            }
        }
        problems
    }

    /// One set: one id, every component present, every file where the manifest says it is.
    fn verdict(
        &self,
        matrix: &templar_build::components::Matrix,
        build_set: &str,
        role: &str,
    ) -> SetVerdict {
        let mut problems = Vec::new();
        let manifest = match self.manifest(build_set) {
            Ok(manifest) => manifest,
            Err(error) => {
                return SetVerdict {
                    build_set: build_set.to_owned(),
                    role: role.to_owned(),
                    ok: false,
                    problems: vec![format!("{error:#}")],
                    pinned: Vec::new(),
                };
            }
        };

        if templar_build::parse_build_set_id(build_set).is_none() {
            problems.push(format!("{build_set} is not a build-set id"));
        }
        for package in &manifest.packages {
            if package.version != manifest.build_set {
                problems.push(format!(
                    "mixed set: {} is version {}, but the set is {}",
                    package.package, package.version, manifest.build_set
                ));
            }
        }
        for component in &matrix.components {
            if !manifest
                .packages
                .iter()
                .any(|package| package.package == component.package)
            {
                problems.push(format!(
                    "incomplete set: no package for {} ({})",
                    component.package, component.name
                ));
            }
        }
        for package in &manifest.packages {
            let path = self.root.join(&package.file);
            match fs::metadata(&path) {
                Err(_) => problems.push(format!(
                    "{} is gone from the pool, so this set can no longer be installed",
                    package.file
                )),
                Ok(metadata) if metadata.len() != package.size_bytes => problems.push(format!(
                    "{} is {} bytes, but the manifest recorded {}",
                    package.file,
                    metadata.len(),
                    package.size_bytes
                )),
                Ok(_) => {}
            }
        }

        SetVerdict {
            build_set: build_set.to_owned(),
            role: role.to_owned(),
            ok: problems.is_empty(),
            problems,
            pinned: manifest.pinned_packages(),
        }
    }
}

/// What a set is to this archive right now.
fn role_of(
    set: &str,
    current: Option<&str>,
    previous: Option<&str>,
    staged: Option<&str>,
) -> &'static str {
    if current == Some(set) {
        "current"
    } else if previous == Some(set) {
        "previous"
    } else if staged == Some(set) {
        "staged"
    } else {
        "retained"
    }
}

/// What `templar dev repo verify` found.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub root: String,
    pub current: Option<String>,
    pub previous: Option<String>,
    pub staged: Option<String>,
    pub sets: Vec<SetVerdict>,
    /// Problems with the archive itself rather than with one set.
    pub problems: Vec<String>,
    pub ok: bool,
}

/// One set's verdict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SetVerdict {
    pub build_set: String,
    /// `current`, `previous`, `staged` or `retained`.
    pub role: String,
    pub ok: bool,
    pub problems: Vec<String>,
    /// `pkg=version` for every package — the pin simulation. Printed, never run.
    pub pinned: Vec<String>,
}

impl VerifyReport {
    /// The report as the operator reads it. The last line is the verdict, so a terminal that
    /// scrolled still answers the question.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{}\n", self.root);
        for (label, marker) in [
            ("current ", &self.current),
            ("previous", &self.previous),
            ("staged  ", &self.staged),
        ] {
            let _ = writeln!(
                out,
                "  {label}  {}",
                marker.clone().unwrap_or_else(|| "(none)".to_owned())
            );
        }
        for verdict in &self.sets {
            let _ = writeln!(
                out,
                "  {} {}  {}",
                verdict.role,
                verdict.build_set,
                if verdict.ok {
                    format!("ok ({} pinned package(s))", verdict.pinned.len())
                } else {
                    format!("{} problem(s)", verdict.problems.len())
                }
            );
            for problem in &verdict.problems {
                let _ = writeln!(out, "    ! {problem}");
            }
        }
        for problem in &self.problems {
            let _ = writeln!(out, "  ! {problem}");
        }
        if let Some(previous) = self.sets.iter().find(|verdict| verdict.role == "previous") {
            let _ = writeln!(
                out,
                "  rollback  {}",
                if previous.ok {
                    format!(
                        "apt-get install --yes --allow-downgrades {}",
                        previous.pinned.join(" ")
                    )
                } else {
                    "NOT INSTALLABLE — this archive cannot roll back".to_owned()
                }
            );
        }
        let _ = writeln!(out, "verify: {}", if self.ok { "ok" } else { "FAILED" });
        out
    }
}

/// `templar dev repo verify`.
///
/// Lives here rather than in `dev/mod.rs` so the whole archive surface — layout, states and the
/// checks over them — is one file.
///
/// # Errors
/// If the archive cannot be read, or (as a non-zero exit) if verification failed.
pub fn verify_command(root: PathBuf, set: Option<&str>, json: bool) -> Result<String> {
    let repo = Repo::open(root);
    let matrix = templar_build::components::Matrix::embedded();
    let report = repo.verify(&matrix, set)?;
    let rendered = if json {
        serde_json::to_string_pretty(&report)?
    } else {
        report.render()
    };
    if report.ok {
        Ok(rendered)
    } else {
        bail!("{rendered}")
    }
}

/// The provenance recorded next to a set's packages, when they came straight out of
/// `cargo xtask build-set`.
///
/// `build-set.json` sits in the directory holding the `.deb` files — that is the handoff format's
/// own rule ([`templar_build::BuildSet::read`]) — so publishing can carry the toolchain and index
/// facts into the archive without every caller having to thread them through. A set assembled
/// from elsewhere simply has no provenance, which is honest.
fn provenance_beside(
    packages: &[(String, String, PathBuf)],
    build_set: &str,
) -> templar_build::Provenance {
    let Some(dir) = packages
        .first()
        .and_then(|(_, _, path)| path.parent())
        .map(Path::to_path_buf)
    else {
        return templar_build::Provenance::default();
    };
    let Ok((set, _)) = templar_build::BuildSet::read(&dir.join("build-set.json")) else {
        return templar_build::Provenance::default();
    };
    if set.id == build_set {
        set.provenance
    } else {
        templar_build::Provenance::default()
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
        let dir =
            std::env::temp_dir().join(format!("templar-devrepo-{name}-{}", std::process::id()));
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
        templar_build::components::Matrix::embedded()
            .packages()
            .into_iter()
            .map(|package| {
                let file = built.join(format!("{package}_{version}_amd64.deb"));
                fs::write(&file, b"deb").unwrap();
                (package.to_owned(), version.to_owned(), file)
            })
            .collect()
    }

    /// The fake `gpg` records its argv and writes nothing, so stand in for the two signatures it
    /// would have produced. Their *content* is never what verification trusts — `gpg --verify` is,
    /// and the fake answers that — but their absence is itself a finding, so they have to exist.
    fn signed(repo: &Repo) {
        fs::write(repo.public_key(), b"fake key").unwrap();
        for name in ["InRelease", "Release.gpg"] {
            fs::write(
                repo.root.join(name),
                b"-----BEGIN PGP SIGNED MESSAGE-----\n",
            )
            .unwrap();
        }
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

    /// 2026-09-29: a publish that pruned an old set left `Packages` naming its deleted files, so
    /// `repo verify` failed on a freshly published archive and apt would have 404ed. Whatever the
    /// prune removed, the index is written again after it.
    #[test]
    fn a_publish_that_prunes_an_old_set_indexes_the_pool_again() {
        let (repo, runner, scratch) = repo("publish-prune");
        let versions = [
            "0.1.0~dev1000+g1111111",
            "0.1.0~dev2000+g2222222",
            "0.1.0~dev3000+g3333333",
        ];
        for version in versions {
            repo.publish(&build_set(&scratch, version), version)
                .unwrap();
        }
        let before = runner.lines().len();
        let newest = "0.1.0~dev4000+g4444444";
        repo.publish(&build_set(&scratch, newest), newest).unwrap();
        let lines = runner.lines();
        let publish = &lines[before..];
        let indexed = publish
            .iter()
            .filter(|line| *line == "apt-ftparchive packages pool")
            .count();
        assert_eq!(indexed, 2, "{publish:#?}");
        assert!(
            !fs::read_dir(repo.pool())
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains("g1111111")),
            "the oldest set's files leave the pool"
        );
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
    fn verify_detects_a_mixed_set() {
        // A set can become mixed after publishing: someone drops one newer .deb into an old set's
        // manifest, or copies a set directory off another machine. Publishing's own check cannot
        // see that; verification is what does.
        let (repo, _runner, scratch) = repo("verify-mixed");
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        let manifest_path = repo.sets().join(SET).join("manifest.json");
        let mut manifest: SetManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.packages[0].version = NEWER.to_owned();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let report = repo
            .verify(&templar_build::components::Matrix::embedded(), None)
            .unwrap();
        assert!(!report.ok, "{}", report.render());
        let current = &report.sets[0];
        assert_eq!(current.role, "current");
        assert!(
            current
                .problems
                .iter()
                .any(|problem| problem.contains("mixed set")),
            "{:#?}",
            current.problems
        );
        assert!(report.render().ends_with("verify: FAILED\n"));
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn verify_says_unreadable_key_not_tampering() {
        // A root-owned archive verified as the user: the key exists but cannot be read. That
        // used to surface as a signature that "does not verify", which reads like tampering.
        let (repo, _runner, scratch) = repo("verify-unreadable-key");
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        signed(&repo);
        let key = repo.public_key();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::File::open(&key).is_ok() {
            // Running with the privileges to read anything (root): the premise does not hold.
            return;
        }
        let report = repo
            .verify(&templar_build::components::Matrix::embedded(), None)
            .unwrap();
        assert!(!report.ok, "{}", report.render());
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem.contains("cannot be read by this user")),
            "{:#?}",
            report.problems
        );
        assert!(
            !report
                .problems
                .iter()
                .any(|problem| problem.contains("does not verify")),
            "{:#?}",
            report.problems
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn manifest_names_an_archive_that_predates_set_manifests() {
        // Published by a `templar dev` from before set manifests existed: current/previous/pool
        // but no sets/ directory at all. The bare "was never published" sends the reader to
        // re-publish the set, when the archive itself is what predates the concept.
        let (repo, _runner, scratch) = repo("predates-manifests");
        let error = repo.manifest(SET).unwrap_err();
        assert!(
            format!("{error:#}").contains("predates set manifests"),
            "{error:#}"
        );
        fs::create_dir_all(repo.sets()).unwrap();
        let error = repo.manifest(SET).unwrap_err();
        assert!(
            !format!("{error:#}").contains("predates set manifests"),
            "{error:#}"
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn verify_detects_a_missing_component_without_a_source_checkout() {
        // The matrix is passed in, and here it is the copy compiled into the binary — which is
        // the whole point: an installed machine has no `distro/deploy/components.toml` and must
        // still be able to say "this set is missing the compositor".
        let (repo, runner, scratch) = repo("verify-missing");
        let embedded = templar_build::components::Matrix::embedded();
        let mut packages = build_set(&scratch, SET);
        let dropped = packages.pop().expect("the matrix is not empty").0;

        repo.publish(&packages, SET).unwrap();
        signed(&repo);
        let report = repo.verify(&embedded, None).unwrap();
        assert!(!report.ok, "{}", report.render());
        assert!(
            report.sets[0]
                .problems
                .iter()
                .any(|problem| problem.contains("incomplete set") && problem.contains(&dropped)),
            "{:#?}",
            report.sets[0].problems
        );

        // The same archive with the whole set present verifies, and names the pins a rollback
        // would use without running apt.
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        signed(&repo);
        let good = repo.verify(&embedded, None).unwrap();
        assert!(good.ok, "{}", good.render());
        assert_eq!(good.sets[0].pinned.len(), embedded.components.len());
        assert!(good.staged.is_none(), "a finished publish stages nothing");
        // The medium and the target carry the public key, never gnupg/: verification must use it.
        let checks: Vec<String> = runner
            .lines()
            .into_iter()
            .filter(|line| line.starts_with("gpgv "))
            .collect();
        assert!(checks.len() >= 2, "{checks:#?}");
        assert!(
            checks
                .iter()
                .all(|line| line.contains(&repo.public_key().display().to_string())),
            "{checks:#?}"
        );
        assert!(
            !runner.lines().iter().any(|line| line.contains("--verify")),
            "the private signing home must not be consulted to verify"
        );
        let _ = fs::remove_dir_all(scratch);
    }

    #[test]
    fn verify_without_the_public_key_reports_unchecked_signatures() {
        let (repo, _runner, scratch) = repo("verify-nokey");
        let embedded = templar_build::components::Matrix::embedded();
        repo.publish(&build_set(&scratch, SET), SET).unwrap();
        signed(&repo);
        fs::remove_file(repo.public_key()).unwrap();
        let report = repo.verify(&embedded, None).unwrap();
        assert!(!report.ok);
        assert!(
            report.problems.iter().any(|p| p.contains("public key")),
            "{:#?}",
            report.problems
        );
        let _ = fs::remove_dir_all(scratch);
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
