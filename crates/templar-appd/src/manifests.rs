//! Loading manifests from the system and user directories.
//!
//! Search order (later wins): `/usr/share/templaros/apps/*.json`, `/etc/templaros/apps/*.json`,
//! `$XDG_DATA_HOME/templaros/apps/*.json`. Desktop-file import (deriving manifests from
//! `.desktop` entries for applications without a native manifest) is a follow-up.

use anyhow::{Context, Result};
use std::{fs, path::Path};
use templar_types::AppManifest;

/// Default system manifest directory.
pub const SYSTEM_DIR: &str = "/usr/share/templaros/apps";
/// Administrator overrides.
pub const ETC_DIR: &str = "/etc/templaros/apps";

/// Load every `*.json` manifest under `dir`. Missing directories yield an empty list.
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
        let manifest: AppManifest =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        manifests.push(manifest);
    }

    Ok(manifests)
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
        assert!(load_dir(&dir.path().join("missing")).unwrap().is_empty());
    }
}
