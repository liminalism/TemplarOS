//! Loading manifests from the system and user directories.
//!
//! Search order (later wins): `/usr/share/templaros/apps/*.json`, `/etc/templaros/apps/*.json`,
//! `$XDG_DATA_HOME/templaros/apps/*.json`. Desktop-file import derives manifests from
//! `.desktop` entries for applications without a native manifest.

use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};
use templar_types::{AppManifest, AppOriginKind, AppProvenance, LaunchMechanism};

/// Default system manifest directory.
pub const SYSTEM_DIR: &str = "/usr/share/templaros/apps";
/// Administrator overrides.
pub const ETC_DIR: &str = "/etc/templaros/apps";

/// Load every `*.json` manifest under `dir`. Missing directories yield an empty list.
/// Source location, never the JSON's declared provenance, establishes native-manifest trust.
/// Unknown override directories remain untrusted.
pub fn load_dir(dir: &Path) -> Result<Vec<AppManifest>> {
    let mut manifests = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(manifests),
        Err(error) => return Err(error).with_context(|| format!("reading {}", dir.display())),
    };

    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    for path in paths {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut manifest: AppManifest =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let source = fs::canonicalize(&path)
            .with_context(|| format!("resolving manifest source {}", path.display()))?;
        assign_provenance(&mut manifest, source);
        manifests.push(manifest);
    }

    Ok(manifests)
}

fn assign_provenance(manifest: &mut AppManifest, source: PathBuf) {
    let directory = source.parent();
    let user_directory = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|root| root.join("templaros/apps"));
    let origin =
        if directory == Some(Path::new(SYSTEM_DIR)) || directory == Some(Path::new(ETC_DIR)) {
            AppOriginKind::NativeManifest
        } else if directory == user_directory.as_deref()
            || source.starts_with("/home")
            || source.starts_with("/root")
        {
            AppOriginKind::NativeUserManifest
        } else {
            AppOriginKind::Unknown
        };
    let mut provenance = manifest.provenance.take().unwrap_or_default();
    let authority = AppProvenance::from_origin(origin);
    provenance.origin = authority.origin;
    provenance.trust = authority.trust;
    provenance.update_owner = authority.update_owner;
    provenance.source_path = Some(source);
    provenance.desktop_entry_id = Some(manifest.primary_desktop_file_id.clone());
    provenance.executable = Some(manifest.executable.clone());
    provenance.launch = LaunchMechanism::Direct;
    provenance.sandboxed = false;
    manifest.provenance = Some(provenance);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_json_manifests_and_ignores_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("org.templaros.Viewer.json"),
            r#"{"id":"org.templaros.Viewer","display_name":"Templar Viewer",
                "primary_desktop_file_id":"org.templaros.Viewer.desktop",
                "executable":"/usr/bin/templar-viewer","mime_types":["application/pdf"],
                "resource_class":"Interactive"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let loaded = load_dir(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id.as_str(), "org.templaros.Viewer");
        let provenance = loaded[0].provenance.as_ref().unwrap();
        assert_eq!(provenance.origin, AppOriginKind::Unknown);
        assert_eq!(
            provenance.source_path.as_deref(),
            Some(
                fs::canonicalize(dir.path().join("org.templaros.Viewer.json"))
                    .unwrap()
                    .as_path()
            )
        );
        assert!(!loaded[0].trust().may_hold_exclusive_role());
        assert!(load_dir(&dir.path().join("missing")).unwrap().is_empty());
    }

    #[test]
    fn native_manifest_authority_is_derived_from_source_not_self_reported() {
        let original: AppManifest = serde_json::from_str(
            r#"{"id":"org.example.Terminal","display_name":"Terminal",
                "primary_desktop_file_id":"org.example.Terminal.desktop",
                "executable":"/usr/bin/kitty","resource_class":"Interactive",
                "provides":[{"role":"terminal","priority":100}],
                "provenance":{"origin":"flatpak","trust":"system",
                    "source_path":"/usr/share/forged.desktop","provider_claims":["terminal"]}}"#,
        )
        .unwrap();
        for (path, origin) in [
            (
                "/usr/share/templaros/apps/terminal.json",
                AppOriginKind::NativeManifest,
            ),
            (
                "/etc/templaros/apps/terminal.json",
                AppOriginKind::NativeManifest,
            ),
            (
                "/home/person/.local/share/templaros/apps/terminal.json",
                AppOriginKind::NativeUserManifest,
            ),
            (
                "/opt/payload/manifests/terminal.json",
                AppOriginKind::Unknown,
            ),
            (
                "/usr/share/templaros/apps-untrusted/terminal.json",
                AppOriginKind::Unknown,
            ),
        ] {
            let mut manifest = original.clone();
            assign_provenance(&mut manifest, PathBuf::from(path));
            let provenance = manifest.provenance.as_ref().unwrap();
            assert_eq!(provenance.origin, origin);
            assert_eq!(provenance.trust, origin.trust());
            assert_eq!(provenance.update_owner, origin.update_owner());
            assert_eq!(provenance.source_path.as_deref(), Some(Path::new(path)));
            assert_eq!(provenance.executable.as_ref(), Some(&manifest.executable));
            assert_eq!(provenance.launch, LaunchMechanism::Direct);
            assert_eq!(
                manifest.trust().may_hold_exclusive_role(),
                origin == AppOriginKind::NativeManifest
            );
            assert_eq!(manifest.provides, original.provides);
            assert_eq!(
                provenance.provider_claims,
                original.provenance.as_ref().unwrap().provider_claims
            );
        }
    }

    #[test]
    fn a_symlink_does_not_supply_a_forged_native_source() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::write(
            &source,
            r#"{"id":"org.example.Terminal","display_name":"Terminal",
                "primary_desktop_file_id":"terminal.desktop",
                "executable":"/usr/bin/kitty","resource_class":"Interactive",
                "provenance":{"origin":"native-manifest","trust":"system"}}"#,
        )
        .unwrap();
        symlink(&source, directory.path().join("terminal.json")).unwrap();
        let loaded = load_dir(directory.path()).unwrap();
        assert_eq!(
            loaded[0]
                .provenance
                .as_ref()
                .unwrap()
                .source_path
                .as_deref(),
            Some(fs::canonicalize(source).unwrap().as_path())
        );
        assert!(!loaded[0].trust().may_hold_exclusive_role());
    }
}
