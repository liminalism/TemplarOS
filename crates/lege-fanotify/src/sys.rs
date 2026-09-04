//! The six syscalls, and nothing else.
//!
//! Every `unsafe` block in this crate is in this file. Each one is a single foreign call whose
//! contract is one line of `fanotify(7)` or `open_by_handle_at(2)`, so the reviewable surface is
//! small and fixed: no pointer arithmetic, no lifetimes crossing the boundary, no callbacks.
//! Event *parsing* — the part with real logic and real room for error — is safe code operating on
//! a `&[u8]` in [`crate::event`].

use std::{
    ffi::CString,
    io,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd},
    os::unix::ffi::OsStrExt,
    path::Path,
};

/// `MAX_HANDLE_SZ` from `linux/fcntl.h`. The kernel refuses to emit a handle larger than this, so
/// one buffer of this size never needs the two-call size negotiation `name_to_handle_at(2)`
/// documents.
pub const MAX_HANDLE_SZ: usize = 128;

/// A `struct file_handle` with its variable tail given a fixed, always-sufficient size.
///
/// `libc::file_handle` ends in a zero-length array, which Rust cannot allocate storage for. This
/// is the same layout with the tail spelled out; `handle_bytes` is set to the capacity before the
/// call and rewritten by the kernel to the length actually used.
#[repr(C)]
struct HandleBuf {
    handle_bytes: libc::c_uint,
    handle_type: libc::c_int,
    f_handle: [u8; MAX_HANDLE_SZ],
}

/// Open a fanotify group.
pub fn init(flags: libc::c_uint, event_flags: libc::c_uint) -> io::Result<OwnedFd> {
    // SAFETY: both arguments are plain scalars and the call allocates no memory the caller owns.
    // A successful return is a fresh file descriptor, which `OwnedFd` then owns exclusively.
    let fd = unsafe { libc::fanotify_init(flags, event_flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a descriptor this call just created and nothing else holds it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Add or remove one mark.
pub fn mark(group: BorrowedFd<'_>, flags: libc::c_uint, mask: u64, path: &Path) -> io::Result<()> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    // SAFETY: `group` is a live descriptor for the lifetime of the borrow, `cpath` is a
    // NUL-terminated C string that outlives the call, and `AT_FDCWD` makes the path absolute-or-cwd
    // relative exactly as the syscall expects. The kernel reads the path and returns; it retains
    // no pointer.
    let rc = unsafe {
        libc::fanotify_mark(
            group.as_raw_fd(),
            flags,
            mask,
            libc::AT_FDCWD,
            cpath.as_ptr(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One blocking `read(2)` of the group's event queue.
pub fn read(group: BorrowedFd<'_>, buffer: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the kernel writes at most `buffer.len()` bytes into a buffer this call owns
    // mutably for its whole duration. `group` is live for the borrow.
    let read = unsafe {
        libc::read(
            group.as_raw_fd(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
            buffer.len(),
        )
    };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    // A non-negative `read` is bounded by `buffer.len()`, so the cast cannot lose information.
    Ok(read as usize)
}

/// Is there at least one event waiting? A zero-timeout `poll(2)`.
///
/// The library itself never needs this — `FanotifySource`'s reader thread is meant to block — but
/// the measurement harness has to drain a blocking group to empty without ever waiting, and the
/// crate's `unsafe` exception is scoped to this file rather than spread to an example.
pub fn readable(group: BorrowedFd<'_>) -> io::Result<bool> {
    let mut fds = [libc::pollfd {
        fd: group.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }];
    // SAFETY: one live `pollfd` in storage this call owns, a matching count, and a zero timeout,
    // so the kernel writes `revents` and returns immediately.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ready > 0 && fds[0].revents & libc::POLLIN != 0)
}

/// The `(handle_type, handle)` pair the kernel will report for this path in an event's fid record.
///
/// This is what makes an unprivileged fanotify backend possible at all. Events name a directory by
/// opaque file handle, and turning a handle back into a path needs `open_by_handle_at(2)`, which
/// needs `CAP_DAC_READ_SEARCH`. Going the other way — path to handle — needs nothing, so an
/// indexer that already enumerates every directory can build the map itself.
pub fn handle_of(path: &Path) -> io::Result<(i32, Vec<u8>)> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut buffer = HandleBuf {
        handle_bytes: MAX_HANDLE_SZ as libc::c_uint,
        handle_type: 0,
        f_handle: [0; MAX_HANDLE_SZ],
    };
    let mut mount_id: libc::c_int = 0;
    // SAFETY: `buffer` is a correctly laid out `struct file_handle` with `handle_bytes` set to the
    // capacity of its tail, so the kernel writes at most `MAX_HANDLE_SZ` bytes into storage this
    // call owns. `mount_id` is a live `c_int`. Both pointers outlive the call.
    let rc = unsafe {
        libc::name_to_handle_at(
            libc::AT_FDCWD,
            cpath.as_ptr(),
            std::ptr::from_mut(&mut buffer).cast::<libc::file_handle>(),
            &raw mut mount_id,
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let used = (buffer.handle_bytes as usize).min(MAX_HANDLE_SZ);
    Ok((buffer.handle_type, buffer.f_handle[..used].to_vec()))
}

/// The filesystem id the kernel stamps into every fid record for this path.
///
/// Two directories on two filesystems can share a handle, so the map key has to carry the fsid
/// too. On Btrfs the fsid is per *subvolume*, which is the whole reason `FAN_MARK_FILESYSTEM` and
/// Btrfs disagree — see `docs/measurements/`.
pub fn fsid_of(path: &Path) -> io::Result<[i32; 2]> {
    let cpath = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `statfs` fills the whole structure on success; the pointer is to storage this call
    // owns for the duration.
    let rc = unsafe { libc::statfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statfs` returned 0, so the structure is initialized.
    let stat = unsafe { stat.assume_init() };
    // `libc::fsid_t` keeps its `[c_int; 2]` payload private, so read it by layout. The type is
    // `#[repr(C)]` around exactly that array, which makes the cast a no-op reinterpretation.
    // SAFETY: `fsid_t` is `#[repr(C)] { __val: [c_int; 2] }`, identical in size and alignment to
    // `[i32; 2]`, and the value is a live local.
    let fsid = unsafe { *std::ptr::from_ref(&stat.f_fsid).cast::<[i32; 2]>() };
    Ok(fsid)
}
