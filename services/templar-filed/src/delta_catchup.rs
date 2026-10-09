//! Catching a root up from a btrfs metadata delta instead of re-walking it.
//!
//! A downtime repair costs what the *tree* costs: a no-op reconcile of a 153,327-entry fixture
//! took 3.24 s and a real `$HOME` is several times that
//! (`@templaros.observation.reconciliation-cost-tracks-tree-size-not-change-size`). `btrfs send
//! --no-data` between two read-only snapshots costs what the *change* costs — flat within 4 ms
//! across a hundredfold tree, about 24 µs per changed path — so where the root sits on btrfs and
//! the daemon still holds the checkpoint it left behind, the delta answers the same question for
//! a fraction of the work.
//!
//! Everything here is a precondition that can fail, and every failure is `None` and a reconcile.
//! There is no configuration to get wrong and no way to end up with a half-applied delta standing
//! in for a pass: either the whole batch commits or the recursive path runs, exactly as it does
//! today. On the ext4 development host every call returns `None` at step 1.
//!
//! The privileged half lives in `templar-hostd` because `BTRFS_IOC_SEND` needs `CAP_SYS_ADMIN`
//! (ADR-0001, ADR-0003); this is the unprivileged consumer of
//! `org.templaros.Host.IndexCheckpoint1`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use templar_file_index::{
    EntryKind, Index, MutationBatch, NativePath, RootStatus,
    btrfs_delta::{self, DeltaReport, DeltaTarget, FsMetadata},
};

/// Above this, give up and reconcile.
///
/// The stream is proportional to changed extents, not to changed paths, and the measured cost is
/// about 420 B per path — so 16 MiB is roughly 40,000 paths. Past that the measurements stop
/// favouring the delta anyway: the build-shaped burst of 5,006 mutations was the one scenario
/// where a crawl won outright, and a dormant window big enough to fill 16 MiB is that case
/// several times over. Overridable with `TEMPLAR_FILED_DELTA_CAP_BYTES` so the number can be moved
/// by measurement rather than by editing this file.
pub const DEFAULT_CAP_BYTES: u64 = 16 * 1024 * 1024;

fn cap_bytes() -> u64 {
    std::env::var("TEMPLAR_FILED_DELTA_CAP_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CAP_BYTES)
}

/// One retained checkpoint, as `org.templaros.Host.IndexCheckpoint1` reports it.
///
/// A local mirror of `templar_hostd::index_checkpoint::Checkpoint` rather than a shared type: hostd
/// is a binary crate on the other side of a bus, the contract between them is the JSON in
/// `interfaces/dbus/org.templaros.Host.IndexCheckpoint1.xml`, and `#[serde(default)]` is what lets
/// either side grow a field without the other one failing to parse.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Checkpoint {
    pub id: String,
    pub path: String,
    pub subvolume: String,
    pub exclusive_bytes: Option<u64>,
}

/// A written delta stream, as the interface describes it. `path` is the name the file *had*:
/// hostd unlinks it before returning the descriptor, so it is a label, not a way back to the
/// bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DeltaStreamInfo {
    pub from: String,
    pub to: String,
    /// Where the live subvolume the stream's paths are relative to is mounted — `/home` for
    /// `@home`. Not the index root, and joining stream paths onto the index root instead
    /// addresses nothing that exists.
    pub subvolume: String,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// The four methods this needs from the privileged daemon.
///
/// A trait so the whole catch-up can be tested without a system bus, a btrfs filesystem or root
/// — none of which a test in this repository is allowed to require.
pub trait Checkpoints: std::fmt::Debug + Send + Sync {
    fn list(&self) -> anyhow::Result<Vec<Checkpoint>>;
    fn create(&self, subvolume: &Path) -> anyhow::Result<Checkpoint>;
    /// The delta between two checkpoints as an open stream. The descriptor is the only route to
    /// the bytes.
    fn delta_open(
        &self,
        from: &str,
        to: &str,
        subvolume: &Path,
    ) -> anyhow::Result<(std::fs::File, DeltaStreamInfo)>;
    fn discard(&self, id: &str) -> anyhow::Result<()>;
}

/// A pointer is usable only for the exact indexed view it was checkpointed against.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct Baseline {
    subvolume: String,
    checkpoint: String,
    root: i64,
    generation: u64,
    policy_hash: String,
    cache_identity: String,
}

#[derive(Debug)]
struct Candidate {
    checkpoints: Arc<dyn Checkpoints>,
    baseline: Baseline,
    retained: std::sync::atomic::AtomicBool,
}

impl Drop for Candidate {
    fn drop(&mut self) {
        if !self.retained.load(std::sync::atomic::Ordering::Relaxed)
            && let Err(error) = self.checkpoints.discard(&self.baseline.checkpoint)
        {
            tracing::warn!(%error, "could not discard an unapplied index checkpoint");
        }
    }
}

/// A pre-walk checkpoint: a post-walk snapshot alone can hide a late creation permanently.
#[derive(Debug)]
pub struct BaselineProbe {
    before: Candidate,
    root_path: NativePath,
}

/// The candidate snapshot is not published until apply, freshness and watch coverage succeed.
#[derive(Debug)]
pub struct CatchUp {
    pub batch: MutationBatch,
    pub report: DeltaReport,
    pub rescan: Vec<NativePath>,
    pub stream: DeltaStreamInfo,
    candidate: Candidate,
    retire: String,
}

#[derive(Debug)]
pub struct DeltaCatchUp {
    checkpoints: Arc<dyn Checkpoints>,
    state_path: PathBuf,
    cap_bytes: u64,
}

impl DeltaCatchUp {
    #[must_use]
    pub fn new(checkpoints: Arc<dyn Checkpoints>, state_path: PathBuf) -> Self {
        Self {
            checkpoints,
            state_path,
            cap_bytes: cap_bytes(),
        }
    }

    /// Every doubt falls back to reconciliation. A pending apply marker means a crash may have
    /// committed a delta without advancing the pointer: do not replay it (rename is not a
    /// general idempotent operation). Only a complete root repair can establish a new baseline.
    pub fn try_catch_up(&self, index: &Index, root: &RootStatus, mounts: &str) -> Option<CatchUp> {
        if self.pending_path(root.root.0).exists() {
            return None;
        }
        let subvolume = btrfs_subvolume_of(&root.path, mounts)?;
        let current = index.root_status(root.root).ok()?;
        if !current.complete {
            return None;
        }
        let from = self
            .read_baseline(root.root.0)
            .filter(|baseline| {
                Path::new(&baseline.subvolume) == subvolume
                    && baseline.root == root.root.0
                    && baseline.generation == current.generation.0
                    && baseline.policy_hash == current.policy_hash
                    && baseline.cache_identity == index.cache_identity()
            })
            .filter(|baseline| {
                self.checkpoints
                    .list()
                    .is_ok_and(|held| held.iter().any(|c| c.id == baseline.checkpoint))
            })?;
        let candidate = self.plant(index, &subvolume, &current)?;
        let (decoded, stream) = self.read_delta(&from, &candidate.baseline)?;
        let policy = index.root_policy(root.root).ok()?;
        let plan = btrfs_delta::plan(&decoded)
            .map_err(|error| tracing::warn!(%error, "the delta stream did not replay"))
            .ok()?;
        let target = DeltaTarget {
            root: root.root,
            root_path: root.path.clone(),
            subvolume_path: NativePath::from_path(&subvolume),
            policy,
        };
        if !target.root_path.starts_with(&target.subvolume_path) {
            return None;
        }
        let (batch, report) = btrfs_delta::to_mutations(&plan, &target, &FsMetadata);
        Some(CatchUp {
            rescan: rescan_targets(&batch),
            batch,
            report,
            stream,
            candidate,
            retire: from.checkpoint,
        })
    }

    /// Durably fence the index commit before applying. Failure to fence means do not apply.
    pub fn prepare_apply(&self, catch_up: &CatchUp) -> std::io::Result<()> {
        atomic_write(
            &self.pending_path(catch_up.candidate.baseline.root),
            b"reconcile before replay\n",
        )
    }

    /// Publish after apply and watch recovery. Crash recovery is conservative: until both the
    /// atomic pointer and removal of the pending marker are durable, restart reconciles instead.
    pub fn committed(&self, catch_up: &CatchUp, root: &RootStatus) -> std::io::Result<()> {
        if !root.complete
            || root.state != templar_file_index::RootState::Current
            || root.root.0 != catch_up.candidate.baseline.root
            || root.policy_hash != catch_up.candidate.baseline.policy_hash
        {
            return Err(std::io::Error::other(
                "catch-up did not restore a complete current root",
            ));
        }
        let mut baseline = catch_up.candidate.baseline.clone();
        baseline.generation = root.generation.0;
        self.write_baseline(&baseline)?;
        catch_up
            .candidate
            .retained
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.clear_pending(root.root.0)?;
        self.retire(&catch_up.retire);
        Ok(())
    }

    /// Capture before a full-root pass, never before a subtree repair.
    pub fn begin_reconcile(
        &self,
        index: &Index,
        root: &RootStatus,
        mounts: &str,
    ) -> Option<BaselineProbe> {
        let subvolume = btrfs_subvolume_of(&root.path, mounts)?;
        Some(BaselineProbe {
            before: self.plant(index, &subvolume, root)?,
            root_path: root.path.clone(),
        })
    }

    /// Only a complete, current pass with no overlapping changes between its bounding snapshots
    /// can establish a baseline. A busy root keeps the ordinary watcher/reconcile backend.
    pub fn complete_reconcile(
        &self,
        index: &Index,
        probe: BaselineProbe,
        root: &RootStatus,
    ) -> bool {
        let BaselineProbe { before, root_path } = probe;
        if !root.complete
            || root.state != templar_file_index::RootState::Current
            || root.root.0 != before.baseline.root
            || root.path != root_path
            || root.policy_hash != before.baseline.policy_hash
            || index.cache_identity() != before.baseline.cache_identity
            || root.generation.0 <= before.baseline.generation
        {
            return false;
        }
        let subvolume = Path::new(&before.baseline.subvolume);
        let Some(candidate) = self.plant(index, subvolume, root) else {
            return false;
        };
        let Some((decoded, _)) = self.read_delta(&before.baseline, &candidate.baseline) else {
            return false;
        };
        let subvolume_native = NativePath::from_path(subvolume);
        // Validate paths through the existing replay contract. Inspect raw commands too: a
        // create/delete can disappear from the net plan while the walker saw the transient row.
        if btrfs_delta::plan(&decoded).is_err()
            || decoded.commands.iter().any(|command| {
                if matches!(
                    command.cmd,
                    btrfs_delta::SendCmd::Subvol
                        | btrfs_delta::SendCmd::Snapshot
                        | btrfs_delta::SendCmd::End
                ) {
                    return false;
                }
                [command.path(), command.path_to(), command.path_link()]
                    .into_iter()
                    .flatten()
                    .map(|relative| subvolume_native.join(relative))
                    .any(|path| path.starts_with(&root_path) || root_path.starts_with(&path))
            })
        {
            tracing::info!(
                root = root.root.0,
                "root changed during checkpoint acquisition; keeping watcher/reconcile backend"
            );
            return false;
        }
        let old = self.read_baseline(root.root.0);
        if let Err(error) = self.write_baseline(&candidate.baseline) {
            tracing::warn!(%error, "could not seed an index checkpoint baseline");
            return false;
        }
        candidate
            .retained
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(error) = self.clear_pending(root.root.0) {
            tracing::warn!(%error, "index checkpoint remains fenced; next downtime will reconcile");
            return false;
        }
        if let Some(old) = old {
            self.retire(&old.checkpoint);
        }
        true
    }

    fn read_delta(
        &self,
        from: &Baseline,
        to: &Baseline,
    ) -> Option<(btrfs_delta::SendStream, DeltaStreamInfo)> {
        let (file, stream) = self
            .checkpoints
            .delta_open(&from.checkpoint, &to.checkpoint, Path::new(&to.subvolume))
            .map_err(|error| tracing::info!(%error, "no delta; reconciling instead"))
            .ok()?;
        if stream.bytes > self.cap_bytes
            || stream.from != from.checkpoint
            || stream.to != to.checkpoint
            || stream.subvolume != to.subvolume
        {
            tracing::info!("delta size or lineage is incompatible; reconciling instead");
            return None;
        }
        let decoded = btrfs_delta::decode_reader_bounded(file, self.cap_bytes)
            .map_err(|error| tracing::warn!(%error, "the delta stream did not decode"))
            .ok()?;
        if !decoded.is_incremental()
            || decoded
                .commands
                .iter()
                .any(|command| matches!(command.cmd, btrfs_delta::SendCmd::Unknown(_)))
        {
            tracing::info!("non-incremental or unsupported delta commands; reconciling instead");
            return None;
        }
        Some((decoded, stream))
    }

    fn plant(&self, index: &Index, subvolume: &Path, root: &RootStatus) -> Option<Candidate> {
        // The checkpoint authority's persisted subvolume identity is UTF-8. Unsupported native
        // mount identities reconcile normally; never plant a checkpoint under a lossy identity.
        let subvolume_text = subvolume.to_str()?;
        let checkpoint = self
            .checkpoints
            .create(subvolume)
            .map_err(|error| tracing::info!(%error, "no index checkpoint; reconciling instead"))
            .ok()?;
        Some(Candidate {
            checkpoints: self.checkpoints.clone(),
            baseline: Baseline {
                subvolume: subvolume_text.to_owned(),
                checkpoint: checkpoint.id,
                root: root.root.0,
                generation: root.generation.0,
                policy_hash: root.policy_hash.clone(),
                cache_identity: index.cache_identity().to_owned(),
            },
            retained: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn retire(&self, id: &str) {
        if let Err(error) = self.checkpoints.discard(id) {
            tracing::warn!(%error, id, "could not discard a spent index checkpoint");
        }
    }

    fn pending_path(&self, root: i64) -> PathBuf {
        self.state_path
            .with_extension(format!("root-{root}.pending"))
    }

    fn clear_pending(&self, root: i64) -> std::io::Result<()> {
        match std::fs::remove_file(self.pending_path(root)) {
            Ok(()) => {
                if let Some(parent) = self.state_path.parent() {
                    std::fs::File::open(parent)?.sync_all()?;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn read_baseline(&self, root: i64) -> Option<Baseline> {
        self.read_baselines().remove(&root)
    }

    fn read_baselines(&self) -> std::collections::BTreeMap<i64, Baseline> {
        std::fs::read(&self.state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write_baseline(&self, baseline: &Baseline) -> std::io::Result<()> {
        let mut baselines = self.read_baselines();
        baselines.insert(baseline.root, baseline.clone());
        atomic_write(&self.state_path, &serde_json::to_vec(&baselines)?)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("checkpoint has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()
}

/// The mount point of the btrfs subvolume `path` lives on, or `None` when it is not btrfs.
///
/// The subvolume's *mount point* is what the stream's paths are relative to: `@home` mounted at
/// `/home` sends `dk/notes.md`, and the index root `/home/dk` is a directory inside it.
fn btrfs_subvolume_of(root: &NativePath, mounts: &str) -> Option<PathBuf> {
    let path = root.to_path();
    let mount = crate::storage::mount_point_of(&path, mounts)?;
    crate::storage::mount_entries(mounts)
        .into_iter()
        .find(|entry| entry.mount_point == mount && entry.fs_type == "btrfs")
        .map(|entry| entry.mount_point)
}

/// Directories the batch creates. Their children are not in the stream — `to_mutations` says so
/// itself — so each one needs a targeted scan before the root can claim to be current.
///
/// A `Move`'s destination is not here: the index re-paths the descendant rows itself, so the
/// subtree arrives with it.
fn rescan_targets(batch: &MutationBatch) -> Vec<NativePath> {
    batch
        .mutations
        .iter()
        .filter_map(|mutation| match mutation {
            templar_file_index::Mutation::Upsert { path, meta, .. }
                if meta.kind == EntryKind::Directory =>
            {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

/// The real backend: `org.templaros.Host.IndexCheckpoint1` on the **system** bus.
///
/// The watcher runs on its own blocking thread, so every call goes through `Handle::block_on` —
/// the same route `templar_client::SyncSnapshotSource` already takes out of a blocking context, and
/// the reason zbus does not need its blocking API enabled for this. It must never be constructed
/// on the reactor thread itself.
#[derive(Debug)]
pub struct HostCheckpoints {
    handle: tokio::runtime::Handle,
    proxy: templar_client::proxies::IndexCheckpointProxy<'static>,
}

impl HostCheckpoints {
    /// `None` when the system bus or `templar-hostd` is not there, which is the normal case in a
    /// test, a nested session and a container.
    #[must_use]
    pub fn connect(handle: &tokio::runtime::Handle) -> Option<Self> {
        let handle = handle.clone();
        let proxy = handle.block_on(async {
            let connection = zbus::Connection::system().await.ok()?;
            templar_client::proxies::IndexCheckpointProxy::new(&connection)
                .await
                .ok()
        })?;
        Some(Self { handle, proxy })
    }

    fn on_bus<T, F>(&self, call: F) -> anyhow::Result<T>
    where
        F: std::future::Future<Output = zbus::Result<T>>,
    {
        Ok(self.handle.clone().block_on(call)?)
    }
}

impl Checkpoints for HostCheckpoints {
    fn list(&self) -> anyhow::Result<Vec<Checkpoint>> {
        Ok(serde_json::from_str(&self.on_bus(self.proxy.list())?)?)
    }

    fn create(&self, subvolume: &Path) -> anyhow::Result<Checkpoint> {
        let request = serde_json::json!({ "subvolume": subvolume.to_string_lossy() }).to_string();
        Ok(serde_json::from_str(
            &self.on_bus(self.proxy.create(&request))?,
        )?)
    }

    fn delta_open(
        &self,
        from: &str,
        to: &str,
        subvolume: &Path,
    ) -> anyhow::Result<(std::fs::File, DeltaStreamInfo)> {
        let request = serde_json::json!({
            "from": from,
            "to": to,
            "subvolume": subvolume.to_string_lossy(),
            "with-data": false,
        })
        .to_string();
        let (fd, json) = self.on_bus(self.proxy.delta_open(&request))?;
        Ok((
            std::fs::File::from(std::os::fd::OwnedFd::from(fd)),
            serde_json::from_str(&json)?,
        ))
    }

    fn discard(&self, id: &str) -> anyhow::Result<()> {
        self.on_bus(self.proxy.discard(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use templar_file_index::{IndexConfig, Mutation, MutationSource, RootPolicy};

    #[test]
    fn delta_failed_decode_or_apply_never_publishes_the_candidate_and_restart_is_fenced() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        let original = std::fs::read(&fixture.state).unwrap();
        *checkpoints.stream.lock() = b"invalid stream".to_vec();
        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        assert_eq!(std::fs::read(&fixture.state).unwrap(), original);
        assert_eq!(checkpoints.held.lock().as_slice(), ["index-home-2"]);
        *checkpoints.stream.lock() = one_created_file();
        let candidate = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .unwrap();
        catch_up.prepare_apply(&candidate).unwrap();
        // An actual invalid root batch fails before commit. The marker remains, so even a
        // restarted backend cannot mistake the old pointer for an unapplied delta.
        let mut invalid = candidate.batch.clone();
        invalid.mutations.push(Mutation::Upsert {
            root: templar_file_index::RootId(-1),
            path: NativePath::from_path(&fixture.home.path().join("missing")),
            meta: templar_file_index::EntryMeta::default(),
        });
        assert!(fixture.index.apply_atomic(&invalid).is_err());
        assert_eq!(std::fs::read(&fixture.state).unwrap(), original);
        let restarted = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        assert!(
            restarted
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        drop(candidate);
        // Recovery is a complete root pass, not replay. It reseeds and clears the durable fence.
        fixture
            .index
            .scan_root(
                fixture.root.root,
                templar_file_index::ScanOptions::reconcile(RootPolicy::default_home()),
            )
            .unwrap();
        let repaired = seed_baseline(&restarted, &fixture, &checkpoints, &mounts);
        assert!(!restarted.pending_path(repaired.root.0).exists());
        assert_eq!(
            restarted.read_baseline(repaired.root.0).unwrap().generation,
            repaired.generation.0
        );
    }

    #[test]
    fn delta_a_crash_after_commit_before_pointer_publication_requires_reconciliation() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        let candidate = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .unwrap();
        catch_up.prepare_apply(&candidate).unwrap();
        fixture.index.begin_catch_up(fixture.root.root).unwrap();
        fixture.index.apply_atomic(&candidate.batch).unwrap();
        let restarted = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        assert!(
            restarted
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        assert_eq!(
            restarted
                .read_baseline(fixture.root.root.0)
                .unwrap()
                .checkpoint,
            "index-home-2"
        );
        assert_eq!(
            fixture
                .index
                .search(
                    &templar_file_index::Query::new("notes"),
                    &templar_file_index::CallContext::internal(),
                )
                .unwrap()
                .hits
                .len(),
            1,
        );
    }

    #[test]
    fn delta_catch_up_rearms_recursive_watches_before_publishing_current() {
        let fixture = fixture();
        std::fs::create_dir_all(fixture.home.path().join("dk/nested/deep")).unwrap();
        fixture
            .index
            .scan_root(
                fixture.root.root,
                templar_file_index::ScanOptions::reconcile(RootPolicy::default_home()),
            )
            .unwrap();
        let current = fixture.index.root_status(fixture.root.root).unwrap();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let delta = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&delta, &fixture, &checkpoints, &mounts);
        fixture
            .index
            .mark_stale(current.root, &templar_file_index::StaleReason::Unwatched)
            .unwrap();
        let mut watcher = crate::watcher::WatchLoop::new(
            fixture.index.clone(),
            templar_file_index::watch::FakeSource::new(),
        );
        watcher.attach_delta(delta);
        assert_eq!(watcher.watches_armed(), 0);
        assert!(watcher.catch_up_with_mounts(current.root, &mounts));
        assert_eq!(watcher.watches_armed(), 3);
        assert_eq!(
            fixture.index.root_status(current.root).unwrap().state,
            templar_file_index::RootState::Current
        );
    }

    /// A checkpoint store that hands out streams the test wrote. Every method records its call,
    /// so a test can assert what was retired as well as what was applied.
    #[derive(Debug, Default)]
    struct FakeCheckpoints {
        held: Mutex<Vec<String>>,
        stream: Mutex<Vec<u8>>,
        discarded: Mutex<Vec<String>>,
        next_id: Mutex<u32>,
        refuse_delta: Mutex<bool>,
    }

    impl FakeCheckpoints {
        fn with_stream(bytes: Vec<u8>) -> Arc<Self> {
            let fake = Self::default();
            *fake.stream.lock() = bytes;
            Arc::new(fake)
        }
    }

    impl Checkpoints for FakeCheckpoints {
        fn list(&self) -> anyhow::Result<Vec<Checkpoint>> {
            Ok(self
                .held
                .lock()
                .iter()
                .map(|id| Checkpoint {
                    id: id.clone(),
                    ..Checkpoint::default()
                })
                .collect())
        }

        fn create(&self, subvolume: &Path) -> anyhow::Result<Checkpoint> {
            let mut next = self.next_id.lock();
            *next += 1;
            let id = format!("index-home-{next}");
            self.held.lock().push(id.clone());
            Ok(Checkpoint {
                id,
                subvolume: subvolume.to_string_lossy().into_owned(),
                ..Checkpoint::default()
            })
        }

        fn delta_open(
            &self,
            from: &str,
            to: &str,
            subvolume: &Path,
        ) -> anyhow::Result<(std::fs::File, DeltaStreamInfo)> {
            anyhow::ensure!(!*self.refuse_delta.lock(), "no delta today");
            let bytes = self.stream.lock().clone();
            // A real DeltaOpen hands over an already-unlinked file; a tempfile that deletes
            // itself is the same contract.
            let mut file = tempfile::tempfile()?;
            std::io::Write::write_all(&mut file, &bytes)?;
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))?;
            Ok((
                file,
                DeltaStreamInfo {
                    from: from.to_owned(),
                    to: to.to_owned(),
                    subvolume: subvolume.to_string_lossy().into_owned(),
                    bytes: bytes.len() as u64,
                    duration_ms: 0,
                },
            ))
        }

        fn discard(&self, id: &str) -> anyhow::Result<()> {
            self.held.lock().retain(|held| held != id);
            self.discarded.lock().push(id.to_owned());
            Ok(())
        }
    }

    /// A real send stream, built the way `btrfs send` builds one: `btrfs-stream\0`, version, then
    /// CRC-checked commands ending in `BTRFS_SEND_C_END`. `templar_file_index::btrfs_delta` rejects
    /// anything else outright, so this cannot be faked with arbitrary bytes.
    type Command = (u16, Vec<(u16, Vec<u8>)>);

    fn send_stream(commands: &[Command]) -> Vec<u8> {
        fn crc32c(bytes: &[u8]) -> u32 {
            let mut crc = 0u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0x82F6_3B78 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            crc
        }

        let mut stream = b"btrfs-stream\0".to_vec();
        stream.extend_from_slice(&1u32.to_le_bytes());
        let header = (
            2,
            vec![
                (15, b"checkpoint".to_vec()),
                (1, vec![1; 16]),
                (2, 1u64.to_le_bytes().to_vec()),
                (20, vec![2; 16]),
                (21, 1u64.to_le_bytes().to_vec()),
            ],
        );
        for (cmd, attrs) in std::iter::once(&header)
            .chain(commands.iter())
            .chain(std::iter::once(&(21u16, vec![])))
        {
            let mut body = Vec::new();
            for (kind, value) in attrs {
                body.extend_from_slice(&kind.to_le_bytes());
                body.extend_from_slice(&(value.len() as u16).to_le_bytes());
                body.extend_from_slice(value);
            }
            let mut command = Vec::new();
            command.extend_from_slice(&(body.len() as u32).to_le_bytes());
            command.extend_from_slice(&cmd.to_le_bytes());
            command.extend_from_slice(&0u32.to_le_bytes());
            command.extend_from_slice(&body);
            let crc = crc32c(&command);
            command[6..10].copy_from_slice(&crc.to_le_bytes());
            stream.extend_from_slice(&command);
        }
        stream
    }

    /// `BTRFS_SEND_C_MKFILE dk/notes.md` — one created file under the indexed root.
    fn one_created_file() -> Vec<u8> {
        send_stream(&[(
            3,
            vec![
                (15, b"dk/notes.md".to_vec()),
                (12, 257u64.to_le_bytes().to_vec()),
            ],
        )])
    }

    struct Fixture {
        home: tempfile::TempDir,
        cache: tempfile::TempDir,
        index: Index,
        root: RootStatus,
        state: PathBuf,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("dk")).unwrap();
        let index =
            Index::open(&cache.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
        let root = index
            .add_root(
                &NativePath::from_path(&home.path().join("dk")),
                RootPolicy::default_home(),
            )
            .unwrap();
        index
            .scan_root(
                root.root,
                templar_file_index::ScanOptions::reconcile(RootPolicy::default_home()),
            )
            .unwrap();
        let root = index.root_status(root.root).unwrap();
        std::fs::write(home.path().join("dk/notes.md"), b"hi").unwrap();
        let state = cache.path().join("delta-checkpoint.json");
        Fixture {
            home,
            cache,
            index,
            root,
            state,
        }
    }

    fn seed_baseline(
        catch_up: &DeltaCatchUp,
        fixture: &Fixture,
        checkpoints: &FakeCheckpoints,
        mounts: &str,
    ) -> RootStatus {
        // Establish an unchanged real full pass. The subsequent creation is what the stored
        // one-file stream describes; it must not already have been scanned into the baseline.
        let notes = fixture.home.path().join("dk/notes.md");
        std::fs::remove_file(&notes).unwrap();
        let stream = std::mem::replace(&mut *checkpoints.stream.lock(), send_stream(&[]));
        let before = fixture.index.root_status(fixture.root.root).unwrap();
        let probe = catch_up
            .begin_reconcile(&fixture.index, &before, mounts)
            .unwrap();
        fixture
            .index
            .scan_root(
                before.root,
                templar_file_index::ScanOptions::reconcile(
                    fixture.index.root_policy(before.root).unwrap(),
                ),
            )
            .unwrap();
        let current = fixture.index.root_status(before.root).unwrap();
        assert!(catch_up.complete_reconcile(&fixture.index, probe, &current));
        *checkpoints.stream.lock() = stream;
        checkpoints.discarded.lock().clear();
        std::fs::write(notes, b"hi").unwrap();
        current
    }

    #[test]
    fn delta_acquisition_refuses_late_creations_and_the_next_full_pass_restores_visible_truth() {
        let fixture = fixture();
        std::fs::remove_file(fixture.home.path().join("dk/notes.md")).unwrap();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let backend = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        let probe = backend
            .begin_reconcile(&fixture.index, &fixture.root, &mounts)
            .unwrap();
        fixture
            .index
            .scan_root(
                fixture.root.root,
                templar_file_index::ScanOptions::default(),
            )
            .unwrap();
        // Arrived after the walker visited the directory: post-walk-only seeding loses this row.
        std::fs::write(fixture.home.path().join("dk/notes.md"), b"late").unwrap();
        let current = fixture.index.root_status(fixture.root.root).unwrap();
        assert!(
            fixture
                .index
                .search(
                    &templar_file_index::Query::new("notes"),
                    &templar_file_index::CallContext::internal()
                )
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(!backend.complete_reconcile(&fixture.index, probe, &current));
        assert!(!fixture.state.exists());
        assert!(checkpoints.held.lock().is_empty());
        *checkpoints.stream.lock() = send_stream(&[]);
        let probe = backend
            .begin_reconcile(&fixture.index, &current, &mounts)
            .unwrap();
        fixture
            .index
            .scan_root(current.root, templar_file_index::ScanOptions::default())
            .unwrap();
        let repaired = fixture.index.root_status(current.root).unwrap();
        assert!(backend.complete_reconcile(&fixture.index, probe, &repaired));
        assert_eq!(
            fixture
                .index
                .search(
                    &templar_file_index::Query::new("notes"),
                    &templar_file_index::CallContext::internal()
                )
                .unwrap()
                .hits
                .len(),
            1
        );
        assert_eq!(checkpoints.held.lock().len(), 1);
    }

    #[test]
    fn delta_acquisition_refuses_transient_rows_even_when_the_net_plan_is_empty() {
        let fixture = fixture();
        let stream = send_stream(&[
            (3, vec![(15, b"dk/transient".to_vec())]),
            (11, vec![(15, b"dk/transient".to_vec())]),
        ]);
        assert!(
            btrfs_delta::plan(&btrfs_delta::decode(&stream).unwrap())
                .unwrap()
                .is_empty()
        );
        let checkpoints = FakeCheckpoints::with_stream(stream);
        let backend = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        let probe = backend
            .begin_reconcile(&fixture.index, &fixture.root, &mounts)
            .unwrap();
        std::fs::write(fixture.home.path().join("dk/transient"), b"seen").unwrap();
        fixture
            .index
            .scan_root(
                fixture.root.root,
                templar_file_index::ScanOptions::default(),
            )
            .unwrap();
        std::fs::remove_file(fixture.home.path().join("dk/transient")).unwrap();
        let current = fixture.index.root_status(fixture.root.root).unwrap();
        assert_eq!(
            fixture
                .index
                .search(
                    &templar_file_index::Query::new("transient"),
                    &templar_file_index::CallContext::internal()
                )
                .unwrap()
                .hits
                .len(),
            1
        );
        assert!(!backend.complete_reconcile(&fixture.index, probe, &current));
        assert!(!fixture.state.exists());
    }

    #[test]
    fn delta_replaced_cache_rejects_a_baseline_with_coincident_root_generation_and_policy() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let backend = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        let old = seed_baseline(&backend, &fixture, &checkpoints, &mounts);
        let replacement = Index::open(
            &fixture.cache.path().join("replacement.sqlite3"),
            IndexConfig::default(),
        )
        .unwrap();
        let root = replacement
            .add_root(&old.path, RootPolicy::default_home())
            .unwrap();
        // Every scan allocates a start generation and publishes a later one, so step until the
        // replacement's published generation coincides with the old baseline's.
        while replacement.root_status(root.root).unwrap().generation < old.generation {
            replacement
                .scan_root(root.root, templar_file_index::ScanOptions::default())
                .unwrap();
        }
        let new = replacement.root_status(root.root).unwrap();
        assert_eq!(
            (old.root, old.generation, &old.policy_hash),
            (new.root, new.generation, &new.policy_hash)
        );
        assert_ne!(fixture.index.cache_identity(), replacement.cache_identity());
        assert!(backend.try_catch_up(&replacement, &new, &mounts).is_none());
        assert_eq!(checkpoints.held.lock().len(), 1);
    }

    /// The mount table the fixture's temporary home would have if it were a btrfs subvolume:
    /// mounted at the tempdir, with the indexed root one directory inside it — which is the real
    /// shape (`@home` at `/home`, the root at `/home/dk`) and the one a target that joins onto
    /// its own root gets wrong.
    fn mounts_for(fixture: &Fixture, fs_type: &str) -> String {
        format!(
            "/dev/fake {} {fs_type} rw 0 0\n",
            fixture.home.path().display()
        )
    }

    #[test]
    fn a_root_that_is_not_on_btrfs_reconciles() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts_for(&fixture, "ext4"))
                .is_none()
        );
        assert!(
            checkpoints.held.lock().is_empty(),
            "an ext4 root must not cost a snapshot"
        );
        // The development host is exactly this case, so it is the branch that has to stay free.
        assert!(!fixture.state.exists());
    }

    #[test]
    fn the_first_downtime_plants_a_baseline_and_reconciles() {
        // There is nothing to take a difference against yet, so the honest answer is a reconcile
        // — and the checkpoint that makes the *next* one cheap.
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");

        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        assert!(
            !fixture.state.exists(),
            "nothing was planted by a failed read"
        );

        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        assert_eq!(checkpoints.held.lock().len(), 1);
        assert!(fixture.state.exists());
    }

    #[test]
    fn a_second_downtime_applies_the_delta_and_retires_the_old_checkpoint() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);

        let result = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        assert_eq!(result.batch.source, MutationSource::Reconcile);
        assert_eq!(result.batch.mutations.len(), 1);
        assert!(matches!(
            &result.batch.mutations[0],
            Mutation::Upsert { path, .. } if path.to_path().ends_with("dk/notes.md")
        ));
        assert!(result.rescan.is_empty(), "a file needs no targeted scan");

        catch_up.prepare_apply(&result).unwrap();
        fixture.index.begin_catch_up(fixture.root.root).unwrap();
        fixture.index.apply_atomic(&result.batch).unwrap();
        let current = fixture.index.finish_catch_up(fixture.root.root).unwrap();
        catch_up.committed(&result, &current).unwrap();
        assert_eq!(
            checkpoints.discarded.lock().as_slice(),
            ["index-home-2"],
            "the checkpoint the delta was taken against is the one that is retired"
        );
        assert_eq!(
            checkpoints.held.lock().as_slice(),
            ["index-home-3"],
            "one retained checkpoint per subvolume, and it is the newer one"
        );
        let baseline = catch_up.read_baseline(fixture.root.root.0).unwrap();
        assert_eq!(baseline.checkpoint, "index-home-3");
        assert_eq!(baseline.generation, current.generation.0);
        assert!(!catch_up.pending_path(fixture.root.root.0).exists());
    }

    #[test]
    fn a_delta_that_is_not_committed_leaves_the_baseline_where_it_was() {
        // The apply is the caller's, and it can fail. Retiring on the way out rather than on the
        // way back would leave the index with no baseline *and* no applied delta.
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);

        let result = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        drop(result);
        assert_eq!(checkpoints.discarded.lock().as_slice(), ["index-home-3"]);
        assert_eq!(
            catch_up
                .read_baseline(fixture.root.root.0)
                .unwrap()
                .checkpoint,
            "index-home-2"
        );
    }

    #[test]
    fn applying_the_same_delta_twice_changes_nothing_the_second_time() {
        // A repeated *new* stream can be harmless, but a crash during publication is fenced
        // separately below; general rename replay is never assumed to be idempotent.
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);

        let first = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        catch_up.prepare_apply(&first).unwrap();
        fixture.index.begin_catch_up(fixture.root.root).unwrap();
        let applied = fixture.index.apply_atomic(&first.batch).unwrap();
        assert_eq!(applied.skipped, 0);
        let current = fixture.index.finish_catch_up(fixture.root.root).unwrap();
        catch_up.committed(&first, &current).unwrap();

        let second = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        let again = fixture.index.apply_atomic(&second.batch).unwrap();
        assert_eq!(
            again.skipped, 1,
            "the index already agreed with every mutation"
        );
        assert!(again.changes.is_empty());
    }

    #[test]
    fn a_stream_over_the_cap_reconciles_and_rebaselines() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let mut catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        catch_up.cap_bytes = 8;

        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        assert_eq!(
            checkpoints.held.lock().as_slice(),
            ["index-home-2"],
            "an oversized candidate must not advance the pointer before reconciliation succeeds"
        );
    }

    #[test]
    fn a_baseline_the_daemon_no_longer_holds_reconciles() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        // hostd was restarted, the store was cleared, a person ran `btrfs subvolume delete`.
        checkpoints.held.lock().clear();

        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
    }

    #[test]
    fn a_daemon_that_will_not_send_reconciles_rather_than_failing() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);
        *checkpoints.refuse_delta.lock() = true;

        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
    }

    #[test]
    fn a_created_directory_is_queued_for_a_targeted_scan() {
        // A directory dragged in from outside the indexed region arrives as one Upsert; its
        // children are not in the stream and only a walk can find them.
        let fixture = fixture();
        std::fs::create_dir_all(fixture.home.path().join("dk/moved-in")).unwrap();
        let stream = send_stream(&[(
            4,
            vec![
                (15, b"dk/moved-in".to_vec()),
                (12, 258u64.to_le_bytes().to_vec()),
            ],
        )]);
        let checkpoints = FakeCheckpoints::with_stream(stream);
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        seed_baseline(&catch_up, &fixture, &checkpoints, &mounts);

        let result = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        assert_eq!(result.rescan.len(), 1);
        assert!(result.rescan[0].to_path().ends_with("dk/moved-in"));
    }
}
