//! Background repair: what a watcher cannot know, and how the index stops pretending it does.
//!
//! Three things are permanently outside a watcher's reach: what happened while the daemon was
//! stopped, what happened after the kernel dropped events, and what happened to a removable disk
//! while it was unplugged. Each of them ends here, and each of them follows the same shape —
//! `current` → `stale` (with a reason a person can read) → `building` (reason `reconcile`) →
//! `current`, and only if the repair pass actually completed.
//!
//! Two rules hold throughout, both from C5 §3:
//!
//! 1. The previous committed generation stays visible for the whole repair. A scan writes to
//!    staging; queries keep answering from the last good generation; a crash costs staging rows
//!    and nothing else.
//! 2. Rows are garbage-collected only by a *complete* pass, and only inside its scope. A repair
//!    that could not read a directory must never turn that directory into a deletion.

use std::path::{Path, PathBuf};

use crate::{
    Coverage, EntryKind, EntryMeta, Index, IndexError, NativePath, RootId, RootState, ScanOptions,
    ScanOutcome, ScanScope, ScannedEntry, StaleReason,
    watch::{EventSource, WatchOwner},
};

/// Why a repair is running. It decides the stale reason a user sees and how much of the tree the
/// pass is allowed to take on trust.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Trigger {
    /// The daemon was not running. Nothing about the tree can be proven.
    Downtime,
    /// The kernel dropped an unknown number of events.
    Overflow,
    /// The watch budget or the kernel's per-user limit ran out, so part of the tree was never
    /// watched at all.
    Exhausted { armed: u64, refused: u64 },
    /// A rename arrived without its other half, or a directory appeared whole.
    RenameAmbiguous,
    /// A removable volume came back.
    MountReturned,
    /// The include/exclude policy changed, so the *index* is wrong even though the tree is not.
    PolicyChanged,
    /// Ordinary low-priority upkeep.
    Periodic,
    /// A client asked.
    Requested,
    /// Membership only: re-derive what exists, and leave every entry's size, times and mode as
    /// the index already has them.
    ///
    /// A separate variant rather than a field on [`Self::Requested`] deliberately. `Requested` is
    /// constructed at five sites across three crates; turning it into a struct variant would
    /// rewrite all of them to say nothing new. What this expresses is the pass a filename search
    /// actually needs — the namespace axis repaired, the metadata axis left honestly
    /// [`MetadataFreshness::Unverified`](crate::MetadataFreshness::Unverified).
    NamespaceOnly,
}

impl Trigger {
    #[must_use]
    pub fn stale_reason(&self) -> StaleReason {
        match self {
            Self::Downtime | Self::Periodic | Self::Requested => StaleReason::Downtime,
            // Upkeep, not a fault: the root reads `stale (reconcile)` while the pass runs, which
            // is what it already reads for every other background repair.
            Self::NamespaceOnly => StaleReason::Reconcile,
            Self::Overflow => StaleReason::WatcherOverflow,
            Self::Exhausted { armed, refused } => StaleReason::WatcherExhausted {
                armed: *armed,
                refused: *refused,
            },
            Self::RenameAmbiguous => StaleReason::RenameAmbiguous,
            Self::MountReturned => StaleReason::MountDisappeared,
            Self::PolicyChanged => StaleReason::PolicyChanged,
        }
    }

    /// Whether the pass must re-read every entry's metadata.
    ///
    /// A directory whose mtime and ctime are unchanged has not gained or lost an entry — that is a
    /// filesystem guarantee, and it lets a pass skip the `readdir`. It says nothing about whether
    /// a *file* under it was rewritten in place, because rewriting a file does not touch its
    /// parent. So a pass that skips child metadata is repairing membership only, and says so on
    /// the root's metadata axis: a policy change, where nothing on disk moved and only the index's
    /// idea of what to keep did, and an explicit namespace-only repair.
    #[must_use]
    pub fn verify_child_metadata(&self) -> bool {
        !matches!(self, Self::PolicyChanged | Self::NamespaceOnly)
    }
}

/// What a repair pass did, beyond the index's own outcome.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReconcileReport {
    pub dirs_walked: u64,
    /// Directories whose membership was proven unchanged by their timestamps, so their contents
    /// were taken from the index instead of from a `readdir`.
    pub dirs_proven_unchanged: u64,
    pub entries_observed: u64,
    pub permission_denied: u64,
    /// Directories the pass read but could not arm a watch on. Everything below one of these is
    /// legible and unobserved, which is a different fault from a directory that would not open.
    pub dirs_unwatched: u64,
    pub complete: bool,
}

/// Run one repair pass.
///
/// The walk is the watcher's, not the scanner's: every directory is watched before it is read, so
/// a change made during the pass produces an event rather than disappearing into the gap between
/// "scanned" and "watched".
#[allow(clippy::needless_pass_by_value)] // `scope` also seeds `options`'s clone at each call site
pub fn reconcile<S: EventSource>(
    index: &Index,
    owner: &mut WatchOwner<S>,
    root: RootId,
    scope: ScanScope,
    trigger: &Trigger,
    options: ScanOptions,
) -> Result<(ScanOutcome, ReconcileReport), IndexError> {
    let status = index.root_status(root)?;
    // `Empty` has nothing to protect and cannot transition straight to `Stale` (only `Building`
    // or `Offline`): a first-ever scan just begins. `Offline`/`Building` are already mid-repair.
    if !matches!(
        status.state,
        RootState::Empty | RootState::Offline | RootState::Building
    ) {
        // Say what is wrong before starting to fix it: a client that queries mid-repair sees
        // "stale, because the daemon was not running", not a confident wrong answer.
        index.mark_stale(root, &trigger.stale_reason())?;
    }

    let start = match &scope {
        ScanScope::Root => status.path.to_path(),
        ScanScope::Subtree(path) => path.to_path(),
    };

    let mut report = ReconcileReport {
        complete: true,
        ..ReconcileReport::default()
    };
    let mut session = index.begin_scan(root, options)?;

    let walked = {
        let mut walk = Walk {
            index,
            root,
            trigger,
            report: &mut report,
        };
        walk.run(owner, &mut session, &start)
    };

    if let Err(error) = walked {
        // Nothing is half-committed: the staging rows go, the previous generation stays exactly
        // as it was, and the root keeps a reason.
        session.abort()?;
        index.mark_stale(
            root,
            &StaleReason::Inconsistent {
                detail: error.to_string(),
            },
        )?;
        return Err(error);
    }

    let outcome = session.commit()?;
    Ok((outcome, report))
}

struct Walk<'a> {
    index: &'a Index,
    root: RootId,
    trigger: &'a Trigger,
    report: &'a mut ReconcileReport,
}

impl Walk<'_> {
    /// One place where "we could not see this" becomes both a local fact and a fact the session
    /// carries into its commit. An incomplete pass may not collect, so this is the difference
    /// between "that subtree was unreadable" and "that subtree is gone" (C5 §3).
    fn note_unreadable(&mut self, session: &mut crate::ScanSession, denied: bool) {
        self.report.complete = false;
        if denied {
            self.report.permission_denied += 1;
        }
        session.note_unreadable(denied);
    }

    /// The same thing for a directory that could be read but not watched. Kept apart from
    /// [`Self::note_unreadable`] so the committed root says `watcher-exhausted` — which sends a
    /// reader to the watch budget — rather than `permission-denied`, which sends them to a
    /// directory's mode for a fault that has nothing to do with it.
    fn note_unwatched(&mut self, session: &mut crate::ScanSession, armed: u64) {
        self.report.complete = false;
        self.report.dirs_unwatched += 1;
        session.note_unwatched(armed);
    }

    fn run<S: EventSource>(
        &mut self,
        owner: &mut WatchOwner<S>,
        session: &mut crate::ScanSession,
        start: &Path,
    ) -> Result<(), IndexError> {
        let mut queue: Vec<PathBuf> = vec![start.to_path_buf()];
        while let Some(dir) = queue.pop() {
            let children = self.visit_directory(owner, session, &dir)?;
            queue.extend(children);
        }
        Ok(())
    }

    /// Whether a directory's contents are materialised at all. An `Opaque` build directory is
    /// stored, watched and sized; walking into it would write rows the policy says do not exist.
    fn descends_into(session: &crate::ScanSession, dir: &NativePath) -> bool {
        session
            .coverage_for(dir, EntryKind::Directory)
            .is_some_and(Coverage::materialises_children)
    }

    /// Watch, then read, then observe — and return the subdirectories to descend into.
    fn visit_directory<S: EventSource>(
        &mut self,
        owner: &mut WatchOwner<S>,
        session: &mut crate::ScanSession,
        dir: &Path,
    ) -> Result<Vec<PathBuf>, IndexError> {
        self.report.dirs_walked += 1;
        let native = NativePath::from_path(dir);

        if !owner.arm_directory(self.root, dir) {
            // Unwatched from here down. The rows are still refreshed, but the pass cannot claim
            // to be complete, because nothing will tell us about the next change here: an
            // unwatched subtree that reports `current` is the exact lie C5 §4 forbids.
            let armed = owner.watches_armed();
            self.note_unwatched(session, armed);
        }

        let on_disk = match dir.symlink_metadata() {
            Ok(metadata) => EntryMeta::from_symlink_metadata(dir, &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                self.note_unreadable(session, true);
                return Ok(Vec::new());
            }
            Err(error) => return Err(IndexError::Io(error)),
        };

        if !Self::descends_into(session, &native) {
            // Stored, and the walk ends here. The directory's own row is still observed, so a
            // consumer knows it exists; its contents wait for an explicit expansion.
            session.observe(ScannedEntry {
                path: native,
                parent: dir.parent().map(NativePath::from_path),
                meta: on_disk,
            })?;
            self.report.entries_observed += 1;
            return Ok(Vec::new());
        }

        if let Some(children) = self.take_on_trust(session, &native, &on_disk)? {
            self.report.dirs_proven_unchanged += 1;
            return Ok(children);
        }

        let reader = match std::fs::read_dir(dir) {
            Ok(reader) => reader,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                self.note_unreadable(session, true);
                return Ok(Vec::new());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(IndexError::Io(error)),
        };

        session.observe(ScannedEntry {
            path: native.clone(),
            parent: dir.parent().map(NativePath::from_path),
            meta: on_disk,
        })?;
        self.report.entries_observed += 1;

        let mut subdirectories = Vec::new();
        for entry in reader {
            let Ok(entry) = entry else {
                self.note_unreadable(session, false);
                continue;
            };
            let path = entry.path();
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            let meta = EntryMeta::from_symlink_metadata(&path, &metadata);
            let child = NativePath::from_path(&path);
            if session.coverage_for(&child, meta.kind).is_none() {
                continue;
            }
            if meta.kind == EntryKind::Directory {
                subdirectories.push(path);
                // The directory row itself is observed when the walk reaches it, so that its
                // timestamps are the ones its own visit read.
                continue;
            }
            session.observe(ScannedEntry {
                path: child,
                parent: Some(native.clone()),
                meta,
            })?;
            self.report.entries_observed += 1;
        }
        Ok(subdirectories)
    }

    /// The only thing a repair is allowed to skip: a directory whose timestamps say its
    /// membership has not changed.
    ///
    /// Returns the subdirectories to descend into when the shortcut applies, `None` when the
    /// directory must be read. Even when membership is proven, every child file is re-stated
    /// unless the trigger says the filesystem did not change at all — a rewrite in place does not
    /// touch the parent directory, so membership is *not* a proof of metadata.
    fn take_on_trust(
        &mut self,
        session: &mut crate::ScanSession,
        dir: &NativePath,
        on_disk: &EntryMeta,
    ) -> Result<Option<Vec<PathBuf>>, IndexError> {
        let Some(row) = self.index.entry(self.root, dir)? else {
            return Ok(None);
        };
        if row.meta.mtime_ns != on_disk.mtime_ns || row.meta.ctime_ns != on_disk.ctime_ns {
            return Ok(None);
        }

        session.observe(ScannedEntry {
            path: dir.clone(),
            parent: None,
            meta: on_disk.clone(),
        })?;
        self.report.entries_observed += 1;

        let verify = self.trigger.verify_child_metadata();
        let mut subdirectories = Vec::new();
        for child in self.index.children(self.root, dir)? {
            let path = child.path.to_path();
            // Coverage is re-derived from the policy for every child even here, where the stat is
            // skipped: a policy change is the one thing a pass like this exists to apply, and the
            // stored tier is precisely what it changed. `observe` does the deriving, so a child
            // the new policy writes no row for simply stops being observed and is collected.
            if child.meta.kind == EntryKind::Directory {
                subdirectories.push(path);
                continue;
            }
            let meta = if verify {
                match path.symlink_metadata() {
                    Ok(metadata) => EntryMeta::from_symlink_metadata(&path, &metadata),
                    // Membership said it is there and it is not: the proof was wrong, so stop
                    // trusting this directory and read it properly.
                    Err(_) => return Ok(None),
                }
            } else {
                // The row's metadata is being re-committed without being re-read. That is the
                // whole saving of a namespace-only pass, and it is also exactly the fact the
                // index used to have no way to state.
                session.note_unverified_metadata();
                child.meta.clone()
            };
            session.observe(ScannedEntry {
                path: child.path.clone(),
                parent: Some(dir.clone()),
                meta,
            })?;
            self.report.entries_observed += 1;
        }
        Ok(Some(subdirectories))
    }
}

/// Bring a removable root back.
///
/// A volume that returns is not automatically the volume that left: a different disk can appear at
/// the same mount point. When the identity differs, everything the index remembers about that path
/// belongs to another disk and the root is rebuilt from nothing rather than merged.
pub fn root_returned(index: &Index, root: RootId) -> Result<RootState, IndexError> {
    let status = index.root_status(root)?;
    let path = status.path.to_path();
    if !path.exists() {
        return Ok(status.state);
    }
    let now = volume_identity(&path);
    let same = match (status.volume.dev_id, now) {
        (Some(before), Some(after)) => before == after,
        // Nothing to compare with: treat it as the same volume and let the repair pass decide.
        _ => true,
    };
    let status = index.mark_online(root)?;
    if !same {
        index.mark_stale(root, &StaleReason::MountDisappeared)?;
    }
    Ok(status.state)
}

fn volume_identity(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    path.metadata().ok().map(|metadata| metadata.dev())
}
