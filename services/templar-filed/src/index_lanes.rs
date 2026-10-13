//! Execution lanes: where index work runs, and how much of it may.
//!
//! The index core is blocking. Calling it from an async D-Bus handler stalls the executor, and
//! handing it to `spawn_blocking` bounds nothing: every request gets a thread, started work cannot
//! be aborted, and a burst of typing becomes a burst of full scans. So index work goes through
//! three fixed lanes — **interactive** (searches, pages), **bulk** (snapshots, large change
//! reads) and **maintenance** (array builds, checkpoints) — each a fixed set of named threads
//! behind a bounded queue. Admission happens before any work starts: a full queue, or a caller
//! already at its own limit, is answered `Busy` at once. A search tagged with a `client_tag`
//! cancels the caller's earlier search with the same tag, queued or running, through its
//! [`CallContext`] (token plus SQLite interrupt). Idle workers wait on a condition variable, so
//! an idle daemon has no lane wakeups at all.

use parking_lot::{Condvar, Mutex};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use templar_file_index::{CallContext, IndexError};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneKind {
    Interactive,
    Bulk,
    Maintenance,
}

impl LaneKind {
    const ALL: [Self; 3] = [Self::Interactive, Self::Bulk, Self::Maintenance];

    fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Bulk => 1,
            Self::Maintenance => 2,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Bulk => "bulk",
            Self::Maintenance => "maintenance",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneConfig {
    pub workers: usize,
    /// Jobs waiting, not counting the ones running.
    pub queue: usize,
    /// Jobs one caller may have queued or running at once.
    pub per_caller: usize,
}

impl LaneConfig {
    /// The defaults: a little interactive concurrency, one snapshot at a time, one build.
    #[must_use]
    pub fn default_for(kind: LaneKind) -> Self {
        match kind {
            LaneKind::Interactive => Self {
                workers: 2,
                queue: 32,
                per_caller: 4,
            },
            LaneKind::Bulk => Self {
                workers: 1,
                queue: 4,
                per_caller: 1,
            },
            LaneKind::Maintenance => Self {
                workers: 1,
                queue: 16,
                per_caller: usize::MAX,
            },
        }
    }
}

pub use templar_file_index::LaneMetrics;

type Work = Box<dyn FnOnce(&CallContext, u32) + Send>;

/// An error from a facade that reports `anyhow`, as the index error the lanes carry: the original
/// when there is one, otherwise its text.
#[must_use]
pub fn index_error(error: anyhow::Error) -> IndexError {
    match error.downcast::<IndexError>() {
        Ok(error) => error,
        Err(other) => IndexError::Unsupported {
            detail: format!("{other:#}"),
        },
    }
}

struct Job {
    caller: Option<Arc<str>>,
    cx: Arc<CallContext>,
    enqueued: Instant,
    work: Work,
    /// Answers the waiting caller when the job is dropped without running.
    abandon: Box<dyn FnOnce(IndexError) + Send>,
}

#[derive(Default)]
struct LaneState {
    queue: VecDeque<Job>,
    /// Queued plus running jobs per caller, for the per-caller limit.
    per_caller: HashMap<Arc<str>, usize>,
    running: Vec<(Option<Arc<str>>, Arc<CallContext>)>,
    waits_ms: VecDeque<u64>,
    completed: u64,
    cancelled: u64,
    rejected_busy: u64,
    stopping: bool,
}

struct Lane {
    kind: LaneKind,
    config: LaneConfig,
    state: Mutex<LaneState>,
    ready: Condvar,
}

/// Recent waits kept for the percentiles.
const WAIT_WINDOW: usize = 256;

/// Interactive work must relinquish its read view even if its consumer never replaces or
/// cancels the request. SQLite's progress handler and array matching honor this deadline.
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(5);

impl Lane {
    fn work_loop(self: &Arc<Self>) {
        loop {
            let job = {
                let mut state = self.state.lock();
                loop {
                    if state.stopping {
                        return;
                    }
                    // Reserve one interactive worker for another caller. Skip a blocked caller's
                    // jobs so it cannot keep everybody else behind its own queue.
                    let position = state.queue.iter().position(|job| {
                        self.kind != LaneKind::Interactive
                            || job.caller.is_none()
                            || state
                                .running
                                .iter()
                                .filter(|(owner, _)| owner == &job.caller)
                                .count()
                                < self.config.workers.saturating_sub(1).max(1)
                    });
                    if let Some(position) = position {
                        let job = state.queue.remove(position).unwrap();
                        state
                            .running
                            .push((job.caller.clone(), Arc::clone(&job.cx)));
                        break job;
                    }
                    self.ready.wait(&mut state);
                }
            };
            let waited = job.enqueued.elapsed().as_millis() as u64;
            if job.cx.check().is_err() {
                let mut state = self.state.lock();
                state.cancelled += 1;
                state.running.retain(|(_, cx)| !Arc::ptr_eq(cx, &job.cx));
                release(&mut state, job.caller.as_ref());
                drop(state);
                (job.abandon)(IndexError::Cancelled);
                self.ready.notify_all();
                continue;
            }
            {
                let mut state = self.state.lock();
                if state.waits_ms.len() == WAIT_WINDOW {
                    state.waits_ms.pop_front();
                }
                state.waits_ms.push_back(waited);
            }
            let cx = Arc::clone(&job.cx);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (job.work)(&cx, u32::try_from(waited).unwrap_or(u32::MAX));
            }));
            let mut state = self.state.lock();
            state
                .running
                .retain(|(_, running)| !Arc::ptr_eq(running, &cx));
            release(&mut state, job.caller.as_ref());
            if cx.check().is_err() {
                state.cancelled += 1;
            } else {
                state.completed += 1;
            }
            drop(state);
            self.ready.notify_all();
            if outcome.is_err() {
                tracing::error!(
                    lane = self.kind.as_str(),
                    "an index job panicked; its caller sees a dropped reply"
                );
            }
        }
    }

    fn metrics(&self) -> LaneMetrics {
        let state = self.state.lock();
        let mut waits: Vec<u64> = state.waits_ms.iter().copied().collect();
        waits.sort_unstable();
        let at = |fraction: f64| {
            if waits.is_empty() {
                0
            } else {
                waits[((waits.len() - 1) as f64 * fraction).round() as usize]
            }
        };
        LaneMetrics {
            lane: self.kind.as_str().to_owned(),
            workers: self.config.workers,
            queued: state.queue.len(),
            running: state.running.len(),
            wait_p50_ms: at(0.50),
            wait_p95_ms: at(0.95),
            completed: state.completed,
            cancelled: state.cancelled,
            rejected_busy: state.rejected_busy,
        }
    }
}

fn release(state: &mut LaneState, caller: Option<&Arc<str>>) {
    if let Some(caller) = caller
        && let Some(count) = state.per_caller.get_mut(caller)
    {
        *count -= 1;
        if *count == 0 {
            state.per_caller.remove(caller);
        }
    }
}

type TagKey = (Arc<str>, String);
type TaggedCalls = HashMap<TagKey, Arc<CallContext>>;

/// The compat `Search1` facade's fixed tag: one in-flight search per bus caller, so a newer
/// keystroke supersedes the older one. Index1 clients choose their own tags instead.
pub const COMPAT_SEARCH_TAG: &str = "search-compat";

fn request_tag(
    caller: Option<&Arc<str>>,
    tag: Option<String>,
) -> Result<Option<TagKey>, IndexError> {
    match (caller, tag) {
        (Some(caller), Some(tag)) if tag.len() <= 64 => Ok(Some((Arc::clone(caller), tag))),
        (_, Some(_)) => Err(IndexError::Unsupported {
            detail: "a client_tag needs a bus caller and at most 64 bytes".to_owned(),
        }),
        _ => Ok(None),
    }
}

/// The three lanes, their worker threads and the latest-request-wins tags.
pub struct Lanes {
    pools: [Arc<Lane>; 3],
    interactive_timeout: Duration,
    pub snapshots: Arc<crate::index_metrics::SnapshotMeter>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    tags: Mutex<TaggedCalls>,
}

impl std::fmt::Debug for Lanes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Lanes").finish_non_exhaustive()
    }
}

/// What a lane answered, and how long the request waited for a worker.
#[derive(Debug)]
pub struct Ran<T> {
    pub value: T,
    pub queue_ms: u32,
}

impl Lanes {
    /// Starts every lane's workers with the default configuration.
    pub fn start() -> std::io::Result<Arc<Self>> {
        Self::with_configs(LaneKind::ALL.map(LaneConfig::default_for))
    }

    pub fn with_configs(configs: [LaneConfig; 3]) -> std::io::Result<Arc<Self>> {
        let lanes = LaneKind::ALL.map(|kind| {
            Arc::new(Lane {
                kind,
                config: configs[kind.index()],
                state: Mutex::new(LaneState::default()),
                ready: Condvar::new(),
            })
        });
        let this = Arc::new(Self {
            pools: lanes,
            interactive_timeout: INTERACTIVE_TIMEOUT,
            snapshots: Arc::default(),
            workers: Mutex::new(Vec::new()),
            tags: Mutex::new(HashMap::new()),
        });
        for lane in &this.pools {
            for number in 0..lane.config.workers {
                let worker = Arc::clone(lane);
                let handle = std::thread::Builder::new()
                    .name(format!("templar-index-{}-{number}", lane.kind.as_str()))
                    .spawn(move || {
                        if worker.kind == LaneKind::Maintenance {
                            crate::index_background::lower_thread_priority();
                        }
                        worker.work_loop();
                    })?;
                this.workers.lock().push(handle);
            }
        }
        Ok(this)
    }

    /// The total read connections the lanes can use at once: one per worker, plus one for inline
    /// callers (`Roots`, policy writes).
    #[must_use]
    pub fn reader_budget(&self) -> usize {
        self.pools
            .iter()
            .map(|lane| lane.config.workers)
            .sum::<usize>()
            + 1
    }

    fn call_context(&self, kind: LaneKind, caller: Option<Arc<str>>) -> Arc<CallContext> {
        let context = CallContext::for_owner(caller);
        Arc::new(if kind == LaneKind::Interactive {
            context.with_deadline(Instant::now() + self.interactive_timeout)
        } else {
            context
        })
    }

    /// Runs `job` on `kind`'s lane for `caller`. `Busy` if the lane's queue or the caller's own
    /// allowance is full; `Cancelled` if a newer request with the same `(caller, tag)` replaced it
    /// before or while it ran, or an interactive request exceeds its five-second deadline.
    pub async fn run<T: Send + 'static>(
        &self,
        kind: LaneKind,
        caller: Option<Arc<str>>,
        tag: Option<String>,
        job: impl FnOnce(&CallContext) -> Result<T, IndexError> + Send + 'static,
    ) -> Result<Ran<T>, IndexError> {
        self.run_with_cleanup(kind, caller, tag, job, |_| {}).await
    }

    /// Dispose of a result if its waiting future disappears or cancellation wins publication.
    /// Searches use this to close a cursor that was created after its consumer went away.
    pub async fn run_with_cleanup<T: Send + 'static>(
        &self,
        kind: LaneKind,
        caller: Option<Arc<str>>,
        tag: Option<String>,
        job: impl FnOnce(&CallContext) -> Result<T, IndexError> + Send + 'static,
        cleanup: impl FnOnce(T) + Send + 'static,
    ) -> Result<Ran<T>, IndexError> {
        let cx = self.call_context(kind, caller.clone());
        let tag_key = request_tag(caller.as_ref(), tag)?;
        let (sender, receiver) = tokio::sync::oneshot::channel::<Result<Ran<T>, IndexError>>();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let abandon = {
            let sender = Arc::clone(&sender);
            Box::new(move |error: IndexError| {
                if let Some(sender) = sender.lock().take() {
                    let _ = sender.send(Err(error));
                }
            })
        };
        let lane = &self.pools[kind.index()];
        let enqueued = Instant::now();
        {
            let mut state = lane.state.lock();
            if let Some(key) = &tag_key {
                let previous = self.tags.lock().get(key).cloned();
                if let Some(previous) = previous {
                    previous.cancel();
                    if let Some(position) = state
                        .queue
                        .iter()
                        .position(|job| Arc::ptr_eq(&job.cx, &previous))
                    {
                        let old = state.queue.remove(position).unwrap();
                        release(&mut state, old.caller.as_ref());
                        state.cancelled += 1;
                        (old.abandon)(IndexError::Cancelled);
                    }
                }
            }
            if state.queue.len() >= lane.config.queue {
                state.rejected_busy += 1;
                return Err(IndexError::Busy {
                    what: "the index lane queue is full",
                });
            }
            if let Some(caller) = &caller
                && state.per_caller.get(caller).copied().unwrap_or(0) >= lane.config.per_caller
            {
                state.rejected_busy += 1;
                return Err(IndexError::Busy {
                    what: "this caller already has its share of index work",
                });
            }
            if let Some(key) = &tag_key {
                self.tags.lock().insert(key.clone(), Arc::clone(&cx));
            }
            if let Some(caller) = &caller {
                *state.per_caller.entry(Arc::clone(caller)).or_insert(0) += 1;
            }
            state.queue.push_back(Job {
                caller,
                cx: Arc::clone(&cx),
                enqueued,
                work: Box::new(move |cx, queue_ms| {
                    let mut result = job(cx);
                    if cx.check().is_err() {
                        if let Ok(value) = result {
                            cleanup(value);
                        }
                        result = Err(IndexError::Cancelled);
                        if let Some(sender) = sender.lock().take() {
                            let _ = sender.send(result.map(|value| Ran { value, queue_ms }));
                        }
                        return;
                    }
                    if let Some(sender) = sender.lock().take() {
                        if let Err(Ok(ran)) =
                            sender.send(result.map(|value| Ran { value, queue_ms }))
                        {
                            cleanup(ran.value);
                        }
                    } else if let Ok(value) = result {
                        cleanup(value);
                    }
                }),
                abandon,
            });
        }
        let mut guard = RequestGuard {
            lanes: self,
            lane,
            cx,
            tag_key,
            answered: false,
        };
        lane.ready.notify_all();
        let result = receiver.await.unwrap_or_else(|_| {
            Err(IndexError::Unsupported {
                detail: "the index job ended without answering".to_owned(),
            })
        });
        guard.answered = true;
        result
    }

    /// A caller left the bus: its queued work is dropped (answered `Cancelled`) and its running
    /// work is cancelled.
    pub fn cancel_caller(&self, caller: &str) {
        for lane in &self.pools {
            let mut dropped = Vec::new();
            {
                let mut state = lane.state.lock();
                let mut kept = VecDeque::with_capacity(state.queue.len());
                while let Some(job) = state.queue.pop_front() {
                    if job.caller.as_deref() == Some(caller) {
                        dropped.push(job);
                    } else {
                        kept.push_back(job);
                    }
                }
                state.queue = kept;
                for job in &dropped {
                    release(&mut state, job.caller.as_ref());
                    state.cancelled += 1;
                }
                for (owner, cx) in &state.running {
                    if owner.as_deref() == Some(caller) {
                        cx.cancel();
                    }
                }
            }
            for job in dropped {
                job.cx.cancel();
                (job.abandon)(IndexError::Cancelled);
            }
        }
        self.tags
            .lock()
            .retain(|(owner, _), _| owner.as_ref() != caller);
    }

    #[must_use]
    pub fn metrics(&self) -> Vec<LaneMetrics> {
        self.pools.iter().map(|lane| lane.metrics()).collect()
    }

    /// The number of lane worker threads, for the bounded-threads check.
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.lock().len()
    }
}

struct RequestGuard<'a> {
    lanes: &'a Lanes,
    lane: &'a Arc<Lane>,
    cx: Arc<CallContext>,
    tag_key: Option<(Arc<str>, String)>,
    answered: bool,
}
impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if !self.answered {
            self.cx.cancel();
            let mut state = self.lane.state.lock();
            if let Some(position) = state
                .queue
                .iter()
                .position(|job| Arc::ptr_eq(&job.cx, &self.cx))
            {
                let job = state.queue.remove(position).unwrap();
                release(&mut state, job.caller.as_ref());
                state.cancelled += 1;
                (job.abandon)(IndexError::Cancelled);
            }
            self.lane.ready.notify_all();
        }
        if let Some(key) = &self.tag_key {
            let mut tags = self.lanes.tags.lock();
            if tags
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, &self.cx))
            {
                tags.remove(key);
            }
        }
    }
}

impl Drop for Lanes {
    fn drop(&mut self) {
        for lane in &self.pools {
            lane.state.lock().stopping = true;
            lane.ready.notify_all();
        }
        for worker in self.workers.lock().drain(..) {
            let _ = worker.join();
        }
    }
}

/// Array builds and other index maintenance run on the maintenance lane, never on a thread of
/// their own: a churning tree cannot stack builds.
impl templar_file_index::MaintenanceExecutor for Lanes {
    fn submit(
        &self,
        name: &'static str,
        job: Box<dyn FnOnce() + Send>,
        refused: Box<dyn FnOnce() + Send>,
    ) {
        let lane = &self.pools[LaneKind::Maintenance.index()];
        let mut state = lane.state.lock();
        if state.queue.len() >= lane.config.queue {
            state.rejected_busy += 1;
            drop(state);
            tracing::debug!(name, "the maintenance lane is full; the job is refused");
            refused();
            return;
        }
        let refused = Mutex::new(Some(refused));
        state.queue.push_back(Job {
            caller: None,
            cx: Arc::new(CallContext::internal()),
            enqueued: Instant::now(),
            work: Box::new(move |_, _| job()),
            abandon: Box::new(move |_| {
                if let Some(refused) = refused.lock().take() {
                    refused();
                }
            }),
        });
        drop(state);
        lane.ready.notify_one();
    }
}

/// How long a lane test waits before calling a job stuck.
#[cfg(test)]
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn lanes(interactive: LaneConfig) -> Arc<Lanes> {
        Lanes::with_configs([
            interactive,
            LaneConfig::default_for(LaneKind::Bulk),
            LaneConfig::default_for(LaneKind::Maintenance),
        ])
        .unwrap()
    }

    /// A job that holds its worker until released or cancelled.
    fn gate() -> (Arc<std::sync::Barrier>, Arc<AtomicUsize>) {
        (
            Arc::new(std::sync::Barrier::new(2)),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_interactive_deadline_interrupts_sql_and_releases_the_reader_and_worker() {
        #[derive(Debug)]
        struct EndlessCount;
        impl templar_file_index::query::QueryPlanner for EndlessCount {
            fn plan(
                &self,
                _: &templar_file_index::Query,
                _: &str,
                _: bool,
            ) -> templar_file_index::query::PlannedQuery {
                templar_file_index::query::PlannedQuery {
                    candidates_sql: "WITH RECURSIVE endless(id) AS
                        (SELECT 1 UNION ALL SELECT 1 FROM endless) SELECT sum(id) FROM endless"
                        .into(),
                    binds: Vec::new(),
                    strategy: "endless-test-count",
                    expand_directories: false,
                }
            }
            fn name(&self) -> &'static str {
                "endless-test-count"
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let index = templar_file_index::Index::open(
            &dir.path().join("index.sqlite3"),
            templar_file_index::IndexConfig::default(),
        )
        .unwrap();
        let root = index
            .add_root(
                &templar_file_index::NativePath::from_path(&home),
                templar_file_index::RootPolicy::default_home(),
            )
            .unwrap()
            .root;
        index
            .scan_root(root, templar_file_index::ScanOptions::default())
            .unwrap();
        let mut lanes = lanes(LaneConfig {
            workers: 1,
            queue: 4,
            per_caller: 4,
        });
        Arc::get_mut(&mut lanes).unwrap().interactive_timeout = Duration::from_secs(1);
        let queried = index.clone();
        let result = lanes
            .run(
                LaneKind::Interactive,
                Some(Arc::from(":1.deadline")),
                None,
                move |cx| {
                    assert!(
                        cx.deadline.is_some(),
                        "interactive work needs a finite deadline"
                    );
                    queried.search_with(
                        &EndlessCount,
                        &templar_file_index::Query::new("needle"),
                        cx,
                    )
                },
            )
            .await;
        assert!(matches!(result, Err(IndexError::Cancelled)));
        assert_eq!(index.reader_metrics().in_use, 0);
        assert_eq!(index.diagnostics().unwrap().cursor_count, 0);
        assert!(
            lanes
                .run(LaneKind::Interactive, None, None, move |cx| {
                    index.search(&templar_file_index::Query::new("home"), cx)
                })
                .await
                .is_ok(),
            "the same worker and read connection must remain usable"
        );
        assert_eq!(lanes.metrics()[0].cancelled, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_newer_search_with_the_same_tag_cancels_the_older_one() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 8,
            per_caller: 4,
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let older = {
            let (lanes, started) = (Arc::clone(&lanes), Arc::clone(&started));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.1")),
                        Some("box".into()),
                        move |cx| {
                            started.notify_one();
                            let until = Instant::now() + PATIENCE;
                            while Instant::now() < until {
                                cx.check()?;
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Ok("older finished")
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        let newer = lanes
            .run(
                LaneKind::Interactive,
                Some(Arc::from(":1.1")),
                Some("box".into()),
                |_| Ok("newer"),
            )
            .await
            .unwrap();
        assert_eq!(newer.value, "newer");
        assert!(matches!(older.await.unwrap(), Err(IndexError::Cancelled)));
        let metrics = &lanes.metrics()[0];
        assert_eq!(metrics.cancelled, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_compat_search_tag_is_latest_wins_per_caller() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 8,
            per_caller: 4,
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let older = {
            let (lanes, started) = (Arc::clone(&lanes), Arc::clone(&started));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.1")),
                        Some(COMPAT_SEARCH_TAG.into()),
                        move |cx| {
                            started.notify_one();
                            let until = Instant::now() + PATIENCE;
                            while Instant::now() < until {
                                cx.check()?;
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Ok("older finished")
                        },
                    )
                    .await
            })
        };
        started.notified().await;
        // Another caller on the same tag is a different keystroke stream: queued, unaffected.
        let other = {
            let lanes = Arc::clone(&lanes);
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.2")),
                        Some(COMPAT_SEARCH_TAG.into()),
                        |_| Ok("other"),
                    )
                    .await
            })
        };
        let newer = lanes
            .run(
                LaneKind::Interactive,
                Some(Arc::from(":1.1")),
                Some(COMPAT_SEARCH_TAG.into()),
                |_| Ok("newer"),
            )
            .await
            .unwrap();
        assert_eq!(newer.value, "newer");
        assert_eq!(other.await.unwrap().unwrap().value, "other");
        assert!(matches!(older.await.unwrap(), Err(IndexError::Cancelled)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_full_interactive_queue_answers_busy_without_starting_work() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 1,
            per_caller: 8,
        });
        let (barrier, ran) = gate();
        let holder = {
            let (lanes, barrier) = (Arc::clone(&lanes), Arc::clone(&barrier));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.2")),
                        None,
                        move |_| {
                            barrier.wait();
                            Ok(())
                        },
                    )
                    .await
            })
        };
        // Wait until the holder occupies the worker, then fill the one queue slot.
        while lanes.metrics()[0].running == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let queued = {
            let (lanes, ran) = (Arc::clone(&lanes), Arc::clone(&ran));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.3")),
                        None,
                        move |_| {
                            ran.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].queued == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let refused_ran = Arc::clone(&ran);
        let refused = lanes
            .run(
                LaneKind::Interactive,
                Some(Arc::from(":1.4")),
                None,
                move |_| {
                    refused_ran.fetch_add(100, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await;
        assert!(matches!(refused, Err(IndexError::Busy { .. })));
        barrier.wait();
        holder.await.unwrap().unwrap();
        queued.await.unwrap().unwrap();
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "the refused job must never start"
        );
        assert_eq!(lanes.metrics()[0].rejected_busy, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_caller_cannot_occupy_every_interactive_worker() {
        let lanes = lanes(LaneConfig {
            workers: 2,
            queue: 8,
            per_caller: 1,
        });
        let (barrier, _) = gate();
        let greedy = {
            let (lanes, barrier) = (Arc::clone(&lanes), Arc::clone(&barrier));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.5")),
                        None,
                        move |_| {
                            barrier.wait();
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].running == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let second = lanes
            .run(LaneKind::Interactive, Some(Arc::from(":1.5")), None, |_| {
                Ok(())
            })
            .await;
        assert!(
            matches!(second, Err(IndexError::Busy { .. })),
            "the greedy caller got a second worker"
        );
        let other = tokio::time::timeout(
            PATIENCE,
            lanes.run(LaneKind::Interactive, Some(Arc::from(":1.6")), None, |_| {
                Ok("served")
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(other.value, "served");
        barrier.wait();
        greedy.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_departed_caller_has_its_queued_work_dropped() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 8,
            per_caller: 8,
        });
        let (barrier, ran) = gate();
        let holder = {
            let (lanes, barrier) = (Arc::clone(&lanes), Arc::clone(&barrier));
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.7")),
                        None,
                        move |_| {
                            barrier.wait();
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].running == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let queued: Vec<_> = (0..3)
            .map(|_| {
                let (lanes, ran) = (Arc::clone(&lanes), Arc::clone(&ran));
                tokio::spawn(async move {
                    lanes
                        .run(
                            LaneKind::Interactive,
                            Some(Arc::from(":1.8")),
                            None,
                            move |_| {
                                ran.fetch_add(1, Ordering::SeqCst);
                                Ok(())
                            },
                        )
                        .await
                })
            })
            .collect();
        while lanes.metrics()[0].queued < 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        lanes.cancel_caller(":1.8");
        for job in queued {
            assert!(matches!(job.await.unwrap(), Err(IndexError::Cancelled)));
        }
        barrier.wait();
        holder.await.unwrap().unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        assert_eq!(lanes.metrics()[0].queued, 0);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tagged_replacement_reuses_a_full_queue_slot() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 1,
            per_caller: 4,
        });
        let (barrier, ran) = gate();
        let holder = {
            let (lanes, barrier) = (lanes.clone(), barrier.clone());
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.holder")),
                        None,
                        move |_| {
                            barrier.wait();
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].running != 1 {
            tokio::task::yield_now().await;
        }
        let old = {
            let (lanes, ran) = (lanes.clone(), ran.clone());
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.typing")),
                        Some("field".into()),
                        move |_| {
                            ran.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].queued != 1 {
            tokio::task::yield_now().await;
        }
        let new = {
            let lanes = lanes.clone();
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.typing")),
                        Some("field".into()),
                        |_| Ok(()),
                    )
                    .await
            })
        };
        assert!(matches!(
            tokio::time::timeout(PATIENCE, old).await.unwrap().unwrap(),
            Err(IndexError::Cancelled)
        ));
        assert_eq!(lanes.metrics()[0].queued, 1);
        barrier.wait();
        holder.await.unwrap().unwrap();
        new.await.unwrap().unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_future_cancels_and_disposes_its_late_result() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 4,
            per_caller: 4,
        });
        let (barrier, cleaned) = gate();
        let worker = {
            let (lanes, barrier, cleaned) = (lanes.clone(), barrier.clone(), cleaned.clone());
            tokio::spawn(async move {
                lanes
                    .run_with_cleanup(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.late")),
                        Some("field".into()),
                        move |_| {
                            barrier.wait();
                            Ok(7)
                        },
                        move |value| {
                            assert_eq!(value, 7);
                            cleaned.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].running != 1 {
            tokio::task::yield_now().await;
        }
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(lanes.tags.lock().is_empty());
        barrier.wait();
        tokio::time::timeout(PATIENCE, async {
            while cleaned.load(Ordering::SeqCst) != 1 || lanes.metrics()[0].running != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lanes.metrics()[0].cancelled, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queue_ms_excludes_the_time_spent_running() {
        let lanes = lanes(LaneConfig {
            workers: 1,
            queue: 4,
            per_caller: 4,
        });
        let result = lanes
            .run(
                LaneKind::Interactive,
                Some(Arc::from(":1.timing")),
                None,
                |_| {
                    std::thread::sleep(Duration::from_millis(150));
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert!(
            result.queue_ms < 100,
            "execution was counted as queue wait: {}",
            result.queue_ms
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_limits_reserve_an_interactive_worker_for_other_callers() {
        let lanes = lanes(LaneConfig::default_for(LaneKind::Interactive));
        let (barrier, _) = gate();
        let first = {
            let (lanes, barrier) = (lanes.clone(), barrier.clone());
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.greedy")),
                        None,
                        move |_| {
                            barrier.wait();
                            Ok(())
                        },
                    )
                    .await
            })
        };
        while lanes.metrics()[0].running != 1 {
            tokio::task::yield_now().await;
        }
        let second = {
            let lanes = lanes.clone();
            tokio::spawn(async move {
                lanes
                    .run(
                        LaneKind::Interactive,
                        Some(Arc::from(":1.greedy")),
                        None,
                        |_| Ok(()),
                    )
                    .await
            })
        };
        while lanes.metrics()[0].queued != 1 {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            PATIENCE,
            lanes.run(
                LaneKind::Interactive,
                Some(Arc::from(":1.other")),
                None,
                |_| Ok(()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(lanes.metrics()[0].running, 1);
        barrier.wait();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
    }
}
