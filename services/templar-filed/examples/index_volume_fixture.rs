//! Owner-run loopback journey companion. Never connects to the live desktop or any bus.
use anyhow::{Context, Result, ensure};
use std::{
    io::{self, BufRead as _, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};
use templar_file_index::{
    Index, IndexConfig, NativePath, OfflineReason, RootId, RootPolicy, RootState, volume,
};
use templar_filed::{
    observer::{Command, WatcherControl},
    watcher::{self, WatcherRoots},
};

fn wait(mut predicate: impl FnMut() -> Result<bool>) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if predicate()? {
            return Ok(());
        }
        ensure!(
            Instant::now() < end,
            "volume journey did not reach its expected state within 15s"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn main() -> Result<()> {
    let base = PathBuf::from(
        std::env::var_os("TEMPLAR_VOLUME_FIXTURE")
            .context("run through scripts/index-volume-fixture.sh")?,
    );
    ensure!(
        base.starts_with(std::env::temp_dir())
            && base.file_name().is_some_and(|name| name
                .to_string_lossy()
                .starts_with("templar-volume-fixture.")),
        "expected an owned temporary fixture directory"
    );
    let tree = base.join("original");
    let db = tempfile::tempdir()?;
    let index = Index::open(&db.path().join("index.sqlite3"), IndexConfig::default())?;
    let status = index.add_root(&NativePath::from_path(&tree), RootPolicy::default_home())?;
    ensure!(
        status.volume.confidence == templar_file_index::VolumeConfidence::Strong,
        "loopback UUID link was not observed"
    );
    let root = status.root;
    let original = NativePath::from_path(&tree.join("notes/original.txt"));
    let mut watcher = watcher::spawn(
        index.clone(),
        WatcherRoots {
            scan: vec![status],
            ..WatcherRoots::default()
        },
        None,
    )?;
    let control = watcher.control();
    control
        .send_required(Command::Observe(
            ":owned-volume-fixture".into(),
            1,
            Arc::new(AtomicBool::new(true)),
        ))
        .map_err(anyhow::Error::msg)?;
    wait(|| {
        Ok(index.root_status(root)?.state == RootState::Current
            && index.entry(root, &original)?.is_some())
    })?;
    println!("READY initial");
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let stage = line?;
        match stage.as_str() {
            "absent" | "relocated" => {
                let relocated = (stage == "relocated").then(|| base.join("relocated"));
                observe_detached(&index, root, &original, relocated.as_deref())?;
            }
            "returned" => observe_returned(&index, root, &tree, &original)?,
            "mount-over" => {
                wait(|| Ok(index.entry(root, &original)?.is_none()))?;
                ensure!(
                    index
                        .entry(root, &NativePath::from_path(&tree.join("notes")))?
                        .is_some(),
                    "mount boundary row missing"
                );
            }
            "mount-removed" => wait(|| Ok(index.entry(root, &original)?.is_some()))?,
            "different" => accept_replacement(&index, root, &tree, &original, &control)?,
            "done" => break,
            _ => anyhow::bail!("unknown stage {stage}"),
        }
        println!("PASS {stage}");
        io::stdout().flush()?;
    }
    watcher.stop();
    println!("PASS volume journey; watcher and mount monitor joined");
    Ok(())
}

fn observe_detached(
    index: &Index,
    root: RootId,
    original: &NativePath,
    relocated_path: Option<&Path>,
) -> Result<()> {
    wait(|| Ok(index.root_status(root)?.state == RootState::Offline))?;
    let status = index.root_status(root)?;
    ensure!(
        matches!(
            status.offline_reason,
            Some(OfflineReason::Unmounted | OfflineReason::VolumeAbsent)
        ),
        "wrong absent reason: {:?}",
        status.offline_reason
    );
    ensure!(
        index.entry(root, original)?.is_some(),
        "detach erased cached rows"
    );
    if let Some(relocated_path) = relocated_path {
        let relocated = volume::identify(relocated_path)?.context("relocated mount absent")?;
        ensure!(
            relocated.fs_uuid == status.volume.fs_uuid
                && relocated.mount_root == status.volume.mount_root,
            "relocated image lost strong identity"
        );
        ensure!(
            matches!(volume::presence(index, root)?, volume::Presence::Absent),
            "old registration followed the relocated mount"
        );
        let table = volume::parse_mountinfo(&std::fs::read("/proc/self/mountinfo")?);
        index.observe_mount_locations(&table)?;
        ensure!(
            index.root_status(root)?.mounted_elsewhere
                == Some(NativePath::from_path(relocated_path)),
            "the offline registration has no observed relocation hint"
        );
    }
    Ok(())
}

fn observe_returned(index: &Index, root: RootId, tree: &Path, original: &NativePath) -> Result<()> {
    wait(|| Ok(index.root_status(root)?.state == RootState::Current))?;
    std::fs::write(tree.join("rearmed-marker.txt"), b"new event after return")?;
    wait(|| {
        Ok(index
            .entry(
                root,
                &NativePath::from_path(&tree.join("rearmed-marker.txt")),
            )?
            .is_some())
    })?;
    ensure!(
        index.entry(root, original)?.is_some(),
        "same-volume return lost cached contents"
    );
    Ok(())
}

fn accept_replacement(
    index: &Index,
    root: RootId,
    tree: &Path,
    original: &NativePath,
    control: &WatcherControl,
) -> Result<()> {
    wait(|| {
        Ok(matches!(
            index.root_status(root)?.offline_reason,
            Some(OfflineReason::DifferentVolume { .. })
        ))
    })?;
    ensure!(
        index.entry(root, original)?.is_some(),
        "different disk erased old rows before acceptance"
    );
    let status = index.root_status(root)?;
    let Some(OfflineReason::DifferentVolume { observed }) = status.offline_reason else {
        unreachable!()
    };
    let generation = index.generation()?;
    ensure!(
        index.accept_volume(root, &"0".repeat(64)).is_err(),
        "stale fingerprint was accepted"
    );
    ensure!(
        index.generation()? == generation,
        "rejected confirmation published a generation"
    );
    let accepted = index.accept_volume(root, &volume::fingerprint(&observed))?;
    ensure!(
        accepted.generation.0 == generation.0 + 1
            && accepted.entry_count == 0
            && index.entry(root, original)?.is_none(),
        "acceptance was not one empty publication"
    );
    control
        .send_required(Command::Reconcile(
            root,
            templar_file_index::reconcile::Trigger::Requested,
        ))
        .map_err(anyhow::Error::msg)?;
    wait(|| {
        Ok(index.root_status(root)?.state == RootState::Current
            && index
                .entry(root, &NativePath::from_path(&tree.join("replacement.txt")))?
                .is_some())
    })?;
    ensure!(
        index.entry(root, original)?.is_none(),
        "old volume row reappeared after replacement"
    );
    Ok(())
}
