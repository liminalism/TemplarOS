//! The watcher host: one owner of every watch in the session, and the only writer of freshness.
//!
//! `lege-filed` is already the one process allowed to own the index (ADR-0001, plan of record
//! §3.1), so this is where the kernel's event stream, the daemon's own file operations and the
//! background repair passes meet. Everything here runs on one blocking thread: the index is a
//! blocking, `Send` handle by design, and an event loop that cannot be starved by the D-Bus
//! executor is worth more than the async it does not use.
//!
//! Startup deserves its own note. The daemon cannot prove anything about a tree it was not
//! running for, so every root that was `current` when the daemon stopped is marked
//! `stale (downtime)` at start and queued for repair. It stays completely searchable throughout —
//! that is the point of keeping the previous generation — but it stops claiming to be current
//! until a complete pass says so. No client start ever triggers a synchronous scan.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use lege_file_index::{
    Index, Mutation, MutationBatch, MutationSource, NativePath, RootId, RootStatus, ScanOptions,
    ScanScope, StaleReason,
    coalesce::{Coalescer, CoalesceConfig, SystemClock},
    reconcile::{self, Trigger},
    watch::{EventSource, InotifySource, RepairRequest, WatchBudget, WatchOwner},
};

use crate::execute::IndexSink;

/// Which roots the watcher must arm, scan and repair. Built from the daemon's startup plan.
#[derive(Clone, Debug, Default)]
pub struct WatcherRoots {
    /// Roots with a committed generation: serve them now, arm them, and repair them in the
    /// background because downtime cannot be reasoned away.
    pub serve: Vec<RootStatus>,
    /// Roots that have never been scanned. The watcher owns this walk too, so that every
    /// directory is watched before it is read.
    pub scan: Vec<RootStatus>,
    /// Roots already known to be stale, offline or incomplete.
    pub reconcile: Vec<RootStatus>,
}

/// A running watcher.
#[derive(Debug)]
pub struct WatcherHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Stop the loop and wait for it. Used by tests and by an orderly shutdown; a crash needs no
    /// cleanup, because an aborted generation is invisible by construction.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the watcher.
///
/// Called after the bus name is claimed: the daemon answers queries from the last committed
/// generation immediately, and the watcher catches up behind it.
pub fn spawn(index: Index, roots: WatcherRoots) -> std::io::Result<WatcherHandle> {
    let source = InotifySource::open()?;
    Ok(spawn_with_source(index, roots, source))
}

/// The same loop over any event source, so the deterministic tests drive the real code.
pub fn spawn_with_source<S: EventSource + 'static>(
    index: Index,
    roots: WatcherRoots,
    source: S,
) -> WatcherHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("lege-index-watcher".to_owned())
        .spawn(move || {
            let mut loop_ = WatchLoop::new(index, source);
            loop_.start(&roots);
            loop_.run(&thread_stop);
        })
        .expect("spawning the watcher thread");
    WatcherHandle {
        stop,
        thread: Some(thread),
    }
}

/// The event loop, separated from its thread so a test can step it.
#[derive(Debug)]
pub struct WatchLoop<S: EventSource> {
    index: Index,
    owner: WatchOwner<S>,
    coalescer: Coalescer,
    clock: Arc<dyn lege_file_index::coalesce::Clock>,
    repairs: Vec<(RootId, ScanScope, Trigger)>,
}

impl<S: EventSource> WatchLoop<S> {
    #[must_use]
    pub fn new(index: Index, source: S) -> Self {
        Self::with_clock(index, source, Arc::new(SystemClock::new()))
    }

    #[must_use]
    pub fn with_clock(
        index: Index,
        source: S,
        clock: Arc<dyn lege_file_index::coalesce::Clock>,
    ) -> Self {
        Self {
            index,
            owner: WatchOwner::new(source, WatchBudget::default()),
            coalescer: Coalescer::new(CoalesceConfig::default()),
            clock,
            repairs: Vec::new(),
        }
    }

    /// Arm what exists, queue what must be rebuilt, and tell the truth about downtime.
    pub fn start(&mut self, roots: &WatcherRoots) {
        for status in &roots.serve {
            self.track(status);
            // Armed now, so changes from this moment on are seen; everything before this moment
            // is what the repair pass is for.
            self.arm_only(status);
            self.repairs
                .push((status.root, ScanScope::Root, Trigger::Downtime));
        }
        for status in &roots.reconcile {
            self.track(status);
            self.arm_only(status);
            self.repairs
                .push((status.root, ScanScope::Root, Trigger::Requested));
        }
        for status in &roots.scan {
            self.track(status);
            // A root with no generation at all: the same walk, but there is nothing to keep
            // visible while it runs.
            self.repairs
                .push((status.root, ScanScope::Root, Trigger::Requested));
        }
        self.publish_status();
    }

    fn track(&mut self, status: &RootStatus) {
        let policy = self
            .index
            .root_policy(status.root)
            .unwrap_or_else(|_| lege_file_index::RootPolicy::default_home());
        self.owner
            .track_root(status.root, &status.path.to_path(), policy);
    }

    fn arm_only(&mut self, status: &RootStatus) {
        // Arming the root directory alone is cheap and closes the window on the top of the tree;
        // the repair pass arms the rest as it walks.
        self.owner.arm_directory(status.root, &status.path.to_path());
    }

    /// One turn of the loop: wait for events, coalesce, commit, then do a slice of repair work.
    pub fn step(&mut self, timeout: Duration) {
        let pumped = self.owner.pump(timeout, self.clock.as_ref());
        for mutation in pumped.mutations {
            self.coalescer.push(self.clock.as_ref(), mutation);
        }
        for request in pumped.repairs {
            self.queue_repair(&request);
        }
        for root in pumped.offline {
            let _ = self.index.mark_offline(root, "the filesystem was unmounted");
        }

        if self.coalescer.ready(self.clock.as_ref())
            && let Some(batch) = self.coalescer.take(MutationSource::Watcher)
            && let Err(error) = self.index.apply(batch)
        {
            tracing::warn!(%error, "could not apply a batch of filesystem events");
        }

        if let Some((root, scope, trigger)) = self.repairs.pop() {
            self.repair(root, scope, &trigger);
        }
        self.publish_status();
    }

    fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            let wait = self
                .coalescer
                .wait_ms(self.clock.as_ref())
                .map_or(500, |ms| ms.clamp(1, 500));
            self.step(Duration::from_millis(wait as u64));
        }
    }

    fn queue_repair(&mut self, request: &RepairRequest) {
        if request.mark_root_stale {
            let _ = self.index.mark_stale(request.root, &request.reason);
        }
        let trigger = match &request.reason {
            StaleReason::WatcherOverflow => Trigger::Overflow,
            StaleReason::WatcherExhausted { armed, refused } => Trigger::Exhausted {
                armed: *armed,
                refused: *refused,
            },
            StaleReason::MountDisappeared => Trigger::MountReturned,
            StaleReason::PolicyChanged => Trigger::PolicyChanged,
            _ => Trigger::RenameAmbiguous,
        };
        self.repairs
            .push((request.root, request.scope.clone(), trigger));
    }

    /// Run one repair to completion. It holds no lock a query needs: the previous generation
    /// answers throughout, and only the final commit swaps it.
    pub fn repair(&mut self, root: RootId, scope: ScanScope, trigger: &Trigger) {
        let Ok(policy) = self.index.root_policy(root) else {
            return;
        };
        let mut options = ScanOptions::reconcile(policy);
        options.scope = scope.clone();
        match reconcile::reconcile(&self.index, &mut self.owner, root, scope, trigger, options) {
            Ok((outcome, report)) => tracing::info!(
                root = root.0,
                generation = %outcome.generation,
                entries = outcome.entries,
                complete = outcome.complete,
                proven_unchanged = report.dirs_proven_unchanged,
                state = outcome.state.as_str(),
                "reconciled"
            ),
            Err(error) => tracing::warn!(root = root.0, %error, "reconciliation failed"),
        }
    }

    fn publish_status(&self) {
        let status = self.owner.status(self.coalescer.len() as u64);
        for root in self.index.roots().unwrap_or_default() {
            let _ = self.index.set_watch_status(root.root, &status);
        }
    }

    /// Test access to the loop's parts.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }
}

// ---------------------------------------------------------------------------------------------
// Direct mutation from the daemon's own file operations.
// ---------------------------------------------------------------------------------------------

/// Applies a committed file operation to the index straight away.
///
/// The watcher will describe the same change a moment later; the index recognizes the echo
/// because the row already says exactly what the event says (`EntryMeta::matches`, C5 §7) and
/// skips it without advancing a generation. Doing it in this order — operation first, event
/// second — is what makes a rename appear in search immediately rather than a coalescing window
/// later.
#[derive(Clone, Debug)]
pub struct OperationSink {
    index: Index,
}

impl OperationSink {
    #[must_use]
    pub fn new(index: Index) -> Self {
        Self { index }
    }

    /// The sink `ExecutorConfig` carries.
    #[must_use]
    pub fn shared(index: Index) -> Arc<dyn IndexSink> {
        Arc::new(Self::new(index))
    }

    fn root_of(&self, path: &Path) -> Option<RootId> {
        self.index
            .root_for_path(&NativePath::from_path(path))
            .ok()
            .flatten()
            .map(|status| status.root)
    }

    fn apply(&self, mutation: Mutation) {
        if let Err(error) = self.index.apply(MutationBatch {
            source: MutationSource::Operation,
            mutations: vec![mutation],
        }) {
            // A file operation must never fail because the index did. The watcher's event and the
            // next repair both still describe this change.
            tracing::warn!(%error, "could not record a file operation in the index");
        }
    }

    fn upsert(&self, path: &Path) {
        let Some(root) = self.root_of(path) else {
            return;
        };
        let Ok(metadata) = path.symlink_metadata() else {
            return;
        };
        let meta = lege_file_index::EntryMeta::from_symlink_metadata(path, &metadata);
        self.apply(Mutation::Upsert {
            root,
            path: NativePath::from_path(path),
            meta,
        });
    }
}

/// The temporary a copy writes before renaming it into place. It is never a row.
fn is_temporary(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with(".legeos-") && name.ends_with(".part")
    })
}

impl IndexSink for OperationSink {
    fn created(&self, path: &Path) {
        if is_temporary(path) {
            return;
        }
        self.upsert(path);
    }

    fn removed(&self, path: &Path) {
        if is_temporary(path) {
            return;
        }
        let Some(root) = self.root_of(path) else {
            return;
        };
        self.apply(Mutation::Remove {
            root,
            path: NativePath::from_path(path),
        });
    }

    fn moved(&self, from: &Path, to: &Path) {
        if is_temporary(to) {
            return;
        }
        if is_temporary(from) {
            // A copy exposing its temporary: to the index this is simply a new file.
            self.upsert(to);
            return;
        }
        let (Some(from_root), Some(to_root)) = (self.root_of(from), self.root_of(to)) else {
            // Only one end is indexed, so it is a departure or an arrival, not a move.
            if let Some(root) = self.root_of(from) {
                self.apply(Mutation::Remove {
                    root,
                    path: NativePath::from_path(from),
                });
            }
            if self.root_of(to).is_some() {
                self.upsert(to);
            }
            return;
        };
        if from_root != to_root {
            self.apply(Mutation::Remove {
                root: from_root,
                path: NativePath::from_path(from),
            });
            self.upsert(to);
            return;
        }
        let Ok(metadata) = to.symlink_metadata() else {
            return;
        };
        self.apply(Mutation::Move {
            root: from_root,
            from: NativePath::from_path(from),
            to: NativePath::from_path(to),
            meta: lege_file_index::EntryMeta::from_symlink_metadata(to, &metadata),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execute::{self, Cancel, ExecutorConfig},
        operations::{self, OperationId, OperationKind, Options},
    };
    use lege_file_index::{
        EntryMeta, IndexConfig, MutationBatch, RootPolicy, ScanOptions,
    };
    use std::fs;

    /// A file operation updates the index the moment it commits, and the watcher's later account
    /// of the same change is recognized as an echo rather than applied twice.
    ///
    /// This is what makes a rename appear in search immediately instead of a coalescing window
    /// later, and it costs nothing in correctness because the index compares what it already
    /// holds against what the event claims (`EntryMeta::matches`).
    #[test]
    fn watcher_operation_hook_dedupes_watch_event() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        let destination = tree.join("out");
        fs::create_dir_all(tree.join("in")).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(tree.join("in/report.txt"), b"contents").unwrap();

        let index = Index::open(&dir.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
        let policy = RootPolicy::default_home();
        let root = index
            .add_root(&NativePath::from_path(&tree), policy.clone())
            .unwrap()
            .root;
        index
            .scan_root(root, ScanOptions::reconcile(policy))
            .unwrap();
        let before = index.generation().unwrap().0;

        let source = tree.join("in/report.txt");
        let surveyed = operations::survey(std::slice::from_ref(&source), Some(&destination));
        let mut operation = operations::plan(
            OperationId::new(),
            OperationKind::Copy {
                sources: vec![source],
                destination_dir: destination.clone(),
            },
            &surveyed,
            Options::default(),
        );
        let config = ExecutorConfig {
            data_home: dir.path().join("data"),
            reserve_percent: 0,
            progress_interval: std::time::Duration::from_millis(0),
            index_sink: Some(OperationSink::shared(index.clone())),
        };
        execute::run(&mut operation, &config, &Cancel::new(), |_| {}).unwrap();

        let copied = destination.join("report.txt");
        assert!(
            index
                .entry(root, &NativePath::from_path(&copied))
                .unwrap()
                .is_some(),
            "the copy is in the index before any watcher event arrives"
        );
        let after_operation = index.generation().unwrap().0;
        assert!(after_operation > before);

        // The kernel's account of the same copy, arriving a batch later.
        let metadata = copied.symlink_metadata().unwrap();
        let applied = index
            .apply(MutationBatch::new(
                MutationSource::Watcher,
                vec![Mutation::Upsert {
                    root,
                    path: NativePath::from_path(&copied),
                    meta: EntryMeta::from_symlink_metadata(&copied, &metadata),
                }],
            ))
            .unwrap();

        assert_eq!(applied.skipped, 1, "the echo was recognized");
        assert!(applied.changes.is_empty());
        assert_eq!(
            index.generation().unwrap().0,
            after_operation,
            "an echo must not advance a generation"
        );
    }

    /// The fixture rule, mechanized.
    ///
    /// Two mistakes have to be impossible rather than discouraged: a test that empties the
    /// developer's real trash, and a test that opens a watcher on the developer's real home. The
    /// first is a source-level rule (the live-mount-table variant of `TrashMounts` reaches the
    /// real mount table, and `Empty` deletes what it finds there); the second is a runtime panic
    /// in the watcher itself. Prose in a document did not stop the first one from happening once
    /// already.
    ///
    /// The needle is built by concatenation rather than written as one literal, and this doc
    /// comment never spells it contiguously either: otherwise this very test — its own source
    /// lives after the module's `#[cfg(test)]` marker, same as everything it scans — would flag
    /// itself the moment it started checking for the thing it exists to forbid.
    #[test]
    fn watcher_fixture_guard_holds() {
        let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let forbidden = ["TrashMounts", "::", "System"].concat();

        for file in rust_files(&crate_root.join("src")) {
            let text = fs::read_to_string(&file).unwrap();
            // Test modules live at the end of a file in this codebase, so everything from the
            // first `#[cfg(test)]` onwards is test code. Over-covering is the safe direction.
            if let Some(tests) = text.find("#[cfg(test)]") {
                assert!(
                    !text[tests..].contains(&forbidden),
                    "{} constructs the live-mount-table TrashMounts variant in test code: a test \
                     must never reach the real mount table (use the Fixed variant instead)",
                    file.display()
                );
            }
        }

        let index_tests = crate_root.join("../../crates/lege-file-index/tests");
        if index_tests.is_dir() {
            for file in rust_files(&index_tests) {
                let text = fs::read_to_string(&file).unwrap();
                assert!(
                    !text.contains("InotifySource"),
                    "{} opens a real inotify watcher: index tests script events through \
                     FakeSource instead",
                    file.display()
                );
            }
        }

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let refused = std::panic::catch_unwind(|| {
            lege_file_index::watch::guard::assert_fixture_root(std::path::Path::new("/"));
        });
        std::panic::set_hook(previous);
        assert!(
            refused.is_err(),
            "the watcher must refuse a root outside the temporary directory when a test binary \
             asks for one"
        );
    }

    fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files = Vec::new();
        let Ok(entries) = fs::read_dir(dir) else {
            return files;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(rust_files(&path));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
        files
    }
}
