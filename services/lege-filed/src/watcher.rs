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
    collections::VecDeque,
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
    ScanScope, StaleReason, WatchStatus,
    coalesce::{CoalesceConfig, Coalescer, SystemClock},
    reconcile::{self, Trigger},
    watch::{EventSource, InotifySource, RepairRequest, SourceWaker, WatchBudget, WatchOwner},
};

use crate::{
    execute::IndexSink,
    observer::{Command, Observer, ObserverConfig, ObserverState, Transition, WatcherControl},
};

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
    waker: Option<SourceWaker>,
    commands: std::sync::mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Stop the loop and wait for it. Used by tests and by an orderly shutdown; a crash needs no
    /// cleanup, because an aborted generation is invisible by construction.
    ///
    /// The poke is not optional. The loop now blocks with no timeout when it has nothing to do,
    /// so a flag nobody wakes it to read is a flag it reads after the next filesystem event —
    /// which on an idle machine is never. (There is no `SIGTERM` handler in `lege-filed` today,
    /// so in production this runs from `Drop` or not at all.)
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(waker) = &self.waker {
            waker.wake();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// The non-blocking handle a D-Bus method uses to report a consumer.
    #[must_use]
    pub fn control(&self) -> WatcherControl {
        WatcherControl::new(self.commands.clone(), self.waker.clone())
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
pub fn spawn(
    index: Index,
    roots: WatcherRoots,
    delta: Option<DeltaSetup>,
) -> std::io::Result<WatcherHandle> {
    let source = InotifySource::open()?;
    Ok(spawn_with_source(index, roots, source, delta))
}

/// What the watcher needs to reach `org.legeos.Host.IndexCheckpoint1`, carried rather than
/// connected: the connection has to be made *on the watcher's own thread*, because it goes
/// through `Handle::block_on` and doing that on the reactor thread deadlocks.
#[derive(Clone, Debug)]
pub struct DeltaSetup {
    pub handle: tokio::runtime::Handle,
    /// Where the baseline checkpoint id is remembered across restarts.
    pub state_path: std::path::PathBuf,
}

/// The same loop over any event source, so the deterministic tests drive the real code.
pub fn spawn_with_source<S: EventSource + 'static>(
    index: Index,
    roots: WatcherRoots,
    source: S,
    delta: Option<DeltaSetup>,
) -> WatcherHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let waker = source.waker();
    let (commands, inbox) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("lege-index-watcher".to_owned())
        .spawn(move || {
            let mut loop_ = WatchLoop::new(index, source);
            if let Some(setup) = delta
                && let Some(checkpoints) =
                    crate::delta_catchup::HostCheckpoints::connect(&setup.handle)
            {
                loop_.attach_delta(crate::delta_catchup::DeltaCatchUp::new(
                    Arc::new(checkpoints),
                    setup.state_path,
                ));
            }
            loop_.attach_commands(inbox);
            loop_.start(&roots);
            loop_.run(&thread_stop);
        })
        .expect("spawning the watcher thread");
    WatcherHandle {
        stop,
        waker,
        commands,
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
    /// The repairs this loop owes itself, oldest first.
    ///
    /// A queue rather than a stack, and deduplicated on the way in: on this machine the watch
    /// budget runs out on every home pass, so overflow and exhaustion repairs are queued
    /// constantly rather than rarely. Appending each one would let a single root's backlog grow
    /// without bound and starve every other root behind it.
    repairs: VecDeque<(RootId, ScanScope, Trigger)>,
    observer: Observer,
    commands: Option<std::sync::mpsc::Receiver<Command>>,
    /// The last status actually written. `WatchStatus` is `Eq`, so "publish on change" is a
    /// comparison rather than a heartbeat — and a heartbeat is precisely what this lane exists to
    /// remove.
    published: Option<WatchStatus>,
    /// The source is gone. Not a lost event: a lost stream. Latched, because polling a dead
    /// source returns instantly and an unbounded wait that returns instantly is a spin.
    dead: bool,
    /// The btrfs metadata delta, when there is a privileged daemon to ask. `None` on the ext4
    /// development host, in every test, and in a nested session — and `None` means "reconcile",
    /// which is what every root did before this existed.
    delta: Option<crate::delta_catchup::DeltaCatchUp>,
    /// Test-only: make every apply through this loop fail, the way a database error does.
    #[cfg(test)]
    apply_fails: bool,
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
            repairs: VecDeque::new(),
            observer: Observer::new(ObserverConfig::default()),
            commands: None,
            published: None,
            dead: false,
            delta: None,
            #[cfg(test)]
            apply_fails: false,
        }
    }

    /// Give the loop a delta backend to try before it walks a tree.
    pub fn attach_delta(&mut self, delta: crate::delta_catchup::DeltaCatchUp) {
        self.delta = Some(delta);
    }

    /// Where consumer leases arrive from the D-Bus tasks.
    pub fn attach_commands(&mut self, commands: std::sync::mpsc::Receiver<Command>) {
        self.commands = Some(commands);
    }

    /// Whether the loop has stopped for good.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    #[must_use]
    pub fn observer_state(&self) -> ObserverState {
        self.observer.state()
    }

    /// Arm what exists, queue what must be rebuilt, and tell the truth about downtime.
    pub fn start(&mut self, roots: &WatcherRoots) {
        for status in &roots.serve {
            self.track(status);
            // Armed now, so changes from this moment on are seen; everything before this moment
            // is what the repair pass is for.
            self.arm_only(status);
            self.enqueue_repair(status.root, ScanScope::Root, Trigger::Downtime);
        }
        for status in &roots.reconcile {
            self.track(status);
            self.arm_only(status);
            self.enqueue_repair(status.root, ScanScope::Root, Trigger::Requested);
        }
        for status in &roots.scan {
            self.track(status);
            // A root with no generation at all: the same walk, but there is nothing to keep
            // visible while it runs.
            self.enqueue_repair(status.root, ScanScope::Root, Trigger::Requested);
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
        self.owner
            .arm_directory(status.root, &status.path.to_path());
    }

    /// One turn of the loop: wait for events, coalesce, commit, then do a slice of repair work.
    ///
    /// `timeout` of `None` is the whole point of this lane — see [`WatchLoop::next_wake`] for the
    /// three (four, with the lifecycle) reasons the loop is not allowed to use it.
    pub fn step(&mut self, timeout: Option<Duration>) {
        self.drain_commands();

        let pumped = self.owner.pump(timeout, self.clock.as_ref());
        if let Some(detail) = pumped.failed {
            self.source_died(&detail);
            return;
        }
        for mutation in pumped.mutations {
            self.coalescer.push(self.clock.as_ref(), mutation);
        }
        for request in pumped.repairs {
            self.queue_repair(&request);
        }
        for root in pumped.offline {
            let _ = self
                .index
                .mark_offline(root, "the filesystem was unmounted");
        }

        if self.coalescer.ready(self.clock.as_ref())
            && let Some(batch) = self.coalescer.take(MutationSource::Watcher)
        {
            self.apply_batch(batch);
        }

        if let Some((root, scope, trigger)) = self.repairs.pop_front() {
            // The root's own pending events are settled *before* the pass that supersedes them.
            // An overflow queues a repair and an `Invalidate` in the same turn, and the repair
            // runs long before the coalescing window closes: applying that invalidation
            // afterwards marks the root stale again with nothing left queued to prove it.
            if let Some(pending) = self.coalescer.take_for_root(root, MutationSource::Watcher)
                && let Err(error) = self.apply(pending)
            {
                // No repair queued for this: one is about to run, and queueing another here is
                // how a permanent write failure becomes a spin.
                tracing::warn!(root = root.0, %error, "could not settle a root before repairing it");
            }
            self.repair(root, scope, &trigger);
        }
        if self.repairs.is_empty() {
            // Armed as far as the budget allowed, which is all `Active` has ever claimed. What
            // the budget refused is already on each root as `Stale{watcher-exhausted}`.
            self.observer.armed();
        }
        if self.observer.tick(self.clock.now_ms()) == Some(Transition::Disarm) {
            self.go_dormant();
        }
        self.publish_status();
    }

    /// How long the loop may block, or `None` for "as long as it takes".
    ///
    /// One place, so that every future deadline has to declare itself here rather than by
    /// reintroducing a timer. The reasons, all of which are things the loop must do *on its own*
    /// with no event to prompt it:
    ///
    /// 1. a coalescing window that has to close;
    /// 2. an unpaired rename half that has to expire — `expire_pending_moves` only runs at the end
    ///    of a pump, and a lone `MOVED_FROM` produces no mutation, so without this a file moved
    ///    out of the watched world would stay in the index until some unrelated event arrived;
    /// 3. a queued repair, because `step` pops exactly one per turn and the startup repairs are
    ///    queued before the first wait;
    /// 4. the observation lifecycle's own lease and grace deadlines.
    ///
    /// `None` when all four are empty — and only then.
    #[must_use]
    pub fn next_wake(&self) -> Option<Duration> {
        if !self.repairs.is_empty() {
            return Some(Duration::ZERO);
        }
        let now = self.clock.now_ms();
        let coalescing = self
            .coalescer
            .wait_ms(self.clock.as_ref())
            .map(|ms| Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0)));
        [
            coalescing,
            self.owner.next_move_deadline(now),
            self.observer.next_deadline(now),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) && !self.dead {
            // Before the wait is computed, not after: a lease that arrived while the thread was
            // parked queues an arm, and an arm queued after `next_wake` has already answered
            // `None` would sit in the queue until some unrelated filesystem event turned up.
            self.drain_commands();
            let wait = self.next_wake();
            self.step(wait);
        }
    }

    /// The event stream ended. Every root this owner was responsible for is now unproven, and
    /// nothing will ever prove it again from this loop.
    fn source_died(&mut self, detail: &str) {
        self.dead = true;
        tracing::error!(
            detail,
            "the watcher's event source failed; watching has stopped"
        );
        let reason = StaleReason::WatcherFailed {
            detail: detail.to_owned(),
        };
        for root in self.owner.tracked_roots() {
            let _ = self.index.mark_stale(root, &reason);
        }
        self.publish_status();
    }

    fn drain_commands(&mut self) {
        let Some(commands) = self.commands.take() else {
            return;
        };
        let now = self.clock.now_ms();
        let mut arm = false;
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Lease(client) => {
                    arm |= self.observer.lease(&client, now) == Some(Transition::Arm);
                }
                Command::Release(client) => self.observer.release(&client, now),
            }
        }
        self.commands = Some(commands);
        if arm {
            self.queue_arm();
        }
    }

    /// Wake the tree up. Queued rather than run here for the same reason `start` queues: the walk
    /// costs seconds and the loop still has to answer events while it happens.
    fn queue_arm(&mut self) {
        for root in self.owner.tracked_roots() {
            self.enqueue_repair(root, ScanScope::Root, Trigger::Downtime);
        }
    }

    /// Stop looking — and say so. Disarming mid-session opens exactly the gap that daemon
    /// downtime opens, so it gets its own reason rather than borrowing `Downtime`, which would
    /// send a `lege doctor` reader looking for a crash that never happened.
    fn go_dormant(&mut self) {
        self.owner.disarm_all();
        self.repairs.clear();
        for root in self.owner.tracked_roots() {
            let _ = self.index.mark_stale(root, &StaleReason::Unwatched);
        }
        tracing::info!("no consumer wants the index observed; watches disarmed");
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
            // Not watching and not running are the same epistemic hole, so they take the same
            // repair. (The remaining catch-all is a pre-existing wart: an unlisted reason is
            // reported as an ambiguous rename, which it usually is not.)
            StaleReason::Downtime | StaleReason::Unwatched => Trigger::Downtime,
            _ => Trigger::RenameAmbiguous,
        };
        self.enqueue_repair(request.root, request.scope.clone(), trigger);
    }

    /// Apply a coalesced batch, and make sure a batch that could not be applied is still repaired.
    ///
    /// A failed apply used to be a warning and nothing else: the events were consumed, the change
    /// they described was never taken, and the root went on claiming `current` over it forever.
    /// Retrying the batch is the wrong answer — a batch that failed once is the likeliest thing to
    /// fail again, and the loop would spin on it — so the roots it touched are marked stale and a
    /// whole-root pass is queued to find the change from the filesystem instead. `enqueue_repair`
    /// collapses those, so a failure that persists owes one repair per root, not one per batch.
    fn apply_batch(&mut self, batch: MutationBatch) {
        let mut roots: Vec<RootId> = Vec::new();
        for mutation in &batch.mutations {
            let root = mutation.root();
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        let Err(error) = self.apply(batch) else {
            return;
        };
        tracing::warn!(%error, "could not apply a batch of filesystem events; repairing instead");
        let reason = StaleReason::Inconsistent {
            detail: format!("a batch of filesystem events could not be applied: {error}"),
        };
        for root in roots {
            let _ = self.index.mark_stale(root, &reason);
            self.enqueue_repair(root, ScanScope::Root, Trigger::Requested);
        }
    }

    /// `Index::apply`, with a seam a test can break.
    ///
    /// The branch that matters is the one where the write fails and the loop still has to record
    /// what that means, and no real `Index` can be made to fail the first while allowing the
    /// second. Test-only, so the seam does not exist in the shipped daemon.
    fn apply(&self, batch: MutationBatch) -> Result<(), String> {
        #[cfg(test)]
        if self.apply_fails {
            return Err("an injected index failure".to_owned());
        }
        self.index
            .apply(batch)
            .map(|_applied| ())
            .map_err(|error| error.to_string())
    }

    /// Queue one repair, collapsing it against the repairs already waiting.
    ///
    /// Two repairs of the same root where one covers the other are one repair: walking a subtree
    /// twice, or walking a subtree the whole-root pass behind it is about to walk anyway, costs
    /// the tree again and proves nothing new. Whichever trigger asks for more survives the
    /// collapse, so a wide `Requested` pass never quietly downgrades the `Overflow` it absorbed.
    ///
    /// What is *not* collapsed is a different root: the queue stays first-in-first-out across
    /// roots, so a root that produces events continuously cannot push itself in front of one that
    /// has been waiting.
    fn enqueue_repair(&mut self, root: RootId, scope: ScanScope, trigger: Trigger) {
        if let Some(covering) = self
            .repairs
            .iter_mut()
            .find(|(queued_root, queued_scope, _)| {
                *queued_root == root && covers(queued_scope, &scope)
            })
        {
            if repair_weight(&trigger) > repair_weight(&covering.2) {
                covering.2 = trigger;
            }
            return;
        }
        let mut trigger = trigger;
        self.repairs
            .retain(|(queued_root, queued_scope, queued_trigger)| {
                let covered = *queued_root == root && covers(&scope, queued_scope);
                if covered && repair_weight(queued_trigger) > repair_weight(&trigger) {
                    trigger = queued_trigger.clone();
                }
                !covered
            });
        self.repairs.push_back((root, scope, trigger));
    }

    /// Run one repair to completion. It holds no lock a query needs: the previous generation
    /// answers throughout, and only the final commit swaps it.
    pub fn repair(&mut self, root: RootId, scope: ScanScope, trigger: &Trigger) {
        // Downtime is the one trigger a delta answers: it asks "what happened while nobody was
        // looking", which is exactly the question two snapshots answer cheaply. Overflow,
        // ambiguity and a policy change are about what the index *believes*, not about a window
        // of missed time, and none of them is fixed by replaying a difference.
        if *trigger == Trigger::Downtime && scope == ScanScope::Root && self.catch_up(root) {
            return;
        }
        let Ok(policy) = self.index.root_policy(root) else {
            return;
        };
        let mut options = ScanOptions::reconcile(policy);
        options.scope = scope.clone();
        let refused_before = self.owner.status(0).watches_refused;
        match reconcile::reconcile(&self.index, &mut self.owner, root, scope, trigger, options) {
            Ok((outcome, report)) => {
                tracing::info!(
                    root = root.0,
                    generation = %outcome.generation,
                    entries = outcome.entries,
                    complete = outcome.complete,
                    proven_unchanged = report.dirs_proven_unchanged,
                    state = outcome.state.as_str(),
                    "reconciled"
                );
                self.account_for_refused_watches(root, outcome.state, refused_before);
            }
            Err(error) => tracing::warn!(root = root.0, %error, "reconciliation failed"),
        }
    }

    /// Try to answer a downtime repair from a btrfs metadata delta. `true` when the root is
    /// current again and the walk is not needed.
    ///
    /// Everything that can go wrong here is a `false` and a reconcile, which is what this branch
    /// did before the backend existed; the recursive path is never removed, only skipped.
    fn catch_up(&mut self, root: RootId) -> bool {
        let Some(delta) = self.delta.as_ref() else {
            return false;
        };
        let Ok(status) = self.index.root_status(root) else {
            return false;
        };
        let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") else {
            return false;
        };
        let Some(caught_up) = delta.try_catch_up(&self.index, &status, &mounts) else {
            return false;
        };

        // ponytail: a directory that arrived from outside the indexed region is one row and the
        // stream never named its children, so the delta alone would leave the subtree missing
        // while the root claimed to be current. Walking instead is correct, and it is the case
        // the measurements liked least anyway (the build-shaped burst was the one scenario a
        // crawl won). Upgrade path: arm each path and queue a `ScanScope::Subtree` repair, and
        // keep the root `CatchingUp` until they drain.
        if !caught_up.rescan.is_empty() {
            tracing::info!(
                root = root.0,
                directories = caught_up.rescan.len(),
                "the delta moved whole directories in; walking instead"
            );
            return false;
        }

        let mutations = caught_up.batch.mutations.len();
        if self.index.begin_catch_up(root).is_err() {
            return false;
        }
        let applied = match self.index.apply(caught_up.batch.clone()) {
            Ok(applied) => applied,
            Err(error) => {
                // The batch is all-or-nothing, so a failure here leaves the previous generation
                // exactly as it was — and the checkpoint it was taken against is still held,
                // because nothing is retired until `committed`.
                tracing::warn!(root = root.0, %error, "the delta did not apply; reconciling");
                return false;
            }
        };
        delta.committed(&caught_up);

        // `finish_catch_up` reaches `current` only if the last commit covered the whole root. A
        // delta cannot repair an incomplete generation — it describes a difference, not a tree —
        // so a root that was already incomplete stays honestly stale and gets walked next time.
        let Ok(status) = self.index.finish_catch_up(root) else {
            return false;
        };

        tracing::info!(
            root = root.0,
            generation = %applied.generation,
            mutations,
            stream_bytes = caught_up.stream.bytes,
            send_ms = caught_up.stream.duration_ms,
            state = status.state.as_str(),
            "caught up from a btrfs delta instead of walking the tree"
        );
        status.state == lege_file_index::RootState::Current
    }

    /// A pass that ran out of watches did not see the whole tree, whatever it committed.
    ///
    /// `ScanSession::commit` (`generation.rs:292`) derives completeness from the entry budget,
    /// permission denials and cancellation. A refused watch produces neither: `reconcile`'s
    /// `note_unreadable(session, false)` only bumps `unreadable_dirs`, which that test does not
    /// read — so a root whose arm pass hit the budget commits `Current` over subtrees nothing is
    /// watching. On this machine that is not a corner: 189,089 home directories against a 125,037
    /// budget means it is the outcome of *every* home pass.
    ///
    /// The honest report is the point of the whole crate, so the host makes it here rather than
    /// waiting for the core to be corrected. Fixing it at source means folding `unreadable_dirs`
    /// into that completeness test, which changes tombstone collection for every caller and so
    /// belongs with whoever owns `generation.rs`.
    fn account_for_refused_watches(
        &mut self,
        root: RootId,
        state: lege_file_index::RootState,
        refused_before: u64,
    ) {
        if state != lege_file_index::RootState::Current {
            return;
        }
        let refused = self
            .owner
            .status(0)
            .watches_refused
            .saturating_sub(refused_before);
        if refused == 0 {
            return;
        }
        let reason = StaleReason::WatcherExhausted {
            armed: self.owner.watches_armed(),
            refused,
        };
        tracing::warn!(
            root = root.0,
            refused,
            "the watch budget ran out during a pass; the root is not current"
        );
        let _ = self.index.mark_stale(root, &reason);
    }

    /// Write the watcher's self-report — but only when it says something new.
    ///
    /// This used to run every turn, which was harmless while the loop ticked twice a second and
    /// is the difference between "no writes at all" and "a `roots` UPDATE per wakeup" now that it
    /// does not. There is deliberately no heartbeat to replace the lost ticks: a heartbeat is a
    /// wakeup, and the wakeup is the thing being removed.
    fn publish_status(&mut self) {
        let status = self.owner.status(self.coalescer.len() as u64);
        if self.published.as_ref() == Some(&status) {
            return;
        }
        for root in self.index.roots().unwrap_or_default() {
            let _ = self.index.set_watch_status(root.root, &status);
        }
        self.published = Some(status);
    }

    /// Test access to the loop's parts.
    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }
}

/// Whether a repair over `wide` makes one over `narrow` redundant.
///
/// A whole-root pass covers every subtree of that root, and a subtree pass covers everything
/// beneath it — including itself, which is what makes an exact duplicate collapse rather than
/// queue twice. `NativePath::starts_with` tests component boundaries, so `/home/ab` does not
/// swallow a repair of `/home/abc`.
fn covers(wide: &ScanScope, narrow: &ScanScope) -> bool {
    match (wide, narrow) {
        (ScanScope::Root, _) => true,
        (ScanScope::Subtree(_), ScanScope::Root) => false,
        (ScanScope::Subtree(wide), ScanScope::Subtree(narrow)) => narrow.starts_with(wide),
    }
}

/// How little the index can prove, so that a collapse keeps the trigger that asks for the most.
///
/// The order matters in one place only: `repair` answers `Downtime` from a btrfs delta where one
/// exists, and every trigger above it describes something a difference between two snapshots
/// cannot repair.
fn repair_weight(trigger: &Trigger) -> u8 {
    match trigger {
        Trigger::Periodic => 0,
        Trigger::NamespaceOnly => 1,
        Trigger::RenameAmbiguous => 2,
        Trigger::Requested => 3,
        Trigger::MountReturned => 4,
        Trigger::Downtime => 5,
        Trigger::PolicyChanged => 6,
        Trigger::Exhausted { .. } => 7,
        Trigger::Overflow => 8,
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
    use lege_file_index::{EntryMeta, IndexConfig, MutationBatch, RootPolicy, ScanOptions};
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

    /// A fixture root, tracked and armed, with a clock the test owns.
    fn loop_fixture(
        dir: &std::path::Path,
    ) -> (
        WatchLoop<lege_file_index::watch::FakeSource>,
        RootId,
        Arc<lege_file_index::coalesce::FakeClock>,
    ) {
        let tree = dir.join("tree");
        fs::create_dir_all(tree.join("notes")).unwrap();
        let index = Index::open(&dir.join("index.sqlite3"), IndexConfig::default()).unwrap();
        let root = index
            .add_root(&NativePath::from_path(&tree), RootPolicy::default_home())
            .unwrap()
            .root;
        let clock = Arc::new(lege_file_index::coalesce::FakeClock::new(0));
        let mut loop_ = WatchLoop::with_clock(
            index,
            lege_file_index::watch::FakeSource::new(),
            clock.clone(),
        );
        let status = loop_.index().root_status(root).unwrap();
        loop_.start(&WatcherRoots {
            scan: vec![status],
            ..WatcherRoots::default()
        });
        // Drain the startup repair, so the loop is genuinely idle.
        while !loop_.repairs.is_empty() {
            loop_.step(Some(Duration::ZERO));
        }
        (loop_, root, clock)
    }

    /// The correctness core of the zero-idle change: `None` means "nothing at all is owed", and
    /// every reason the loop might have to act on its own without an event has to appear here.
    #[test]
    fn the_wait_is_unbounded_only_when_nothing_is_owed() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, clock) = loop_fixture(dir.path());
        assert_eq!(
            loop_.next_wake(),
            None,
            "a settled loop with nothing pending must block indefinitely"
        );

        // 1. A queued repair. `step` pops one per turn, so a repair nobody wakes for never runs.
        loop_.enqueue_repair(root, ScanScope::Root, Trigger::Requested);
        assert_eq!(loop_.next_wake(), Some(Duration::ZERO));
        loop_.repairs.clear();
        assert_eq!(loop_.next_wake(), None);

        // 2. A coalescing window that has to close.
        loop_.coalescer.push(
            clock.as_ref(),
            Mutation::Remove {
                root,
                path: NativePath::from_path(&dir.path().join("tree/notes/x")),
            },
        );
        assert_eq!(loop_.next_wake(), Some(Duration::from_millis(250)));
        let _ = loop_.coalescer.take(MutationSource::Watcher);
        assert_eq!(loop_.next_wake(), None);

        // 3. An unpaired rename half. It contributes no mutation, so nothing else would notice.
        let tree = dir.path().join("tree");
        let watch = loop_
            .owner
            .source_mut()
            .watch_for(&tree)
            .expect("the root directory was armed");
        loop_.owner.source_mut().push_named(
            watch,
            "vanished.txt",
            lege_file_index::watch::RawKind::MovedFrom,
            false,
        );
        loop_.step(Some(Duration::ZERO));
        assert_eq!(
            loop_.next_wake(),
            Some(Duration::from_millis(250)),
            "an unpaired rename must not be able to strand the loop in a blocking wait"
        );
        clock.advance(250);
        loop_.step(Some(Duration::ZERO));
        assert!(loop_.next_wake().is_some(), "the removal is now coalescing");
        clock.advance(250);
        loop_.step(Some(Duration::ZERO));
        assert_eq!(
            loop_.next_wake(),
            None,
            "and then there is nothing left to do"
        );
    }

    /// The repair queue is a statement of what is owed, not a log of what asked.
    ///
    /// Watch exhaustion is the ordinary outcome of a home pass on this machine, not a corner, so
    /// these requests arrive continuously: appending each one grew the queue without bound and
    /// walked the same subtree over and over for the same answer.
    #[test]
    fn a_queued_repair_absorbs_every_repair_it_already_covers() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, _clock) = loop_fixture(dir.path());
        let notes = NativePath::from_path(&dir.path().join("tree/notes"));
        let deeper = NativePath::from_path(&dir.path().join("tree/notes/inbox"));
        let sibling = NativePath::from_path(&dir.path().join("tree/notes-2"));

        loop_.enqueue_repair(root, ScanScope::Subtree(notes.clone()), Trigger::Requested);
        loop_.enqueue_repair(root, ScanScope::Subtree(notes), Trigger::Requested);
        assert_eq!(loop_.repairs.len(), 1, "an exact duplicate is one repair");

        loop_.enqueue_repair(root, ScanScope::Subtree(deeper), Trigger::Overflow);
        assert_eq!(
            loop_.repairs.len(),
            1,
            "an ancestor pass already walks its descendants"
        );
        assert_eq!(
            loop_.repairs[0].2,
            Trigger::Overflow,
            "but a collapse must keep whichever reason asks for more"
        );

        // Component boundaries, not byte prefixes: `notes-2` is not inside `notes`.
        loop_.enqueue_repair(root, ScanScope::Subtree(sibling), Trigger::Requested);
        assert_eq!(loop_.repairs.len(), 2);

        loop_.enqueue_repair(root, ScanScope::Root, Trigger::Requested);
        assert_eq!(
            loop_.repairs.len(),
            1,
            "a whole-root pass replaces every subtree of that root"
        );
        assert_eq!(loop_.repairs[0].1, ScanScope::Root);
        assert_eq!(
            loop_.repairs[0].2,
            Trigger::Overflow,
            "and inherits what the repairs it swallowed were queued for"
        );
    }

    /// Collapsing repairs must not turn into starving a root.
    ///
    /// The queue was a `Vec` popped from the back, so a root that produces events continuously
    /// buried one that had been waiting since startup: the oldest repair ran last, or never.
    #[test]
    fn a_root_that_has_been_waiting_is_served_before_a_busy_one() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, _clock) = loop_fixture(dir.path());
        let other = RootId(root.0 + 1);

        loop_.enqueue_repair(other, ScanScope::Root, Trigger::Requested);
        for index in 0..8 {
            let subtree = NativePath::from_path(&dir.path().join(format!("tree/busy{index}")));
            loop_.enqueue_repair(root, ScanScope::Subtree(subtree), Trigger::Requested);
        }

        assert_eq!(
            loop_.repairs.len(),
            9,
            "disjoint subtrees are disjoint work; only overlapping ones collapse"
        );
        assert_eq!(
            loop_.repairs.pop_front().map(|queued| queued.0),
            Some(other),
            "the root that has been waiting goes first, not last"
        );
    }

    /// Recovery has to be ordered, or it undoes itself.
    ///
    /// An overflow queues a repair *and* an `Invalidate` in the same turn. The repair runs at
    /// once; the invalidation sits below the coalescing window and lands afterwards, marking the
    /// root stale again — and with nothing queued, because the pass that would have answered it
    /// has already run. A root that ends a successful recovery stale is worse than one that never
    /// recovered: it looks repaired and is not.
    #[test]
    fn an_overflow_repair_settles_its_invalidation_before_it_runs() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, clock) = loop_fixture(dir.path());
        assert_eq!(
            loop_.index().root_status(root).unwrap().state,
            lege_file_index::RootState::Current,
            "the fixture starts from a proven root"
        );

        loop_.owner.source_mut().push_overflow();
        loop_.step(Some(Duration::ZERO));
        assert_eq!(
            loop_.index().root_status(root).unwrap().state,
            lege_file_index::RootState::Current,
            "the repair the overflow queued walks the tree and proves it again"
        );

        // Long enough for the window the invalidation was still sitting in to close.
        clock.advance(1_000);
        loop_.step(Some(Duration::ZERO));

        assert_eq!(
            loop_.index().root_status(root).unwrap().state,
            lege_file_index::RootState::Current,
            "an invalidation the repair already answered must not land behind it"
        );
        assert!(
            loop_.repairs.is_empty(),
            "and nothing may be left stale with no repair queued to prove it"
        );
        assert_eq!(loop_.next_wake(), None);
    }

    /// A batch the index refuses is a change the watcher saw and the index does not hold.
    ///
    /// It used to be a warning and nothing else: the events were consumed, no pass was scheduled
    /// to find them again, and the root went on calling itself `current` over the file it was
    /// missing. The batch is the wrong thing to retry, so the root is marked stale and walked.
    #[test]
    fn a_batch_that_fails_to_apply_is_repaired_rather_than_lost() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, clock) = loop_fixture(dir.path());
        let notes = dir.path().join("tree/notes");
        let created = notes.join("late.txt");
        fs::write(&created, b"seen by the watcher, refused by the index").unwrap();

        // The mark and the queue, before the same turn's repair consumes them.
        loop_.apply_fails = true;
        loop_.apply_batch(MutationBatch::new(
            MutationSource::Watcher,
            vec![Mutation::Remove {
                root,
                path: NativePath::from_path(&created),
            }],
        ));
        assert_eq!(
            loop_.index().root_status(root).unwrap().state,
            lege_file_index::RootState::Stale,
            "an unapplied batch means the index and the tree disagree"
        );
        assert_eq!(
            loop_.repairs.len(),
            1,
            "and the disagreement is owed a pass, not a log line"
        );
        loop_.repairs.clear();

        let watch = loop_
            .owner
            .source_mut()
            .watch_for(&notes)
            .expect("the repair pass armed the subdirectory");
        loop_.owner.source_mut().push_named(
            watch,
            "late.txt",
            lege_file_index::watch::RawKind::Created,
            false,
        );
        loop_.step(Some(Duration::ZERO));
        clock.advance(250);
        loop_.step(Some(Duration::ZERO));

        assert!(
            loop_
                .index()
                .entry(root, &NativePath::from_path(&created))
                .unwrap()
                .is_some(),
            "the repair the failure queued has to find what the batch could not record"
        );
        assert!(
            loop_.repairs.is_empty(),
            "one repair per root, collapsed, and it has run"
        );
        assert!(!loop_.is_dead());
        assert_eq!(
            loop_.next_wake(),
            None,
            "a failure that persists must leave the loop parked, not spinning on a zero wait"
        );
    }

    /// The regression the unbounded wait would otherwise introduce.
    ///
    /// A failing source returns from `poll` instantly, so before this the loop would have spun at
    /// 100% CPU with no backoff — strictly worse than the 500 ms-bounded loop it replaced. The
    /// answer is not a backoff but an exit: a source that has lost its reader can never produce
    /// another event, and every root it was responsible for is now unproven.
    #[test]
    fn a_dead_source_stops_the_loop_and_marks_every_root_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, _clock) = loop_fixture(dir.path());
        loop_.owner.source_mut().fail_with = Some("the reader thread stopped".to_owned());

        loop_.step(Some(Duration::ZERO));

        assert!(
            loop_.is_dead(),
            "a lost stream ends the loop rather than spinning it"
        );
        let status = loop_.index().root_status(root).unwrap();
        assert_eq!(status.state, lege_file_index::RootState::Stale);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("watcher-failed:")),
            "the reason has to name the failure, not a generic staleness: {:?}",
            status.reason
        );
    }

    /// `publish_status` used to write the `roots` table on every turn, which was invisible at two
    /// ticks a second and is a write per wakeup once the ticks are gone. It publishes on change
    /// now — proved by writing something else into the row and watching it survive a turn.
    #[test]
    fn the_watch_status_is_published_on_change_rather_than_on_a_timer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, _clock) = loop_fixture(dir.path());
        loop_.step(Some(Duration::ZERO));

        let sentinel = WatchStatus {
            backend: "written-by-somebody-else".to_owned(),
            ..WatchStatus::default()
        };
        loop_.index().set_watch_status(root, &sentinel).unwrap();

        loop_.step(Some(Duration::ZERO));

        let status = loop_.index().root_status(root).unwrap();
        assert_eq!(
            status.watch.as_ref().map(|watch| watch.backend.as_str()),
            Some("written-by-somebody-else"),
            "an unchanged status must not be rewritten every turn"
        );
    }

    /// The observation lifecycle, driven end to end through the real loop on a fake clock: a
    /// consumer arrives and leaves, the grace window runs out, and the daemon stops looking —
    /// saying so, because a root nobody is watching is exactly as unproven as one the daemon was
    /// not running for.
    #[test]
    fn the_last_consumer_leaving_disarms_the_watches_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, clock) = loop_fixture(dir.path());
        let (commands, inbox) = std::sync::mpsc::channel();
        loop_.attach_commands(inbox);
        assert!(
            loop_.owner.watches_armed() > 0,
            "the startup pass armed the tree"
        );

        commands.send(Command::Lease(":1.42".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        assert_eq!(loop_.observer_state(), ObserverState::Active);

        commands.send(Command::Release(":1.42".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        assert_eq!(loop_.observer_state(), ObserverState::Grace);
        assert!(
            loop_.owner.watches_armed() > 0,
            "grace exists so a window that reopens costs no second walk"
        );
        assert_eq!(
            loop_.next_wake(),
            Some(Duration::from_millis(60_000)),
            "the grace window is a deadline the loop owns"
        );

        clock.advance(60_000);
        loop_.step(Some(Duration::ZERO));
        assert_eq!(loop_.observer_state(), ObserverState::Dormant);
        assert_eq!(loop_.owner.watches_armed(), 0);
        assert_eq!(
            loop_.next_wake(),
            None,
            "a dormant watcher is the state that costs nothing"
        );
        let status = loop_.index().root_status(root).unwrap();
        assert_eq!(status.state, lege_file_index::RootState::Stale);
        assert_eq!(
            status.reason.as_deref(),
            Some("unwatched"),
            "not `downtime`: the daemon was up and chose not to look"
        );

        // And a consumer coming back queues the walk rather than running it on the caller.
        commands.send(Command::Lease(":1.7".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        assert!(loop_.owner.watches_armed() > 0, "the tree is armed again");
        assert_eq!(loop_.observer_state(), ObserverState::Active);
    }

    /// The ordering that makes a lease usable: work a command queues has to be visible to the
    /// decision about how long to wait, or the loop parks with an arm still in the queue.
    #[test]
    fn a_lease_is_drained_before_the_wait_is_chosen() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, _root, clock) = loop_fixture(dir.path());
        let (commands, inbox) = std::sync::mpsc::channel();
        loop_.attach_commands(inbox);
        commands.send(Command::Lease(":1.1".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        commands.send(Command::Release(":1.1".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        clock.advance(60_000);
        loop_.step(Some(Duration::ZERO));
        assert_eq!(loop_.observer_state(), ObserverState::Dormant);
        assert_eq!(loop_.next_wake(), None, "parked with nothing owed");

        commands.send(Command::Lease(":1.2".to_owned())).unwrap();
        loop_.drain_commands();
        assert_eq!(
            loop_.next_wake(),
            Some(Duration::ZERO),
            "the arm the lease queued must be visible to the wait, not after it"
        );
    }

    /// Direct mutation is the one thing that must not depend on observation: `OperationSink`
    /// holds an `Index` and nothing else, so a copy through `lege-filed` is indexed even with
    /// every watch disarmed.
    #[test]
    fn a_dormant_watcher_still_records_the_daemons_own_operations() {
        let dir = tempfile::tempdir().unwrap();
        let (mut loop_, root, clock) = loop_fixture(dir.path());
        let (commands, inbox) = std::sync::mpsc::channel();
        loop_.attach_commands(inbox);
        commands.send(Command::Lease(":1.1".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        commands.send(Command::Release(":1.1".to_owned())).unwrap();
        loop_.step(Some(Duration::ZERO));
        clock.advance(60_000);
        loop_.step(Some(Duration::ZERO));
        assert_eq!(loop_.observer_state(), ObserverState::Dormant);

        let created = dir.path().join("tree/notes/written-while-dormant.txt");
        fs::write(&created, b"still indexed").unwrap();
        let sink = OperationSink::new(loop_.index().clone());
        sink.created(&created);

        assert!(
            loop_
                .index()
                .entry(root, &NativePath::from_path(&created))
                .unwrap()
                .is_some(),
            "an operation the daemon performed itself is known however hard it is looking"
        );
    }

    /// A blocking wait that shutdown cannot interrupt is a daemon that never exits. There is no
    /// `SIGTERM` handler in `lege-filed` today, so this runs from `Drop` — but it has to work.
    #[test]
    fn stop_interrupts_an_unbounded_wait() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.sqlite3"), IndexConfig::default()).unwrap();
        let mut handle = spawn(index, WatcherRoots::default(), None).expect("a watcher");
        // Let the loop reach its blocking wait: with no roots there is nothing at all to do.
        std::thread::sleep(Duration::from_millis(100));

        let (done, stopped) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            handle.stop();
            let _ = done.send(());
        });
        stopped
            .recv_timeout(Duration::from_secs(5))
            .expect("stop() must wake a thread parked in poll(None)");
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
