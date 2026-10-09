//! Physical paths are temporary; mount observations are injectable so no host mounts change.
use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use templar_file_index::{
    Index, IndexConfig, IndexError, NativePath, OfflineReason, RootId, RootPolicy, RootState,
    ScanOptions, ScanScope, VolumeConfidence, VolumeIdentity,
    reconcile::{self, Trigger},
    volume::{self, MountRecord, Presence, VolumeProvider},
    watch::{FakeSource, WatchBudget, WatchOwner},
};

#[derive(Debug)]
struct FakeVolumes {
    observed: Mutex<Option<VolumeIdentity>>,
    boundary: Mutex<Option<NativePath>>,
    unreadable: Mutex<bool>,
    locations: Mutex<HashMap<NativePath, VolumeIdentity>>,
}
impl VolumeProvider for FakeVolumes {
    fn identify(&self, path: &NativePath) -> io::Result<Option<VolumeIdentity>> {
        if *self.unreadable.lock().unwrap() {
            return Err(io::Error::other("fake unreadable mount table"));
        }
        if let Some(identity) = self.locations.lock().unwrap().get(path) {
            return Ok(Some(identity.clone()));
        }
        Ok(self.observed.lock().unwrap().clone())
    }
    fn mount_id(&self, path: &NativePath) -> io::Result<Option<u64>> {
        if self
            .boundary
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|point| path.starts_with(point))
        {
            return Ok(Some(99));
        }
        Ok(self
            .observed
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|identity| identity.mount_id))
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    tree: PathBuf,
    index: Index,
    root: RootId,
    volumes: Arc<FakeVolumes>,
}
impl Fixture {
    fn new(confidence: VolumeConfidence) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let tree = temp.path().join("volume");
        fs::create_dir_all(tree.join("notes")).unwrap();
        fs::write(tree.join("notes/original.txt"), b"one").unwrap();
        let volumes = Arc::new(FakeVolumes {
            observed: Mutex::new(Some(VolumeIdentity {
                dev_id: Some(1),
                fsid: Some("original-fsid".into()),
                fs_uuid: (confidence == VolumeConfidence::Strong).then(|| "original-uuid".into()),
                mount_point: Some(NativePath::from_path(&tree)),
                mount_root: Some(NativePath::from_bytes(b"/".to_vec())),
                mount_id: Some(42),
                confidence,
                ..VolumeIdentity::default()
            })),
            boundary: Mutex::new(None),
            unreadable: Mutex::new(false),
            locations: Mutex::new(HashMap::new()),
        });
        let index = Index::open(
            &temp.path().join("index.sqlite3"),
            IndexConfig {
                volume_provider: volumes.clone(),
                ..IndexConfig::default()
            },
        )
        .unwrap();
        let root = index
            .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
            .unwrap()
            .root;
        index.scan_root(root, ScanOptions::default()).unwrap();
        Self {
            _temp: temp,
            tree,
            index,
            root,
            volumes,
        }
    }
    fn old(&self) -> Option<templar_file_index::EntryRow> {
        self.index
            .entry(
                self.root,
                &NativePath::from_path(&self.tree.join("notes/original.txt")),
            )
            .unwrap()
    }
    fn owner(&self) -> WatchOwner<FakeSource> {
        let mut owner = WatchOwner::new(FakeSource::new(), WatchBudget { max_watches: 64 });
        owner.set_volume_provider(self.volumes.clone());
        owner.track_root(
            self.root,
            &self.tree,
            self.index.root_policy(self.root).unwrap(),
        );
        owner
    }
    fn replacement(&self) -> VolumeIdentity {
        let mut identity = self.index.root_status(self.root).unwrap().volume;
        identity.fs_uuid = Some("replacement-uuid".into());
        identity.mount_id = Some(43);
        identity
    }
    fn relocate(&self) -> (NativePath, MountRecord) {
        let path = NativePath::from_path(&self.tree.with_file_name("relocated"));
        let mut identity = self.index.root_status(self.root).unwrap().volume;
        identity.mount_point = Some(path.clone());
        identity.mount_id = Some(43);
        self.volumes
            .locations
            .lock()
            .unwrap()
            .insert(path.clone(), identity);
        *self.volumes.observed.lock().unwrap() = None;
        (
            path.clone(),
            MountRecord {
                mount_id: 43,
                parent_id: 1,
                major: 8,
                minor: 1,
                mount_root: NativePath::from_bytes(b"/".to_vec()),
                mount_point: path,
                fs_type: "ext4".into(),
            },
        )
    }
}

#[test]
fn a_location_hint_retains_the_offline_registration_rows_and_generation() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let (location, mount) = f.relocate();
    let generation = f.index.generation().unwrap();
    assert!(volume::ensure_present(&f.index, f.root).is_err());
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    let status = f.index.root_status(f.root).unwrap();
    assert_eq!(status.state, RootState::Offline);
    assert_eq!(status.offline_reason, Some(OfflineReason::VolumeAbsent));
    assert_eq!(status.mounted_elsewhere.as_ref(), Some(&location));
    assert_eq!(status.path, NativePath::from_path(&f.tree));
    assert_eq!(
        f.index.roots().unwrap()[0].mounted_elsewhere,
        Some(location.clone())
    );
    assert_eq!(
        f.index.diagnostics().unwrap().roots[0].mounted_elsewhere,
        Some(location)
    );
    assert!(f.index.scan_root(f.root, ScanOptions::default()).is_err());
    assert!(f.old().is_some());
    assert_eq!(f.index.generation().unwrap(), generation);
    f.index.observe_mount_locations(&[]).unwrap();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_none()
    );
    assert!(f.old().is_some());
}

#[test]
fn location_hints_clear_on_observation_failure_return_or_identity_change() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let recorded = f.index.root_status(f.root).unwrap().volume;
    let (_, mount) = f.relocate();
    f.index
        .mark_offline_reason(f.root, &OfflineReason::VolumeAbsent)
        .unwrap();
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_some()
    );
    *f.volumes.unreadable.lock().unwrap() = true;
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_none()
    );
    *f.volumes.unreadable.lock().unwrap() = false;
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    f.index.clear_mount_locations();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_none()
    );
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    let mut replacement = recorded.clone();
    replacement.fs_uuid = Some("replacement-uuid".into());
    f.index.set_volume_identity(f.root, &replacement).unwrap();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_none()
    );
    f.index.set_volume_identity(f.root, &recorded).unwrap();
    *f.volumes.observed.lock().unwrap() = Some(recorded);
    f.index
        .observe_mount_locations(std::slice::from_ref(&mount))
        .unwrap();
    assert!(
        f.index
            .root_status(f.root)
            .unwrap()
            .mounted_elsewhere
            .is_none()
    );
    f.index.mark_online(f.root).unwrap();
    assert_eq!(f.index.root_status(f.root).unwrap().state, RootState::Stale);
    assert!(f.old().is_some());
}

#[test]
fn relocated_native_subdirectories_are_lossless_and_ambiguous_locations_are_refused() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let recorded = f.index.root_status(f.root).unwrap().volume;
    let (_, first) = f.relocate();
    let suffix = NativePath::from_bytes(b"notes/native-\xff".to_vec());
    let old_path = NativePath::from_path(&f.tree.join(suffix.as_path()));
    let first_path = NativePath::from_path(&first.mount_point.as_path().join(suffix.as_path()));
    let first_identity = f.volumes.locations.lock().unwrap()[&first.mount_point].clone();
    f.volumes
        .locations
        .lock()
        .unwrap()
        .insert(first_path.clone(), first_identity);
    assert_eq!(
        volume::mounted_elsewhere(
            f.volumes.as_ref(),
            &old_path,
            &recorded,
            std::slice::from_ref(&first)
        )
        .unwrap(),
        Some(first_path)
    );
    let mut second = first.clone();
    second.mount_id = 44;
    second.mount_point = NativePath::from_path(&f.tree.with_file_name("clone"));
    let mut identity = recorded.clone();
    identity.mount_point = Some(second.mount_point.clone());
    identity.mount_id = Some(second.mount_id);
    let second_path = NativePath::from_path(&second.mount_point.as_path().join(suffix.as_path()));
    f.volumes
        .locations
        .lock()
        .unwrap()
        .insert(second_path, identity);
    assert_eq!(
        volume::mounted_elsewhere(f.volumes.as_ref(), &old_path, &recorded, &[first, second])
            .unwrap(),
        None
    );
}

#[test]
fn weak_mismatched_or_shadowed_mounts_never_supply_a_location_hint() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let recorded = f.index.root_status(f.root).unwrap().volume;
    let (path, mount) = f.relocate();
    let original = f.volumes.locations.lock().unwrap()[&path].clone();
    for confidence in [VolumeConfidence::Weak, VolumeConfidence::None] {
        let mut weak = recorded.clone();
        weak.confidence = confidence;
        assert_eq!(
            volume::mounted_elsewhere(
                f.volumes.as_ref(),
                &NativePath::from_path(&f.tree),
                &weak,
                std::slice::from_ref(&mount)
            )
            .unwrap(),
            None
        );
    }
    for mismatch in 0..4 {
        let mut candidate = original.clone();
        match mismatch {
            0 => candidate.fs_uuid = Some("another-uuid".into()),
            1 => candidate.mount_root = Some(NativePath::from_bytes(b"/subvolume".to_vec())),
            2 => candidate.mount_id = Some(99),
            _ => candidate.confidence = VolumeConfidence::Weak,
        }
        f.volumes
            .locations
            .lock()
            .unwrap()
            .insert(path.clone(), candidate);
        assert_eq!(
            volume::mounted_elsewhere(
                f.volumes.as_ref(),
                &NativePath::from_path(&f.tree),
                &recorded,
                std::slice::from_ref(&mount)
            )
            .unwrap(),
            None
        );
    }
    f.volumes.locations.lock().unwrap().insert(path, original);
    let mut other_subvolume = mount;
    other_subvolume.mount_root = NativePath::from_bytes(b"/subvolume".to_vec());
    assert_eq!(
        volume::mounted_elsewhere(
            f.volumes.as_ref(),
            &NativePath::from_path(&f.tree),
            &recorded,
            &[other_subvolume]
        )
        .unwrap(),
        None
    );
}

#[test]
fn an_absent_volume_is_never_scanned_as_an_empty_directory() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let generation = f.index.generation().unwrap();
    let mut parent = f.index.root_status(f.root).unwrap().volume;
    parent.mount_point = Some(NativePath::from_path(f.tree.parent().unwrap()));
    *f.volumes.observed.lock().unwrap() = Some(parent);
    fs::remove_file(f.tree.join("notes/original.txt")).unwrap();
    assert!(f.index.scan_root(f.root, ScanOptions::default()).is_err());
    assert!(f.old().is_some());
    assert_eq!(f.index.generation().unwrap(), generation);
    let status = f.index.root_status(f.root).unwrap();
    assert_eq!(status.state, RootState::Offline);
    assert_eq!(status.offline_reason, Some(OfflineReason::VolumeAbsent));
    assert!(status.offline_since_ms.is_some());
    f.index
        .mark_stale(f.root, &templar_file_index::StaleReason::Unwatched)
        .unwrap();
    assert_eq!(
        f.index.root_status(f.root).unwrap().offline_reason,
        status.offline_reason
    );
}

#[test]
fn a_different_volume_at_the_same_path_keeps_the_old_rows_offline() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let observed = f.replacement();
    *f.volumes.observed.lock().unwrap() = Some(observed.clone());
    assert!(
        reconcile::reconcile(
            &f.index,
            &mut f.owner(),
            f.root,
            ScanScope::Root,
            &Trigger::Requested,
            ScanOptions::default()
        )
        .is_err()
    );
    assert!(f.old().is_some());
    assert_eq!(
        f.index.root_status(f.root).unwrap().offline_reason,
        Some(OfflineReason::DifferentVolume { observed })
    );
}

#[test]
fn accepting_a_new_volume_forgets_the_old_rows_in_one_published_generation() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let before = f.index.generation().unwrap();
    let observed = f.replacement();
    *f.volumes.observed.lock().unwrap() = Some(observed.clone());
    assert!(volume::ensure_present(&f.index, f.root).is_err());
    assert!(matches!(
        f.index.accept_volume(f.root, &"0".repeat(64)),
        Err(IndexError::SnapshotRequired { .. })
    ));
    assert!(f.old().is_some());
    assert_eq!(f.index.generation().unwrap(), before);
    let status = f
        .index
        .accept_volume(f.root, &volume::fingerprint(&observed))
        .unwrap();
    assert_eq!(status.state, RootState::Building);
    assert!(!status.complete);
    assert!(status.offline_reason.is_none());
    assert_eq!(status.volume.fs_uuid, observed.fs_uuid);
    assert_eq!(status.generation.0, before.0 + 1);
    assert_eq!(f.index.generation().unwrap(), status.generation);
    assert!(f.old().is_none());
    assert_eq!(status.entry_count, 0);
    assert!(matches!(
        f.index
            .accept_volume(f.root, &volume::fingerprint(&observed)),
        Err(IndexError::SnapshotRequired { .. })
    ));
}

#[test]
fn a_replacement_changed_after_confirmation_is_not_accepted() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let observed = f.replacement();
    *f.volumes.observed.lock().unwrap() = Some(observed.clone());
    assert!(volume::ensure_present(&f.index, f.root).is_err());
    let before = f.index.generation().unwrap();
    let mut other = observed.clone();
    other.fs_uuid = Some("third-uuid".into());
    *f.volumes.observed.lock().unwrap() = Some(other);
    assert!(matches!(
        f.index
            .accept_volume(f.root, &volume::fingerprint(&observed)),
        Err(IndexError::SnapshotRequired { .. })
    ));
    assert_eq!(f.index.generation().unwrap(), before);
    assert!(f.old().is_some());
}

#[test]
fn an_unverified_return_runs_a_full_metadata_reconcile() {
    let f = Fixture::new(VolumeConfidence::Weak);
    f.index
        .mark_offline_reason(f.root, &OfflineReason::Unmounted)
        .unwrap();
    fs::write(
        f.tree.join("notes/original.txt"),
        b"a much larger changed file",
    )
    .unwrap();
    let (outcome, report) = reconcile::reconcile(
        &f.index,
        &mut f.owner(),
        f.root,
        ScanScope::Subtree(NativePath::from_path(&f.tree.join("notes"))),
        &Trigger::NamespaceOnly,
        ScanOptions::default(),
    )
    .unwrap();
    assert!(outcome.complete);
    assert_eq!(report.dirs_proven_unchanged, 0);
    assert_eq!(
        outcome.metadata,
        templar_file_index::MetadataFreshness::Verified
    );
    assert_eq!(
        f.old().unwrap().meta.size_bytes,
        b"a much larger changed file".len() as u64
    );
    assert_eq!(
        f.index.root_status(f.root).unwrap().state,
        RootState::Current
    );
}

#[test]
fn a_mount_appearing_inside_a_root_becomes_a_boundary_and_tombstones_shadowed_rows() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let mut owner = f.owner();
    // Same physical device: only a mount ID distinguishes this fake bind mount.
    *f.volumes.boundary.lock().unwrap() = Some(NativePath::from_path(&f.tree.join("notes")));
    let (outcome, _) = reconcile::reconcile(
        &f.index,
        &mut owner,
        f.root,
        ScanScope::Subtree(NativePath::from_path(&f.tree.join("notes"))),
        &Trigger::Requested,
        ScanOptions::default(),
    )
    .unwrap();
    assert!(outcome.complete);
    assert!(f.old().is_none());
    assert!(
        f.index
            .entry(f.root, &NativePath::from_path(&f.tree.join("notes")))
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_returned_uuid_can_have_a_new_mount_id_without_losing_children() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let mut returned = f.index.root_status(f.root).unwrap().volume;
    returned.mount_id = Some(100);
    *f.volumes.observed.lock().unwrap() = Some(returned);
    f.index
        .mark_offline_reason(f.root, &OfflineReason::Unmounted)
        .unwrap();
    let outcome = f.index.scan_root(f.root, ScanOptions::default()).unwrap();
    assert!(outcome.complete);
    assert!(f.old().is_some());
    assert_eq!(
        f.index.root_status(f.root).unwrap().volume.mount_id,
        Some(100)
    );
}

#[test]
fn the_same_uuid_at_a_different_mount_root_is_a_different_volume() {
    let f = Fixture::new(VolumeConfidence::Strong);
    let recorded = f.index.root_status(f.root).unwrap().volume;
    let mut observed = recorded.clone();
    observed.mount_root = Some(NativePath::from_bytes(b"/another-subvolume".to_vec()));
    assert!(matches!(
        volume::compare(&recorded, Some(observed)),
        Presence::Different(_)
    ));
}

#[test]
fn unknown_presence_never_scans_or_erases_cached_contents() {
    let f = Fixture::new(VolumeConfidence::Strong);
    *f.volumes.unreadable.lock().unwrap() = true;
    assert!(f.index.scan_root(f.root, ScanOptions::default()).is_err());
    assert!(f.old().is_some());
    assert!(matches!(
        f.index.root_status(f.root).unwrap().offline_reason,
        Some(OfflineReason::PresenceUnknown { .. })
    ));
}
