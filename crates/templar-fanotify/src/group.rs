//! One fanotify group: the fd, the flags it was opened with, and the marks placed on it.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    path::Path,
};

use crate::sys;

/// What one mark covers.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MarkScope {
    /// One mark per directory, plus `FAN_EVENT_ON_CHILD`. The only scope an unprivileged process
    /// may use, and the same per-inode accounting inotify has.
    #[default]
    Inode,
    /// One mark per mount. Needs `CAP_SYS_ADMIN`.
    Mount,
    /// One mark per filesystem — the variant that would make the watch-count problem disappear.
    /// Needs `CAP_SYS_ADMIN`, and refuses a Btrfs subvolume whose fsid differs from the
    /// superblock root's.
    Filesystem,
}

impl MarkScope {
    #[must_use]
    pub fn flag(self) -> libc::c_uint {
        match self {
            Self::Inode => libc::FAN_MARK_INODE,
            Self::Mount => libc::FAN_MARK_MOUNT,
            Self::Filesystem => libc::FAN_MARK_FILESYSTEM,
        }
    }

    #[must_use]
    pub fn needs_cap_sys_admin(self) -> bool {
        self != Self::Inode
    }
}

/// How a group is opened.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub scope: MarkScope,
    /// Ask for `FAN_RENAME`: one event carrying the old and the new directory and name, instead of
    /// an unpaired `FAN_MOVED_FROM` and `FAN_MOVED_TO` with nothing linking them. fanotify has no
    /// equivalent of inotify's rename cookie, so without this a rename cannot be reconstructed at
    /// all — it degrades into a delete plus an unrelated create.
    pub atomic_rename: bool,
    /// Add `FAN_REPORT_TARGET_FID`, which names the moved inode itself as well as its two
    /// directories. Kernel 5.17 and later.
    pub target_fid: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scope: MarkScope::Inode,
            atomic_rename: true,
            target_fid: false,
        }
    }
}

impl Config {
    #[must_use]
    pub fn init_flags(self) -> libc::c_uint {
        let mut flags = libc::FAN_CLASS_NOTIF | libc::FAN_CLOEXEC | libc::FAN_REPORT_DFID_NAME;
        // `FAN_REPORT_FID` is what makes a self-event (the watched directory itself deleted or
        // moved) identifiable: without it such an event arrives with no record naming the inode.
        flags |= libc::FAN_REPORT_FID;
        if self.target_fid {
            flags |= libc::FAN_REPORT_TARGET_FID;
        }
        flags
    }

    /// The event mask every mark carries.
    ///
    /// `FAN_CLOSE_WRITE` rather than `FAN_MODIFY`, for the same reason `watch.rs` picks
    /// `IN_CLOSE_WRITE`: one event when a writer is done instead of one per write.
    #[must_use]
    pub fn mask(self) -> u64 {
        let mut mask = libc::FAN_CREATE
            | libc::FAN_DELETE
            | libc::FAN_DELETE_SELF
            | libc::FAN_MOVE_SELF
            | libc::FAN_CLOSE_WRITE
            | libc::FAN_ATTRIB
            | libc::FAN_ONDIR;
        if self.scope == MarkScope::Inode {
            mask |= libc::FAN_EVENT_ON_CHILD;
        }
        if self.atomic_rename {
            mask |= libc::FAN_RENAME;
        } else {
            mask |= libc::FAN_MOVED_FROM | libc::FAN_MOVED_TO;
        }
        mask
    }
}

/// An open fanotify group.
#[derive(Debug)]
pub struct Group {
    fd: OwnedFd,
    config: Config,
}

impl Group {
    /// Open a group. `EPERM` here means the kernel refused the *class or report flags*, not the
    /// marks: unprivileged `FAN_CLASS_NOTIF` with fid reporting has been allowed since Linux 5.13,
    /// and everything else still needs `CAP_SYS_ADMIN`.
    pub fn open(config: Config) -> io::Result<Self> {
        // `O_RDONLY | O_LARGEFILE` is the conventional event-fd flag set; in fid mode no event fd
        // is ever produced, but the argument is still validated.
        let fd = sys::init(
            config.init_flags(),
            (libc::O_RDONLY | libc::O_LARGEFILE) as libc::c_uint,
        )?;
        Ok(Self { fd, config })
    }

    #[must_use]
    pub fn config(&self) -> Config {
        self.config
    }

    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    pub fn try_clone_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }

    /// Place the group's mark on `path`.
    pub fn add(&self, path: &Path) -> io::Result<()> {
        let mut flags = libc::FAN_MARK_ADD | self.config.scope.flag();
        if self.config.scope == MarkScope::Inode {
            flags |= libc::FAN_MARK_ONLYDIR;
        }
        sys::mark(self.fd.as_fd(), flags, self.config.mask(), path)
    }

    /// Remove the group's mark from `path`. A mark the kernel already dropped (the directory is
    /// gone) fails with `ENOENT`, which is the expected path rather than an error.
    pub fn remove(&self, path: &Path) -> io::Result<()> {
        let mut flags = libc::FAN_MARK_REMOVE | self.config.scope.flag();
        if self.config.scope == MarkScope::Inode {
            flags |= libc::FAN_MARK_ONLYDIR;
        }
        sys::mark(self.fd.as_fd(), flags, self.config.mask(), path)
    }
}

/// What the kernel will let a fanotify group have, read from `/proc/sys/fs/fanotify/`.
///
/// Reported through `templar_file_index::watch::WatchLimits` so `templar doctor` can print one shape for
/// either backend: `max_user_watches` carries `max_user_marks`, and `max_user_instances` carries
/// `max_user_groups`. The names are inotify's; the numbers are fanotify's.
#[must_use]
pub fn limits() -> templar_file_index::watch::WatchLimits {
    fn read(name: &str) -> Option<u64> {
        std::fs::read_to_string(format!("/proc/sys/fs/fanotify/{name}"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    templar_file_index::watch::WatchLimits {
        max_user_watches: read("max_user_marks"),
        max_queued_events: read("max_queued_events"),
        max_user_instances: read("max_user_groups"),
    }
}
