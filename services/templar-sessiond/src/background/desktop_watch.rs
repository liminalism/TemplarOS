//! Re-import `.desktop` files that appear or change after startup, so installing an
//! application takes effect without restarting the session.
//!
//! The startup scan in `load_services` runs once; without this task a `.desktop` file written
//! later (a package install, a person's own `~/.local/share/applications/firefox.desktop`) sits
//! unread until the next login. This task polls the same directory list on a slow interval and
//! re-runs exactly the startup pair — [`desktop_import::import`] then
//! [`desktop_import::regroup_and_persist`] — when the on-disk set differs, announcing through the
//! same `AppIdentity1.ApplicationsChanged` signal the autostart path uses.
//!
//! Polling, not watching: sessiond has no file-watching dependency, and a 15-second stat-only
//! snapshot of a handful of directories costs no content reads. Additions, edits and removals
//! refresh catalogue and provider choices without losing explicit selections.

use parking_lot::RwLock;
use std::{
    collections::BTreeMap, os::unix::fs::MetadataExt as _, path::PathBuf, sync::Arc, time::Duration,
};
use templar_appd::{Registry, desktop_entry::ExecResolver};
use templar_session_core::providers::ProviderRegistry;
use templar_state::Store;

/// How often the application directories are re-stat'ed. Slow on purpose: installs are rare,
/// and a change triggers the real import, so the interval only bounds how stale a new
/// application looks, never correctness.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Watch `dirs` for new or changed `.desktop` files for the rest of the session. Never returns;
/// logs and continues on every failure, so a broken directory can never end the task.
///
/// A rescan re-runs the startup pair — import, then the claimed-role pass — so an installed or
/// edited entry's `X-TemplarOS-Provides=` claim takes effect without a restart, the same way it
/// does at login.
pub async fn run(
    registry: Arc<Registry>,
    store: Store,
    resolver: Arc<dyn ExecResolver>,
    connection: zbus::Connection,
    dirs: Vec<PathBuf>,
    providers: Arc<RwLock<ProviderRegistry>>,
) {
    let mut before = None;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let after = match snapshot_dirs(&dirs) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(%error, "desktop-entry snapshot failed");
                continue;
            }
        };
        if before.as_ref() == Some(&after) {
            continue;
        }
        let task_registry = registry.clone();
        let task_store = store.clone();
        let task_resolver = resolver.clone();
        let task_dirs = dirs.clone();
        let task_providers = providers.clone();
        let changed = tokio::task::spawn_blocking(move || {
            let imported = super::desktop_import::import(
                &task_registry,
                &task_store,
                &task_dirs,
                task_resolver.as_ref(),
            )?;
            let grouped = super::desktop_import::regroup_and_persist(&task_registry, &task_store)?;
            let changed_roles = {
                let connection = task_store.lock();
                let mut current = task_providers.write();
                let mut refreshed = templar_session_core::providers::load_within(&connection)?;
                refreshed.declare_installed(task_registry.as_ref());
                let changed: Vec<_> = templar_types::ProviderRole::ALL
                    .iter()
                    .copied()
                    .filter(|role| current.provider(*role) != refreshed.provider(*role))
                    .collect();
                *current = refreshed;
                changed
            };
            Ok::<_, anyhow::Error>((imported, grouped, changed_roles))
        })
        .await;
        match changed {
            Ok(Ok((imported, grouped, changed_roles))) => {
                before = Some(after);
                tracing::info!(imported, grouped, "desktop-entry rescan applied");
                if imported > 0 {
                    announce_applications_changed(&connection).await;
                }
                for role in changed_roles {
                    if let Err(error) = connection
                        .emit_signal(
                            None::<&str>,
                            templar_types::dbus::session::PROVIDERS_PATH,
                            "org.templaros.Providers1",
                            "Changed",
                            &(role.as_str(),),
                        )
                        .await
                    {
                        tracing::warn!(%error, "catalogue refresh provider notification failed");
                    }
                }
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "desktop-entry rescan failed");
            }
            Err(error) => {
                tracing::warn!(%error, "desktop-entry rescan task panicked");
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DesktopStamp {
    device: u64,
    inode: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

/// Native paths and nanosecond stat identity; unreadable data never becomes a successful epoch.
fn snapshot_dirs(dirs: &[PathBuf]) -> anyhow::Result<BTreeMap<PathBuf, DesktopStamp>> {
    let mut snapshot = BTreeMap::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension() != Some(std::ffi::OsStr::new("desktop")) {
                continue;
            }
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            snapshot.insert(
                path,
                DesktopStamp {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    len: metadata.len(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                    changed: (metadata.ctime(), metadata.ctime_nsec()),
                },
            );
        }
    }
    Ok(snapshot)
}

/// Emit `AppIdentity1.ApplicationsChanged`. Best-effort: a listener that misses it re-reads on its
/// next natural refresh, and failing to announce a change must never abort autostart.
pub(crate) async fn announce_applications_changed(connection: &zbus::Connection) {
    if let Ok(associations) = connection
        .object_server()
        .interface::<_, templar_associationd::dbus::AssociationApi>(
            templar_types::dbus::session::ASSOCIATIONS_PATH,
        )
        .await
    {
        associations.get().await.refresh_catalog();
    }
    let emitter = match zbus::object_server::SignalEmitter::new(
        connection,
        templar_types::dbus::session::APP_IDENTITY_PATH,
    ) {
        Ok(emitter) => emitter,
        Err(error) => {
            tracing::debug!(%error, "could not address AppIdentity1 to announce a change");
            return;
        }
    };
    if let Err(error) = templar_appd::dbus::IdentityApi::applications_changed(&emitter).await {
        tracing::debug!(%error, "announcing ApplicationsChanged failed");
    }
}

#[cfg(test)]
#[path = "desktop_watch_native_tests.rs"]
mod native_tests;
