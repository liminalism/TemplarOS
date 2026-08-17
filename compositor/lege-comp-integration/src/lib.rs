//! Integration crate for the compositor fork (`forks/cosmic-comp`, see `../FORK.md`).
//!
//! The render thread only ever touches [`DbusIdentityCache::resolve_cached`] (a lock-free-ish
//! map lookup). Resolution requests are queued to a tokio task that talks to
//! `org.legeos.AppIdentity1`; answers are drained by the compositor loop through
//! [`DbusIdentityCache::drain_answers`] and fed to `WindowService::update_identity`.

#![forbid(unsafe_code)]

use lege_client::proxies::AppIdentityProxy;
use lege_types::{IdentityConfidence, InstanceId, WindowClaims};
use lege_window::{CachedIdentity, IdentityCache, WindowId};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;

/// Names of the private shell protocol (XML in `interfaces/wayland/`).
pub mod protocol {
    pub const SHELL_V1: &str = "lege_shell_v1";
    pub const RECOVERY_V1: &str = "lege_recovery_v1";
}

#[derive(Debug)]
struct Request {
    window_id: WindowId,
    pid: u32,
    claims: WindowClaims,
}

/// Identity cache backed by D-Bus. Cheap to clone (Arc inside).
#[derive(Clone, Debug)]
pub struct DbusIdentityCache {
    by_pid: Arc<Mutex<HashMap<u32, CachedIdentity>>>,
    answers: Arc<Mutex<Vec<(WindowId, CachedIdentity)>>>,
    requests: mpsc::UnboundedSender<Request>,
}

impl DbusIdentityCache {
    /// Spawn the resolver task on the given runtime handle.
    pub fn spawn(handle: &tokio::runtime::Handle) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Request>();
        let by_pid: Arc<Mutex<HashMap<u32, CachedIdentity>>> = Arc::default();
        let answers: Arc<Mutex<Vec<(WindowId, CachedIdentity)>>> = Arc::default();
        let (by_pid_task, answers_task) = (Arc::clone(&by_pid), Arc::clone(&answers));

        handle.spawn(async move {
            let connection = match zbus::Connection::session().await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::warn!(%error, "no session bus; identities stay provisional");
                    return;
                }
            };
            let proxy = match AppIdentityProxy::new(&connection).await {
                Ok(proxy) => proxy,
                Err(error) => {
                    tracing::warn!(%error, "AppIdentity proxy unavailable");
                    return;
                }
            };
            while let Some(request) = rx.recv().await {
                let claims = &request.claims;
                let result = proxy
                    .resolve_window(
                        request.pid,
                        claims.wayland_app_id.as_deref().unwrap_or(""),
                        claims.startup_wm_class.as_deref().unwrap_or(""),
                        claims.sandbox_app_id.as_deref().unwrap_or(""),
                    )
                    .await;
                let identity = match result {
                    Ok((app, instance, confidence, matches)) => {
                        let confidence = IdentityConfidence::from_wire(confidence);
                        CachedIdentity {
                            app_id: app.parse().ok(),
                            instance_id: InstanceId::parse(&instance),
                            verified: confidence.is_verified(),
                            claim_mismatch: !matches && confidence.is_verified(),
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, pid = request.pid, "identity resolution failed; provisional");
                        CachedIdentity::UNKNOWN
                    }
                };
                by_pid_task.lock().insert(request.pid, identity.clone());
                answers_task.lock().push((request.window_id, identity));
            }
        });

        Self {
            by_pid,
            answers,
            requests: tx,
        }
    }

    /// Take resolved answers to apply with `WindowService::update_identity`.
    pub fn drain_answers(&self) -> Vec<(WindowId, CachedIdentity)> {
        std::mem::take(&mut *self.answers.lock())
    }

    /// Forget a PID (process exited).
    pub fn forget(&self, pid: u32) {
        self.by_pid.lock().remove(&pid);
    }
}

impl IdentityCache for DbusIdentityCache {
    fn resolve_cached(&self, pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
        self.by_pid.lock().get(&pid).cloned()
    }

    fn request_resolution(&self, window_id: WindowId, pid: u32, claims: WindowClaims) {
        let _ = self.requests.send(Request {
            window_id,
            pid,
            claims,
        });
    }
}
