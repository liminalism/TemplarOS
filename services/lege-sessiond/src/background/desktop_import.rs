//! Import [`AppManifest`](lege_types::AppManifest)s from installed `.desktop` files for
//! applications with no native manifest (`docs/architecture/identity.md`'s "follow-up").
//! A real manifest always wins: a `.desktop` file whose merged ID any already-loaded manifest
//! already claims (as its `primary_desktop_file_id` or in `aliases.desktop_file_ids`) is
//! skipped entirely, so this can only ever fill a gap.

use anyhow::{Context, Result};
use lege_appd::{
    Registry,
    desktop_entry::{self, ExecResolver},
    store as appd_store,
};
use lege_state::Store;
use lege_types::AppCatalog;
use std::{collections::BTreeSet, path::PathBuf};

/// Every directory that may hold `.desktop` application entries, later ones taking priority
/// over earlier ones for the same desktop ID (the usual XDG precedence: user over system).
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

    // Later directories override earlier ones for the same desktop ID.
    let mut candidates: std::collections::BTreeMap<String, PathBuf> =
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
            candidates.insert(desktop_id, path);
        }
    }

    let mut imported = 0;
    for (desktop_id, path) in candidates {
        if native_ids.contains(&desktop_id) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let mtime_ms = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_millis() as i64);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(entry) = desktop_entry::parse(&text) else {
            continue;
        };
        let Some(derived) = desktop_entry::derive_manifest(&entry, &desktop_id, resolver) else {
            continue;
        };

        registry.insert_manifest(derived.manifest.clone());
        store
            .transaction(|tx| {
                appd_store::record_desktop_import_within(
                    tx,
                    &desktop_id,
                    &path,
                    mtime_ms,
                    &derived.manifest.id,
                )
            })
            .with_context(|| format!("recording desktop_import bookkeeping for {desktop_id}"))?;
        imported += 1;
    }
    Ok(imported)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::{AppAliases, AppManifest, ResourceClass};
    use std::collections::HashMap;

    struct StaticResolver(HashMap<&'static str, &'static str>);

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
}
