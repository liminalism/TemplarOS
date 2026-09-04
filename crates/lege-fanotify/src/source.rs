//! `FanotifySource`: fanotify behind the index's existing [`EventSource`] seam.
//!
//! The seam names a directory by [`WatchId`] and an entry by its name inside it, because that is
//! what inotify hands over. fanotify hands over something else — an opaque file handle for the
//! directory, plus a name — so this source keeps the map that turns one into the other. Building
//! it costs nothing extra: `name_to_handle_at(2)` is unprivileged, and an indexer that arms a
//! directory has already stat'd it.
//!
//! Two normalisation decisions are load-bearing, and both are measured in
//! `docs/measurements/fanotify-2026-09-04.md`:
//!
//! * **Renames.** fanotify has no cookie. `FAN_MOVED_FROM` and `FAN_MOVED_TO` arrive with nothing
//!   linking them, so `WatchOwner::pending_moves` — which pairs by cookie — could never pair a
//!   fanotify rename, and every rename would degrade into a delete plus a create of a file with no
//!   history. `FAN_RENAME` (Linux 5.17) instead delivers *one* event carrying both directories and
//!   both names, so this source splits it back into a `MovedFrom`/`MovedTo` pair under a synthetic
//!   cookie of its own. The pairing is therefore exact rather than time-windowed: the kernel did
//!   it, not the watcher.
//! * **Merging.** fanotify coalesces queued events with the same fid and name into one event with
//!   the masks ORed; inotify never does. Observed on this host, unprompted: an `mkdir` and the
//!   `rmdir` that followed it arrived as a single `FAN_CREATE|FAN_DELETE|FAN_ONDIR`. A merged mask
//!   therefore carries no order and no repetition count, and reducing it by priority alone is
//!   wrong — "existed, deleted, recreated, deleted" merges to `CREATE|DELETE` with the entry gone,
//!   and picking `Created` would leave the index holding a file that is not there. So a mask
//!   carrying both an appearance and a disappearance is resolved by asking the filesystem what is
//!   there now; everything else reduces by the same priority order `watch.rs` uses for inotify.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

use lege_file_index::watch::{EventSource, RawEvent, RawKind, SourceWaker, WatchId, WatchLimits};

use crate::{
    event::{DirKey, Event, parse},
    group::{Config, Group, MarkScope, limits},
    sys,
};

/// What travels the reader thread's channel. Mirrors `InotifySource`'s, deliberately: the two
/// backends must have the same shutdown and wake behaviour or the watcher loop's idle guarantees
/// depend on which one is selected.
#[derive(Debug)]
enum SourceMessage {
    Event(Box<Event>),
    Wake,
    ReaderStopped,
}

/// Announces the reader thread's exit however it happens, including by unwind.
struct ReaderExit(Sender<SourceMessage>);

impl Drop for ReaderExit {
    fn drop(&mut self) {
        let _ = self.0.send(SourceMessage::ReaderStopped);
    }
}

/// A fanotify-backed [`EventSource`].
#[derive(Debug)]
pub struct FanotifySource {
    group: Group,
    events: Receiver<SourceMessage>,
    /// Retained so a [`SourceWaker`] can end a blocking `recv`, and so the channel never
    /// disconnects on its own.
    wake: Sender<SourceMessage>,
    ids: HashMap<DirKey, WatchId>,
    marked: HashMap<WatchId, (DirKey, PathBuf)>,
    next_id: u64,
    /// fanotify supplies no rename cookie, so the source mints its own. Only ever read by
    /// `WatchOwner` within one pump, and both halves are emitted in the same batch.
    next_cookie: u32,
    limits: WatchLimits,
    reader_stopped: bool,
}

impl FanotifySource {
    /// Open a group and start draining it.
    pub fn open(config: Config) -> io::Result<Self> {
        let group = Group::open(config)?;
        let reader_fd = group.try_clone_fd()?;
        let (sender, events) = std::sync::mpsc::channel();
        let wake = sender.clone();
        std::thread::Builder::new()
            .name("lege-index-fanotify".to_owned())
            .spawn(move || {
                let _exit = ReaderExit(sender.clone());
                let fd = reader_fd;
                // The kernel refuses a read smaller than one event, and an event with a long name
                // is a few hundred bytes; 64 KiB drains a burst in one call.
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    let read = match sys::read(fd.as_fd_ref(), &mut buffer) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    let Ok(batch) = parse(&buffer[..read]) else {
                        break;
                    };
                    for event in batch {
                        if sender.send(SourceMessage::Event(Box::new(event))).is_err() {
                            return;
                        }
                    }
                }
            })?;
        Ok(Self {
            group,
            events,
            wake,
            ids: HashMap::new(),
            marked: HashMap::new(),
            next_id: 1,
            next_cookie: 1,
            limits: limits(),
            reader_stopped: false,
        })
    }

    /// Open the unprivileged variant: `FAN_CLASS_NOTIF` with per-directory inode marks.
    pub fn unprivileged() -> io::Result<Self> {
        Self::open(Config::default())
    }

    #[must_use]
    pub fn scope(&self) -> MarkScope {
        self.group.config().scope
    }

    /// Place the group's single mount- or filesystem-wide mark.
    ///
    /// Meaningless in `Inode` scope, where [`EventSource::add_watch`] places every mark. In
    /// `Filesystem` scope this is the *whole* kernel-side cost of watching a filesystem — and the
    /// call that returns `EXDEV` on a Btrfs subvolume whose fsid differs from its superblock root.
    pub fn arm_scope(&self, path: &Path) -> io::Result<()> {
        self.group.add(path)
    }

    /// How many directories the source can attribute an event to.
    #[must_use]
    pub fn known_directories(&self) -> usize {
        self.ids.len()
    }

    fn key_of(path: &Path) -> io::Result<DirKey> {
        let (handle_type, handle) = sys::handle_of(path)?;
        Ok(DirKey {
            fsid: sys::fsid_of(path)?,
            handle_type,
            handle,
        })
    }

    fn absorb(message: SourceMessage, kernel: &mut Vec<Event>) -> bool {
        match message {
            SourceMessage::Event(event) => {
                kernel.push(*event);
                true
            }
            SourceMessage::Wake => true,
            SourceMessage::ReaderStopped => false,
        }
    }

    /// One kernel event becomes zero, one or two `RawEvent`s.
    fn convert(&mut self, event: &Event, out: &mut Vec<RawEvent>) {
        convert(event, &self.ids, &self.marked, &mut self.next_cookie, out);
    }
}

/// The whole normalisation, as a free function over the two maps it reads.
///
/// Split out so it can be tested without opening a kernel group: the rename pairing and the
/// merged-mask resolution are the two places this backend can silently corrupt an index, and a
/// test that needs a live fanotify group is a test that does not run in CI.
fn convert(
    event: &Event,
    ids: &HashMap<DirKey, WatchId>,
    marked: &HashMap<WatchId, (DirKey, PathBuf)>,
    next_cookie: &mut u32,
    out: &mut Vec<RawEvent>,
) {
    if event.is_overflow() {
        out.push(RawEvent {
            watch: WatchId(0),
            name: None,
            kind: RawKind::Overflow,
            cookie: 0,
            is_dir: false,
        });
        return;
    }
    let is_dir = event.mask & libc::FAN_ONDIR != 0;

    if event.mask & libc::FAN_RENAME != 0 {
        let cookie = *next_cookie;
        *next_cookie = next_cookie.wrapping_add(1).max(1);
        let old = event.record(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME);
        let new = event.record(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME);
        for (record, kind) in [(old, RawKind::MovedFrom), (new, RawKind::MovedTo)] {
            let Some(record) = record else { continue };
            let Some(watch) = ids.get(&record.key).copied() else {
                continue;
            };
            out.push(RawEvent {
                watch,
                name: record.name.clone(),
                kind,
                cookie,
                is_dir,
            });
        }
        return;
    }

    // A self event names the inode itself, not a parent and a child.
    if event.mask & (libc::FAN_DELETE_SELF | libc::FAN_MOVE_SELF) != 0 {
        if let Some(record) = event.record(libc::FAN_EVENT_INFO_TYPE_FID)
            && let Some(watch) = ids.get(&record.key).copied()
        {
            out.push(RawEvent {
                watch,
                name: None,
                kind: RawKind::SelfGone,
                cookie: 0,
                is_dir,
            });
        }
        return;
    }

    let Some(record) = event
        .record(libc::FAN_EVENT_INFO_TYPE_DFID_NAME)
        .or_else(|| event.record(libc::FAN_EVENT_INFO_TYPE_DFID))
    else {
        return;
    };
    let Some(watch) = ids.get(&record.key).copied() else {
        return;
    };
    let kind = if merged(event.mask) {
        // Both an appearance and a disappearance in one mask: the kernel folded at least two
        // changes together and kept neither their order nor their number. The only honest
        // reading is the current state of the name.
        let exists =
            marked
                .get(&watch)
                .zip(record.name.as_ref())
                .is_some_and(|((_, dir), name)| {
                    std::fs::symlink_metadata(dir.join(bytes_to_path(name))).is_ok()
                });
        if exists {
            RawKind::Created
        } else {
            RawKind::Deleted
        }
    } else if let Some(kind) = kind_of(event.mask) {
        kind
    } else {
        return;
    };
    out.push(RawEvent {
        watch,
        name: record.name.clone(),
        kind,
        cookie: 0,
        is_dir,
    });
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

/// A name that both appeared and disappeared inside one event.
fn merged(mask: u64) -> bool {
    const APPEARED: u64 = libc::FAN_CREATE | libc::FAN_MOVED_TO;
    const VANISHED: u64 = libc::FAN_DELETE | libc::FAN_MOVED_FROM;
    mask & APPEARED != 0 && mask & VANISHED != 0
}

/// Reduce a (possibly merged) mask to one kind, in the same priority order `watch.rs` applies to
/// inotify's mask bits: a creation outranks a write, because the creation is what makes the
/// watcher arm and enumerate a new directory.
fn kind_of(mask: u64) -> Option<RawKind> {
    if mask & libc::FAN_CREATE != 0 {
        Some(RawKind::Created)
    } else if mask & libc::FAN_DELETE != 0 {
        Some(RawKind::Deleted)
    } else if mask & libc::FAN_MOVED_FROM != 0 {
        Some(RawKind::MovedFrom)
    } else if mask & libc::FAN_MOVED_TO != 0 {
        Some(RawKind::MovedTo)
    } else if mask & libc::FAN_CLOSE_WRITE != 0 {
        Some(RawKind::Written)
    } else if mask & libc::FAN_ATTRIB != 0 {
        Some(RawKind::Attributes)
    } else {
        None
    }
}

impl EventSource for FanotifySource {
    fn backend(&self) -> &'static str {
        match self.group.config().scope {
            MarkScope::Inode => "fanotify-inode",
            MarkScope::Mount => "fanotify-mount",
            MarkScope::Filesystem => "fanotify-filesystem",
        }
    }

    fn limits(&self) -> WatchLimits {
        self.limits
    }

    /// Arm one directory.
    ///
    /// In `Inode` scope this places a mark. In `Mount` or `Filesystem` scope the single mark
    /// already covers everything, so this only records the handle — and that is the finding worth
    /// stating plainly: a filesystem mark removes the *kernel's* per-directory cost, not the
    /// indexer's. Something must still map a file handle back to a path, and doing it without
    /// `CAP_DAC_READ_SEARCH` means keeping the map in userspace, one entry per directory.
    fn add_watch(&mut self, dir: &Path) -> io::Result<WatchId> {
        let key = Self::key_of(dir)?;
        if let Some(id) = self.ids.get(&key) {
            return Ok(*id);
        }
        if self.group.config().scope == MarkScope::Inode {
            self.group.add(dir)?;
        }
        let id = WatchId(self.next_id);
        self.next_id += 1;
        self.ids.insert(key.clone(), id);
        self.marked.insert(id, (key, dir.to_path_buf()));
        Ok(id)
    }

    fn remove_watch(&mut self, watch: WatchId) {
        if let Some((key, path)) = self.marked.remove(&watch) {
            self.ids.remove(&key);
            if self.group.config().scope == MarkScope::Inode {
                let _ = self.group.remove(&path);
            }
        }
    }

    fn waker(&self) -> Option<SourceWaker> {
        let wake = self.wake.clone();
        Some(SourceWaker::new(std::sync::Arc::new(move || {
            let _ = wake.send(SourceMessage::Wake);
        })))
    }

    fn poll(&mut self, timeout: Option<Duration>) -> io::Result<Vec<RawEvent>> {
        if self.reader_stopped {
            return Err(io::Error::other("the fanotify reader thread stopped"));
        }
        let mut kernel = Vec::new();
        let mut alive = match timeout {
            Some(timeout) => match self.events.recv_timeout(timeout) {
                Ok(message) => Self::absorb(message, &mut kernel),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => false,
            },
            None => match self.events.recv() {
                Ok(message) => Self::absorb(message, &mut kernel),
                Err(std::sync::mpsc::RecvError) => false,
            },
        };
        while let Ok(message) = self.events.try_recv() {
            alive &= Self::absorb(message, &mut kernel);
        }
        if !alive {
            self.reader_stopped = true;
            return Err(io::Error::other("the fanotify reader thread stopped"));
        }
        let mut out = Vec::with_capacity(kernel.len());
        for event in &kernel {
            self.convert(event, &mut out);
        }
        Ok(out)
    }
}

/// `BorrowedFd` from an `OwnedFd` without importing `AsFd` at every call site.
trait AsFdRef {
    fn as_fd_ref(&self) -> std::os::fd::BorrowedFd<'_>;
}

impl AsFdRef for std::os::fd::OwnedFd {
    fn as_fd_ref(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{convert, kind_of, merged};
    use crate::event::{DirKey, Event, FidRecord};
    use lege_file_index::watch::{RawEvent, RawKind, WatchId};
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    fn key(byte: u8) -> DirKey {
        DirKey {
            fsid: [1, 2],
            handle_type: 1,
            handle: vec![byte],
        }
    }

    fn record(info_type: u8, byte: u8, name: &str) -> FidRecord {
        FidRecord {
            info_type,
            key: key(byte),
            name: Some(name.as_bytes().to_vec()),
        }
    }

    /// Two known directories, both claiming to live at `dir`.
    fn maps(
        dir: &Path,
    ) -> (
        HashMap<DirKey, WatchId>,
        HashMap<WatchId, (DirKey, PathBuf)>,
    ) {
        let ids = HashMap::from([(key(1), WatchId(11)), (key(2), WatchId(22))]);
        let marked = HashMap::from([
            (WatchId(11), (key(1), dir.to_path_buf())),
            (WatchId(22), (key(2), dir.to_path_buf())),
        ]);
        (ids, marked)
    }

    fn run(event: &Event, dir: &Path) -> Vec<RawEvent> {
        let (ids, marked) = maps(dir);
        let mut cookie = 1;
        let mut out = Vec::new();
        convert(event, &ids, &marked, &mut cookie, &mut out);
        out
    }

    /// The whole reason `FAN_RENAME` is requested instead of `FAN_MOVED_FROM`/`FAN_MOVED_TO`:
    /// `WatchOwner` pairs a rename by cookie, and fanotify has no cookie of its own.
    #[test]
    fn a_rename_becomes_a_cookie_paired_pair() {
        let event = Event {
            mask: libc::FAN_RENAME,
            fd: -1,
            records: vec![
                record(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, 1, "before"),
                record(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME, 2, "after"),
            ],
        };
        let out = run(&event, Path::new("/nonexistent"));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, RawKind::MovedFrom);
        assert_eq!(out[0].watch, WatchId(11));
        assert_eq!(out[0].name.as_deref(), Some(&b"before"[..]));
        assert_eq!(out[1].kind, RawKind::MovedTo);
        assert_eq!(out[1].watch, WatchId(22));
        assert_eq!(out[0].cookie, out[1].cookie);
        assert_ne!(out[0].cookie, 0, "a zero cookie would pair every rename");
    }

    /// Successive renames must not share a cookie, or `pending_moves` pairs the wrong halves.
    #[test]
    fn successive_renames_get_different_cookies() {
        let (ids, marked) = maps(Path::new("/nonexistent"));
        let event = Event {
            mask: libc::FAN_RENAME,
            fd: -1,
            records: vec![
                record(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, 1, "before"),
                record(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME, 2, "after"),
            ],
        };
        let mut cookie = 1;
        let mut out = Vec::new();
        convert(&event, &ids, &marked, &mut cookie, &mut out);
        convert(&event, &ids, &marked, &mut cookie, &mut out);
        assert_ne!(out[0].cookie, out[2].cookie);
    }

    /// A rename whose other half is outside the watched world leaves exactly one half, which is
    /// what `expire_pending_moves` exists to resolve.
    #[test]
    fn a_rename_out_of_the_watched_world_leaves_one_half() {
        let event = Event {
            mask: libc::FAN_RENAME,
            fd: -1,
            records: vec![
                record(libc::FAN_EVENT_INFO_TYPE_OLD_DFID_NAME, 1, "before"),
                record(libc::FAN_EVENT_INFO_TYPE_NEW_DFID_NAME, 9, "elsewhere"),
            ],
        };
        let out = run(&event, Path::new("/nonexistent"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, RawKind::MovedFrom);
    }

    #[test]
    fn an_overflow_is_reported_even_though_it_names_no_directory() {
        let event = Event {
            mask: libc::FAN_Q_OVERFLOW,
            fd: -1,
            records: Vec::new(),
        };
        let out = run(&event, Path::new("/nonexistent"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, RawKind::Overflow);
    }

    #[test]
    fn a_watched_directory_that_deletes_itself_reports_self_gone() {
        let event = Event {
            mask: libc::FAN_DELETE_SELF | libc::FAN_ONDIR,
            fd: -1,
            records: vec![FidRecord {
                info_type: libc::FAN_EVENT_INFO_TYPE_FID,
                key: key(1),
                name: None,
            }],
        };
        let out = run(&event, Path::new("/nonexistent"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, RawKind::SelfGone);
        assert_eq!(out[0].watch, WatchId(11));
    }

    /// fanotify merges queued events by ORing their masks. inotify never does, so the reduction
    /// has to reproduce inotify's priority or a merged create-then-write would arrive as a write
    /// and the new directory would never be armed.
    #[test]
    fn a_merged_mask_reduces_the_way_inotify_would_have_ordered_it() {
        assert_eq!(
            kind_of(libc::FAN_CREATE | libc::FAN_CLOSE_WRITE | libc::FAN_ATTRIB),
            Some(RawKind::Created)
        );
        assert_eq!(
            kind_of(libc::FAN_CLOSE_WRITE | libc::FAN_ATTRIB),
            Some(RawKind::Written)
        );
        assert_eq!(kind_of(libc::FAN_ATTRIB), Some(RawKind::Attributes));
        assert_eq!(kind_of(libc::FAN_OPEN), None);
        assert!(merged(libc::FAN_CREATE | libc::FAN_DELETE));
        assert!(!merged(libc::FAN_CREATE | libc::FAN_CLOSE_WRITE));
    }

    /// The observed hazard: `mkdir` immediately followed by `rmdir` arrives as one
    /// `FAN_CREATE|FAN_DELETE|FAN_ONDIR`. Priority alone would call it a creation and leave the
    /// index holding a directory that is not there, so the name is re-stat'd instead.
    #[test]
    fn a_create_and_delete_folded_into_one_event_is_resolved_by_looking() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let event = Event {
            mask: libc::FAN_CREATE | libc::FAN_DELETE | libc::FAN_ONDIR,
            fd: -1,
            records: vec![record(libc::FAN_EVENT_INFO_TYPE_DFID_NAME, 1, "gone")],
        };
        let out = run(&event, dir.path());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].kind,
            RawKind::Deleted,
            "nothing is there, so the fold must read as a removal"
        );

        std::fs::create_dir(dir.path().join("gone")).expect("create the directory back");
        let out = run(&event, dir.path());
        assert_eq!(
            out[0].kind,
            RawKind::Created,
            "something is there, so the same fold must read as an appearance"
        );
    }
}
