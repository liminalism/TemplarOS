//! Import [`AppManifest`](templar_types::AppManifest)s from installed `.desktop` files for
//! applications with no native manifest (`docs/architecture/identity.md`'s "follow-up").
//! A native manifest wins over an imported desktop copy. Rescans replace edited imported
//! manifests and their original Exec templates/native paths and retire removed or masked copies;
//! a prior import is not itself a native-manifest claim. Every scan rereads the authoritative
//! files, including startup with unchanged SQLite bookkeeping, to restore launch templates.

use anyhow::{Context, Result};
use std::{collections::BTreeSet, path::PathBuf};
use templar_appd::{
    Registry,
    desktop_entry::{self, ExecResolver, ExecTemplate, OriginNamespace, SourceContext},
    store as appd_store,
};
use templar_session_core::{
    autostart, history,
    history::{Event, EventId, EventKind},
    providers,
};
use templar_state::Store;
use templar_types::{AppCatalog, AppId, TrustTier};

/// Every directory that may hold `.desktop` application entries. Within the XDG chain later ones
/// take priority over earlier ones for the same desktop ID (the usual precedence: user over
/// system); the Flatpak export trees are a separate namespace and neither shadow it nor are
/// shadowed by it.
pub fn application_dirs() -> Vec<PathBuf> {
    application_dirs_from(
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            }),
    )
}

/// The well-known system data roots, searched whether or not `XDG_DATA_DIRS` names them.
///
/// The environment is consulted but never trusted to be complete: sessiond runs as a systemd
/// user service, and the environment it inherits is whatever the session imported, which on a
/// plain login is the spec default `/usr/local/share:/usr/share` — without the snap export
/// directory a Firefox or Chromium installed as a snap lives in, so the registry silently had no
/// browser. The same rule `templar doctor` applies (`cli/templar/src/doctor/checks.rs`).
const WELL_KNOWN_DATA_ROOTS: [&str; 4] = [
    "/usr/local/share",
    "/usr/share",
    "/var/lib/flatpak/exports/share",
    "/var/lib/snapd/desktop",
];

/// [`application_dirs`] over an explicit environment, lowest precedence first. The chain is
/// `XDG_DATA_DIRS` (highest first, as the spec has it) with the well-known roots appended as the
/// fallback tail, deduplicated at the highest position each root holds, then reversed so a
/// later directory shadows an earlier one; the person's own directories come last of all.
fn application_dirs_from(
    data_dirs: Option<&std::ffi::OsStr>,
    data_home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut highest_first: Vec<PathBuf> = Vec::new();
    let named = data_dirs
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|dir| dir.is_absolute());
    for root in named.chain(WELL_KNOWN_DATA_ROOTS.iter().map(PathBuf::from)) {
        if !highest_first.contains(&root) {
            highest_first.push(root);
        }
    }
    let mut dirs: Vec<PathBuf> = highest_first
        .into_iter()
        .rev()
        .map(|root| root.join("applications"))
        .collect();
    if let Some(data_home) = data_home {
        for dir in [
            data_home.join("flatpak/exports/share/applications"),
            data_home.join("applications"),
        ] {
            dirs.retain(|known| *known != dir);
            dirs.push(dir);
        }
    }
    dirs
}

fn is_desktop_import(manifest: &templar_types::AppManifest) -> bool {
    manifest.provenance.as_ref().and_then(|provenance| provenance.source_path.as_ref())
        .is_some_and(|path| path.extension() == Some(std::ffi::OsStr::new("desktop")))
}

/// Reconcile importable desktop copies from `dirs`, without retiring another importer's sources.
/// Stage every candidate and commit one bookkeeping/grant transaction before publishing the whole
/// installed-manifest batch; a failed scan leaves it intact.
pub fn import(
    registry: &Registry,
    store: &Store,
    dirs: &[PathBuf],
    resolver: &dyn ExecResolver,
) -> Result<usize> {
    let installed = registry.manifests();
    let native_ids: BTreeSet<String> = installed
        .iter()
        .filter(|manifest| !is_desktop_import(manifest))
        .flat_map(|manifest| {
            let mut ids = vec![manifest.primary_desktop_file_id.clone()];
            ids.extend(manifest.aliases.desktop_file_ids.iter().cloned());
            ids
        })
        .collect();

    // Keyed on (origin namespace, desktop id), not on the desktop id alone. Within the XDG chain a
    // later directory still shadows an earlier one — a person's own `firefox.desktop` is *meant* to
    // replace the system's. Across namespaces it must not: keyed on the id alone, a Flatpak export
    // and a distribution package that share `firefox.desktop` collapse into one candidate, and
    // whichever directory happened to be scanned last erased the other application from the
    // registry entirely. They are two copies of a program; they get two entries.
    let mut candidates: std::collections::BTreeMap<(OriginNamespace, String), SourceContext> =
        std::collections::BTreeMap::new();
    for dir in dirs {
        let read_dir = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("reading application directory {}", dir.display())),
        };
        for entry in read_dir {
            let entry = entry.with_context(|| format!("reading application directory entry in {}", dir.display()))?;
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
            let source = SourceContext::for_file(dir, path);
            candidates.insert((source.namespace, desktop_id), source);
        }
    }

    let mut prepared = Vec::new();
    for ((_namespace, desktop_id), source) in candidates {
        if !native_ids.contains(&desktop_id)
            && let Some(candidate) = prepare_import(desktop_id, source, resolver)?
        {
            prepared.push(candidate);
        }
    }
    let surviving: BTreeSet<_> = prepared.iter().map(|candidate| &candidate.manifest.id).collect();
    let removed: Vec<_> = installed.iter()
        .filter(|manifest| is_desktop_import(manifest) && !surviving.contains(&manifest.id)
            && manifest.provenance.as_ref().and_then(|provenance| provenance.source_path.as_ref())
                .is_some_and(|path| dirs.iter().any(|root| path.starts_with(root))))
        .map(|manifest| &manifest.id).collect();
    drop(surviving);
    store.transaction(|tx| {
        for candidate in &mut prepared { persist_import(tx, candidate)?; }
        for id in &removed { providers::undeclare_within(tx, id)?; }
        Ok(())
    }).context("committing desktop-copy reconciliation")?;
    let changed = prepared.len() + removed.len();
    registry.update_manifests(
        prepared.into_iter().map(|candidate| (candidate.manifest, candidate.exec, candidate.path)),
        removed,
    );
    Ok(changed)
}

struct PreparedImport {
    desktop_id: String,
    path: PathBuf,
    mtime_ms: i64,
    manifest: templar_types::AppManifest,
    exec: ExecTemplate,
}

/// Missing or invalid desktop entries are not applications; inaccessible data is not deletion.
fn prepare_import(
    desktop_id: String,
    source: SourceContext,
    resolver: &dyn ExecResolver,
) -> Result<Option<PreparedImport>> {
    let metadata = match std::fs::metadata(&source.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading desktop metadata {}", source.path.display())),
    };
    if !metadata.is_file() { return Ok(None); }
    let mtime_ms = metadata.modified().ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis() as i64);
    let text = match std::fs::read_to_string(&source.path) {
        Ok(text) => text,
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading desktop entry {}", source.path.display())),
    };
    let Some(entry) = desktop_entry::parse(&text) else { return Ok(None); };
    let Some(derived) = desktop_entry::derive_manifest(&entry, &desktop_id, &source, resolver) else { return Ok(None); };
    Ok(Some(PreparedImport {
        desktop_id,
        path: source.path,
        mtime_ms,
        manifest: derived.manifest,
        exec: derived.exec_template,
    }))
}

fn persist_import(tx: &rusqlite::Transaction<'_>, candidate: &mut PreparedImport) -> Result<()> {
    let manifest = &mut candidate.manifest;
    let origin = manifest.provenance.as_ref().map_or(templar_types::AppOriginKind::Unknown, |provenance| provenance.origin);
    if let Some(first_seen) = appd_store::variant_first_seen_within(tx, &manifest.id)?
        && let Some(provenance) = manifest.provenance.as_mut()
    {
        provenance.first_seen_ms = first_seen;
    }
    // Retain the rollback-readable stored ID; aliases only resolve the installed copy on read.
    if origin == templar_types::AppOriginKind::Flatpak {
        let previous = desktop_entry::derive_app_id(&candidate.desktop_id);
        if previous != manifest.id {
            appd_store::save_variant_alias_within(tx, &previous, &manifest.id, "flatpak-export-renamed")?;
        }
    }
    if appd_store::variant_trust_within(tx, &manifest.id)? == Some(TrustTier::System)
        && manifest.trust() == TrustTier::User
    {
        invalidate_shadowed_grants(tx, &manifest.id, &manifest.display_name)?;
    }
    appd_store::record_desktop_import_within(
        tx, &candidate.desktop_id, &candidate.path, candidate.mtime_ms, &manifest.id, origin,
    ).with_context(|| format!("recording desktop_import bookkeeping for {}", candidate.desktop_id))
}

/// Drop the rights a variant earned while the system owned it, now that it does not.
///
/// Provider declarations and autostart approvals both key on the variant id, and the id survives
/// the file underneath it being replaced. Trust does not: a copy that used to come from a package
/// and now comes from a file this account can rewrite is, for these purposes, a different
/// application wearing the same name.
fn invalidate_shadowed_grants(tx: &rusqlite::Transaction<'_>, variant: &AppId, display_name: &str) -> Result<()> {
    providers::undeclare_within(tx, variant)?;
    autostart::set_approval_within(tx, variant, autostart::Approval::Undecided)?;
    history::append_within(
            tx,
            &Event {
                id: EventId::new(),
                at_ms: templar_state::now_ms(),
                kind: EventKind::ProviderChange,
                app_id: Some(variant.clone()),
                instance_id: None,
                message: format!(
                    "{display_name} is now provided by a file in your own account rather than by \
                     an installed package. Its system roles and its permission to start at login \
                     were withdrawn; you can grant them again if you meant to replace it."
                ),
                cause: Some("provenance-moved-to-user".into()),
                actions: Vec::new(),
                reversible: false,
            },
        )
    .map(|_| ())
}

/// Group everything installed into logical applications and persist the result, then load the
/// alias table onto the registry.
///
/// Run after every scan that could change the installed set. Grouping is presentation, so a
/// failure here is logged and the session continues with whatever grouping it had.
pub fn regroup_and_persist(registry: &Registry, store: &Store) -> Result<usize> {
    let persisted = appd_store::load_variants(store).context("loading recorded app variants")?;
    let index = templar_appd::variants::group_catalog(registry, &persisted);
    let aliases = appd_store::load_variant_aliases(store).context("loading variant aliases")?;

    let rows = index.rows();
    store
        .transaction(|tx| {
            for (variant, logical, source) in &rows {
                let provenance = registry
                    .manifest(variant)
                    .and_then(|manifest| manifest.provenance.clone())
                    .unwrap_or_default();
                appd_store::save_variant_within(tx, variant, logical, *source, &provenance)?;
            }
            Ok(())
        })
        .context("persisting app variants")?;

    registry.set_logical_index(index);
    registry.set_variant_aliases(aliases);
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_rescan_retires_only_its_sources_and_preserves_io_failures() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let apps = directory.path().join("applications");
        let autostart = directory.path().join("autostart");
        std::fs::create_dir(&apps).unwrap();
        std::fs::create_dir(&autostart).unwrap();
        for (root, name) in [(&apps, "app"), (&autostart, "login")] {
            write_desktop_file(root, &format!("{name}.desktop"),
                &format!("[Desktop Entry]\nType=Application\nName={name}\nExec=viewer --{name}\n"));
        }
        let store = Store::in_memory().unwrap();
        let registry = Registry::default();
        let resolver = StaticResolver(HashMap::from([("viewer", "/usr/bin/viewer")]));
        import(&registry, &store, &[apps.clone(), autostart.clone()], &resolver).unwrap();
        let app: AppId = "desktop-import.app.desktop".parse().unwrap();
        let login: AppId = "desktop-import.login.desktop".parse().unwrap();
        let role = templar_types::ProviderRole::NetworkStatusUi;
        store.transaction(|tx| {
            providers::declare_within(tx, &login, &templar_types::ProviderCapability { role, priority: 30 })?;
            providers::apply_selection_within(tx, role, Some(&login), "autostart-owner-fixture")?;
            Ok(())
        }).unwrap();
        let original = registry.launch_template(&login).unwrap();
        std::fs::remove_file(apps.join("app.desktop")).unwrap();
        let unreadable = apps.join("loop.desktop");
        symlink("loop.desktop", &unreadable).unwrap();
        assert!(import(&registry, &store, &[apps.clone()], &resolver).is_err());
        assert!(registry.manifest(&app).is_some(), "I/O failure must not publish deletion");
        assert!(registry.launch_template(&app).is_some());
        std::fs::remove_file(unreadable).unwrap();
        import(&registry, &store, &[apps], &resolver).unwrap();
        assert!(registry.manifest(&app).is_none());
        assert!(registry.launch_template(&app).is_none());
        assert!(registry.manifest(&login).is_some());
        let retained = registry.launch_template(&login).unwrap();
        assert_eq!(retained.exec, original.exec);
        assert_eq!(retained.desktop_file_path, autostart.join("login.desktop"));
        assert_eq!(providers::load(&store).unwrap().provider(role), Some(login));
    }

    #[test]
    fn a_later_copy_fault_keeps_the_catalogue_then_edits_and_uninstall_reconcile() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        let registry = Registry::default();
        let resolver = templar_appd::desktop_entry::PathExecResolver::from_env();
        let dirs = [directory.path().to_path_buf()];
        for name in ["a", "b"] {
            std::fs::write(directory.path().join(format!("{name}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName=Before-{name}\nExec=/usr/bin/true --before %F\nMimeType=application/pdf;\n")).unwrap();
        }
        import(&registry, &store, &dirs, &resolver).unwrap();
        let selected: AppId = "desktop-import.a.desktop".parse().unwrap();
        let role = templar_types::ProviderRole::NetworkStatusUi;
        store.transaction(|tx| {
            providers::declare_within(tx, &selected, &templar_types::ProviderCapability { role, priority: 30 })?;
            providers::apply_selection_within(tx, role, Some(&selected), "native-catalogue-fixture")?;
            Ok(())
        }).unwrap();
        store.lock().execute_batch(
            "CREATE TEMP TRIGGER refuse_second_copy BEFORE INSERT ON desktop_import
             WHEN NEW.desktop_id='b.desktop' BEGIN SELECT RAISE(ABORT,'second-copy fault'); END;",
        ).unwrap();
        for name in ["a", "b"] {
            std::fs::write(directory.path().join(format!("{name}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName=After-{name}\nExec=/usr/bin/false --after %u\nMimeType=text/plain;\n")).unwrap();
        }
        assert!(import(&registry, &store, &dirs, &resolver).is_err());
        for name in ["a", "b"] {
            let manifest = registry.manifest(&format!("desktop-import.{name}.desktop").parse().unwrap()).unwrap();
            assert_eq!(manifest.display_name, format!("Before-{name}"));
            assert_eq!(manifest.executable, std::path::Path::new("/usr/bin/true"));
            assert!(manifest.supports_mime("application/pdf") && !manifest.supports_mime("text/plain"));
            let launch = registry.launch_template(&manifest.id).unwrap();
            assert_eq!(launch.exec, ExecTemplate::parse("/usr/bin/true --before %F"));
            assert_eq!(launch.desktop_file_path, directory.path().join(format!("{name}.desktop")));
        }
        store.lock().execute_batch("DROP TRIGGER refuse_second_copy").unwrap();
        import(&registry, &store, &dirs, &resolver).unwrap();
        for name in ["a", "b"] {
            let manifest = registry.manifest(&format!("desktop-import.{name}.desktop").parse().unwrap()).unwrap();
            assert_eq!(manifest.display_name, format!("After-{name}"));
            assert_eq!(manifest.executable, std::path::Path::new("/usr/bin/false"));
            assert!(manifest.supports_mime("text/plain") && !manifest.supports_mime("application/pdf"));
            let launch = registry.launch_template(&manifest.id).unwrap();
            assert_eq!(launch.exec, ExecTemplate::parse("/usr/bin/false --after %u"));
            assert_eq!(launch.desktop_file_path, directory.path().join(format!("{name}.desktop")));
        }
        std::fs::remove_file(directory.path().join("a.desktop")).unwrap();
        import(&registry, &store, &dirs, &resolver).unwrap();
        assert!(registry.manifest(&selected).is_none());
        assert!(registry.launch_template(&selected).is_none());
        let refreshed = providers::load(&store).unwrap();
        assert!(refreshed.provider(role).is_none());
        assert_eq!(refreshed.selections.get(&role), Some(&selected));
    }

    #[test]
    fn authoritative_templates_survive_restart_and_follow_shadowing_masking_and_removal() {
        let system_dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();
        let desktop_id = "org.example.Viewer.desktop";
        let system_path = system_dir.path().join(desktop_id);
        let user_path = user_dir.path().join(desktop_id);
        let system_exec = "viewer --profile \"Work profile\" %F %i %c %k";
        write_desktop_file(
            system_dir.path(), desktop_id,
            &format!("[Desktop Entry]\nType=Application\nName=Viewer\nIcon=viewer\nExec={system_exec}\n"),
        );
        let dirs = [system_dir.path().to_path_buf(), user_dir.path().to_path_buf()];
        let resolver = StaticResolver(HashMap::from([("viewer", "/usr/bin/viewer")]));
        let store = Store::in_memory().unwrap();
        let app: AppId = "desktop-import.org.example.Viewer.desktop".parse().unwrap();
        let registry = Registry::default();
        import(&registry, &store, &dirs, &resolver).unwrap();
        let original = registry.launch_template(&app).unwrap();
        assert_eq!(original.exec, ExecTemplate::parse(system_exec));
        assert_eq!(original.desktop_file_path, system_path);
        let bookkeeping = appd_store::load_desktop_imports(&store).unwrap();

        let restarted = Registry::default();
        import(&restarted, &store, &dirs, &resolver).unwrap();
        let restored = restarted.launch_template(&app).unwrap();
        assert_eq!(restored.exec, original.exec);
        assert_eq!(restored.desktop_file_path, original.desktop_file_path);
        assert_eq!(appd_store::load_desktop_imports(&store).unwrap(), bookkeeping);

        let user_exec = "viewer --private %u %k";
        write_desktop_file(
            user_dir.path(), desktop_id,
            &format!("[Desktop Entry]\nType=Application\nName=Private Viewer\nExec={user_exec}\n"),
        );
        import(&restarted, &store, &dirs, &resolver).unwrap();
        let shadowed = restarted.launch_template(&app).unwrap();
        assert_eq!(shadowed.exec, ExecTemplate::parse(user_exec));
        assert_eq!(shadowed.desktop_file_path, user_path);
        let manifest = restarted.manifest(&app).unwrap();
        assert_eq!(manifest.provenance.as_ref().unwrap().source_path.as_ref(), Some(&user_path));
        let persisted = appd_store::load_desktop_imports(&store).unwrap();
        assert_eq!(persisted[0].path, shadowed.desktop_file_path);
        assert_eq!(persisted[0].app_id, manifest.id);

        write_desktop_file(
            user_dir.path(), desktop_id,
            "[Desktop Entry]\nType=Application\nName=Masked\nExec=viewer\nHidden=true\n",
        );
        import(&restarted, &store, &dirs, &resolver).unwrap();
        assert!(restarted.manifest(&app).is_none());
        assert!(restarted.launch_template(&app).is_none());

        std::fs::remove_file(&user_path).unwrap();
        import(&restarted, &store, &dirs, &resolver).unwrap();
        let unmasked = restarted.launch_template(&app).unwrap();
        assert_eq!(unmasked.exec, original.exec);
        assert_eq!(unmasked.desktop_file_path, system_path);

        std::fs::remove_file(&system_path).unwrap();
        import(&restarted, &store, &dirs, &resolver).unwrap();
        assert!(restarted.manifest(&app).is_none());
        assert!(restarted.launch_template(&app).is_none());
    }

    /// The snap export directory is scanned whether or not the environment names it, the
    /// environment only adds roots, the person's own directory comes last, and a root named
    /// twice is scanned once.
    #[test]
    fn application_dirs_always_include_the_snap_exports_and_end_with_the_persons_own() {
        let dirs = application_dirs_from(
            Some(std::ffi::OsStr::new(
                "/home/p/.nix-profile/share:/usr/share:/opt/extra/share:relative/share",
            )),
            Some(PathBuf::from("/home/p/.local/share")),
        );
        let text: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
        assert!(
            text.contains(&"/var/lib/snapd/desktop/applications".to_owned()),
            "{text:?}"
        );
        assert!(
            text.contains(&"/opt/extra/share/applications".to_owned()),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|dir| dir.starts_with("relative")),
            "{text:?}"
        );
        assert_eq!(
            text.iter()
                .filter(|dir| dir.as_str() == "/usr/share/applications")
                .count(),
            1,
            "{text:?}"
        );
        // XDG precedence: the nix profile is named first in the environment, so it shadows
        // /usr/share and is scanned after it.
        let usr = text
            .iter()
            .position(|d| d == "/usr/share/applications")
            .unwrap();
        let nix = text
            .iter()
            .position(|d| d == "/home/p/.nix-profile/share/applications")
            .unwrap();
        assert!(nix > usr, "{text:?}");
        assert_eq!(
            text.last().map(String::as_str),
            Some("/home/p/.local/share/applications")
        );

        let bare = application_dirs_from(None, None);
        assert_eq!(
            bare.last().map(|d| d.display().to_string()).as_deref(),
            Some("/usr/local/share/applications"),
            "with no environment the spec default still ranks /usr/local/share highest"
        );
        assert!(
            bare.iter()
                .any(|d| d.ends_with("snapd/desktop/applications"))
        );
    }

    use std::collections::HashMap;
    use templar_types::{AppAliases, AppManifest, ResourceClass};

    struct StaticResolver(HashMap<&'static str, &'static str>);

    struct FlatpakAwareResolver {
        programs: HashMap<&'static str, &'static str>,
        flatpaks: HashMap<&'static str, &'static str>,
    }

    impl ExecResolver for FlatpakAwareResolver {
        fn resolve_program(&self, name_or_path: &str) -> Option<PathBuf> {
            self.programs.get(name_or_path).map(PathBuf::from)
        }
        fn resolve_flatpak(&self, flatpak_id: &str) -> Option<PathBuf> {
            self.flatpaks.get(flatpak_id).map(PathBuf::from)
        }
    }

    impl ExecResolver for StaticResolver {
        fn resolve_program(&self, name_or_path: &str) -> Option<PathBuf> {
            self.0.get(name_or_path).map(PathBuf::from)
        }
        fn resolve_flatpak(&self, _flatpak_id: &str) -> Option<PathBuf> {
            None
        }
    }

    fn write_desktop_file(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn imports_new_apps_and_skips_ones_a_native_manifest_already_covers() {
        let system_dir = tempfile::tempdir().unwrap();
        write_desktop_file(
            system_dir.path(),
            "org.example.Sync.desktop",
            "[Desktop Entry]\nType=Application\nName=Sync\nExec=sync-agent\n",
        );
        write_desktop_file(
            system_dir.path(),
            "org.templaros.Viewer.desktop",
            "[Desktop Entry]\nType=Application\nName=Should Not Import\nExec=impostor-viewer\n",
        );

        let registry = Registry::default();
        registry.insert_manifest(AppManifest {
            id: "org.templaros.Viewer".parse().unwrap(),
            display_name: "Templar Viewer".into(),
            primary_desktop_file_id: "org.templaros.Viewer.desktop".into(),
            executable: "/usr/bin/templar-viewer".into(),
            aliases: AppAliases::default(),
            mime_types: std::collections::BTreeSet::default(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: None,
            categories: std::collections::BTreeSet::default(),
            keywords: std::collections::BTreeSet::default(),
            no_display: false,
            provenance: None,
            provides: Vec::new(),
        });

        let store = Store::in_memory().unwrap();
        let resolver = StaticResolver(HashMap::from([("sync-agent", "/usr/bin/sync-agent")]));
        let imported = import(
            &registry,
            &store,
            &[system_dir.path().to_path_buf()],
            &resolver,
        )
        .unwrap();

        assert_eq!(imported, 1, "only the Sync entry should be newly imported");
        assert!(
            registry
                .manifest(&"desktop-import.org.example.Sync.desktop".parse().unwrap())
                .is_some()
        );
        assert_eq!(
            registry
                .manifest(&"org.templaros.Viewer".parse().unwrap())
                .unwrap()
                .executable,
            PathBuf::from("/usr/bin/templar-viewer"),
            "the native manifest must not be overwritten by the impostor .desktop file"
        );

        let bookkeeping = appd_store::load_desktop_imports(&store).unwrap();
        assert_eq!(bookkeeping.len(), 1);
        assert_eq!(bookkeeping[0].desktop_id, "org.example.Sync.desktop");
    }

    #[test]
    fn a_later_directory_overrides_an_earlier_one_for_the_same_desktop_id() {
        let system_dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();
        write_desktop_file(
            system_dir.path(),
            "org.example.Tool.desktop",
            "[Desktop Entry]\nType=Application\nName=System Tool\nExec=system-tool\n",
        );
        write_desktop_file(
            user_dir.path(),
            "org.example.Tool.desktop",
            "[Desktop Entry]\nType=Application\nName=User Tool\nExec=user-tool\n",
        );

        let registry = Registry::default();
        let store = Store::in_memory().unwrap();
        let resolver = StaticResolver(HashMap::from([
            ("system-tool", "/usr/bin/system-tool"),
            ("user-tool", "/usr/bin/user-tool"),
        ]));
        import(
            &registry,
            &store,
            &[
                system_dir.path().to_path_buf(),
                user_dir.path().to_path_buf(),
            ],
            &resolver,
        )
        .unwrap();

        let manifest = registry
            .manifest(&"desktop-import.org.example.Tool.desktop".parse().unwrap())
            .unwrap();
        assert_eq!(manifest.display_name, "User Tool");
    }

    #[test]
    fn a_flatpak_and_a_distro_package_sharing_a_desktop_id_are_two_applications() {
        // The live bug this fixes: keyed on the desktop id alone, these two collapsed into one
        // candidate and whichever directory was scanned last erased the other from the registry.
        // A person with both installed simply lost one of them, silently.
        let system_dir = tempfile::tempdir().unwrap();
        let flatpak_dir = tempfile::tempdir()
            .unwrap()
            .path()
            .join("flatpak/exports/share/applications");
        std::fs::create_dir_all(&flatpak_dir).unwrap();
        write_desktop_file(
            system_dir.path(),
            "firefox.desktop",
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox\n",
        );
        write_desktop_file(
            &flatpak_dir,
            "firefox.desktop",
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=/usr/bin/flatpak run org.mozilla.firefox %u\nX-Flatpak=org.mozilla.firefox\n",
        );

        let registry = Registry::default();
        let store = Store::in_memory().unwrap();
        let resolver = FlatpakAwareResolver {
            programs: HashMap::from([
                ("firefox", "/usr/bin/firefox"),
                ("/usr/bin/flatpak", "/usr/bin/flatpak"),
            ]),
            flatpaks: HashMap::from([(
                "org.mozilla.firefox",
                "/var/lib/flatpak/exports/bin/org.mozilla.firefox",
            )]),
        };
        let imported = import(
            &registry,
            &store,
            &[system_dir.path().to_path_buf(), flatpak_dir.clone()],
            &resolver,
        )
        .unwrap();

        assert_eq!(imported, 2, "both copies must import");
        let distro = registry
            .manifest(&"desktop-import.firefox.desktop".parse().unwrap())
            .expect("the distribution package survives the Flatpak being installed");
        assert_eq!(distro.executable, PathBuf::from("/usr/bin/firefox"));
        let flatpak = registry
            .manifest(&"flatpak.org.mozilla.firefox".parse().unwrap())
            .expect("the Flatpak gets an id of its own");
        assert_eq!(
            flatpak.executable,
            PathBuf::from("/usr/bin/flatpak")
        );
        assert!(flatpak.aliases.executable_paths.contains(
            &PathBuf::from("/var/lib/flatpak/exports/bin/org.mozilla.firefox")
        ));
        let launch = registry.launch_template(&flatpak.id).unwrap();
        assert_eq!(
            launch.exec,
            ExecTemplate::parse("/usr/bin/flatpak run org.mozilla.firefox %u")
        );
        assert_eq!(launch.desktop_file_path, flatpak_dir.join("firefox.desktop"));
        assert_eq!(
            flatpak.provenance.as_ref().unwrap().origin,
            templar_types::AppOriginKind::Flatpak
        );
    }

    #[test]
    fn grouping_persists_and_the_moved_flatpak_id_leaves_an_alias_behind() {
        let system_dir = tempfile::tempdir().unwrap();
        let flatpak_root = tempfile::tempdir().unwrap();
        let flatpak_dir = flatpak_root
            .path()
            .join("flatpak/exports/share/applications");
        std::fs::create_dir_all(&flatpak_dir).unwrap();
        write_desktop_file(
            system_dir.path(),
            "firefox.desktop",
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox\nStartupWMClass=firefox\n",
        );
        write_desktop_file(
            &flatpak_dir,
            "firefox.desktop",
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=/usr/bin/flatpak run org.mozilla.firefox %u\nStartupWMClass=Firefox\nX-Flatpak=org.mozilla.firefox\n",
        );

        let registry = Registry::default();
        let store = Store::in_memory().unwrap();
        let resolver = FlatpakAwareResolver {
            programs: HashMap::from([
                ("firefox", "/usr/bin/firefox"),
                ("/usr/bin/flatpak", "/usr/bin/flatpak"),
            ]),
            flatpaks: HashMap::from([(
                "org.mozilla.firefox",
                "/var/lib/flatpak/exports/bin/org.mozilla.firefox",
            )]),
        };
        import(
            &registry,
            &store,
            &[system_dir.path().to_path_buf(), flatpak_dir.clone()],
            &resolver,
        )
        .unwrap();
        let rows = regroup_and_persist(&registry, &store).unwrap();
        assert_eq!(rows, 2, "two variants were recorded");

        // One Firefox, two copies of it.
        let index = registry.logical_index();
        assert_eq!(index.logical_apps().count(), 1);
        let distro: templar_types::AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let flatpak: templar_types::AppId = "flatpak.org.mozilla.firefox".parse().unwrap();
        assert_eq!(registry.logical_of(&distro), registry.logical_of(&flatpak));

        // The Flatpak's old id is recorded as an alias, but the distribution's package is
        // *installed* under exactly that id — so the live copy answers for it, and the alias must
        // not shadow it.
        let aliases = appd_store::load_variant_aliases(&store).unwrap();
        assert_eq!(
            aliases.get(&distro),
            Some(&flatpak),
            "the id the Flatpak used to have is recorded"
        );
        assert_eq!(
            registry.resolve_variant(&distro),
            distro,
            "an installed variant always answers for its own id"
        );

        // A second run over an unchanged system changes nothing.
        let before = index.rows();
        regroup_and_persist(&registry, &store).unwrap();
        assert_eq!(registry.logical_index().rows(), before);
    }

    #[test]
    fn a_user_file_taking_over_a_packages_desktop_id_loses_the_packages_rights() {
        // The live hole: provider declarations and autostart approvals key on the variant id, and
        // the id survives the file underneath it being replaced. Drop a `.desktop` file into your
        // own applications directory carrying a packaged application's desktop id, and it
        // inherited that package's system role and its permission to start at login.
        let resolver = StaticResolver(HashMap::from([
            ("sync-agent", "/usr/bin/sync-agent"),
            ("impostor", "/usr/bin/impostor"),
        ]));
        let derive = |exec: &str, directory: &str| {
            let text = format!("[Desktop Entry]\nType=Application\nName=Sync\nExec={exec}\n");
            let entry = desktop_entry::parse(&text).unwrap();
            desktop_entry::derive_manifest(
                &entry,
                "sync.desktop",
                &SourceContext::for_file(
                    std::path::Path::new(directory),
                    std::path::PathBuf::from(format!("{directory}/sync.desktop")),
                ),
                &resolver,
            )
            .unwrap()
            .manifest
        };

        let packaged = derive("sync-agent", "/usr/share/applications");
        let mine = derive("impostor", "/home/person/.local/share/applications");
        let variant: AppId = "desktop-import.sync.desktop".parse().unwrap();
        assert_eq!(packaged.id, variant);
        assert_eq!(
            mine.id, variant,
            "the id does not change — that is exactly why the grants leaked"
        );
        assert_eq!(packaged.trust(), TrustTier::System);
        assert_eq!(mine.trust(), TrustTier::User);

        // The packaged copy is recorded, and earns a role and an autostart approval.
        let store = Store::in_memory().unwrap();
        store
            .transaction(|tx| {
                appd_store::save_variant_within(
                    tx,
                    &variant,
                    &"logical.sync".parse().unwrap(),
                    templar_appd::LogicalSource::Derived,
                    packaged.provenance.as_ref().unwrap(),
                )?;
                providers::declare_within(
                    tx,
                    &variant,
                    &templar_types::ProviderCapability {
                        role: templar_types::ProviderRole::NotificationServer,
                        priority: 100,
                    },
                )?;
                autostart::set_approval_within(tx, &variant, autostart::Approval::Approved)
            })
            .unwrap();
        assert_eq!(
            appd_store::variant_trust_within(&store.lock(), &variant).unwrap(),
            Some(TrustTier::System)
        );

        store.transaction(|tx| invalidate_shadowed_grants(tx, &variant, &mine.display_name)).unwrap();

        let declarations: i64 = store
            .lock()
            .query_row("SELECT COUNT(*) FROM provider_declarations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(declarations, 0, "the role the package earned is withdrawn");
        // Storing Undecided deletes the row: the absence of a row *is* undecided, so the person
        // is asked again rather than the old yes being kept.
        assert_eq!(
            autostart::approval_for(&autostart::load_approvals(&store).unwrap(), &variant),
            autostart::Approval::Undecided,
            "and so is the permission to start at login"
        );
        let explained: i64 = store
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM history WHERE cause = 'provenance-moved-to-user'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(explained, 1, "and the person is told why");
    }
}
