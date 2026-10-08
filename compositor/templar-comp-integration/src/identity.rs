//! Hardened D-Bus identity cache: PID-reuse-safe, bounded, and self-healing when
//! `templar-sessiond` is slow to start or restarts underneath it.
//!
//! The render thread only ever touches [`DbusIdentityCache::resolve_cached`] (a map lookup plus
//! one local `/proc/<pid>/stat` read — no D-Bus). Resolution requests are queued to a tokio task
//! that talks to `org.templaros.AppIdentity1`. Its answer sink must wake the compositor's
//! event loop and apply `WindowService::update_identity` there, never on the resolver task.

use crate::pid;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use templar_client::proxies::AppIdentityProxy;
use templar_types::{IdentityConfidence, InstanceId, WindowClaims};
use templar_window::{CachedIdentity, IdentityCache, WindowId};
use tokio::sync::mpsc;

/// Cache capacity: generous for even a very busy desktop's live window count, small enough that
/// a compositor which never calls [`DbusIdentityCache::forget`] cannot leak unboundedly.
const CACHE_CAPACITY: usize = 512;
/// How long to wait before retrying `org.templaros.Session` after it was unreachable. Applies both
/// to the initial connection and to recovering from a `resolve_window` call that failed (which
/// this module conservatively treats as "the connection may be stale" — see `run`).
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
struct Request {
    window_id: WindowId,
    pid: u32,
    claims: WindowClaims,
    /// `pid`'s `/proc` start time when this request was issued. `None` when unreadable (the
    /// process already gone, or a test environment with no matching fixture). Compared against
    /// a fresh read when the answer arrives to detect PID reuse in between.
    start_time: Option<u64>,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    start_time: u64,
    identity: CachedIdentity,
}

/// A `HashMap` capped at [`CACHE_CAPACITY`] entries with FIFO eviction (not LRU: eviction order
/// is insertion order, not access order). Simpler than true LRU and sufficient here — the
/// working set is "windows currently open", which naturally stays far below the cap; eviction
/// only matters as a backstop against an unbounded leak.
#[derive(Debug, Default)]
struct BoundedCache {
    entries: HashMap<u32, CacheEntry>,
    insertion_order: VecDeque<u32>,
}

impl BoundedCache {
    fn insert(&mut self, pid: u32, entry: CacheEntry) {
        if self.entries.insert(pid, entry).is_none() {
            self.insertion_order.push_back(pid);
            while self.insertion_order.len() > CACHE_CAPACITY {
                if let Some(oldest) = self.insertion_order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
    }

    fn get(&self, pid: u32) -> Option<&CacheEntry> {
        self.entries.get(&pid)
    }

    fn remove(&mut self, pid: u32) {
        if self.entries.remove(&pid).is_some() {
            self.insertion_order.retain(|candidate| *candidate != pid);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Identity cache backed by D-Bus. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct DbusIdentityCache {
    proc_root: PathBuf,
    cache: Arc<Mutex<BoundedCache>>,
    requests: mpsc::UnboundedSender<Request>,
}

impl DbusIdentityCache {
    /// Spawn the resolver task, reading process start times from the real `/proc`.
    /// `deliver` runs off the render thread and must enqueue an event-loop update.
    pub fn spawn(
        handle: &tokio::runtime::Handle,
        deliver: impl Fn(WindowId, CachedIdentity) + Send + 'static,
    ) -> Self {
        Self::spawn_with_proc_root(handle, PathBuf::from("/proc"), deliver)
    }

    /// As [`Self::spawn`], but reading start times from `proc_root` — the seam tests use to
    /// avoid depending on the real `/proc`.
    pub fn spawn_with_proc_root(
        handle: &tokio::runtime::Handle,
        proc_root: PathBuf,
        deliver: impl Fn(WindowId, CachedIdentity) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<Request>();
        let cache: Arc<Mutex<BoundedCache>> = Arc::default();

        handle.spawn(run(rx, Arc::clone(&cache), deliver, proc_root.clone()));

        Self {
            proc_root,
            cache,
            requests: tx,
        }
    }

    /// Forget a PID (process exited): drop it from the bounded cache immediately rather than
    /// waiting for FIFO eviction, so a fast-cycling PID does not push out unrelated entries.
    pub fn forget(&self, pid: u32) {
        self.cache.lock().remove(pid);
    }

    #[cfg(test)]
    fn cached_len(&self) -> usize {
        self.cache.lock().len()
    }
}

impl IdentityCache for DbusIdentityCache {
    fn resolve_cached(&self, pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
        let current_start_time = pid::read_start_time(&self.proc_root, pid)?;
        let cache = self.cache.lock();
        let entry = cache.get(pid)?;
        // A cached entry only answers for the exact process it was resolved for. If `pid` now
        // belongs to a different process, this is a cache miss, not stale data (planning doc 02
        // §3: PID reuse must not hand a window a stranger's identity).
        (entry.start_time == current_start_time).then(|| entry.identity.clone())
    }

    fn request_resolution(&self, window_id: WindowId, pid: u32, claims: WindowClaims) {
        let start_time = pid::read_start_time(&self.proc_root, pid);
        let _ = self.requests.send(Request {
            window_id,
            pid,
            claims,
            start_time,
        });
    }
}

async fn run(
    mut requests: mpsc::UnboundedReceiver<Request>,
    cache: Arc<Mutex<BoundedCache>>,
    deliver: impl Fn(WindowId, CachedIdentity) + Send + 'static,
    proc_root: PathBuf,
) {
    // `AppIdentityProxy` owns everything it needs internally (zbus connections are Arc-backed),
    // so no separate `Connection` binding needs to be kept alive alongside it.
    let mut proxy: Option<AppIdentityProxy<'_>> = None;
    let mut next_attempt = Instant::now();
    let mut pending = VecDeque::new();
    loop {
        let request = if pending.is_empty() {
            let Some(request) = requests.recv().await else {
                break;
            };
            request
        } else {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    request
                }
                _ = tokio::time::sleep(RECONNECT_BACKOFF) => pending.pop_front().expect("pending identity"),
            }
        };
        if request.start_time.is_none()
            || pid::read_start_time(&proc_root, request.pid) != request.start_time
        {
            continue;
        }
        if proxy.is_none() && Instant::now() >= next_attempt {
            match zbus::Connection::session().await {
                Ok(new_connection) => match AppIdentityProxy::new(&new_connection).await {
                    Ok(new_proxy) => proxy = Some(new_proxy),
                    Err(error) => {
                        tracing::debug!(%error, "AppIdentity1 proxy unavailable; will retry");
                        next_attempt = Instant::now() + RECONNECT_BACKOFF;
                    }
                },
                Err(error) => {
                    tracing::debug!(%error, "no session bus; identities stay provisional, will retry");
                    next_attempt = Instant::now() + RECONNECT_BACKOFF;
                }
            }
        }

        let identity = if let Some(active) = &proxy {
            let claims = &request.claims;
            match active
                .resolve_window(
                    request.pid,
                    claims.wayland_app_id.as_deref().unwrap_or(""),
                    claims.startup_wm_class.as_deref().unwrap_or(""),
                    claims.sandbox_app_id.as_deref().unwrap_or(""),
                )
                .await
            {
                Ok((app, instance, confidence, matches)) => {
                    let confidence = IdentityConfidence::from_wire(confidence);
                    Some(CachedIdentity {
                        app_id: app.parse().ok(),
                        instance_id: InstanceId::parse(&instance),
                        verified: confidence.is_verified(),
                        claim_mismatch: !matches && confidence.is_verified(),
                    })
                }
                Err(error) => {
                    tracing::debug!(%error, pid = request.pid, "identity resolution failed; provisional");
                    // Conservative: a call failure might mean the whole connection is stale
                    // (`templar-sessiond` restarted mid-flight), not just that this one call lost a
                    // race. Drop it so the next request re-establishes from scratch — cheap, and
                    // self-healing beats staying wedged on a dead connection forever.
                    proxy = None;
                    next_attempt = Instant::now();
                    None
                }
            }
        } else {
            None
        };

        // Only trust the answer, and only cache it for future PID lookups, if the PID still
        // names the same process it did when the request was issued — otherwise it may describe
        // whatever process reused the PID while the round trip was in flight.
        let current_start_time = pid::read_start_time(&proc_root, request.pid);
        let identity = identity.filter(|_| {
            matches!(
                (request.start_time, current_start_time),
                (Some(then), Some(now)) if then == now
            )
        });

        let managed =
            std::fs::read_to_string(proc_root.join(request.pid.to_string()).join("cgroup"))
                .ok()
                .is_some_and(|membership| {
                    membership.lines().any(|line| {
                        line.strip_prefix("0::").is_some_and(|group| {
                            templar_types::unit::parse_cgroup_path(std::path::Path::new(group))
                                .is_some()
                        })
                    })
                });
        // A transient startup/import race must not permanently cache a '?' for this process.
        // Retry only unavailable service answers or an actual managed-unit provisional answer.
        let retry = current_start_time == request.start_time
            && identity
                .as_ref()
                .is_none_or(|answer| !answer.verified && managed);
        if retry && pending.len() < CACHE_CAPACITY {
            pending.retain(|old: &Request| old.window_id != request.window_id);
            pending.push_back(request.clone());
        }
        if let Some(identity) = identity {
            if identity.verified
                && let Some(start_time) = request.start_time
            {
                cache.lock().insert(
                    request.pid,
                    CacheEntry {
                        start_time,
                        identity: identity.clone(),
                    },
                );
            }
            deliver(request.window_id, identity);
        } else {
            deliver(request.window_id, CachedIdentity::UNKNOWN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_cache_evicts_fifo_beyond_capacity() {
        let mut cache = BoundedCache::default();
        for pid in 0..(CACHE_CAPACITY as u32 + 10) {
            cache.insert(
                pid,
                CacheEntry {
                    start_time: 0,
                    identity: CachedIdentity::UNKNOWN,
                },
            );
        }
        assert_eq!(cache.len(), CACHE_CAPACITY);
        // The earliest-inserted entries are the ones evicted.
        assert!(cache.get(0).is_none());
        assert!(cache.get(9).is_none());
        assert!(cache.get(10).is_some());
        assert!(cache.get(CACHE_CAPACITY as u32 + 9).is_some());
    }

    #[test]
    fn forget_removes_from_both_the_map_and_eviction_order() {
        let mut cache = BoundedCache::default();
        cache.insert(
            1,
            CacheEntry {
                start_time: 0,
                identity: CachedIdentity::UNKNOWN,
            },
        );
        cache.remove(1);
        assert!(cache.get(1).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn resolve_cached_is_none_with_no_session_bus_and_no_prior_answer() {
        let handle = tokio::runtime::Handle::current();
        let dir = tempfile::tempdir().unwrap();
        // No fixture PID directories at all: every start-time read fails, so every cache lookup
        // is a clean miss rather than a panic.
        let cache =
            DbusIdentityCache::spawn_with_proc_root(&handle, dir.path().to_path_buf(), |_, _| {
                panic!("a nonexistent PID must not produce an identity answer")
            });
        assert!(
            cache
                .resolve_cached(4242, &WindowClaims::default())
                .is_none()
        );
        assert_eq!(cache.cached_len(), 0);
    }

    #[test]
    fn forget_is_a_harmless_no_op_on_an_empty_cache() {
        let mut cache = BoundedCache::default();
        cache.remove(999);
        assert_eq!(cache.len(), 0);
    }
}
