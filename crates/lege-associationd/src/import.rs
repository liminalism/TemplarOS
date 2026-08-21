//! One-time import of an existing `mimeapps.list` into the typed association database
//! (`docs/architecture/associations.md`: "`mimeapps.list` is *generated* by the broker — import
//! once, then own it"). After the first run, `lege-associationd` is the only writer; this module
//! is never consulted again.

use lege_types::{AppCatalog, AppId};
use std::collections::BTreeMap;

/// Parse the `[Default Applications]` section of a `mimeapps.list`-format file. Multiple
/// semicolon-separated candidates per key are legal; the first (highest-priority) one is used.
pub fn parse_default_applications(text: &str) -> BTreeMap<String, String> {
    let mut in_section = false;
    let mut defaults = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            in_section = name == "Default Applications";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((mime, value)) = line.split_once('=') else {
            continue;
        };
        let Some(first) = value
            .split(';')
            .map(str::trim)
            .find(|part| !part.is_empty())
        else {
            continue;
        };
        defaults.insert(mime.trim().to_owned(), first.to_owned());
    }
    defaults
}

/// Resolve each imported `mime -> desktop-file-id` pair to a canonical [`AppId`] via the
/// catalog. A desktop ID that matches no installed manifest (native or desktop-entry-derived) is
/// dropped rather than guessed at — an unresolvable default is not worth carrying forward.
pub fn resolve_against_catalog(
    defaults: &BTreeMap<String, String>,
    catalog: &dyn AppCatalog,
) -> Vec<(String, AppId)> {
    let mut by_desktop_id: BTreeMap<String, AppId> = BTreeMap::new();
    for manifest in catalog.manifests() {
        by_desktop_id
            .entry(manifest.primary_desktop_file_id.clone())
            .or_insert_with(|| manifest.id.clone());
        for alias in &manifest.aliases.desktop_file_ids {
            by_desktop_id
                .entry(alias.clone())
                .or_insert_with(|| manifest.id.clone());
        }
    }

    defaults
        .iter()
        .filter_map(|(mime, desktop_id)| {
            by_desktop_id
                .get(desktop_id)
                .map(|app_id| (mime.clone(), app_id.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::StaticCatalog;

    const SAMPLE: &str = "\
[Default Applications]
application/pdf=org.legeos.Viewer.desktop;
text/html=firefox.desktop;
# a comment
image/png=unknown-app.desktop;

[Added Associations]
application/pdf=org.legeos.Viewer.desktop;
";

    #[test]
    fn parses_only_the_default_applications_section() {
        let defaults = parse_default_applications(SAMPLE);
        assert_eq!(defaults.len(), 3);
        assert_eq!(
            defaults.get("application/pdf").map(String::as_str),
            Some("org.legeos.Viewer.desktop")
        );
        assert_eq!(
            defaults.get("text/html").map(String::as_str),
            Some("firefox.desktop")
        );
    }

    #[test]
    fn unresolvable_desktop_ids_are_dropped_not_guessed() {
        let catalog = StaticCatalog::with(&[("org.legeos.Viewer", &["application/pdf"])]);
        let defaults = parse_default_applications(SAMPLE);
        let resolved = resolve_against_catalog(&defaults, &*catalog);
        assert_eq!(
            resolved,
            vec![(
                "application/pdf".to_owned(),
                "org.legeos.Viewer".parse().unwrap()
            )]
        );
    }
}
