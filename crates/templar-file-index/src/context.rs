//! Who is asking, and whether they still want the answer.
//!
//! Every query-shaped call (`search`, `fetch_page`, `write_snapshot`, `changes_since`) takes a
//! [`CallContext`]. It carries the caller's unique bus name, so cursors belong to the client that
//! opened them and are released when it leaves; a cancellation token and an optional deadline,
//! checked cooperatively while rows are scanned; and, while a SQLite statement runs on the
//! caller's behalf, that connection's interrupt handle — so a cancel stops a running statement
//! instead of waiting for it. Dropping a future does not stop work already running on a thread;
//! this is what does.

use crate::{IndexError, Result, generation::CancelToken};
use parking_lot::Mutex;
use std::{sync::Arc, time::Instant};

#[derive(Clone, Default)]
pub struct CallContext {
    /// The caller's unique bus name; `None` for in-process work (tests, tools, maintenance).
    pub owner: Option<Arc<str>>,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    interrupt: Arc<Mutex<Option<rusqlite::InterruptHandle>>>,
}

impl std::fmt::Debug for CallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallContext")
            .field("owner", &self.owner)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

impl CallContext {
    /// In-process work with no owner, no deadline and nobody to cancel it.
    #[must_use]
    pub fn internal() -> Self {
        Self::default()
    }

    /// Work on behalf of a bus client.
    #[must_use]
    pub fn for_owner(owner: Option<Arc<str>>) -> Self {
        Self {
            owner,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// A stricter deadline for the same call. Cancellation and the active interrupt registration
    /// remain shared with the lane, so cancelling the original also stops this bounded operation.
    #[must_use]
    pub fn bounded_by(&self, deadline: Instant) -> Self {
        let mut bounded = self.clone();
        bounded.deadline = Some(
            self.deadline
                .map_or(deadline, |original| original.min(deadline)),
        );
        bounded
    }

    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Cancels: sets the token and interrupts the statement running for this call, if any.
    pub fn cancel(&self) {
        self.cancel.cancel();
        if let Some(handle) = self.interrupt.lock().as_ref() {
            handle.interrupt();
        }
    }

    /// `Err(Cancelled)` once the token is set or the deadline has passed.
    pub fn check(&self) -> Result<()> {
        if self.cancel.is_cancelled()
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(IndexError::Cancelled);
        }
        Ok(())
    }

    /// The connection a statement for this call is about to run on. A cancel that arrived first
    /// is honoured immediately rather than lost between the check and the statement.
    pub(crate) fn arm(&self, handle: rusqlite::InterruptHandle) {
        let mut slot = self.interrupt.lock();
        if self.cancel.is_cancelled() {
            handle.interrupt();
        }
        *slot = Some(handle);
    }

    pub(crate) fn disarm(&self) {
        *self.interrupt.lock() = None;
    }

    #[must_use]
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}
