//! Bounded coalescing: many filesystem events, one committed generation.
//!
//! A busy tree produces events far faster than it produces *meaning*. Unpacking an archive is
//! thousands of creates that a user thinks of as one thing; a copy through `templar-filed` writes a
//! temporary, flushes it and renames it over the destination, which is three events for one new
//! file. Committing a generation per event would make the change log useless and the database hot
//! for no gain, so events are held for a short window, collapsed to their net effect, and applied
//! as one batch — which the index turns into exactly one generation (C5 §3).
//!
//! The window is deliberately small (a quarter of a second of quiet, a second at the outside):
//! the acceptance target is that an ordinary change is searchable within two seconds, and the
//! batch is the only thing between the event and the query.

use std::collections::HashMap;

use crate::{Mutation, MutationBatch, MutationSource, NativePath};

/// Monotonic milliseconds. A trait so tests can drive the window without sleeping.
pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now_ms(&self) -> i64;
}

/// Milliseconds since the process started watching.
#[derive(Debug)]
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        i64::try_from(self.start.elapsed().as_millis()).unwrap_or(i64::MAX)
    }
}

/// A clock the caller advances by hand.
///
/// Public rather than `#[cfg(test)]` because the crate's integration tests are separate binaries:
/// a batching bound that can only be tested by sleeping is a bound that is never tested.
#[derive(Debug, Default)]
pub struct FakeClock {
    ms: std::sync::atomic::AtomicI64,
}

impl FakeClock {
    #[must_use]
    pub fn new(start_ms: i64) -> Self {
        Self {
            ms: std::sync::atomic::AtomicI64::new(start_ms),
        }
    }

    pub fn advance(&self, ms: i64) {
        self.ms.fetch_add(ms, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.ms.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// When a batch closes.
#[derive(Clone, Copy, Debug)]
pub struct CoalesceConfig {
    /// Close the batch once nothing has arrived for this long. The common case: a burst ends and
    /// the change is visible a quarter of a second later.
    pub quiet_ms: i64,
    /// Close the batch this long after its first event however busy the tree is, so a sustained
    /// storm still commits progress instead of starving every reader of freshness.
    pub max_batch_ms: i64,
    /// Close the batch at this many mutations. Matches the index's own `apply` batch ceiling, so
    /// a batch is never split behind our back.
    pub max_len: usize,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            quiet_ms: 250,
            max_batch_ms: 1000,
            max_len: 10_000,
        }
    }
}

/// The temporary a copy writes before it renames it into place (`operations::temporary_for`).
/// The watcher sees these files; the index must never carry them.
const TEMPORARY_PREFIX: &[u8] = b".templaros-";
const TEMPORARY_SUFFIX: &[u8] = b".part";

fn is_temporary(path: &NativePath) -> bool {
    path.file_name().is_some_and(|name| {
        let bytes = name.as_bytes();
        bytes.starts_with(TEMPORARY_PREFIX) && bytes.ends_with(TEMPORARY_SUFFIX)
    })
}

/// Holds events for a window and hands back their net effect.
#[derive(Debug)]
pub struct Coalescer {
    config: CoalesceConfig,
    pending: Vec<Mutation>,
    first_ms: Option<i64>,
    last_ms: Option<i64>,
}

impl Coalescer {
    #[must_use]
    pub fn new(config: CoalesceConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
            first_ms: None,
            last_ms: None,
        }
    }

    pub fn push(&mut self, clock: &dyn Clock, mutation: Mutation) {
        let now = clock.now_ms();
        self.first_ms.get_or_insert(now);
        self.last_ms = Some(now);
        self.pending.push(mutation);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the window has closed.
    #[must_use]
    pub fn ready(&self, clock: &dyn Clock) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        if self.pending.len() >= self.config.max_len {
            return true;
        }
        let now = clock.now_ms();
        let aged = self
            .first_ms
            .is_some_and(|first| now.saturating_sub(first) >= self.config.max_batch_ms);
        let quiet = self
            .last_ms
            .is_some_and(|last| now.saturating_sub(last) >= self.config.quiet_ms);
        aged || quiet
    }

    /// How long the caller may block waiting for the next event before the window must close.
    /// `None` means "nothing pending, block as long as you like".
    #[must_use]
    pub fn wait_ms(&self, clock: &dyn Clock) -> Option<i64> {
        let (first, last) = (self.first_ms?, self.last_ms?);
        let now = clock.now_ms();
        let until_quiet = self.config.quiet_ms - now.saturating_sub(last);
        let until_aged = self.config.max_batch_ms - now.saturating_sub(first);
        Some(until_quiet.min(until_aged).max(0))
    }

    /// Take one root's pending mutations, however far its window still had to run.
    ///
    /// A repair answers the same question the pending mutations do — "what does this tree hold
    /// now" — and it answers it from the filesystem rather than from a window of events. Left in
    /// the coalescer they land *after* the repair, and an `Invalidate` that arrives after the
    /// pass it was queued alongside marks the root stale again with nothing left to repair it.
    /// Settling the root first is what makes the recovery ordered: the invalidation is applied,
    /// then the repair proves the root, and nothing older is waiting to contradict it.
    ///
    /// Other roots keep their window: this is not a flush.
    pub fn take_for_root(
        &mut self,
        root: crate::RootId,
        source: MutationSource,
    ) -> Option<MutationBatch> {
        if self.pending.is_empty() {
            return None;
        }
        let (mine, rest): (Vec<Mutation>, Vec<Mutation>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|mutation| mutation.root() == root);
        self.pending = rest;
        if self.pending.is_empty() {
            self.first_ms = None;
            self.last_ms = None;
        }
        let mutations = collapse(mine);
        (!mutations.is_empty()).then_some(MutationBatch { source, mutations })
    }

    /// Take the window's net effect. `None` when nothing is pending or everything collapsed away.
    pub fn take(&mut self, source: MutationSource) -> Option<MutationBatch> {
        if self.pending.is_empty() {
            return None;
        }
        let drained = std::mem::take(&mut self.pending);
        self.first_ms = None;
        self.last_ms = None;
        let mutations = collapse(drained);
        (!mutations.is_empty()).then_some(MutationBatch { source, mutations })
    }
}

/// Reduce a window to its net effect.
///
/// The rules are deliberately conservative: where collapsing two events could lose a change, the
/// events are kept. The index deduplicates a mutation that says nothing new (C5 §7), so keeping
/// one too many costs a comparison, while dropping one too many costs correctness.
#[must_use]
pub fn collapse(mutations: Vec<Mutation>) -> Vec<Mutation> {
    let mut out: Vec<Mutation> = Vec::with_capacity(mutations.len());
    // Where the surviving mutation for a path currently sits in `out`.
    let mut at: HashMap<Vec<u8>, usize> = HashMap::new();
    let mut invalidations: Vec<Mutation> = Vec::new();

    for mutation in mutations {
        match &mutation {
            // Invalidations are hoisted to the end of the batch: whatever else the window said,
            // the root must come out of it marked stale.
            Mutation::Invalidate { .. } => {
                invalidations.push(mutation);
                continue;
            }
            // The temporary a copy writes is not a file anyone searches for. Its rename into
            // place is the real event, and that arrives as `Move { from: temporary, to: .. }`,
            // rewritten below into the create it actually is.
            Mutation::Upsert { path, .. } | Mutation::Touch { path, .. } if is_temporary(path) => {
                continue;
            }
            Mutation::Remove { path, .. } if is_temporary(path) => continue,
            _ => {}
        }

        let mutation = match mutation {
            Mutation::Move {
                root,
                from,
                to,
                meta,
            } if is_temporary(&from) => Mutation::Upsert {
                root,
                path: to,
                meta,
            },
            other => other,
        };

        match &mutation {
            Mutation::Upsert { path, .. } | Mutation::Touch { path, .. } => {
                let key = path.as_bytes().to_vec();
                if let Some(index) = at.get(&key).copied() {
                    // A later create/metadata event supersedes an earlier one for the same path,
                    // unless what stands there is a removal: create-after-delete is a real
                    // sequence and both halves matter to a client reading the change log.
                    if matches!(out[index], Mutation::Upsert { .. } | Mutation::Touch { .. }) {
                        out[index] = mutation;
                        continue;
                    }
                }
                at.insert(key, out.len());
                out.push(mutation);
            }
            Mutation::Remove { path, .. } => {
                let key = path.as_bytes().to_vec();
                if let Some(index) = at.get(&key).copied()
                    && matches!(out[index], Mutation::Upsert { .. } | Mutation::Touch { .. })
                {
                    // Created and removed inside one window: the net effect is the removal, which
                    // the index skips if the row never existed.
                    out[index] = mutation;
                    continue;
                }
                at.insert(key, out.len());
                out.push(mutation);
            }
            Mutation::Move { from, to, .. } => {
                // A move is a barrier for both of its paths: anything pending for either of them
                // was about a name that no longer means the same thing.
                at.remove(from.as_bytes());
                at.insert(to.as_bytes().to_vec(), out.len());
                out.push(mutation);
            }
            Mutation::Invalidate { .. } => unreachable!("hoisted above"),
        }
    }

    out.extend(invalidations);
    out
}
