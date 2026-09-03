//! Build identity — which build this is, and which build is actually running.
//!
//! Two questions the OS could not answer before this crate existed:
//!
//! 1. **What is installed?** Every package in a set carries the same version string, the
//!    [`BuildSetId`](build_set_id): `0.1.0~dev<epoch>+g<sha7>[.dirty]`. One string across all
//!    thirteen packages turns "is this a coherent set?" into a string comparison, and turns
//!    rollback into `apt-get install --allow-downgrades pkg=<previous>`. `lege doctor` used to
//!    guess at this from package mtimes, and said so in a comment.
//!
//! 2. **What is running?** Every long-lived process writes a *stamp* — a small JSON file under
//!    `$XDG_RUNTIME_DIR/legeos/build/<component>.json` (`/run/legeos/build/` for system services)
//!    — at startup. Neither `lege-comp` nor `lege-shell` serves any D-Bus, so a bus property could
//!    never have answered for them; a file survives a stalled bus, needs no interface version, and
//!    is readable by `lege dev status` with no session at all.
//!
//! **Why not `RuntimeDirectory=`**: systemd would give each unit its own directory and *delete its
//! contents* on restart. Every component stamping into one shared directory is the point — one
//! restart must not erase its siblings' answers. The directory is created by whoever gets there
//! first, and cleared by the tmpfs at reboot.
//!
//! **Stale stamps**: a process that dies leaves its stamp behind. Readers therefore ignore any
//! stamp whose pid is no longer alive, so a crashed component reads as *not running* rather than
//! as running an old build. That check is injectable ([`read_stamps_in`]) so the policy is
//! testable without spawning processes.

#![forbid(unsafe_code)]

pub mod components;
pub mod interfaces;

use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// The build this binary was compiled as.
///
/// Compile-time constants, so reading it cannot fail and cannot lie about a different build.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildIdentity {
    /// The Debian version shared by every package in this build set.
    pub build_set: String,
    /// Abbreviated commit the set was built from.
    pub git_sha: String,
    /// Whether the working tree had uncommitted tracked changes at build time.
    pub dirty: bool,
}

impl BuildIdentity {
    /// This binary's identity, from the values `build.rs` derived.
    #[must_use]
    pub fn current() -> Self {
        Self {
            build_set: env!("LEGEOS_BUILD_SET_ID").to_owned(),
            git_sha: env!("LEGEOS_GIT_SHA").to_owned(),
            dirty: env!("LEGEOS_GIT_DIRTY") == "true",
        }
    }
}

/// Formats a build-set id.
///
/// `<version>~dev<epoch>+g<sha7>[.dirty]`. The `~dev` makes it sort *below* a plain `0.1.0`, so a
/// developer build is never mistaken by apt for an archive release and an archive package always
/// wins an upgrade comparison; `+g<sha7>` is what makes two sets from the same second distinct;
/// `.dirty` is deliberately loud.
#[must_use]
pub fn build_set_id(version: &str, epoch: u64, git_sha: &str, dirty: bool) -> String {
    let sha: String = git_sha.chars().take(7).collect();
    format!(
        "{version}~dev{epoch}+g{sha}{suffix}",
        suffix = if dirty { ".dirty" } else { "" }
    )
}

/// What one `cargo xtask build-set` run produced — `target/debian/build-set.json`.
///
/// Written by xtask, read by `lege dev repo publish`. It lives here rather than in either of them
/// because it is the handoff between the two, and a handoff format that only one side owns drifts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuildSet {
    /// The build-set id: the Debian version every package in the set carries.
    pub id: String,
    pub git_sha: String,
    #[serde(default)]
    pub dirty: bool,
    /// Seconds since the Unix epoch — the `<epoch>` inside the id, so the id can be checked
    /// without re-parsing it.
    pub epoch: u64,
    pub packages: Vec<BuiltPackage>,
    /// Every shipped D-Bus signature in this set, so a deployment can ask whether the daemons it
    /// is about to restart still speak what the clients already running were built against —
    /// without needing a source checkout on the machine being deployed to.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub interfaces: BTreeMap<String, crate::interfaces::InterfaceSignatures>,
    /// What this set's binaries make of each persistent store.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schemas: BTreeMap<String, StoreSchema>,
    /// Toolchain, host and package-index facts about the build. Defaulted, so a manifest written
    /// before provenance existed still reads.
    #[serde(default, skip_serializing_if = "Provenance::is_empty")]
    pub provenance: Provenance,
}

/// What one build set's binaries make of one persistent store.
///
/// There is a single LegeOS state database (`$XDG_STATE_HOME/legeos/session.db`) today, keyed
/// here as `session`; every domain's migrations live in it, so there is one counter rather than
/// one per domain. The map exists because a second store would otherwise have to be smuggled
/// into the first one's numbers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct StoreSchema {
    /// The schema generation this set's binaries write.
    pub version: u32,
    /// The oldest generation that can still read what they wrote.
    pub min_reader_version: u32,
}

/// One `.deb` in a build set.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BuiltPackage {
    /// Component matrix row name.
    pub component: String,
    /// Debian package name.
    pub package: String,
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub version: String,
    /// The `.deb`'s file name, resolved against the directory holding `build-set.json`. A bare
    /// name rather than a path so the set can be read from wherever it was copied to.
    pub file: String,
    #[serde(default)]
    pub size_bytes: u64,
}

impl BuildSet {
    /// Reads a `build-set.json`, resolving each package's file against the manifest's directory.
    ///
    /// # Errors
    /// If the manifest cannot be read or parsed.
    pub fn read(path: &Path) -> Result<(Self, PathBuf), io::Error> {
        let body = fs::read(path)?;
        let set: Self = serde_json::from_slice(&body).map_err(io::Error::other)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok((set, dir))
    }
}

/// A build-set id taken apart again.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedBuildSetId {
    pub version: String,
    pub epoch: u64,
    pub git_sha: String,
    pub dirty: bool,
}

/// Reads a build-set id back into its parts.
///
/// The epoch is what orders two sets. Comparing the strings would not: `~dev9` sorts after
/// `~dev10` alphabetically, and an id built from an older commit can carry a newer epoch, which
/// is correct — the epoch is when it was *built*, and that is what "the previous set" means.
///
/// Returns `None` for anything that is not one of our ids, including an archive version, so a
/// caller can tell "not a dev build" apart from "a dev build I cannot order".
#[must_use]
pub fn parse_build_set_id(id: &str) -> Option<ParsedBuildSetId> {
    let (version, rest) = id.split_once("~dev")?;
    let (epoch, rest) = rest.split_once("+g")?;
    let (git_sha, dirty) = match rest.strip_suffix(".dirty") {
        Some(sha) => (sha, true),
        None => (rest, false),
    };
    if git_sha.is_empty() || !git_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(ParsedBuildSetId {
        version: version.to_owned(),
        epoch: epoch.parse().ok()?,
        git_sha: git_sha.to_owned(),
        dirty,
    })
}

/// The Debian changelog stanza for a build set.
///
/// `debian/changelog` used to carry a hand-written `0.1.0~dev1` while `cargo deb` stamped every
/// package with the build-set id, so a source build and a `cargo deb` build could ship the same
/// commit under two different versions. There is now one authority — the id — and this is the
/// function that turns it into the one thing dpkg reads. The stanza is generated into a
/// *disposable* copy of the tree at build time (`ci/source-build.sh`): writing it into the
/// checkout would make the working tree dirty, which changes the id it was derived from.
///
/// `when` is seconds since the Unix epoch — the same epoch the id carries, so the date in the
/// changelog and the number in the version cannot disagree.
#[must_use]
pub fn changelog_entry(id: &str, distribution: &str, maintainer: &str, when: u64) -> String {
    let commit = parse_build_set_id(id).map_or_else(
        || "an unrecognised id".to_owned(),
        |parsed| {
            format!(
                "commit {}{}",
                parsed.git_sha,
                if parsed.dirty { " (dirty tree)" } else { "" }
            )
        },
    );
    format!(
        "legeos ({id}) {distribution}; urgency=medium\n\
         \n\
         \x20 * Build set {id} from {commit}, generated by `cargo xtask archive changelog`.\n\
         \x20   Do not edit by hand: the build-set id is the one version authority.\n\
         \n\
         \x20-- {maintainer}  {date}\n",
        date = rfc5322_date(when)
    )
}

/// The version in a changelog's head entry — what `dpkg-parsechangelog -S Version` would say.
///
/// Returns `None` when the text does not open with a stanza header, which is how "this file was
/// not generated from a build set" looks to the guard in `debian/rules`.
#[must_use]
pub fn changelog_head_version(text: &str) -> Option<String> {
    let head = text.lines().find(|line| !line.trim().is_empty())?;
    let (_, rest) = head.split_once('(')?;
    let (version, _) = rest.split_once(')')?;
    let version = version.trim();
    (!version.is_empty()).then(|| version.to_owned())
}

/// Puts a generated `stanza` at the head of an existing changelog.
///
/// A previously *generated* head entry is replaced rather than stacked on: two development builds
/// an hour apart must not leave a changelog of a thousand `~dev` entries. A hand-written or
/// released entry — one whose version is not a build-set id — is history, so it survives and the
/// new stanza goes above it.
#[must_use]
pub fn changelog_with_head(existing: &str, stanza: &str) -> String {
    let generated_head = changelog_head_version(existing)
        .and_then(|version| parse_build_set_id(&version))
        .is_some();
    if !generated_head {
        return if existing.trim().is_empty() {
            stanza.to_owned()
        } else {
            format!("{stanza}\n{existing}")
        };
    }
    // A stanza ends at its trailer line, which is the one starting with " -- ".
    let rest: String = existing
        .split_inclusive('\n')
        .skip_while(|line| !line.starts_with(" -- "))
        .skip(1)
        .collect();
    let rest = rest.trim_start_matches('\n');
    if rest.is_empty() {
        stanza.to_owned()
    } else {
        format!("{stanza}\n{rest}")
    }
}

/// `Mon, 17 Aug 2026 12:00:00 +0000` — the date format dpkg requires, in UTC.
///
/// Written out rather than pulled in: this crate has three dependencies and none of them is a
/// calendar, and a changelog date is the only date it will ever need to format.
fn rfc5322_date(when: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = when / 86_400;
    let seconds = when % 86_400;
    // Howard Hinnant's civil-from-days, with 1970-01-01 (a Thursday) as day zero.
    let z = i64::try_from(days).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!(
        "{weekday}, {day:02} {month} {year} {hour:02}:{minute:02}:{second:02} +0000",
        weekday = DAYS[usize::try_from(days % 7).unwrap_or(0)],
        month = MONTHS[usize::try_from(month - 1).unwrap_or(0)],
        hour = seconds / 3600,
        minute = (seconds % 3600) / 60,
        second = seconds % 60,
    )
}

/// Where a build set came from: the toolchain, the machine and the package index it resolved
/// against.
///
/// Every field is optional in practice (an empty string, or `None`) because a set built on a host
/// without one of these tools is still a valid set — provenance describes a build, it does not
/// gate one. Deliberately no hostname and no user name: the question is "was this reproducible?",
/// not "who built it".
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// `rustc --version`, verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rustc: String,
    /// `uname -srm` — kernel and architecture, nothing identifying.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// The Debian architecture the packages were built for, e.g. `amd64`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    /// When the Ubuntu package index this set resolved against was observed, from D1's package
    /// closure report. ISO-8601, as the report writes it. Absent when no report was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubuntu_index_observed_at: Option<String>,
    /// That report's own verdict — `resolved`, `unavailable` or `foreign-release`. A set built
    /// against an unresolved index is not wrong, but it is not a closure either, and the manifest
    /// must not imply otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubuntu_index_resolution: Option<String>,
    /// The closure report the two fields above came from, as given on the command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ubuntu_index_source: Option<String>,
}

impl Provenance {
    /// Nothing was recorded — so nothing is written into the manifest either, and a reader can
    /// tell "not recorded" from "recorded as empty".
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// What a running process claims about itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunningStamp {
    /// Component name — the key of the deployment matrix row, not the binary path.
    pub component: String,
    /// Build set the running binary was compiled as.
    pub build_set: String,
    pub git_sha: String,
    #[serde(default)]
    pub dirty: bool,
    /// Pid of the writer. A stamp whose pid is gone is ignored on read.
    pub pid: u32,
    /// When the stamp was written, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
    /// Schema versions this build can read, per store. Absent in stamps written before the
    /// compatibility work landed, which is why it defaults rather than refusing to parse.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub schema_versions: BTreeMap<String, u32>,
}

impl RunningStamp {
    /// A stamp for this process, with no schema versions.
    #[must_use]
    fn new(component: &str, identity: &BuildIdentity, pid: u32, now_ms: u64) -> Self {
        Self {
            component: component.to_owned(),
            build_set: identity.build_set.clone(),
            git_sha: identity.git_sha.clone(),
            dirty: identity.dirty,
            pid,
            started_at_ms: now_ms,
            schema_versions: BTreeMap::new(),
        }
    }
}

/// Where stamps live for this process.
///
/// `LEGEOS_BUILD_STAMP_DIR` overrides everything (tests, and `lege dev status --stamps`);
/// otherwise `$XDG_RUNTIME_DIR/legeos/build` for a session process and `/run/legeos/build` for a
/// system one, which is the same location seen through the same tmpfs.
#[must_use]
pub fn stamp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LEGEOS_BUILD_STAMP_DIR") {
        return PathBuf::from(dir);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("legeos/build"),
        None => PathBuf::from("/run/legeos/build"),
    }
}

/// Writes this process's stamp. One line in a `main`, before anything that can block.
///
/// Failure is returned, never fatal to the caller: not knowing which build is running is a
/// diagnostic loss, and a shell that refuses to start because a tmpfs is full would be a far
/// worse one.
///
/// # Errors
/// If the stamp directory cannot be created, or the file cannot be written or renamed.
pub fn write_stamp(component: &str) -> io::Result<PathBuf> {
    write_stamp_with(component, BTreeMap::new())
}

/// [`write_stamp`] for a component that also carries schema versions.
///
/// A component that owns or reads a versioned store records what generation of it this build
/// speaks, so a deployment can refuse to install a set whose stores the *previous* set could not
/// read back — the check that makes a rollback safe, made from the running system's own claims
/// rather than from a table someone has to keep up to date.
///
/// # Errors
/// As [`write_stamp`].
pub fn write_stamp_with(
    component: &str,
    schema_versions: BTreeMap<String, u32>,
) -> io::Result<PathBuf> {
    let mut stamp = RunningStamp::new(
        component,
        &BuildIdentity::current(),
        std::process::id(),
        now_ms(),
    );
    stamp.schema_versions = schema_versions;
    write_stamp_value(&stamp_dir(), &stamp)
}

/// [`write_stamp`] with every ambient input passed in — the whole of it is testable.
///
/// The write is atomic: a temporary file in the same directory, then a rename. A reader therefore
/// sees either the previous stamp or the new one, never a half-written one, which matters because
/// `lege dev status` runs while components are restarting.
///
/// # Errors
/// If the component name is not a plain file name, or any filesystem step fails.
pub fn write_stamp_in(
    dir: &Path,
    component: &str,
    identity: &BuildIdentity,
    pid: u32,
    now_ms: u64,
) -> io::Result<PathBuf> {
    write_stamp_value(dir, &RunningStamp::new(component, identity, pid, now_ms))
}

/// Writes a prepared stamp.
fn write_stamp_value(dir: &Path, stamp: &RunningStamp) -> io::Result<PathBuf> {
    let component = stamp.component.as_str();
    let pid = stamp.pid;
    if !is_plain_component_name(component) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{component:?} is not a plain component name"),
        ));
    }

    fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("{component}.json"));
    let temp_path = dir.join(format!(".{component}.{pid}.tmp"));
    let body = serde_json::to_vec_pretty(stamp).map_err(io::Error::other)?;
    {
        let mut file = fs::File::create(&temp_path)?;
        file.write_all(&body)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    match fs::rename(&temp_path, &final_path) {
        Ok(()) => Ok(final_path),
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(err)
        }
    }
}

/// Every live component's stamp, keyed by component name.
///
/// Unreadable and unparsable files are skipped rather than failing the read: one component that
/// wrote nonsense must not hide the twelve that did not.
#[must_use]
pub fn read_stamps() -> BTreeMap<String, RunningStamp> {
    read_stamps_in(&stamp_dir(), &pid_is_alive)
}

/// [`read_stamps`] against a given directory and liveness test.
#[must_use]
pub fn read_stamps_in(
    dir: &Path,
    is_alive: &dyn Fn(u32) -> bool,
) -> BTreeMap<String, RunningStamp> {
    let Ok(entries) = fs::read_dir(dir) else {
        return BTreeMap::new();
    };
    let mut stamps = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(body) = fs::read(&path) else { continue };
        let Ok(stamp) = serde_json::from_slice::<RunningStamp>(&body) else {
            continue;
        };
        if !is_alive(stamp.pid) {
            continue;
        }
        stamps.insert(stamp.component.clone(), stamp);
    }
    stamps
}

/// Whether a pid names a live process. `/proc` is the only authority available without signals,
/// and this crate deliberately sends none — a diagnostic read must never touch a running process.
#[must_use]
pub fn pid_is_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).is_dir()
}

fn is_plain_component_name(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.starts_with('.')
        && component
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[must_use]
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(build_set: &str) -> BuildIdentity {
        BuildIdentity {
            build_set: build_set.to_owned(),
            git_sha: "abcdef1".to_owned(),
            dirty: false,
        }
    }

    #[test]
    fn a_build_set_id_says_version_epoch_and_commit() {
        assert_eq!(
            build_set_id("0.1.0", 1_756_700_000, "abcdef1234", false),
            "0.1.0~dev1756700000+gabcdef1"
        );
        assert_eq!(
            build_set_id("0.1.0", 1_756_700_000, "abcdef1234", true),
            "0.1.0~dev1756700000+gabcdef1.dirty"
        );
    }

    #[test]
    fn a_dev_build_set_sorts_below_the_plain_release_version() {
        // `~` is what makes this true for apt; assert the shape we rely on rather than shelling
        // out to dpkg, which is not present everywhere the tests run.
        assert!(build_set_id("0.1.0", 1, "aaaaaaa", false).starts_with("0.1.0~dev"));
    }

    #[test]
    fn a_build_set_id_comes_apart_again() {
        let parsed = parse_build_set_id("0.1.0~dev1756700000+gabcdef1.dirty").unwrap();
        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.epoch, 1_756_700_000);
        assert_eq!(parsed.git_sha, "abcdef1");
        assert!(parsed.dirty);
        assert!(
            !parse_build_set_id("0.1.0~dev1756700000+gabcdef1")
                .unwrap()
                .dirty
        );
    }

    #[test]
    fn an_archive_version_is_not_a_build_set_id() {
        assert!(parse_build_set_id("0.1.0-1").is_none());
        assert!(parse_build_set_id("0.1.0~dev1+gzzzz").is_none());
        assert!(parse_build_set_id("").is_none());
    }

    #[test]
    fn build_sets_are_ordered_by_epoch_not_by_string() {
        // "~dev9" sorts after "~dev10" as text; the epoch is what actually orders two builds.
        let older = build_set_id("0.1.0", 9, "aaaaaaa", false);
        let newer = build_set_id("0.1.0", 10, "bbbbbbb", false);
        assert!(
            older > newer,
            "the string comparison is the trap this guards against"
        );
        assert!(
            parse_build_set_id(&older).unwrap().epoch < parse_build_set_id(&newer).unwrap().epoch
        );
    }

    #[test]
    fn the_changelog_head_entry_is_derived_from_the_build_set_id() {
        // The failure this guards against is the one that was in the tree: debian/changelog said
        // 0.1.0~dev1 while every .deb said 0.1.0~dev<epoch>+g<sha>. One authority means the
        // stanza must come apart into exactly the id it was built from.
        let id = build_set_id("0.1.0", 1_788_439_680, "8b284c9ab", false);
        let stanza = changelog_entry(
            &id,
            "resolute",
            "LegeOS contributors <dev@legeos.org>",
            1_788_439_680,
        );
        assert_eq!(
            changelog_head_version(&stanza).as_deref(),
            Some(id.as_str())
        );
        let parsed = parse_build_set_id(&changelog_head_version(&stanza).unwrap()).unwrap();
        assert_eq!(parsed.epoch, 1_788_439_680);
        assert_eq!(parsed.git_sha, "8b284c9");
        assert!(stanza.starts_with(&format!("legeos ({id}) resolute; urgency=medium")));
        assert!(
            stanza.contains(" -- LegeOS contributors <dev@legeos.org>  "),
            "{stanza}"
        );
        // Epoch 1788439680 is 2026-09-03T12:48:00Z; the date must be the id's own moment.
        assert!(
            stanza.contains("Thu, 03 Sep 2026 12:48:00 +0000"),
            "{stanza}"
        );
        // A dirty set says so in the body, because a dirty build must never look reproducible.
        let dirty = changelog_entry(
            &build_set_id("0.1.0", 1_788_439_680, "8b284c9ab", true),
            "resolute",
            "LegeOS contributors <dev@legeos.org>",
            1_788_439_680,
        );
        assert!(dirty.contains("dirty tree"), "{dirty}");
        // The pre-existing hand-written stanza shape still parses, so the guard can compare it.
        assert_eq!(
            changelog_head_version("legeos (0.1.0~dev1) resolute; urgency=medium\n").as_deref(),
            Some("0.1.0~dev1")
        );
        assert!(changelog_head_version("").is_none());

        // Regenerating replaces the generated head entry; a released one is history and survives.
        let older = changelog_entry(
            &build_set_id("0.1.0", 1, "aaaaaaa", false),
            "resolute",
            "x <x@x>",
            1,
        );
        let regenerated = changelog_with_head(&older, &stanza);
        assert_eq!(
            changelog_head_version(&regenerated).as_deref(),
            Some(id.as_str())
        );
        assert!(
            !regenerated.contains("~dev1+gaaaaaaa"),
            "a generated entry must be replaced, not stacked: {regenerated}"
        );
        let released = "legeos (0.1.0-1) resolute; urgency=medium\n\n  * release\n\n -- x <x@x>  Thu, 03 Sep 2026 12:48:00 +0000\n";
        let prepended = changelog_with_head(released, &stanza);
        assert_eq!(
            changelog_head_version(&prepended).as_deref(),
            Some(id.as_str())
        );
        assert!(
            prepended.contains("0.1.0-1"),
            "history must survive: {prepended}"
        );
        assert_eq!(changelog_with_head("", &stanza), stanza);
    }

    #[test]
    fn a_stamp_round_trips_through_the_directory() {
        let scratch = tempdir();
        let dir = scratch.path();
        write_stamp_in(
            dir,
            "lege-shell",
            &identity("0.1.0~dev1+gaaaaaaa"),
            42,
            1000,
        )
        .unwrap();
        let stamps = read_stamps_in(dir, &|_| true);
        let stamp = &stamps["lege-shell"];
        assert_eq!(stamp.build_set, "0.1.0~dev1+gaaaaaaa");
        assert_eq!(stamp.pid, 42);
        assert_eq!(stamp.started_at_ms, 1000);
    }

    #[test]
    fn a_stamp_whose_process_is_gone_is_not_running() {
        let scratch = tempdir();
        let dir = scratch.path();
        write_stamp_in(dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        write_stamp_in(dir, "lege-comp", &identity("a"), 43, 1).unwrap();
        let live = read_stamps_in(dir, &|pid| pid == 43);
        assert_eq!(live.keys().collect::<Vec<_>>(), vec!["lege-comp"]);
    }

    #[test]
    fn one_components_restart_does_not_erase_its_siblings() {
        // The whole reason the stamps share a directory nobody owns.
        let scratch = tempdir();
        let dir = scratch.path();
        write_stamp_in(dir, "lege-comp", &identity("a"), 43, 1).unwrap();
        write_stamp_in(dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        write_stamp_in(dir, "lege-shell", &identity("b"), 44, 2).unwrap();
        let stamps = read_stamps_in(dir, &|_| true);
        assert_eq!(stamps.len(), 2);
        assert_eq!(stamps["lege-shell"].build_set, "b");
        assert_eq!(stamps["lege-comp"].build_set, "a");
    }

    #[test]
    fn a_corrupt_stamp_hides_only_itself() {
        let scratch = tempdir();
        let dir = scratch.path();
        write_stamp_in(dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        fs::write(dir.join("lege-comp.json"), b"{ not json").unwrap();
        let stamps = read_stamps_in(dir, &|_| true);
        assert_eq!(stamps.keys().collect::<Vec<_>>(), vec!["lege-shell"]);
    }

    #[test]
    fn a_component_name_may_not_escape_its_directory() {
        let scratch = tempdir();
        let dir = scratch.path();
        for bad in ["../escape", "a/b", "", ".", ".."] {
            assert!(
                write_stamp_in(dir, bad, &identity("a"), 1, 1).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn no_temporary_file_survives_a_write() {
        let scratch = tempdir();
        let dir = scratch.path();
        write_stamp_in(dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        let names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["lege-shell.json".to_owned()]);
    }

    #[test]
    fn a_stamp_written_by_an_older_build_still_parses() {
        // The additive-field rule: `lege dev status` from a new build must be able to read the
        // stamp a not-yet-restarted old component wrote.
        let scratch = tempdir();
        let dir = scratch.path();
        fs::write(
            dir.join("lege-filed.json"),
            br#"{"component":"lege-filed","build_set":"old","git_sha":"0000000","pid":7,"started_at_ms":5}"#,
        )
        .unwrap();
        let stamps = read_stamps_in(dir, &|_| true);
        assert_eq!(stamps["lege-filed"].build_set, "old");
        assert!(stamps["lege-filed"].schema_versions.is_empty());
    }

    /// A scratch directory that removes itself when the test ends.
    ///
    /// `tempfile` rather than a hand-rolled path under `/tmp`: the hand-rolled version cleaned up
    /// at the *start* of each test, which is one run behind — 287 directories had accumulated on
    /// the machine this was written on before anyone looked.
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a scratch directory")
    }
}
