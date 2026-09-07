//! Catching a root up from a btrfs metadata delta instead of re-walking it.
//!
//! A downtime repair costs what the *tree* costs: a no-op reconcile of a 153,327-entry fixture
//! took 3.24 s and a real `$HOME` is several times that
//! (`@legeos.observation.reconciliation-cost-tracks-tree-size-not-change-size`). `btrfs send
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
//! The privileged half lives in `lege-hostd` because `BTRFS_IOC_SEND` needs `CAP_SYS_ADMIN`
//! (ADR-0001, ADR-0003); this is the unprivileged consumer of
//! `org.legeos.Host.IndexCheckpoint1`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use lege_file_index::{
    EntryKind, Index, MutationBatch, NativePath, RootStatus,
    btrfs_delta::{self, DeltaReport, DeltaTarget, FsMetadata},
};
use serde::{Deserialize, Serialize};

/// Above this, give up and reconcile.
///
/// The stream is proportional to changed extents, not to changed paths, and the measured cost is
/// about 420 B per path — so 16 MiB is roughly 40,000 paths. Past that the measurements stop
/// favouring the delta anyway: the build-shaped burst of 5,006 mutations was the one scenario
/// where a crawl won outright, and a dormant window big enough to fill 16 MiB is that case
/// several times over. Overridable with `LEGE_FILED_DELTA_CAP_BYTES` so the number can be moved
/// by measurement rather than by editing this file.
pub const DEFAULT_CAP_BYTES: u64 = 16 * 1024 * 1024;

fn cap_bytes() -> u64 {
    std::env::var("LEGE_FILED_DELTA_CAP_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CAP_BYTES)
}

/// One retained checkpoint, as `org.legeos.Host.IndexCheckpoint1` reports it.
///
/// A local mirror of `lege_hostd::index_checkpoint::Checkpoint` rather than a shared type: hostd
/// is a binary crate on the other side of a bus, the contract between them is the JSON in
/// `interfaces/dbus/org.legeos.Host.IndexCheckpoint1.xml`, and `#[serde(default)]` is what lets
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

/// What the last successful catch-up left behind: the checkpoint the next delta is taken against.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Baseline {
    subvolume: String,
    checkpoint: String,
}

/// A delta ready to be applied. The caller commits the batch, rescans what a delta cannot walk,
/// and then calls [`DeltaCatchUp::committed`] — which is what retires the old checkpoint, so a
/// failed apply leaves the baseline exactly where it was.
#[derive(Debug)]
pub struct CatchUp {
    pub batch: MutationBatch,
    pub report: DeltaReport,
    /// Directories that need a targeted scan: a directory that arrives from outside the indexed
    /// region is one `Upsert`, because the stream names the directory and not the children that
    /// came with it.
    pub rescan: Vec<NativePath>,
    pub stream: DeltaStreamInfo,
    baseline: Baseline,
    retire: String,
}

/// The catch-up backend for one session.
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

    /// Try to answer a downtime repair with a delta.
    ///
    /// `None` means "reconcile instead", and it is the answer to every doubt: the root is not on
    /// btrfs, no baseline exists yet, the baseline is gone, the stream is over the cap, the
    /// daemon is not there, the decode failed. The first call on a new root always returns `None`
    /// and plants the baseline the *second* one uses — there is nothing to take a difference
    /// against before that.
    pub fn try_catch_up(&self, index: &Index, root: &RootStatus, mounts: &str) -> Option<CatchUp> {
        let subvolume = btrfs_subvolume_of(&root.path, mounts)?;
        let baseline = self.read_baseline();

        // A baseline for a different subvolume is not a baseline for this one.
        let from = baseline
            .filter(|baseline| Path::new(&baseline.subvolume) == subvolume)
            .filter(|baseline| {
                self.checkpoints
                    .list()
                    .is_ok_and(|held| held.iter().any(|c| c.id == baseline.checkpoint))
            })?;

        let to = self.plant(&subvolume)?;
        let (file, stream) = self
            .checkpoints
            .delta_open(&from.checkpoint, &to.checkpoint, &subvolume)
            .map_err(|error| tracing::info!(%error, "no delta; reconciling instead"))
            .ok()?;

        if stream.bytes > self.cap_bytes {
            // The delta is bigger than a reconcile is worth. The *new* checkpoint stays and
            // becomes the baseline anyway — the reconcile that follows brings the index current,
            // so the next delta should be taken from here and not from the stale one.
            tracing::info!(
                bytes = stream.bytes,
                cap = self.cap_bytes,
                "the delta is over the cap; reconciling instead"
            );
            self.retire(&from.checkpoint);
            return None;
        }

        let policy = index.root_policy(root.root).ok()?;
        let decoded = btrfs_delta::decode_reader(file)
            .map_err(|error| tracing::warn!(%error, "the delta stream did not decode"))
            .ok()?;
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
            // Cannot happen while the subvolume comes from this root's own mount entry, but it is
            // the one mistake that silently applies nothing, so it is checked rather than trusted.
            tracing::warn!("the root is not inside the sent subvolume; reconciling instead");
            self.retire(&from.checkpoint);
            return None;
        }
        let (batch, report) = btrfs_delta::to_mutations(&plan, &target, &FsMetadata);

        Some(CatchUp {
            rescan: rescan_targets(&batch),
            batch,
            report,
            stream,
            baseline: to,
            retire: from.checkpoint,
        })
    }

    /// Record that the batch committed: retire the checkpoint it was taken against, and make the
    /// newer one the baseline for next time.
    pub fn committed(&self, catch_up: &CatchUp) {
        self.retire(&catch_up.retire);
        self.write_baseline(&catch_up.baseline);
    }

    /// Make sure a baseline exists for `subvolume`, so the *next* downtime has something to take
    /// a difference against. Cheap and idempotent; call it whenever a root is known current.
    pub fn ensure_baseline(&self, root: &RootStatus, mounts: &str) {
        let Some(subvolume) = btrfs_subvolume_of(&root.path, mounts) else {
            return;
        };
        let held = self.read_baseline().is_some_and(|baseline| {
            Path::new(&baseline.subvolume) == subvolume
                && self
                    .checkpoints
                    .list()
                    .is_ok_and(|held| held.iter().any(|c| c.id == baseline.checkpoint))
        });
        if !held {
            self.plant(&subvolume);
        }
    }

    /// Take a checkpoint and record it as the baseline.
    fn plant(&self, subvolume: &Path) -> Option<Baseline> {
        let checkpoint = self
            .checkpoints
            .create(subvolume)
            .map_err(|error| tracing::info!(%error, "no index checkpoint; reconciling instead"))
            .ok()?;
        let baseline = Baseline {
            subvolume: subvolume.to_string_lossy().into_owned(),
            checkpoint: checkpoint.id,
        };
        self.write_baseline(&baseline);
        Some(baseline)
    }

    fn retire(&self, id: &str) {
        if let Err(error) = self.checkpoints.discard(id) {
            // A snapshot that outlives its use pins disk space, which is user-visible, so this is
            // a warning even though nothing about the index is wrong.
            tracing::warn!(%error, id, "could not discard a spent index checkpoint");
        }
    }

    fn read_baseline(&self) -> Option<Baseline> {
        let text = std::fs::read_to_string(&self.state_path).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn write_baseline(&self, baseline: &Baseline) {
        let write = || -> std::io::Result<()> {
            if let Some(parent) = self.state_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&self.state_path, serde_json::to_vec(baseline)?)
        };
        if let Err(error) = write() {
            tracing::warn!(%error, path = %self.state_path.display(), "could not record the index checkpoint baseline");
        }
    }
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
            lege_file_index::Mutation::Upsert { path, meta, .. }
                if meta.kind == EntryKind::Directory =>
            {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

/// The real backend: `org.legeos.Host.IndexCheckpoint1` on the **system** bus.
///
/// The watcher runs on its own blocking thread, so every call goes through `Handle::block_on` —
/// the same route `lege_client::SyncSnapshotSource` already takes out of a blocking context, and
/// the reason zbus does not need its blocking API enabled for this. It must never be constructed
/// on the reactor thread itself.
#[derive(Debug)]
pub struct HostCheckpoints {
    handle: tokio::runtime::Handle,
    proxy: lege_client::proxies::IndexCheckpointProxy<'static>,
}

impl HostCheckpoints {
    /// `None` when the system bus or `lege-hostd` is not there, which is the normal case in a
    /// test, a nested session and a container.
    #[must_use]
    pub fn connect(handle: &tokio::runtime::Handle) -> Option<Self> {
        let handle = handle.clone();
        let proxy = handle.block_on(async {
            let connection = zbus::Connection::system().await.ok()?;
            lege_client::proxies::IndexCheckpointProxy::new(&connection)
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
    use lege_file_index::{IndexConfig, Mutation, MutationSource, RootPolicy};
    use std::sync::Mutex;

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
            *fake.stream.lock().unwrap() = bytes;
            Arc::new(fake)
        }
    }

    impl Checkpoints for FakeCheckpoints {
        fn list(&self) -> anyhow::Result<Vec<Checkpoint>> {
            Ok(self
                .held
                .lock()
                .unwrap()
                .iter()
                .map(|id| Checkpoint {
                    id: id.clone(),
                    ..Checkpoint::default()
                })
                .collect())
        }

        fn create(&self, subvolume: &Path) -> anyhow::Result<Checkpoint> {
            let mut next = self.next_id.lock().unwrap();
            *next += 1;
            let id = format!("index-home-{next}");
            self.held.lock().unwrap().push(id.clone());
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
            anyhow::ensure!(!*self.refuse_delta.lock().unwrap(), "no delta today");
            let bytes = self.stream.lock().unwrap().clone();
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
            self.held.lock().unwrap().retain(|held| held != id);
            self.discarded.lock().unwrap().push(id.to_owned());
            Ok(())
        }
    }

    /// A real send stream, built the way `btrfs send` builds one: `btrfs-stream\0`, version, then
    /// CRC-checked commands ending in `BTRFS_SEND_C_END`. `lege_file_index::btrfs_delta` rejects
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
        for (cmd, attrs) in commands.iter().chain(std::iter::once(&(21u16, vec![]))) {
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
        _cache: tempfile::TempDir,
        index: Index,
        root: RootStatus,
        state: PathBuf,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("dk")).unwrap();
        std::fs::write(home.path().join("dk/notes.md"), b"hi").unwrap();
        let index =
            Index::open(&cache.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
        let root = index
            .add_root(
                &NativePath::from_path(&home.path().join("dk")),
                RootPolicy::default_home(),
            )
            .unwrap();
        let state = cache.path().join("delta-checkpoint.json");
        Fixture {
            home,
            _cache: cache,
            index,
            root,
            state,
        }
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
            checkpoints.held.lock().unwrap().is_empty(),
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

        catch_up.ensure_baseline(&fixture.root, &mounts);
        assert_eq!(checkpoints.held.lock().unwrap().len(), 1);
        assert!(fixture.state.exists());
    }

    #[test]
    fn a_second_downtime_applies_the_delta_and_retires_the_old_checkpoint() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        catch_up.ensure_baseline(&fixture.root, &mounts);

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

        catch_up.committed(&result);
        assert_eq!(
            checkpoints.discarded.lock().unwrap().as_slice(),
            ["index-home-1"],
            "the checkpoint the delta was taken against is the one that is retired"
        );
        assert_eq!(
            checkpoints.held.lock().unwrap().as_slice(),
            ["index-home-2"],
            "one retained checkpoint per subvolume, and it is the newer one"
        );
    }

    #[test]
    fn a_delta_that_is_not_committed_leaves_the_baseline_where_it_was() {
        // The apply is the caller's, and it can fail. Retiring on the way out rather than on the
        // way back would leave the index with no baseline *and* no applied delta.
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        catch_up.ensure_baseline(&fixture.root, &mounts);

        let result = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        drop(result);
        assert!(checkpoints.discarded.lock().unwrap().is_empty());
    }

    #[test]
    fn applying_the_same_delta_twice_changes_nothing_the_second_time() {
        // A repair can be interrupted between the commit and the state write, so the next start
        // replays the same window. Every mutation the decoder emits is idempotent by
        // construction; this is the test that says so out loud.
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        catch_up.ensure_baseline(&fixture.root, &mounts);

        let first = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        let applied = fixture.index.apply(first.batch.clone()).unwrap();
        assert_eq!(applied.skipped, 0);
        catch_up.committed(&first);

        let second = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        let again = fixture.index.apply(second.batch.clone()).unwrap();
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
        catch_up.cap_bytes = 8;
        let mounts = mounts_for(&fixture, "btrfs");
        catch_up.ensure_baseline(&fixture.root, &mounts);

        assert!(
            catch_up
                .try_catch_up(&fixture.index, &fixture.root, &mounts)
                .is_none()
        );
        assert_eq!(
            checkpoints.held.lock().unwrap().as_slice(),
            ["index-home-2"],
            "the reconcile that follows brings the index current, so the newer checkpoint is the \
             one worth keeping"
        );
    }

    #[test]
    fn a_baseline_the_daemon_no_longer_holds_reconciles() {
        let fixture = fixture();
        let checkpoints = FakeCheckpoints::with_stream(one_created_file());
        let catch_up = DeltaCatchUp::new(checkpoints.clone(), fixture.state.clone());
        let mounts = mounts_for(&fixture, "btrfs");
        catch_up.ensure_baseline(&fixture.root, &mounts);
        // hostd was restarted, the store was cleared, a person ran `btrfs subvolume delete`.
        checkpoints.held.lock().unwrap().clear();

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
        catch_up.ensure_baseline(&fixture.root, &mounts);
        *checkpoints.refuse_delta.lock().unwrap() = true;

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
        catch_up.ensure_baseline(&fixture.root, &mounts);

        let result = catch_up
            .try_catch_up(&fixture.index, &fixture.root, &mounts)
            .expect("a delta");
        assert_eq!(result.rescan.len(), 1);
        assert!(result.rescan[0].to_path().ends_with("dk/moved-in"));
    }
}
