//! Mount-aware volume identity. Paths remain native bytes; mount IDs locate observations but
//! are not persistent identity across remounts. UUID plus mount-root is the strong fence.
use crate::{
    Index, IndexError, NativePath, OfflineReason, Result, RootId, RootStatus, VolumeConfidence,
    VolumeIdentity,
};
use sha2::{Digest, Sha256};
use std::{
    io,
    os::unix::fs::{FileTypeExt as _, MetadataExt as _},
    path::Path,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountRecord {
    pub mount_id: u64,
    pub parent_id: u64,
    pub major: u32,
    pub minor: u32,
    pub mount_root: NativePath,
    pub mount_point: NativePath,
    pub fs_type: String,
}

fn unescape(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let octal = bytes.get(i + 1..i + 4)?;
            if !octal.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                return None;
            }
            let value = u16::from(octal[0] - b'0') * 64
                + u16::from(octal[1] - b'0') * 8
                + u16::from(octal[2] - b'0');
            out.push(u8::try_from(value).ok()?);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[must_use]
pub fn parse_mountinfo(bytes: &[u8]) -> Vec<MountRecord> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|line| {
            let fields: Vec<_> = line
                .split(|byte| *byte == b' ')
                .filter(|part| !part.is_empty())
                .collect();
            let separator = fields.iter().position(|part| *part == b"-")?;
            if separator < 6 || fields.len() < separator + 4 {
                return None;
            }
            let number = |bytes: &[u8]| std::str::from_utf8(bytes).ok()?.parse::<u64>().ok();
            let mut device = fields[2].split(|byte| *byte == b':');
            let major = u32::try_from(number(device.next()?)?).ok()?;
            let minor = u32::try_from(number(device.next()?)?).ok()?;
            if device.next().is_some() {
                return None;
            }
            let root = unescape(fields[3])?;
            let point = unescape(fields[4])?;
            if root.first() != Some(&b'/')
                || point.first() != Some(&b'/')
                || root.contains(&0)
                || point.contains(&0)
            {
                return None;
            }
            Some(MountRecord {
                mount_id: number(fields[0])?,
                parent_id: number(fields[1])?,
                major,
                minor,
                mount_root: NativePath::from_bytes(root),
                mount_point: NativePath::from_bytes(point),
                fs_type: std::str::from_utf8(fields[separator + 1]).ok()?.to_owned(),
            })
        })
        .collect()
}

#[must_use]
pub fn mount_for<'a>(path: &NativePath, mounts: &'a [MountRecord]) -> Option<&'a MountRecord> {
    mounts
        .iter()
        .filter(|record| path.starts_with(&record.mount_point))
        .max_by_key(|record| record.mount_point.as_bytes().len())
}

/// Injectable only through IndexConfig. A missing path is None; inability to observe is Err,
/// never evidence that an empty parent mount is the original disk. A mount table that
/// disagrees with `stat` is also None: the volume is unobservable, and callers route it
/// through the absent/offline flow with cached rows retained.
pub trait VolumeProvider: std::fmt::Debug + Send + Sync {
    fn identify(&self, path: &NativePath) -> io::Result<Option<VolumeIdentity>>;
    /// Limit discovery before stating candidate directories. Injected providers can inspect
    /// the supplied table; the native provider checks the UUID's block device first.
    fn relocation_mounts<'a>(
        &self,
        _recorded: &VolumeIdentity,
        mounts: &'a [MountRecord],
    ) -> io::Result<Vec<&'a MountRecord>> {
        Ok(mounts.iter().collect())
    }
    fn mount_id(&self, path: &NativePath) -> io::Result<Option<u64>> {
        Ok(self.identify(path)?.and_then(|identity| identity.mount_id))
    }
}

#[derive(Debug)]
pub struct NativeVolumes;
impl VolumeProvider for NativeVolumes {
    fn identify(&self, path: &NativePath) -> io::Result<Option<VolumeIdentity>> {
        identify(path.as_path())
    }
    fn relocation_mounts<'a>(
        &self,
        recorded: &VolumeIdentity,
        mounts: &'a [MountRecord],
    ) -> io::Result<Vec<&'a MountRecord>> {
        let Some(uuid) = &recorded.fs_uuid else {
            return Ok(Vec::new());
        };
        // A cached identifier is one directory entry, never a pathname into another domain.
        if uuid.is_empty()
            || Path::new(uuid).components().count() != 1
            || !matches!(
                Path::new(uuid).components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Ok(Vec::new());
        }
        let device = match std::fs::metadata(Path::new("/dev/disk/by-uuid").join(uuid)) {
            Ok(metadata) if metadata.file_type().is_block_device() => metadata.rdev(),
            Ok(_) => return Ok(Vec::new()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(mounts
            .iter()
            .filter(|mount| rustix::fs::makedev(mount.major, mount.minor) == device)
            .collect())
    }
    fn mount_id(&self, path: &NativePath) -> io::Result<Option<u64>> {
        let stat = rustix::fs::statx(
            rustix::fs::CWD,
            path.as_path(),
            rustix::fs::AtFlags::empty(),
            rustix::fs::StatxFlags::MNT_ID,
        )?;
        Ok((stat.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0).then_some(stat.stx_mnt_id))
    }
}

/// The mount record covering `native`, fenced against the `stat` device: `Ok(None)` when
/// the table device disagrees with `stat`. Stacking and subvolume filesystems (overlayfs,
/// btrfs) legitimately report a `st_dev` that differs from the mount table device, so that is
/// an unobservable volume rather than a fatal error — callers route `None` through the
/// absent/offline flow, which retains cached rows. Only a path no record covers is `Err`,
/// never evidence that an empty parent mount is the original disk.
fn fenced_mount<'a>(
    mounts: &'a [MountRecord],
    native: &NativePath,
    mnt_id: Option<u64>,
    dev: u64,
) -> io::Result<Option<&'a MountRecord>> {
    let mount = mnt_id
        .and_then(|id| mounts.iter().find(|mount| mount.mount_id == id))
        .or_else(|| mount_for(native, mounts))
        .ok_or_else(|| io::Error::other("no mount record covers the native root"))?;
    if rustix::fs::makedev(mount.major, mount.minor) != dev {
        return Ok(None);
    }
    Ok(Some(mount))
}

pub fn identify(path: &Path) -> io::Result<Option<VolumeIdentity>> {
    let metadata = match path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mounts = parse_mountinfo(&std::fs::read("/proc/self/mountinfo")?);
    let native = NativePath::from_path(path);
    let statx = rustix::fs::statx(
        rustix::fs::CWD,
        path,
        rustix::fs::AtFlags::empty(),
        rustix::fs::StatxFlags::MNT_ID,
    )
    .ok();
    let mnt_id = statx
        .as_ref()
        .filter(|stat| stat.stx_mask & rustix::fs::StatxFlags::MNT_ID.bits() != 0)
        .map(|stat| stat.stx_mnt_id);
    let Some(mount) = fenced_mount(&mounts, &native, mnt_id, metadata.dev())? else {
        return Ok(None);
    };
    let fsid = rustix::fs::statfs(path)
        .ok()
        .map(|stat| format!("{:?}", stat.f_fsid));
    let fs_uuid = std::fs::read_dir("/dev/disk/by-uuid")
        .ok()
        .and_then(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .find_map(|entry| {
                    (entry.path().metadata().ok()?.rdev() == metadata.dev())
                        .then(|| entry.file_name().into_string().ok())
                        .flatten()
                })
        });
    let mut identity = VolumeIdentity {
        dev_id: Some(metadata.dev()),
        uuid: fs_uuid.clone(),
        fs_uuid,
        fs_type: Some(mount.fs_type.clone()),
        fsid,
        mount_point: Some(mount.mount_point.clone()),
        mount_root: Some(mount.mount_root.clone()),
        mount_id: Some(mount.mount_id),
        ..VolumeIdentity::default()
    };
    identity.confidence = confidence(&identity);
    Ok(Some(identity))
}

#[must_use]
pub fn confidence(identity: &VolumeIdentity) -> VolumeConfidence {
    if identity
        .fs_uuid
        .as_ref()
        .is_some_and(|uuid| !uuid.is_empty())
        && identity.mount_root.is_some()
    {
        VolumeConfidence::Strong
    } else if identity.dev_id.is_some() || identity.fsid.is_some() {
        VolumeConfidence::Weak
    } else {
        VolumeConfidence::None
    }
}

#[must_use]
pub fn fingerprint(identity: &VolumeIdentity) -> String {
    // Struct field order is deterministic; never serialize an unordered map into this fence.
    let bytes = serde_json::to_vec(identity).expect("volume identity has only serializable fields");
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Presence {
    Present {
        identity: VolumeIdentity,
        verified: bool,
    },
    Absent,
    Different(VolumeIdentity),
}

#[must_use]
pub fn compare(recorded: &VolumeIdentity, observed: Option<VolumeIdentity>) -> Presence {
    // Records predating identity capture retain legacy acquisition, but no return is verified.
    if recorded.confidence == VolumeConfidence::None {
        return Presence::Present {
            identity: observed.unwrap_or_default(),
            verified: false,
        };
    }
    let Some(observed) = observed else {
        return Presence::Absent;
    };
    if recorded.mount_point != observed.mount_point {
        return Presence::Absent;
    }
    if recorded.confidence == VolumeConfidence::Strong {
        if observed.confidence != VolumeConfidence::Strong {
            // Parent FS exposed at an empty mountpoint, or no persistent observed identity.
            return Presence::Absent;
        }
        if recorded.fs_uuid != observed.fs_uuid || recorded.mount_root != observed.mount_root {
            return Presence::Different(observed);
        }
        Presence::Present {
            identity: observed,
            verified: true,
        }
    } else if recorded.dev_id != observed.dev_id || recorded.fsid != observed.fsid {
        Presence::Absent
    } else {
        Presence::Present {
            identity: observed,
            verified: false,
        }
    }
}

pub fn presence(index: &Index, root: RootId) -> Result<Presence> {
    let status = index.root_status(root)?;
    Ok(compare(
        &status.volume,
        index.config().volume_provider.identify(&status.path)?,
    ))
}

/// A unique observed location of this strong identity, for an explicit new registration.
/// This never updates a root path, scans its contents or accepts a replacement volume.
pub fn mounted_elsewhere(
    provider: &dyn VolumeProvider,
    path: &NativePath,
    recorded: &VolumeIdentity,
    mounts: &[MountRecord],
) -> io::Result<Option<NativePath>> {
    if recorded.confidence != VolumeConfidence::Strong {
        return Ok(None);
    }
    let (Some(uuid), Some(old_mount), Some(mount_root)) = (
        &recorded.fs_uuid,
        &recorded.mount_point,
        &recorded.mount_root,
    ) else {
        return Ok(None);
    };
    if uuid.is_empty()
        || matches!(
            compare(recorded, provider.identify(path)?),
            Presence::Present { .. }
        )
    {
        return Ok(None);
    }
    let Ok(relative) = path.as_path().strip_prefix(old_mount.as_path()) else {
        return Ok(None);
    };
    if relative
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Ok(None);
    }
    let mut found = None;
    for mount in provider.relocation_mounts(recorded, mounts)? {
        if mount.mount_point == *old_mount || mount.mount_root != *mount_root {
            continue;
        }
        let candidate = if relative.as_os_str().is_empty() {
            mount.mount_point.clone()
        } else {
            NativePath::from_path(&mount.mount_point.as_path().join(relative))
        };
        let Some(observed) = provider.identify(&candidate)? else {
            continue;
        };
        if observed.confidence != VolumeConfidence::Strong
            || observed.fs_uuid.as_ref() != Some(uuid)
            || observed.mount_root.as_ref() != Some(mount_root)
            || observed.mount_point.as_ref() != Some(&mount.mount_point)
            || observed.mount_id != Some(mount.mount_id)
        {
            continue;
        }
        if found
            .as_ref()
            .is_some_and(|previous| previous != &candidate)
        {
            // Clones/bind mounts with the same UUID cannot be picked on the person's behalf.
            return Ok(None);
        }
        found = Some(candidate);
    }
    Ok(found)
}

pub fn ensure_present(index: &Index, root: RootId) -> Result<VolumeIdentity> {
    match presence(index, root) {
        Ok(Presence::Present { identity, .. }) => Ok(identity),
        Ok(Presence::Absent) => {
            index.mark_offline_reason(root, &OfflineReason::VolumeAbsent)?;
            Err(IndexError::Unsupported {
                detail: "the indexed volume is absent".into(),
            })
        }
        Ok(Presence::Different(observed)) => {
            index.mark_offline_reason(root, &OfflineReason::DifferentVolume { observed })?;
            Err(IndexError::Unsupported {
                detail: "a different volume occupies the indexed path".into(),
            })
        }
        Err(error) => {
            index.mark_offline_reason(
                root,
                &OfflineReason::PresenceUnknown {
                    detail: error.to_string(),
                },
            )?;
            Err(error)
        }
    }
}

pub(crate) fn store_identity(
    connection: &rusqlite::Connection,
    root: RootId,
    volume: &VolumeIdentity,
) -> Result<()> {
    let confidence = match volume.confidence {
        VolumeConfidence::Strong => "strong",
        VolumeConfidence::Weak => "weak",
        VolumeConfidence::None => "none",
    };
    connection.execute(
        "UPDATE roots SET volume_dev_id=?2, volume_uuid=?3, volume_mount=?4,
        volume_label=?5, volume_fs_type=?6, volume_fs_uuid=?3, volume_fsid=?7,
        volume_mount_root=?8, volume_mount_id=?9, volume_confidence=?10 WHERE root_id=?1",
        rusqlite::params![
            root.0,
            volume.dev_id.map(|id| id as i64),
            volume.fs_uuid.as_ref().or(volume.uuid.as_ref()),
            volume.mount_point.as_ref().map(NativePath::as_bytes),
            volume.label,
            volume.fs_type,
            volume.fsid,
            volume.mount_root.as_ref().map(NativePath::as_bytes),
            volume.mount_id.map(|id| id as i64),
            confidence
        ],
    )?;
    Ok(())
}

impl Index {
    pub fn mark_offline_reason(&self, root: RootId, reason: &OfflineReason) -> Result<RootStatus> {
        let json = serde_json::to_string(reason)?;
        let kind = match reason {
            OfflineReason::VolumeAbsent => "volume-absent",
            OfflineReason::DifferentVolume { .. } => "different-volume",
            OfflineReason::Unmounted => "unmounted",
            OfflineReason::PresenceUnknown { .. } => "presence-unknown",
        };
        self.with_write(|connection| {
            let tx = connection.transaction()?;
            crate::set_state(&tx, root, crate::RootState::Offline, Some(kind))?;
            tx.execute("UPDATE roots SET offline_reason_json=?2, offline_since_ms=COALESCE(offline_since_ms,?3) WHERE root_id=?1",
                rusqlite::params![root.0,json,crate::model::now_ms()])?;
            tx.commit()?;
            Ok(())
        })?;
        let status = self.root_status(root)?;
        self.notify_root(&status);
        Ok(status)
    }

    /// An explicit fence is required before replacing cached contents. Re-observe and compare
    /// inside the writer transaction, so a late confirmation cannot adopt a changed mount.
    pub fn accept_volume(&self, root: RootId, expected: &str) -> Result<RootStatus> {
        let _claim = self.claim_scan(root)?;
        let (generation, change) = self.with_write(|connection| {
            let tx = connection.transaction()?;
            let status = crate::read_root(&tx, root)?;
            let Some(OfflineReason::DifferentVolume { observed: confirmed }) = &status.offline_reason else {
                return Err(IndexError::SnapshotRequired { reason: "refresh the different-volume observation before accepting" });
            };
            if status.state != crate::RootState::Offline || fingerprint(confirmed) != expected {
                return Err(IndexError::SnapshotRequired { reason: "the replacement confirmation no longer matches the offline root" });
            }
            let observed = self.config().volume_provider.identify(&status.path)?
                .ok_or_else(|| IndexError::Unsupported { detail: "the replacement volume is absent".into() })?;
            if fingerprint(&observed) != expected || !matches!(compare(&status.volume, Some(observed.clone())), Presence::Different(_)) {
                return Err(IndexError::SnapshotRequired { reason: "the observed volume changed before confirmation" });
            }
            let generation = crate::publish_generation(&tx)?;
            tx.execute("UPDATE entries SET deleted_generation=?2, written_generation=?2 WHERE root_id=?1 AND deleted_generation IS NULL",
                rusqlite::params![root.0,generation.0 as i64])?;
            tx.execute("DELETE FROM entries_staging WHERE root_id=?1",[root.0])?;
            store_identity(&tx, root, &observed)?;
            crate::set_state(&tx, root, crate::RootState::Building, Some("volume-accepted"))?;
            tx.execute("UPDATE roots SET committed_generation=?2, complete=0, entry_count=0,
                building_generation=NULL, offline_reason_json=NULL, offline_since_ms=NULL WHERE root_id=?1",
                rusqlite::params![root.0,generation.0 as i64])?;
            if self.fts_available() { tx.execute("DELETE FROM entries_fts WHERE rowid IN (SELECT entry_id FROM entries WHERE root_id=?1)",[root.0])?; }
            let mut change = crate::Change { seq:0, generation, root, op:crate::ChangeOp::Rescan,
                entry_id:None,path:status.path,old_path:None,kind:crate::EntryKind::Directory,size_bytes:0,mtime_ns:0,at_ms:crate::model::now_ms() };
            change.seq = crate::mutation::append_change(&tx,&change)?;
            tx.commit()?;
            Ok((generation, change))
        })?;
        if let Some(sink) = self.sink() {
            sink.generation_advanced(generation, &[change]);
        }
        let status = self.root_status(root)?;
        self.notify_root(&status);
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mountinfo_parsing_decodes_escaped_mount_points() {
        let records=parse_mountinfo(b"42 1 8:3 /sub\\040root /mnt/a\\040b\\134c rw shared:1 - ext4 /dev/sda3 rw\n43 1 0:3 / /native-\xff rw - tmpfs tmpfs rw\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].mount_point.as_bytes(), b"/mnt/a b\\c");
        assert_eq!(records[0].mount_root.as_bytes(), b"/sub root");
        assert_eq!(records[1].mount_point.as_bytes(), b"/native-\xff");
        assert!(parse_mountinfo(b"1 2 3:4 / /bad\\999 rw - ext4 /dev/a rw\n").is_empty());
    }
    #[test]
    fn the_longest_mount_prefix_owns_a_path() {
        let records=parse_mountinfo(b"1 0 8:1 / / rw - ext4 /dev/a rw\n2 1 8:2 / /mnt/a rw - ext4 /dev/b rw\n3 2 8:3 / /mnt/a/b rw - ext4 /dev/c rw\n");
        assert_eq!(
            mount_for(&NativePath::from_bytes(b"/mnt/a/b/c".to_vec()), &records)
                .unwrap()
                .mount_id,
            3
        );
        assert_eq!(
            mount_for(&NativePath::from_bytes(b"/mnt/ab".to_vec()), &records)
                .unwrap()
                .mount_id,
            1
        );
    }
    #[test]
    fn a_mount_table_device_that_disagrees_with_stat_is_unobservable_not_fatal() {
        // btrfs `/`: the table carries 0:35 while subvolume stats report 0:47.
        let mounts =
            parse_mountinfo(b"45 1 0:35 /@ / rw,relatime shared:1 - btrfs /dev/nvme1n1p6 rw\n");
        let native = NativePath::from_bytes(b"/home/dk".to_vec());
        assert_eq!(
            fenced_mount(&mounts, &native, Some(45), rustix::fs::makedev(0, 47)).unwrap(),
            None
        );
        assert!(
            fenced_mount(&mounts, &native, Some(45), rustix::fs::makedev(0, 35))
                .unwrap()
                .is_some()
        );
        assert!(fenced_mount(&[], &native, None, rustix::fs::makedev(0, 35)).is_err());
    }
    #[test]
    fn identity_confidence_is_strong_only_with_uuid_and_mount_root() {
        let mut identity = VolumeIdentity::default();
        assert_eq!(confidence(&identity), VolumeConfidence::None);
        identity.dev_id = Some(1);
        assert_eq!(confidence(&identity), VolumeConfidence::Weak);
        identity.fs_uuid = Some("uuid".into());
        assert_eq!(confidence(&identity), VolumeConfidence::Weak);
        identity.mount_root = Some(NativePath::from_bytes(b"/".to_vec()));
        assert_eq!(confidence(&identity), VolumeConfidence::Strong);
    }
}
