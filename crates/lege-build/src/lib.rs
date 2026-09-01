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
    write_stamp_in(
        &stamp_dir(),
        component,
        &BuildIdentity::current(),
        std::process::id(),
        now_ms(),
    )
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
    if !is_plain_component_name(component) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{component:?} is not a plain component name"),
        ));
    }
    let stamp = RunningStamp {
        component: component.to_owned(),
        build_set: identity.build_set.clone(),
        git_sha: identity.git_sha.clone(),
        dirty: identity.dirty,
        pid,
        started_at_ms: now_ms,
        schema_versions: BTreeMap::new(),
    };

    fs::create_dir_all(dir)?;
    let final_path = dir.join(format!("{component}.json"));
    let temp_path = dir.join(format!(".{component}.{pid}.tmp"));
    let body = serde_json::to_vec_pretty(&stamp).map_err(io::Error::other)?;
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
    fn a_stamp_round_trips_through_the_directory() {
        let dir = tempdir();
        write_stamp_in(
            &dir,
            "lege-shell",
            &identity("0.1.0~dev1+gaaaaaaa"),
            42,
            1000,
        )
        .unwrap();
        let stamps = read_stamps_in(&dir, &|_| true);
        let stamp = &stamps["lege-shell"];
        assert_eq!(stamp.build_set, "0.1.0~dev1+gaaaaaaa");
        assert_eq!(stamp.pid, 42);
        assert_eq!(stamp.started_at_ms, 1000);
    }

    #[test]
    fn a_stamp_whose_process_is_gone_is_not_running() {
        let dir = tempdir();
        write_stamp_in(&dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        write_stamp_in(&dir, "lege-comp", &identity("a"), 43, 1).unwrap();
        let live = read_stamps_in(&dir, &|pid| pid == 43);
        assert_eq!(live.keys().collect::<Vec<_>>(), vec!["lege-comp"]);
    }

    #[test]
    fn one_components_restart_does_not_erase_its_siblings() {
        // The whole reason the stamps share a directory nobody owns.
        let dir = tempdir();
        write_stamp_in(&dir, "lege-comp", &identity("a"), 43, 1).unwrap();
        write_stamp_in(&dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        write_stamp_in(&dir, "lege-shell", &identity("b"), 44, 2).unwrap();
        let stamps = read_stamps_in(&dir, &|_| true);
        assert_eq!(stamps.len(), 2);
        assert_eq!(stamps["lege-shell"].build_set, "b");
        assert_eq!(stamps["lege-comp"].build_set, "a");
    }

    #[test]
    fn a_corrupt_stamp_hides_only_itself() {
        let dir = tempdir();
        write_stamp_in(&dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        fs::write(dir.join("lege-comp.json"), b"{ not json").unwrap();
        let stamps = read_stamps_in(&dir, &|_| true);
        assert_eq!(stamps.keys().collect::<Vec<_>>(), vec!["lege-shell"]);
    }

    #[test]
    fn a_component_name_may_not_escape_its_directory() {
        let dir = tempdir();
        for bad in ["../escape", "a/b", "", ".", ".."] {
            assert!(
                write_stamp_in(&dir, bad, &identity("a"), 1, 1).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn no_temporary_file_survives_a_write() {
        let dir = tempdir();
        write_stamp_in(&dir, "lege-shell", &identity("a"), 42, 1).unwrap();
        let names: Vec<_> = fs::read_dir(&dir)
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
        let dir = tempdir();
        fs::write(
            dir.join("lege-filed.json"),
            br#"{"component":"lege-filed","build_set":"old","git_sha":"0000000","pid":7,"started_at_ms":5}"#,
        )
        .unwrap();
        let stamps = read_stamps_in(&dir, &|_| true);
        assert_eq!(stamps["lege-filed"].build_set, "old");
        assert!(stamps["lege-filed"].schema_versions.is_empty());
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lege-build-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }
}
