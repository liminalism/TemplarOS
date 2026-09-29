//! Tells `templar-sessiond` when the compositor turned the screen off for want of input, and when
//! it came back on (`org.templaros.Session.Lifecycle1.NotifyIdle`). Session time — the Home
//! garden's clock above all — must not advance while an unlocked session sits untouched, and the
//! compositor is the only process that sees the seat, so it is the only one that can say so.
//!
//! Like the governor notifier this is best-effort and never blocks the render thread: the last
//! state wins, and a sessiond that is not on the bus yet is simply told the next time it changes.

use templar_client::proxies::LifecycleProxy;
use tokio::sync::watch;

/// Reports blanking to sessiond from a background task. Cheap to clone.
#[derive(Clone, Debug)]
pub struct IdleNotifier {
    tx: watch::Sender<bool>,
}

impl IdleNotifier {
    /// Spawn the background task on `handle`. The screen starts lit, so nothing is sent until the
    /// first blank.
    pub fn spawn(handle: &tokio::runtime::Handle) -> Self {
        let (tx, rx) = watch::channel(false);
        handle.spawn(run(rx));
        Self { tx }
    }

    /// The outputs went off (`true`) or came back on (`false`). Only real transitions reach the
    /// bus; repeating the current state sends nothing.
    pub fn set_idle(&self, idle: bool) {
        self.tx.send_if_modified(|current| {
            let changed = *current != idle;
            *current = idle;
            changed
        });
    }
}

async fn run(mut rx: watch::Receiver<bool>) {
    let mut proxy: Option<LifecycleProxy<'static>> = None;
    while rx.changed().await.is_ok() {
        let idle = *rx.borrow_and_update();
        if proxy.is_none() {
            match zbus::Connection::session().await {
                Ok(connection) => match LifecycleProxy::new(&connection).await {
                    Ok(new_proxy) => proxy = Some(new_proxy),
                    Err(error) => {
                        tracing::debug!(%error, "Lifecycle1 unavailable; idle state not reported");
                        continue;
                    }
                },
                Err(error) => {
                    tracing::debug!(%error, "no session bus; idle state not reported");
                    continue;
                }
            }
        }
        if let Some(active) = &proxy
            && let Err(error) = active.notify_idle(idle).await
        {
            tracing::debug!(%error, idle, "NotifyIdle failed; reconnecting at the next change");
            proxy = None;
        }
    }
}
