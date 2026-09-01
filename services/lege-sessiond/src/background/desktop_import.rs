//! Import [`AppManifest`](lege_types::AppManifest)s from installed `.desktop` files for
//! applications with no native manifest (`docs/architecture/identity.md`'s "follow-up").
//! A real manifest always wins: a `.desktop` file whose merged ID any already-loaded manifest
//! already claims (as its `primary_desktop_file_id` or in `aliases.desktop_file_ids`) is
//! skipped entirely, so this can only ever fill a gap.

use anyhow::{Context, Result};
use lege_appd::{
    Registry,
    desktop_entry::{self, ExecResolver, OriginNamespace, SourceContext},
    store as appd_store,
};
use lege_session_core::{
    autostart, history,
    history::{Event, EventId, EventKind},
    providers,
};
use lege_state::Store;
use lege_types::{AppCatalog, AppId, TrustTier};
use std::{collections::BTreeSet, path::PathBuf};

/// Every directory that may hold `.desktop` application entries. Within the XDG chain later ones
/// take priority over earlier ones for the same desktop ID (the usual precedence: user over
/// system); the Flatpak export trees are a separate namespace and neither shadow it nor are
/// shadowed by it.
pub fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/var/lib/flatpak/exports/share/applications"),
    ];
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    {
        dirs.push(data_home.join("flatpak/exports/share/applications"));
        dirs.push(data_home.join("applications"));
    }
    dirs
}

/// Import manifests for every importable `.desktop` file under `dirs` that no manifest already
/// loaded into `registry` claims. Returns how many were imported.
pub fn import(
    registry: &Registry,
    store: &Store,
    dirs: &[PathBuf],
    resolver: &dyn ExecResolver,
) -> Result<usize> {
    let native_ids: BTreeSet<String> = registry
        .manifests()
        .into_iter()
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
            let source = SourceContext::for_file(dir, path);
            candidates.insert((source.namespace, desktop_id), source);
        }
    }

    let mut imported = 0;
    for ((_namespace, desktop_id), source) in candidates {
        if native_ids.contains(&desktop_id) {
            continue;
        }
        if import_one(registry, store, &desktop_id, &source, resolver)? {
            imported += 1;
        }
    }
    Ok(imported)
}

/// Import one candidate. `Ok(false)` when the file turned out not to be an importable application
/// — a directory full of `.desktop` files is full of entries that are not.
fn import_one(
    registry: &Registry,
    store: &Store,
    desktop_id: &str,
    source: &SourceContext,
    resolver: &dyn ExecResolver,
) -> Result<bool> {
    let path = source.path.clone();
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Ok(false);
    };
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_millis() as i64);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Some(entry) = desktop_entry::parse(&text) else {
        return Ok(false);
    };
    let Some(derived) = desktop_entry::derive_manifest(&entry, desktop_id, source, resolver) else {
        return Ok(false);
    };

    let mut manifest = derived.manifest;
    let origin = manifest
        .provenance
        .as_ref()
        .map_or(lege_types::AppOriginKind::Unknown, |p| p.origin);
    // A variant seen before keeps the moment it was first seen: it is what makes "the copy that has
    // been here longest" a real answer rather than "whichever was scanned first".
    if let Some(first_seen) = appd_store::variant_first_seen(store, &manifest.id)
        && let Some(provenance) = manifest.provenance.as_mut()
    {
        provenance.first_seen_ms = first_seen;
    }

    // A Flatpak export used to be imported as `desktop-import.<desktop id>`. Its id has moved, so
    // record where the old one now points. Nothing stored elsewhere is rewritten: a MIME default or
    // a project holding the old id resolves through this alias on read, and a rolled-back binary
    // reads exactly the rows it wrote.
    if origin == lege_types::AppOriginKind::Flatpak {
        let previous = desktop_entry::derive_app_id(desktop_id);
        if previous != manifest.id {
            let current = manifest.id.clone();
            store
                .transaction(|tx| {
                    appd_store::save_variant_alias_within(
                        tx,
                        &previous,
                        &current,
                        "flatpak-export-renamed",
                    )
                })
                .with_context(|| format!("recording the variant alias for {desktop_id}"))?;
        }
    }

    // The hole this closes: a `.desktop` file the account can rewrite, dropped on top of a desktop
    // id a package used to own, inherited that package's provider declarations and autostart
    // approval — the grants were keyed on the id, and the id had not changed. They are dropped
    // instead, and the person is told.
    if appd_store::variant_trust(store, &manifest.id) == Some(TrustTier::System)
        && manifest.trust() == TrustTier::User
    {
        invalidate_shadowed_grants(store, &manifest.id, &manifest.display_name);
    }

    registry.insert_manifest(manifest.clone());
    store
        .transaction(|tx| {
            appd_store::record_desktop_import_within(
                tx,
                desktop_id,
                &path,
                mtime_ms,
                &manifest.id,
                origin,
            )
        })
        .with_context(|| format!("recording desktop_import bookkeeping for {desktop_id}"))?;
    Ok(true)
}

/// Drop the rights a variant earned while the system owned it, now that it does not.
///
/// Provider declarations and autostart approvals both key on the variant id, and the id survives
/// the file underneath it being replaced. Trust does not: a copy that used to come from a package
/// and now comes from a file this account can rewrite is, for these purposes, a different
/// application wearing the same name.
fn invalidate_shadowed_grants(store: &Store, variant: &AppId, display_name: &str) {
    let result = store.transaction(|tx| {
        providers::undeclare_within(tx, variant)?;
        autostart::set_approval_within(tx, variant, autostart::Approval::Undecided)?;
        history::append_within(
            tx,
            &Event {
                id: EventId::new(),
                at_ms: lege_state::now_ms(),
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
    });
    if let Err(error) = result {
        tracing::warn!(app = %variant, %error, "failed to withdraw grants after a provenance move");
    }
}

/// Group everything installed into logical applications and persist the result, then load the
/// alias table onto the registry.
///
/// Run after every scan that could change the installed set. Grouping is presentation, so a
/// failure here is logged and the session continues with whatever grouping it had.
pub fn regroup_and_persist(registry: &Registry, store: &Store) -> Result<usize> {
    let persisted = appd_store::load_variants(store).context("loading recorded app variants")?;
    let index = registry.regroup(&persisted);

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

    let aliases = appd_store::load_variant_aliases(store).context("loading variant aliases")?;
    registry.set_variant_aliases(aliases);
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppAliases, AppManifest, ResourceClass};
    use std::collections::HashMap;

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
            "org.legeos.Viewer.desktop",
            "[Desktop Entry]\nType=Application\nName=Should Not Import\nExec=impostor-viewer\n",
        );

        let registry = Registry::default();
        registry.insert_manifest(AppManifest {
            id: "org.legeos.Viewer".parse().unwrap(),
            display_name: "Lege Viewer".into(),
            primary_desktop_file_id: "org.legeos.Viewer.desktop".into(),
            executable: "/usr/bin/lege-viewer".into(),
            aliases: AppAliases::default(),
            mime_types: std::collections::BTreeSet::default(),
            resource_class: ResourceClass::Interactive,
            supports_checkpoint: false,
            icon: None,
            categories: std::collections::BTreeSet::default(),
            keywords: std::collections::BTreeSet::default(),
            no_display: false,
            provenance: None,
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
                .manifest(&"org.legeos.Viewer".parse().unwrap())
                .unwrap()
                .executable,
            PathBuf::from("/usr/bin/lege-viewer"),
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
            programs: HashMap::from([("firefox", "/usr/bin/firefox")]),
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
            PathBuf::from("/var/lib/flatpak/exports/bin/org.mozilla.firefox")
        );
        assert_eq!(
            flatpak.provenance.as_ref().unwrap().origin,
            lege_types::AppOriginKind::Flatpak
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
            programs: HashMap::from([("firefox", "/usr/bin/firefox")]),
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
        let distro: lege_types::AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let flatpak: lege_types::AppId = "flatpak.org.mozilla.firefox".parse().unwrap();
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
                    lege_appd::LogicalSource::Derived,
                    packaged.provenance.as_ref().unwrap(),
                )?;
                providers::declare_within(
                    tx,
                    &variant,
                    &lege_types::ProviderCapability {
                        role: lege_types::ProviderRole::NotificationServer,
                        priority: 100,
                    },
                )?;
                autostart::set_approval_within(tx, &variant, autostart::Approval::Approved)
            })
            .unwrap();
        assert_eq!(
            appd_store::variant_trust(&store, &variant),
            Some(TrustTier::System)
        );

        invalidate_shadowed_grants(&store, &variant, &mine.display_name);

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
