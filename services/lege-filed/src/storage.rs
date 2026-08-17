//! Storage information that must be unavoidable in the UI, and the reserve policy.
//!
//! The OS reserves enough space for logging in, updating and recovering. When a volume approaches
//! that reserve, applications are prevented from consuming the last usable blocks without an
//! explicit override. A full root filesystem is almost as destructive as exhausted RAM.

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
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let wanted = mount_point.to_string_lossy();
    mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let _device = fields.next()?;
        let point = fields.next()?;
        let fs = fields.next()?;
        (point == wanted).then(|| fs.to_owned())
    })
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
}
