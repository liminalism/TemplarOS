//! Storage information that must be unavoidable in the UI, and the reserve policy.
//!
//! The OS reserves enough space for logging in, updating and recovering. When a volume approaches
//! that reserve, applications are prevented from consuming the last usable blocks without an
//! explicit override. A full root filesystem is almost as destructive as exhausted RAM.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub mount_point: std::path::PathBuf,
    pub fs_type: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    /// Space the OS keeps back for login/update/recovery.
    pub reserved_bytes: u64,
    pub read_only: bool,
}

impl VolumeInfo {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }

    /// What applications may still consume before the reserve.
    pub fn usable_bytes(&self) -> u64 {
        self.available_bytes.saturating_sub(self.reserved_bytes)
    }
}

/// Reserve check outcome for a pending write of `n` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReserveVerdict {
    Ok,
    /// Fits, but the volume will be within 2× the reserve afterwards: warn.
    Warn,
    /// Would eat into the reserve; refuse unless the user overrides.
    Refuse,
}

pub fn check_reserve(volume: &VolumeInfo, pending_bytes: u64) -> ReserveVerdict {
    let after = volume.available_bytes.saturating_sub(pending_bytes);
    if after < volume.reserved_bytes {
        ReserveVerdict::Refuse
    } else if after < volume.reserved_bytes.saturating_mul(2) {
        ReserveVerdict::Warn
    } else {
        ReserveVerdict::Ok
    }
}

/// Reserve size for a volume: `percent` of the total, clamped to `[min, max]`.
pub fn reserve_for(total_bytes: u64, percent: u64, min: u64, max: u64) -> u64 {
    (total_bytes / 100).saturating_mul(percent).clamp(min, max)
}

/// Query a live volume via `statvfs`.
pub fn query(mount_point: &Path, reserve_percent: u64) -> anyhow::Result<VolumeInfo> {
    let (total, free, available, read_only) = statvfs(mount_point)?;
    let reserved = reserve_for(
        total,
        reserve_percent,
        512 * 1024 * 1024,
        16 * 1024 * 1024 * 1024,
    );
    Ok(VolumeInfo {
        mount_point: mount_point.to_path_buf(),
        fs_type: fs_type_of(mount_point).unwrap_or_else(|| "unknown".into()),
        total_bytes: total,
        free_bytes: free,
        available_bytes: available,
        reserved_bytes: reserved,
        read_only,
    })
}

fn statvfs(path: &Path) -> anyhow::Result<(u64, u64, u64, bool)> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs writes into the zeroed struct we pass; c_path outlives the call.
    #[allow(unsafe_code)]
    let stats = unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &raw mut stats) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        stats
    };
    // libc's statvfs fields are u64 on x86_64 (fsblkcnt_t); the `as` casts keep 32-bit targets building.
    #[allow(clippy::unnecessary_cast)]
    let (frsize, blocks, bfree, bavail) = (
        stats.f_frsize as u64,
        stats.f_blocks as u64,
        stats.f_bfree as u64,
        stats.f_bavail as u64,
    );
    Ok((
        blocks * frsize,
        bfree * frsize,
        bavail * frsize,
        stats.f_flag & libc::ST_RDONLY != 0,
    ))
}

fn fs_type_of(mount_point: &Path) -> Option<String> {
    let mounts = std::fs::read_to_string(MOUNTS).ok()?;
    mount_entries(&mounts)
        .into_iter()
        .find(|entry| entry.mount_point == mount_point)
        .map(|entry| entry.fs_type)
}

const MOUNTS: &str = "/proc/self/mounts";

/// Filesystem types that are never a "drive" in the Drives drawer: kernel and container
/// plumbing the user has no business seeing next to their USB stick.
const PSEUDO_FILESYSTEMS: &[&str] = &[
    "autofs",
    "bpf",
    "binfmt_misc",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fuse.gvfsd-fuse",
    "fuse.portal",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "overlay",
    "proc",
    "pstore",
    "ramfs",
    "securityfs",
    "selinuxfs",
    "squashfs",
    "sysfs",
    "tracefs",
];

/// One line of `/proc/self/mounts`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: std::path::PathBuf,
    pub fs_type: String,
    pub options: Vec<String>,
}

impl MountEntry {
    /// Whether this is something a person would call a drive.
    pub fn is_user_visible(&self) -> bool {
        !PSEUDO_FILESYSTEMS.contains(&self.fs_type.as_str())
            && (self.device.starts_with('/') || self.fs_type == "zfs" || self.fs_type == "nfs")
    }

    pub fn read_only(&self) -> bool {
        self.options.iter().any(|option| option == "ro")
    }
}

/// Parse the mount table. `/proc` escapes spaces, tabs, newlines and backslashes in the device
/// and mount-point fields as octal, so a volume mounted at `/media/My Drive` parses correctly
/// rather than being silently truncated at the space.
pub fn mount_entries(mounts: &str) -> Vec<MountEntry> {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let device = unescape_octal(fields.next()?);
            let mount_point = std::path::PathBuf::from(unescape_octal(fields.next()?));
            let fs_type = fields.next()?.to_owned();
            let options = fields
                .next()
                .map(|value| value.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            Some(MountEntry {
                device,
                mount_point,
                fs_type,
                options,
            })
        })
        .collect()
}

fn unescape_octal(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 4])
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 8).ok())
        {
            out.push(byte);
            index += 4;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every user-visible volume, with its free space and reserve.
///
/// This is what makes storage information unavoidable: the sidebar, the Drives drawer and the
/// file chooser all render the same list rather than each inventing one, and none of them can
/// show a drive without also showing how full it is.
pub fn volumes(reserve_percent: u64) -> anyhow::Result<Vec<VolumeInfo>> {
    let mounts = std::fs::read_to_string(MOUNTS).with_context(|| format!("reading {MOUNTS}"))?;
    let mut seen = std::collections::BTreeSet::new();
    let mut volumes = Vec::new();
    for entry in mount_entries(&mounts) {
        if !entry.is_user_visible() || !seen.insert(entry.mount_point.clone()) {
            continue;
        }
        match query(&entry.mount_point, reserve_percent) {
            Ok(mut info) => {
                // A bind mount reports the same device; keep the first, which is the real one.
                info.read_only |= entry.read_only();
                info.fs_type.clone_from(&entry.fs_type);
                volumes.push(info);
            }
            Err(error) => tracing::debug!(
                mount = %entry.mount_point.display(),
                %error,
                "skipping a volume that could not be queried"
            ),
        }
    }
    volumes.sort_by(|left, right| left.mount_point.cmp(&right.mount_point));
    Ok(volumes)
}

/// The mount point that holds `path`, from the mount table rather than by walking devices.
pub fn mount_point_of(path: &Path, mounts: &str) -> Option<std::path::PathBuf> {
    mount_entries(mounts)
        .into_iter()
        .filter(|entry| path.starts_with(&entry.mount_point))
        .max_by_key(|entry| entry.mount_point.components().count())
        .map(|entry| entry.mount_point)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(available: u64, reserved: u64) -> VolumeInfo {
        VolumeInfo {
            mount_point: "/".into(),
            fs_type: "btrfs".into(),
            total_bytes: 100,
            free_bytes: available,
            available_bytes: available,
            reserved_bytes: reserved,
            read_only: false,
        }
    }

    #[test]
    fn reserve_verdicts() {
        assert_eq!(check_reserve(&volume(100, 10), 50), ReserveVerdict::Ok);
        assert_eq!(check_reserve(&volume(100, 10), 85), ReserveVerdict::Warn);
        assert_eq!(check_reserve(&volume(100, 10), 95), ReserveVerdict::Refuse);
        assert_eq!(volume(100, 10).usable_bytes(), 90);
    }

    #[test]
    fn reserve_size_is_clamped() {
        assert_eq!(reserve_for(1_000, 5, 10, 40), 40);
        assert_eq!(reserve_for(1_000, 5, 60, 400), 60);
        assert_eq!(reserve_for(1_000, 5, 10, 400), 50);
    }

    #[test]
    fn root_can_be_queried() {
        let info = query(Path::new("/"), 5).unwrap();
        assert!(info.total_bytes > 0);
    }

    const SAMPLE_MOUNTS: &str = concat!(
        "proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n",
        "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n",
        "/dev/nvme0n1p2 / btrfs rw,relatime,subvol=/@ 0 0\n",
        "/dev/nvme0n1p1 /boot/efi vfat ro,relatime 0 0\n",
        "/dev/sdb1 /media/My\\040Drive exfat rw,nosuid,relatime 0 0\n",
        "tmpfs /run/user/1000 tmpfs rw,nosuid,nodev,relatime 0 0\n",
    );

    #[test]
    fn the_mount_table_parses_including_escaped_names() {
        let entries = mount_entries(SAMPLE_MOUNTS);
        assert_eq!(entries.len(), 6);
        let removable = entries
            .iter()
            .find(|entry| entry.fs_type == "exfat")
            .unwrap();
        assert_eq!(
            removable.mount_point,
            std::path::PathBuf::from("/media/My Drive")
        );
        assert!(!removable.read_only());

        let esp = entries
            .iter()
            .find(|entry| entry.fs_type == "vfat")
            .unwrap();
        assert!(esp.read_only());
    }

    #[test]
    fn kernel_filesystems_are_not_drives() {
        let entries = mount_entries(SAMPLE_MOUNTS);
        let visible: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.is_user_visible())
            .map(|entry| entry.fs_type.as_str())
            .collect();
        assert_eq!(visible, ["btrfs", "vfat", "exfat"]);
    }

    #[test]
    fn the_deepest_mount_point_wins() {
        assert_eq!(
            mount_point_of(Path::new("/boot/efi/EFI/templaros"), SAMPLE_MOUNTS),
            Some(std::path::PathBuf::from("/boot/efi"))
        );
        assert_eq!(
            mount_point_of(Path::new("/home/u/x"), SAMPLE_MOUNTS),
            Some(std::path::PathBuf::from("/"))
        );
        assert_eq!(mount_point_of(Path::new("relative"), SAMPLE_MOUNTS), None);
    }

    #[test]
    fn this_machine_has_at_least_a_root_volume() {
        let listed = volumes(5).unwrap();
        assert!(
            listed
                .iter()
                .any(|volume| volume.mount_point == Path::new("/")),
            "no root volume in {listed:?}"
        );
        assert!(listed.iter().all(|volume| volume.total_bytes > 0));
    }
}
