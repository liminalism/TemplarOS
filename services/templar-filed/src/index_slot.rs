//! The index cache as the file service holds it: open, or unavailable with a reason.
//!
//! An index cache that cannot be opened — another owner, a read-only file, a full disk — is not a
//! reason to stop serving browsing, operations, trash and previews, and it is never a reason to
//! delete the cache (`templar_file_index::Index::open`). The slot keeps the classified reason so
//! `Index1.Roots` can report it, answers every index call with `Error.Unavailable`, and retries
//! the open without a timer: on an index call at least [`RETRY_AFTER`] after the last attempt,
//! and explicitly on `Index1.Reconcile`. Cache open/recovery runs on the bounded maintenance lane,
//! including lazy retries, so `Roots` never scans a cache on the async executor. A successful retry runs the revival hook main installs
//! (watcher, change signals, configured roots) exactly as a healthy start does.

use parking_lot::{Mutex, RwLock};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use templar_file_index::{
    Availability, Index, IndexConfig, IndexError, StorageBytes, error::OpenFailure, model::now_ms,
};

/// The shortest gap between two lazy reopen attempts. `Reconcile` is not rate limited: it is an
/// explicit request.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

type Revival = Box<dyn Fn(&Index) + Send + Sync>;

enum State {
    Open(Index),
    Unavailable {
        reason: OpenFailure,
        since_ms: i64,
        last_attempt: Instant,
    },
}

pub struct IndexSlot {
    path: PathBuf,
    config: IndexConfig,
    state: RwLock<State>,
    /// Serialises reopen attempts so two callers never race to open the same cache.
    reopening: Mutex<()>,
    revival: OnceLock<Revival>,
    maintenance: RwLock<Option<Weak<dyn templar_file_index::MaintenanceExecutor>>>,
    retry_pending: AtomicBool,
}

impl std::fmt::Debug for IndexSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IndexSlot")
            .field("path", &self.path)
            .field("available", &self.availability().is_available())
            .finish_non_exhaustive()
    }
}

impl IndexSlot {
    /// Opens the cache. Only an `Unavailable` failure becomes an unavailable slot; anything else
    /// the core cannot classify is returned, because it is a defect rather than an environment.
    pub fn open(path: &Path, config: IndexConfig) -> Result<Arc<Self>, IndexError> {
        let state = match Index::open(path, config.clone()) {
            Ok(index) => State::Open(index),
            Err(IndexError::Unavailable { reason }) => {
                tracing::warn!(%reason, path = %path.display(),
                    "the file index is unavailable; other file services keep running");
                State::Unavailable {
                    reason,
                    since_ms: now_ms(),
                    last_attempt: Instant::now(),
                }
            }
            Err(other) => return Err(other),
        };
        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            config,
            state: RwLock::new(state),
            reopening: Mutex::new(()),
            revival: OnceLock::new(),
            maintenance: RwLock::new(None),
            retry_pending: AtomicBool::new(false),
        }))
    }

    /// A slot over an index someone else opened (tests and private cores).
    #[must_use]
    pub fn opened(index: Index) -> Arc<Self> {
        Arc::new(Self {
            path: index.path().to_path_buf(),
            config: index.config().clone(),
            state: RwLock::new(State::Open(index)),
            reopening: Mutex::new(()),
            revival: OnceLock::new(),
            maintenance: RwLock::new(None),
            retry_pending: AtomicBool::new(false),
        })
    }

    /// What runs once the cache becomes available after starting unavailable. Installed once.
    pub fn on_revival(&self, revive: impl Fn(&Index) + Send + Sync + 'static) {
        if self.revival.set(Box::new(revive)).is_err() {
            tracing::warn!("the index revival hook was installed twice; keeping the first");
        }
    }

    /// The open index, or `Unavailable`. An unavailable slot retries here when the last attempt
    /// is at least [`RETRY_AFTER`] old.
    pub fn get(self: &Arc<Self>) -> Result<Index, IndexError> {
        let unavailable = match &*self.state.read() {
            State::Open(index) => return Ok(index.clone()),
            State::Unavailable { reason, .. } => IndexError::Unavailable {
                reason: reason.clone(),
            },
        };
        self.schedule_retry();
        Err(unavailable)
    }

    /// Install the service's fixed maintenance lane, including on a cache revived later.
    pub fn set_maintenance_executor(
        &self,
        executor: Arc<dyn templar_file_index::MaintenanceExecutor>,
    ) {
        *self.maintenance.write() = Some(Arc::downgrade(&executor));
        if let Some(index) = self.current() {
            index.set_maintenance_executor(executor);
        }
    }

    fn schedule_retry(self: &Arc<Self>) {
        if !matches!(&*self.state.read(), State::Unavailable { last_attempt, .. }
            if last_attempt.elapsed() >= RETRY_AFTER)
        {
            return;
        }
        let Some(executor) = self.maintenance.read().as_ref().and_then(Weak::upgrade) else {
            return;
        };
        if self.retry_pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let job = Arc::downgrade(self);
        let refused = Arc::downgrade(self);
        executor.submit(
            "index-cache-retry",
            Box::new(move || {
                if let Some(slot) = job.upgrade() {
                    struct Reset<'a>(&'a AtomicBool);
                    impl Drop for Reset<'_> {
                        fn drop(&mut self) {
                            self.0.store(false, Ordering::Release);
                        }
                    }
                    let _pending = Reset(&slot.retry_pending);
                    let _ = slot.retry();
                }
            }),
            Box::new(move || {
                if let Some(slot) = refused.upgrade() {
                    slot.retry_pending.store(false, Ordering::Release);
                }
            }),
        );
    }

    /// The open index if there is one, without retrying: for callers that only want to feed a
    /// live index and lose nothing by skipping an unavailable one.
    #[must_use]
    pub fn current(&self) -> Option<Index> {
        match &*self.state.read() {
            State::Open(index) => Some(index.clone()),
            State::Unavailable { .. } => None,
        }
    }

    /// Reopens now, whatever the last attempt's age: `Index1.Reconcile`.
    pub fn retry(&self) -> Result<Index, IndexError> {
        let _attempt = self.reopening.lock();
        if let State::Open(index) = &*self.state.read() {
            return Ok(index.clone());
        }
        match Index::open(&self.path, self.config.clone()) {
            Ok(index) => {
                if let Some(executor) = self.maintenance.read().as_ref().and_then(Weak::upgrade) {
                    index.set_maintenance_executor(executor);
                }
                *self.state.write() = State::Open(index.clone());
                tracing::info!(path = %self.path.display(), "the file index is available again");
                if let Some(revive) = self.revival.get() {
                    revive(&index);
                }
                Ok(index)
            }
            Err(error) => {
                let reason = error.open_failure();
                let mut state = self.state.write();
                let since_ms = match &*state {
                    State::Unavailable { since_ms, .. } => *since_ms,
                    State::Open(_) => now_ms(),
                };
                *state = State::Unavailable {
                    reason: reason.clone(),
                    since_ms,
                    last_attempt: Instant::now(),
                };
                Err(IndexError::Unavailable { reason })
            }
        }
    }

    #[must_use]
    pub fn availability(&self) -> Availability {
        match &*self.state.read() {
            State::Open(_) => Availability::default(),
            State::Unavailable {
                reason, since_ms, ..
            } => Availability {
                state: "unavailable".to_owned(),
                reason: Some(reason.to_string()),
                since_ms: Some(*since_ms),
            },
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn storage(&self) -> StorageBytes {
        StorageBytes::of(&self.path)
    }
}
