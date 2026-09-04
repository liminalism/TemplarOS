//! Index checkpoints: read-only `@home` snapshots, and the metadata delta between two of them.
//!
//! The file index has no way to learn what happened while it was not watching, so it re-walks the
//! tree — a cost proportional to the size of the tree rather than to the size of the change. On
//! btrfs there is a better answer: keep a read-only snapshot of `@home` as a checkpoint, and when
//! the index comes back, ask btrfs for the difference between that checkpoint and a fresh one.
//! `btrfs send --no-data` writes exactly that difference and reads no file data to do it.
//!
//! **Unadopted.** Nothing autostarts this and nothing calls it by default; it exists so the idea
//! can be measured (`docs/measurements/`) before anyone depends on it. The decoder that turns a
//! stream into index mutations is `lege_file_index::btrfs_delta`.
//!
//! Why it lives in hostd rather than in the per-user file daemon: `BTRFS_IOC_SEND` requires
//! `CAP_SYS_ADMIN`, and ADR-0001/ADR-0003 say a per-user daemon does not get capabilities — the one
//! privileged system-bus surface does. Snapshot creation and sealing reuse [`Snapshotter`], which
//! `lege-hostd` already drives for deployments; nothing new shells out to btrfs.
//!
//! `@snapshots` (declared in `distro/deploy/layout.toml`, mounted at `/.snapshots` by
//! `installer/provision.sh`) is consumed by nothing today. This is what it is for.
//!
//! One thing a consumer cannot work out for itself, so this module has to say it: every path in
//! the stream is relative to the **root of the snapshotted subvolume**, not to whatever directory
//! the consumer happens to index. A delta of `@home` names `dk/notes.md` and `other/secret`, and
//! `@home` mounts at `/home` — so the index root `/home/dk` is a *subdirectory* of the namespace
//! the stream speaks in. [`DeltaStream::subvolume`] carries the mount point that makes those paths
//! absolute; joining them onto the index root instead produces `/home/dk/dk/notes.md`, which
//! exists nowhere, and a delta that quietly applies nothing.

use crate::{
    clock::{self, Clock},
    cmd::CommandRunner,
    snapshot::Snapshotter,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// One retained checkpoint.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    /// One path segment, safe as a btrfs subvolume name — the same grammar the deployment id uses.
    pub id: String,
    /// Where the read-only snapshot lives.
    pub path: String,
    /// The live subvolume it was taken from.
    pub subvolume: String,
    /// btrfs's own identity for the snapshot, when `btrfs subvolume show` could be read. A delta
    /// is only valid between two snapshots of the same lineage, and this is how a consumer checks
    /// that the stream it decoded really was taken against the checkpoint it thinks it holds.
    pub uuid: Option<String>,
    pub ctransid: Option<u64>,
    pub created_ms: i64,
}

/// A written delta stream.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeltaStream {
    pub from: String,
    pub to: String,
    /// Where the live subvolume these paths are relative to is mounted — `/home` for `@home`.
    /// `lege_file_index::btrfs_delta::DeltaTarget::subvolume_path` wants exactly this, and it is
    /// not the same thing as the index root.
    pub subvolume: String,
    /// Where the stream was written. Mode 0600, root-owned.
    ///
    /// Handing this to the per-user `lege-filed` is the unsolved half of adoption: no
    /// `org.legeos.Host.*` method passes a file descriptor today (`type="h"` appears nowhere in
    /// `interfaces/dbus/`), and a metadata delta names every path under `@home`, so it must not be
    /// world-readable. Passing an fd, or resolving the caller's uid from the bus and handing the
    /// file over, are both new surface — deliberately not built for a spike.
    pub path: String,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// What a checkpoint request may ask for. A JSON argument rather than a widened signature, so this
/// can grow without a contract change (`interfaces/README.md`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CheckpointRequest {
    /// The live subvolume to checkpoint. Empty means the configured default (`@home`).
    ///
    /// `@home` specifically, and never `@`: btrfs snapshots stop at a nested subvolume boundary,
    /// so a snapshot of `@` would contain an empty `home/` directory and the delta would report
    /// the user's entire home as deleted.
    pub subvolume: String,
}

/// What a delta request may ask for.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DeltaRequest {
    pub from: String,
    pub to: String,
    /// The live subvolume both checkpoints were taken from. Empty means the configured default
    /// (`@home`), which is what [`IndexCheckpoints::create`] snapshots when a request names none.
    /// It is echoed into [`DeltaStream::subvolume`] because the stream's paths mean nothing
    /// without it, and a checkpoint on disk does not record which subvolume it came from.
    pub subvolume: String,
    /// Include file data. Off by default and the only reason this method is cheap: the index needs
    /// the namespace and the metadata, never the contents.
    pub with_data: bool,
}

/// The checkpoint store.
#[derive(Debug)]
pub struct IndexCheckpoints {
    snapshotter: Arc<dyn Snapshotter>,
    commands: Arc<dyn CommandRunner>,
    clock: Arc<dyn Clock>,
    /// Where checkpoints are created — a directory on the same btrfs filesystem as the source.
    store: PathBuf,
    /// Where delta streams are written.
    streams: PathBuf,
    /// The subvolume checkpointed when a request names none.
    default_subvolume: PathBuf,
}

impl IndexCheckpoints {
    pub fn new(
        snapshotter: Arc<dyn Snapshotter>,
        commands: Arc<dyn CommandRunner>,
        clock: Arc<dyn Clock>,
        store: PathBuf,
        streams: PathBuf,
        default_subvolume: PathBuf,
    ) -> Self {
        Self {
            snapshotter,
            commands,
            clock,
            store,
            streams,
            default_subvolume,
        }
    }

    /// Take a read-only checkpoint of `request.subvolume`.
    pub fn create(&self, request: &CheckpointRequest) -> Result<Checkpoint> {
        let subvolume = if request.subvolume.is_empty() {
            self.default_subvolume.clone()
        } else {
            PathBuf::from(&request.subvolume)
        };
        validate_subvolume(&subvolume)?;
        std::fs::create_dir_all(&self.store)
            .with_context(|| format!("creating {}", self.store.display()))?;

        let id = self.mint_id(&subvolume);
        let path = self.store.join(&id);
        // Reuses the deployment path's snapshotter rather than shelling out again: create, then
        // seal. `btrfs send` refuses anything that is not read-only, so the seal is not optional.
        self.snapshotter
            .create_rw(&subvolume, &path)
            .with_context(|| format!("snapshotting {}", subvolume.display()))?;
        self.snapshotter
            .seal_read_only(&path)
            .with_context(|| format!("sealing {}", path.display()))?;

        let (uuid, ctransid) = self.identity(&path);
        Ok(Checkpoint {
            id,
            path: path.to_string_lossy().into_owned(),
            subvolume: subvolume.to_string_lossy().into_owned(),
            uuid,
            ctransid,
            created_ms: self.now_ms(),
        })
    }

    /// Every checkpoint currently on disk, oldest id first.
    pub fn list(&self) -> Result<Vec<Checkpoint>> {
        let mut checkpoints = Vec::new();
        let reader = match std::fs::read_dir(&self.store) {
            Ok(reader) => reader,
            // No store yet is not an error: nothing has been checkpointed.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(checkpoints),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", self.store.display()));
            }
        };
        for entry in reader.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let (uuid, ctransid) = self.identity(&path);
            let created_ms = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.created().ok())
                .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| since.as_millis() as i64);
            checkpoints.push(Checkpoint {
                id,
                path: path.to_string_lossy().into_owned(),
                subvolume: self.default_subvolume.to_string_lossy().into_owned(),
                uuid,
                ctransid,
                created_ms,
            });
        }
        checkpoints.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(checkpoints)
    }

    /// `btrfs send --no-data -p <from> -f <stream> <to>` — the metadata difference between two
    /// checkpoints, written to a file rather than piped, so nothing has to buffer it in memory.
    pub fn delta(&self, request: &DeltaRequest) -> Result<DeltaStream> {
        let from = self.checkpoint_path(&request.from)?;
        let to = self.checkpoint_path(&request.to)?;
        let subvolume = if request.subvolume.is_empty() {
            self.default_subvolume.clone()
        } else {
            PathBuf::from(&request.subvolume)
        };
        validate_subvolume(&subvolume)?;
        if from == to {
            bail!("a delta from a checkpoint to itself is empty by definition");
        }
        std::fs::create_dir_all(&self.streams)
            .with_context(|| format!("creating {}", self.streams.display()))?;
        let stream = self
            .streams
            .join(format!("{}..{}.stream", request.from, request.to));
        // A stale stream from an interrupted run would otherwise be appended to or mistaken for
        // this one's output.
        let _ = std::fs::remove_file(&stream);

        let (from, to, stream_path) = (
            from.to_string_lossy().into_owned(),
            to.to_string_lossy().into_owned(),
            stream.to_string_lossy().into_owned(),
        );
        let mut args = vec!["send"];
        if !request.with_data {
            args.push("--no-data");
        }
        args.extend(["-p", from.as_str(), "-f", stream_path.as_str(), to.as_str()]);

        let started = std::time::Instant::now();
        let output = self
            .commands
            .run("btrfs", &args)
            .context("running btrfs send")?;
        if !output.status.success() {
            let _ = std::fs::remove_file(&stream);
            bail!(
                "btrfs send failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let duration_ms = started.elapsed().as_millis() as u64;
        // Root-only until the fd-passing question above is answered: a metadata delta lists every
        // path under @home, and every local user can read a world-readable file.
        restrict(&stream);
        let bytes = std::fs::metadata(&stream).map_or(0, |metadata| metadata.len());
        Ok(DeltaStream {
            from: request.from.clone(),
            to: request.to.clone(),
            subvolume: subvolume.to_string_lossy().into_owned(),
            path: stream_path,
            bytes,
            duration_ms,
        })
    }

    /// Delete a checkpoint. The delta protocol retains the newer snapshot and drops the older one,
    /// so this runs once per successful catch-up.
    pub fn discard(&self, id: &str) -> Result<()> {
        let path = self.checkpoint_path(id)?;
        self.snapshotter
            .delete(&path)
            .with_context(|| format!("deleting {}", path.display()))
    }

    /// Resolve a checkpoint id to its path, refusing anything that is not a single safe segment.
    ///
    /// This is the one place a caller-supplied string reaches the filesystem, and the caller is a
    /// D-Bus peer: `../../etc` must not become a path this daemon deletes.
    fn checkpoint_path(&self, id: &str) -> Result<PathBuf> {
        if !is_safe_id(id) {
            bail!("{id:?} is not a checkpoint id");
        }
        let path = self.store.join(id);
        if !path.is_dir() {
            bail!("no checkpoint {id}");
        }
        Ok(path)
    }

    fn mint_id(&self, subvolume: &Path) -> String {
        let name = subvolume.file_name().map_or_else(
            || "root".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let name: String = name
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
            .collect();
        let name = if name.is_empty() {
            "root".to_owned()
        } else {
            name
        };
        format!("index-{name}-{}", clock::stamp(self.clock.now()))
    }

    fn now_ms(&self) -> i64 {
        self.clock
            .now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as i64)
    }

    /// `btrfs subvolume show`'s UUID and generation, or `None` when it cannot be read. Diagnostic
    /// only: nothing here fails because a snapshot would not describe itself.
    fn identity(&self, path: &Path) -> (Option<String>, Option<u64>) {
        let Ok(output) = self
            .commands
            .run("btrfs", &["subvolume", "show", &path.to_string_lossy()])
        else {
            return (None, None);
        };
        if !output.status.success() {
            return (None, None);
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut uuid = None;
        let mut ctransid = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("UUID:") {
                uuid = Some(value.trim().to_owned());
            } else if let Some(value) = line.strip_prefix("Generation:") {
                ctransid = value.trim().parse().ok();
            }
        }
        (uuid, ctransid)
    }
}

/// A checkpoint id is one path segment of the same grammar `distro/deploy/layout.toml` gives a
/// deployment id: nothing that can escape the store or confuse a shell-free `btrfs` argument.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
        && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
}

/// A subvolume path from a bus peer: absolute, and with no way to walk out of wherever it looks
/// like it points.
fn validate_subvolume(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("{} is not an absolute path", path.display());
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("{} contains ..", path.display());
    }
    Ok(())
}

fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clock::FixedClock, cmd::FakeCommandRunner, snapshot::FakeSnapshotter};
    use std::time::{Duration, UNIX_EPOCH};

    fn store(dir: &Path) -> (IndexCheckpoints, Arc<FakeCommandRunner>) {
        let commands = Arc::new(FakeCommandRunner::default());
        let checkpoints = IndexCheckpoints::new(
            Arc::new(FakeSnapshotter::default()),
            commands.clone(),
            Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_800_000_000))),
            dir.join("snapshots/index"),
            dir.join("streams"),
            dir.join("home"),
        );
        (checkpoints, commands)
    }

    #[test]
    fn a_checkpoint_is_created_and_then_sealed_read_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::write(dir.path().join("home/marker"), b"hi").unwrap();
        let (checkpoints, _) = store(dir.path());

        let checkpoint = checkpoints.create(&CheckpointRequest::default()).unwrap();
        assert!(checkpoint.id.starts_with("index-home-"));
        assert!(Path::new(&checkpoint.path).join("marker").exists());
        assert_eq!(checkpoints.list().unwrap().len(), 1);
    }

    #[test]
    fn a_delta_runs_send_with_no_data_and_a_parent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let (checkpoints, commands) = store(dir.path());
        let first = checkpoints.create(&CheckpointRequest::default()).unwrap();
        // The clock is fixed, so a second checkpoint would collide; place it directly.
        let second_path = dir.path().join("snapshots/index/index-home-second");
        std::fs::create_dir_all(&second_path).unwrap();

        let delta = checkpoints
            .delta(&DeltaRequest {
                from: first.id.clone(),
                to: "index-home-second".to_owned(),
                with_data: false,
                ..DeltaRequest::default()
            })
            .unwrap();
        assert!(delta.path.ends_with(".stream"));

        let send = commands
            .calls()
            .into_iter()
            .find(|call| call.args.first().is_some_and(|arg| arg == "send"))
            .expect("a send call");
        assert!(send.args.contains(&"--no-data".to_owned()));
        assert!(send.args.contains(&"-p".to_owned()));
        assert!(send.args.contains(&first.path.clone()));
        assert!(
            send.args
                .contains(&second_path.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn a_delta_says_which_subvolume_its_paths_are_relative_to() {
        // Without this a consumer has to guess, and the obvious guess — its own index root — is
        // wrong whenever the index covers a subdirectory of the subvolume, which is the normal
        // case: `@home` mounts at `/home` and the root is `/home/<user>`.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let (checkpoints, _) = store(dir.path());
        let first = checkpoints.create(&CheckpointRequest::default()).unwrap();
        std::fs::create_dir_all(dir.path().join("snapshots/index/index-home-second")).unwrap();

        let delta = checkpoints
            .delta(&DeltaRequest {
                from: first.id.clone(),
                to: "index-home-second".to_owned(),
                ..DeltaRequest::default()
            })
            .unwrap();
        assert_eq!(delta.subvolume, first.subvolume);
        assert_eq!(
            delta.subvolume,
            dir.path().join("home").to_string_lossy(),
            "the default subvolume, not the store and not an index root"
        );

        let named = checkpoints
            .delta(&DeltaRequest {
                from: first.id.clone(),
                to: "index-home-second".to_owned(),
                subvolume: "/srv/data".to_owned(),
                ..DeltaRequest::default()
            })
            .unwrap();
        assert_eq!(named.subvolume, "/srv/data");

        let refused = checkpoints.delta(&DeltaRequest {
            from: first.id,
            to: "index-home-second".to_owned(),
            subvolume: "relative/home".to_owned(),
            ..DeltaRequest::default()
        });
        assert!(
            refused.is_err(),
            "a relative subvolume is not a mount point"
        );
    }

    #[test]
    fn a_checkpoint_id_cannot_escape_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let (checkpoints, _) = store(dir.path());
        for id in ["../../etc", "..", "a/b", "", "-leading", "with space"] {
            assert!(
                checkpoints.discard(id).is_err(),
                "{id:?} was accepted as a checkpoint id"
            );
        }
    }

    #[test]
    fn a_subvolume_argument_must_be_absolute_and_free_of_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let (checkpoints, _) = store(dir.path());
        for subvolume in ["relative/home", "/home/../etc"] {
            let error = checkpoints
                .create(&CheckpointRequest {
                    subvolume: subvolume.to_owned(),
                })
                .expect_err("must be refused");
            assert!(
                error.to_string().contains("absolute") || error.to_string().contains(".."),
                "{error}"
            );
        }
    }

    #[test]
    fn a_delta_between_a_checkpoint_and_itself_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let (checkpoints, _) = store(dir.path());
        let first = checkpoints.create(&CheckpointRequest::default()).unwrap();
        let error = checkpoints
            .delta(&DeltaRequest {
                from: first.id.clone(),
                to: first.id,
                with_data: false,
                ..DeltaRequest::default()
            })
            .expect_err("must be refused");
        assert!(error.to_string().contains("itself"), "{error}");
    }

    #[test]
    fn discarding_a_checkpoint_deletes_the_subvolume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        let (checkpoints, _) = store(dir.path());
        let checkpoint = checkpoints.create(&CheckpointRequest::default()).unwrap();
        checkpoints.discard(&checkpoint.id).unwrap();
        assert!(checkpoints.list().unwrap().is_empty());
    }

    #[test]
    fn listing_an_absent_store_is_empty_rather_than_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (checkpoints, _) = store(dir.path());
        assert!(checkpoints.list().unwrap().is_empty());
    }
}
