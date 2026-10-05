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
//! snapshot of a handful of directories costs nothing. Additions and content edits take effect;
//! deletions do not (the import path only ever adds — a removed file's manifest needs a prune
//! the registry does not have yet, so removals still wait for a restart).

use parking_lot::RwLock;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
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
    let mut before = snapshot_dirs(&dirs);
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        let after = snapshot_dirs(&dirs);
        if after == before {
            continue;
        }
        before = after;
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
            if imported > 0 {
                crate::ensure_claimed_provider_declarations(
                    task_registry.as_ref(),
                    &task_providers,
                );
            }
            Ok::<_, anyhow::Error>((imported, grouped))
        })
        .await;
        match changed {
            Ok(Ok((imported, grouped))) => {
                tracing::info!(imported, grouped, "desktop-entry rescan applied");
                if imported > 0 {
                    super::autostart::announce_applications_changed(&connection).await;
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

/// What the watched directories hold right now: each `.desktop` file's path to its content
/// mtime. Additions, removals and content edits all move the map; anything that is not a
/// `.desktop` file is invisible to it. Missing or unreadable directories simply contribute
/// nothing, so a directory that appears later — a snap export tree that did not exist at
/// login — still registers as a change.
fn snapshot_dirs(dirs: &[PathBuf]) -> BTreeMap<String, i64> {
    fn mtime_ms(path: &std::path::Path) -> Option<i64> {
        std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as i64)
    }

    let mut snapshot = BTreeMap::new();
    for dir in dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(file_mtime) = mtime_ms(&path) {
                snapshot.insert(format!("file:{}", path.display()), file_mtime);
            }
        }
    }
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_desktop(dir: &std::path::Path, name: &str, exec: &str) {
        std::fs::write(
            dir.join(name),
            format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n"),
        )
        .expect("fixture .desktop file writes");
    }

    #[test]
    fn an_unchanged_tree_snapshots_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_desktop(dir.path(), "a.desktop", "/usr/bin/a");
        let dirs = vec![dir.path().to_path_buf()];
        assert_eq!(snapshot_dirs(&dirs), snapshot_dirs(&dirs));
    }

    #[test]
    fn a_new_desktop_file_changes_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = vec![dir.path().to_path_buf()];
        let before = snapshot_dirs(&dirs);
        write_desktop(dir.path(), "new.desktop", "/usr/bin/new");
        assert_ne!(before, snapshot_dirs(&dirs));
    }

    #[test]
    fn an_edited_desktop_file_changes_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_desktop(dir.path(), "a.desktop", "/usr/bin/a");
        let dirs = vec![dir.path().to_path_buf()];
        let before = snapshot_dirs(&dirs);
        // A rewrite moves the file's mtime; the sleep keeps the test honest on filesystems
        // with coarse timestamp granularity.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_desktop(dir.path(), "a.desktop", "/usr/bin/b");
        assert_ne!(before, snapshot_dirs(&dirs));
    }

    #[test]
    fn a_removed_desktop_file_changes_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_desktop(dir.path(), "a.desktop", "/usr/bin/a");
        let dirs = vec![dir.path().to_path_buf()];
        let before = snapshot_dirs(&dirs);
        std::fs::remove_file(dir.path().join("a.desktop")).expect("fixture removes");
        assert_ne!(before, snapshot_dirs(&dirs));
    }

    #[test]
    fn non_desktop_files_are_invisible_to_the_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dirs = vec![dir.path().to_path_buf()];
        let before = snapshot_dirs(&dirs);
        std::fs::write(dir.path().join("notes.txt"), "not an application").expect("fixture writes");
        assert_eq!(before, snapshot_dirs(&dirs));
    }

    #[test]
    fn a_missing_directory_contributes_nothing_until_it_exists() {
        let outer = tempfile::tempdir().expect("tempdir");
        let missing = outer.path().join("not-yet-there");
        let dirs = vec![missing.clone()];
        let before = snapshot_dirs(&dirs);
        assert!(before.is_empty());
        std::fs::create_dir(&missing).expect("fixture creates dir");
        write_desktop(&missing, "late.desktop", "/usr/bin/late");
        assert_ne!(before, snapshot_dirs(&dirs));
    }
}
