//! Scan XDG autostart entries, plan with [`templar_session_core::autostart::plan`] and launch the
//! approved ones through the launcher, recording an `AutostartDecision` history event per entry
//! (`docs/architecture/session.md`; `xdg-desktop-autostart.target` is masked so nothing bypasses
//! this).

use parking_lot::RwLock;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use templar_appd::{
    Registry,
    desktop_entry::{self, DesktopEntry, ExecResolver, SourceContext},
    launcher::{self, LaunchBackend},
};
use templar_session_core::{
    autostart::{self, AutostartEntry, BackgroundPermission, Verdict},
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    providers::ProviderRegistry,
};
use templar_state::Store;
use templar_types::{AppCatalog, AppId, ProviderRole};

/// Directories XDG autostart entries live in, later ones overriding earlier ones by desktop ID
/// (`docs/architecture/session.md`).
pub fn autostart_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/etc/xdg/autostart")];
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    {
        dirs.push(config_home.join("autostart"));
    }
    dirs
}

/// Read every `.desktop` file under `dirs`, later directories overriding earlier ones for the
/// same desktop ID; `Hidden=true` entries (the standard "masked" signal) are dropped.
///
/// Each entry carries the [`SourceContext`] it was read from, so an autostart entry that has to be
/// imported gets the provenance — and therefore the trust tier — of the directory it came from.
/// `/etc/xdg/autostart` is the system's; `~/.config/autostart` is the person's, and a file there
/// must not inherit a packaged application's standing.
pub fn scan_entries(dirs: &[PathBuf]) -> Vec<(String, DesktopEntry, SourceContext)> {
    let mut by_id: BTreeMap<String, (DesktopEntry, SourceContext)> = BTreeMap::new();
    for dir in dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("desktop") {
                continue;
            }
            let Some(desktop_id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(parsed) = desktop_entry::parse(&text) else {
                continue;
            };
            by_id.insert(desktop_id, (parsed, SourceContext::for_file(dir, path)));
        }
    }
    by_id
        .into_iter()
        .filter(|(_, (entry, _))| !entry.hidden)
        .map(|(id, (entry, source))| (id, entry, source))
        .collect()
}

/// TemplarOS' convention for autostart entries beyond what the desktop-entry spec defines:
/// `X-TemplarOS-Provides=<provider-role>` ties the entry to an exclusive role (it only starts when
/// the app is the selected provider); `X-TemplarOS-Background=<permission>[,<permission>...]`
/// declares what it needs while running (planning doc 01 §7). Absent, an autostart entry is
/// assumed to want at least login autostart — it would not otherwise be here.
pub fn to_request(entry: &DesktopEntry, app_id: AppId) -> AutostartEntry {
    let role = entry.extra.get("X-TemplarOS-Provides").and_then(|value| {
        ProviderRole::ALL
            .iter()
            .copied()
            .find(|role| role.as_str() == value.trim())
    });
    let requested = entry.extra.get("X-TemplarOS-Background").map_or_else(
        || vec![BackgroundPermission::LoginAutostart],
        |value| {
            value
                .split(',')
                .filter_map(BackgroundPermission::parse)
                .collect()
        },
    );
    AutostartEntry {
        app_id,
        role,
        requested,
    }
}

/// Run the autostart sequence once, at session start: resolve each entry to a canonical AppId
/// (importing a manifest for it if none is installed and it is not already known), plan against
/// approvals and the provider registry, launch what is approved, and record one history event
/// per entry.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    registry: Arc<Registry>,
    providers: Arc<RwLock<ProviderRegistry>>,
    store: Store,
    history: Arc<SqliteHistory>,
    notifier: crate::apis::history::HistoryChangeNotifier,
    backend: LaunchBackend,
    connection: zbus::Connection,
    resolver: Arc<dyn ExecResolver>,
    dirs: Vec<PathBuf>,
) {
    let approvals = match autostart::load_approvals(&store) {
        Ok(approvals) => approvals,
        Err(error) => {
            tracing::error!(%error, "failed to load autostart approvals; skipping autostart");
            return;
        }
    };

    let mut imported_any = false;
    let mut entries: Vec<(AutostartEntry, autostart::Approval)> = Vec::new();
    for (desktop_id, entry, source) in scan_entries(&dirs) {
        let existing = registry
            .manifests()
            .into_iter()
            .find(|manifest| {
                manifest.primary_desktop_file_id == desktop_id
                    || manifest.aliases.desktop_file_ids.contains(&desktop_id)
            })
            .map(|manifest| manifest.id.clone());
        let app_id = if let Some(id) = existing {
            id
        } else if let Some(derived) =
            desktop_entry::derive_manifest(&entry, &desktop_id, &source, resolver.as_ref())
        {
            let id = derived.manifest.id.clone();
            registry.insert_manifest(derived.manifest);
            imported_any = true;
            id
        } else {
            tracing::debug!(
                desktop_id,
                "autostart entry has no resolvable executable; skipped"
            );
            continue;
        };
        let request = to_request(&entry, app_id);
        if let Err(error) = store.transaction(|tx| autostart::save_declaration_within(tx, &request))
        {
            tracing::warn!(app = %request.app_id, %error, "failed to persist background declaration");
        }
        let approval = autostart::approval_for(&approvals, &request.app_id);
        entries.push((request, approval));
    }

    // Autostart may have been the first thing to see an application at all. Regroup before the
    // decisions are recorded, and tell anything listening that the installed set moved — a
    // launcher drawn before this point is showing a stale list.
    if imported_any {
        if let Err(error) =
            crate::background::desktop_import::regroup_and_persist(&registry, &store)
        {
            tracing::warn!(%error, "regrouping after autostart import failed");
        }
        announce_applications_changed(&connection).await;
    }

    let plan = autostart::plan(&entries, &providers.read());
    for (app_id, verdict) in plan {
        let message = match &verdict {
            Verdict::Launch => format!("{app_id} started at login."),
            Verdict::SkippedDenied => format!("{app_id}'s autostart request was denied."),
            Verdict::SkippedNotSelectedProvider { role } => {
                format!(
                    "{app_id} did not start: another application is the selected {role} provider."
                )
            }
            Verdict::SkippedUndecided => {
                format!("{app_id}'s autostart request has not been decided yet.")
            }
        };

        let mut instance_id = None;
        if matches!(verdict, Verdict::Launch)
            && let Some(manifest) = registry.manifest(&app_id)
        {
            match launcher::launch(
                Arc::clone(&registry),
                manifest,
                &[],
                None,
                backend,
                Some(&connection),
            )
            .await
            {
                Ok(record) => instance_id = Some(record.id),
                Err(error) => tracing::warn!(app = %app_id, %error, "autostart launch failed"),
            }
        }

        match history.append(Event {
            id: EventId::new(),
            at_ms: templar_state::now_ms(),
            kind: EventKind::AutostartDecision,
            app_id: Some(app_id),
            instance_id,
            message,
            cause: Some(format!("autostart:{verdict:?}").to_lowercase()),
            actions: Vec::new(),
            reversible: false,
        }) {
            Ok(()) => notifier.notify().await,
            Err(error) => tracing::warn!(%error, "failed to record autostart history event"),
        }
    }
}

/// Emit `AppIdentity1.ApplicationsChanged`. Best-effort: a listener that misses it re-reads on its
/// next natural refresh, and failing to announce a change must never abort autostart.
pub(crate) async fn announce_applications_changed(connection: &zbus::Connection) {
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
mod tests {
    use super::*;

    #[test]
    fn scan_entries_merges_directories_and_drops_hidden_ones() {
        let system_dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            system_dir.path().join("org.example.Sync.desktop"),
            "[Desktop Entry]\nType=Application\nName=Sync\nExec=sync-agent\n",
        )
        .unwrap();
        std::fs::write(
            system_dir.path().join("org.example.Masked.desktop"),
            "[Desktop Entry]\nType=Application\nName=Masked\nExec=masked\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(
            user_dir.path().join("org.example.Sync.desktop"),
            "[Desktop Entry]\nType=Application\nName=Sync (user override)\nExec=sync-agent\n",
        )
        .unwrap();

        let entries = scan_entries(&[
            system_dir.path().to_path_buf(),
            user_dir.path().to_path_buf(),
        ]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "org.example.Sync.desktop");
        assert_eq!(
            entries[0].1.name, "Sync (user override)",
            "the user directory must win"
        );
    }

    #[test]
    fn to_request_reads_the_templaros_conventions() {
        let mut entry = DesktopEntry {
            entry_type: "Application".into(),
            name: "Sync".into(),
            exec: Some("sync-agent".into()),
            ..DesktopEntry::default()
        };
        entry.extra.insert(
            "X-TemplarOS-Background".into(),
            "continuous-background,network-while-hidden".into(),
        );
        entry
            .extra
            .insert("X-TemplarOS-Provides".into(), "notification-server".into());

        let app_id: AppId = "org.example.Sync".parse().unwrap();
        let request = to_request(&entry, app_id.clone());
        assert_eq!(request.app_id, app_id);
        assert_eq!(request.role, Some(ProviderRole::NotificationServer));
        assert_eq!(
            request.requested,
            vec![
                BackgroundPermission::ContinuousBackground,
                BackgroundPermission::NetworkWhileHidden
            ]
        );
    }

    #[test]
    fn to_request_defaults_to_login_autostart_with_no_conventions_present() {
        let entry = DesktopEntry {
            entry_type: "Application".into(),
            name: "Plain".into(),
            exec: Some("plain".into()),
            ..DesktopEntry::default()
        };
        let request = to_request(&entry, "org.example.Plain".parse().unwrap());
        assert_eq!(request.role, None);
        assert_eq!(
            request.requested,
            vec![BackgroundPermission::LoginAutostart]
        );
    }
}
