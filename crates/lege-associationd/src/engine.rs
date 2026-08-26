//! Association rule engine (pure; no I/O).

use anyhow::{Result, bail};
use lege_types::{AppCatalog, AppId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, sync::Arc};

/// An explicit per-extension rule.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionRule {
    pub app_id: AppId,

    /// False by default. When false, the extension rule is only used if content detection
    /// returned a generic type such as `application/octet-stream`.
    #[serde(default)]
    pub override_specific_mime: bool,
}

/// The typed association database (persisted by `lege-state`; exported to `mimeapps.list`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssociationDb {
    pub mime_defaults: BTreeMap<String, AppId>,
    pub extension_defaults: BTreeMap<String, ExtensionRule>,

    /// project_id → MIME type → application
    pub project_mime_defaults: BTreeMap<String, BTreeMap<String, AppId>>,
}

/// Why a handler was (or was not) selected — always reportable to the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionReason {
    ProjectMimeDefault,
    ExplicitExtensionOverride,
    GlobalMimeDefault,
    GenericExtensionFallback,
    AskUser,
}

impl DecisionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectMimeDefault => "project-mime-default",
            Self::ExplicitExtensionOverride => "explicit-extension-override",
            Self::GlobalMimeDefault => "global-mime-default",
            Self::GenericExtensionFallback => "generic-extension-fallback",
            Self::AskUser => "ask-user",
        }
    }
}

/// The engine's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenDecision {
    pub app_id: Option<AppId>,
    pub detected_mime: String,
    pub reason: DecisionReason,
}

/// Rule engine over an [`AssociationDb`] and the application catalog.
pub struct AssociationEngine {
    db: AssociationDb,
    catalog: Arc<dyn AppCatalog>,
}

impl std::fmt::Debug for AssociationEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssociationEngine")
            .field("db", &self.db)
            .finish_non_exhaustive()
    }
}

impl AssociationEngine {
    pub fn new(db: AssociationDb, catalog: Arc<dyn AppCatalog>) -> Self {
        Self { db, catalog }
    }

    pub fn database(&self) -> &AssociationDb {
        &self.db
    }

    /// Replace the whole database at once — used after `History1.Undo` reverses a persisted
    /// change, to bring the in-memory engine back in sync with `lege-state` without needing a
    /// bespoke in-memory inverse for every kind of change.
    pub fn replace_database(&mut self, db: AssociationDb) {
        self.db = db;
    }

    /// Resolve the handler for `path` whose content was detected as `detected_mime`.
    pub fn resolve(
        &self,
        path: &Path,
        detected_mime: &str,
        project_id: Option<&str>,
    ) -> OpenDecision {
        if let Some(project_id) = project_id
            && let Some(app_id) = self
                .db
                .project_mime_defaults
                .get(project_id)
                .and_then(|rules| rules.get(detected_mime))
            && self.app_supports(app_id, detected_mime)
        {
            return decision(
                app_id.clone(),
                detected_mime,
                DecisionReason::ProjectMimeDefault,
            );
        }

        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);

        let extension_rule = extension
            .as_ref()
            .and_then(|extension| self.db.extension_defaults.get(extension));

        if let Some(rule) = extension_rule
            && rule.override_specific_mime
            && self.app_exists(&rule.app_id)
        {
            return decision(
                rule.app_id.clone(),
                detected_mime,
                DecisionReason::ExplicitExtensionOverride,
            );
        }

        if let Some(app_id) = self.db.mime_defaults.get(detected_mime)
            && self.app_supports(app_id, detected_mime)
        {
            return decision(
                app_id.clone(),
                detected_mime,
                DecisionReason::GlobalMimeDefault,
            );
        }

        let generic_type = matches!(detected_mime, "application/octet-stream" | "text/plain");
        if generic_type
            && let Some(rule) = extension_rule
            && self.app_exists(&rule.app_id)
        {
            return decision(
                rule.app_id.clone(),
                detected_mime,
                DecisionReason::GenericExtensionFallback,
            );
        }

        OpenDecision {
            app_id: None,
            detected_mime: detected_mime.to_owned(),
            reason: DecisionReason::AskUser,
        }
    }

    /// Set the global default for a MIME type. The application must declare support for it.
    pub fn set_mime_default(&mut self, mime: &str, app_id: AppId) -> Result<()> {
        self.validate_mime_default(mime, &app_id)?;
        self.apply_mime_default(mime, app_id);
        Ok(())
    }

    /// Check whether `app_id` may become `mime`'s default, without mutating anything. Split out
    /// from [`Self::set_mime_default`] so a D-Bus-backed caller can validate, persist to
    /// `lege-state`, and only then apply in-memory — never the reverse order, which could leave
    /// the in-memory engine ahead of a failed database write.
    pub fn validate_mime_default(&self, mime: &str, app_id: &AppId) -> Result<()> {
        validate_mime(mime)?;
        if !self.app_supports(app_id, mime) {
            bail!("{app_id} is not registered as a handler for {mime}");
        }
        Ok(())
    }

    /// Apply an already-validated (and, in production, already-persisted) MIME default.
    pub fn apply_mime_default(&mut self, mime: &str, app_id: AppId) {
        self.db.mime_defaults.insert(mime.to_owned(), app_id);
    }

    /// Set (or override) the rule for a file extension.
    pub fn set_extension_default(&mut self, extension: &str, rule: ExtensionRule) -> Result<()> {
        let extension = normalize_extension(extension)?;
        self.validate_extension_default(&rule)?;
        self.apply_extension_default(extension, rule);
        Ok(())
    }

    /// See [`Self::validate_mime_default`].
    pub fn validate_extension_default(&self, rule: &ExtensionRule) -> Result<()> {
        if !self.app_exists(&rule.app_id) {
            bail!("unknown application {}", rule.app_id);
        }
        Ok(())
    }

    /// Apply an already-validated extension rule. `extension` must already be normalized
    /// ([`normalize_extension`]).
    pub fn apply_extension_default(&mut self, extension: String, rule: ExtensionRule) {
        self.db.extension_defaults.insert(extension, rule);
    }

    /// Set the default for a MIME type inside one project only.
    pub fn set_project_mime_default(
        &mut self,
        project_id: &str,
        mime: &str,
        app_id: AppId,
    ) -> Result<()> {
        self.validate_project_mime_default(project_id, mime, &app_id)?;
        self.apply_project_mime_default(project_id, mime, app_id);
        Ok(())
    }

    /// See [`Self::validate_mime_default`].
    pub fn validate_project_mime_default(
        &self,
        project_id: &str,
        mime: &str,
        app_id: &AppId,
    ) -> Result<()> {
        validate_mime(mime)?;
        if project_id.trim().is_empty() {
            bail!("project id must not be empty");
        }
        if !self.app_supports(app_id, mime) {
            bail!("{app_id} is not registered as a handler for {mime}");
        }
        Ok(())
    }

    /// Apply an already-validated project-scoped default.
    pub fn apply_project_mime_default(&mut self, project_id: &str, mime: &str, app_id: AppId) {
        self.db
            .project_mime_defaults
            .entry(project_id.to_owned())
            .or_default()
            .insert(mime.to_owned(), app_id);
    }

    /// Resolve the handler for a URI scheme (`x-scheme-handler/<scheme>`, per
    /// `docs/protocols/launch-and-activation.md`'s `ResolveUri`). There is no extension to fall
    /// back to, so the order is simply project default, then global default, then ask.
    pub fn resolve_scheme(&self, scheme: &str, project_id: Option<&str>) -> OpenDecision {
        let mime = format!("x-scheme-handler/{scheme}");

        if let Some(project_id) = project_id
            && let Some(app_id) = self
                .db
                .project_mime_defaults
                .get(project_id)
                .and_then(|rules| rules.get(&mime))
            && self.app_supports(app_id, &mime)
        {
            return decision(app_id.clone(), &mime, DecisionReason::ProjectMimeDefault);
        }

        if let Some(app_id) = self.db.mime_defaults.get(&mime)
            && self.app_supports(app_id, &mime)
        {
            return decision(app_id.clone(), &mime, DecisionReason::GlobalMimeDefault);
        }

        OpenDecision {
            app_id: None,
            detected_mime: mime,
            reason: DecisionReason::AskUser,
        }
    }

    /// Remove the global default for `mime`, if any; returns what was removed.
    pub fn forget_mime_default(&mut self, mime: &str) -> Option<AppId> {
        self.db.mime_defaults.remove(mime)
    }

    /// Remove the rule for `extension`, if any; returns what was removed.
    pub fn forget_extension_default(&mut self, extension: &str) -> Option<ExtensionRule> {
        let extension = normalize_extension(extension).ok()?;
        self.db.extension_defaults.remove(&extension)
    }

    /// Remove one project's default for `mime`, if any; returns what was removed.
    pub fn forget_project_mime_default(&mut self, project_id: &str, mime: &str) -> Option<AppId> {
        self.db
            .project_mime_defaults
            .get_mut(project_id)?
            .remove(mime)
    }

    /// Drop defaults that point at applications which are no longer installed; returns them.
    pub fn prune_stale(&mut self) -> Vec<(String, AppId)> {
        let mut removed = Vec::new();
        self.db.mime_defaults.retain(|mime, app_id| {
            let keep = self.catalog.manifest(app_id).is_some();
            if !keep {
                removed.push((mime.clone(), app_id.clone()));
            }
            keep
        });
        self.db.extension_defaults.retain(|extension, rule| {
            let keep = self.catalog.manifest(&rule.app_id).is_some();
            if !keep {
                removed.push((format!(".{extension}"), rule.app_id.clone()));
            }
            keep
        });
        removed
    }

    fn app_exists(&self, app_id: &AppId) -> bool {
        self.catalog.manifest(app_id).is_some()
    }

    fn app_supports(&self, app_id: &AppId, mime: &str) -> bool {
        self.catalog
            .manifest(app_id)
            .is_some_and(|manifest| manifest.supports_mime(mime))
    }
}

fn decision(app_id: AppId, detected_mime: &str, reason: DecisionReason) -> OpenDecision {
    OpenDecision {
        app_id: Some(app_id),
        detected_mime: detected_mime.to_owned(),
        reason,
    }
}

pub(crate) fn validate_mime(mime: &str) -> Result<()> {
    let valid = mime.contains('/')
        && !mime.contains(['\n', '\r', ';', '='])
        && mime.bytes().all(|byte| byte.is_ascii_graphic());
    if !valid {
        bail!("invalid MIME type: {mime:?}");
    }
    Ok(())
}

pub(crate) fn normalize_extension(extension: &str) -> Result<String> {
    let extension = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let valid = !extension.is_empty()
        && extension.len() <= 32
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if !valid {
        bail!("invalid file extension");
    }
    Ok(extension)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lege_types::{AppAliases, AppManifest, ResourceClass};
    use parking_lot::RwLock;
    use std::collections::HashMap;

    #[derive(Default)]
    pub(crate) struct StaticCatalog(pub RwLock<HashMap<AppId, Arc<AppManifest>>>);

    impl StaticCatalog {
        pub(crate) fn with(ids: &[(&str, &[&str])]) -> Arc<Self> {
            let catalog = Self::default();
            for (id, mimes) in ids {
                let manifest = AppManifest {
                    id: id.parse().unwrap(),
                    display_name: (*id).to_owned(),
                    primary_desktop_file_id: format!("{id}.desktop"),
                    executable: "/usr/bin/true".into(),
                    aliases: AppAliases::default(),
                    mime_types: mimes.iter().map(|m| (*m).to_owned()).collect(),
                    resource_class: ResourceClass::Normal,
                    supports_checkpoint: false,
                    icon: None,
                    categories: std::collections::BTreeSet::default(),
                    keywords: std::collections::BTreeSet::default(),
                    no_display: false,
                };
                catalog
                    .0
                    .write()
                    .insert(manifest.id.clone(), Arc::new(manifest));
            }
            Arc::new(catalog)
        }
    }

    impl AppCatalog for StaticCatalog {
        fn manifest(&self, id: &AppId) -> Option<Arc<AppManifest>> {
            self.0.read().get(id).cloned()
        }
        fn manifests(&self) -> Vec<Arc<AppManifest>> {
            self.0.read().values().cloned().collect()
        }
    }

    fn engine() -> AssociationEngine {
        let catalog = StaticCatalog::with(&[
            ("org.legeos.Viewer", &["application/pdf", "image/*"]),
            (
                "org.example.Browser",
                &["application/pdf", "text/html", "x-scheme-handler/https"],
            ),
            ("org.example.Editor", &["text/plain", "text/*"]),
        ]);
        AssociationEngine::new(AssociationDb::default(), catalog)
    }

    #[test]
    fn detected_mime_beats_extension_unless_overridden() {
        let mut engine = engine();
        engine
            .set_mime_default("application/pdf", "org.legeos.Viewer".parse().unwrap())
            .unwrap();
        engine
            .set_extension_default(
                "pdf",
                ExtensionRule {
                    app_id: "org.example.Editor".parse().unwrap(),
                    override_specific_mime: false,
                },
            )
            .unwrap();

        // A real PDF opens with the MIME default even though an extension rule exists.
        let d = engine.resolve(Path::new("/tmp/report.pdf"), "application/pdf", None);
        assert_eq!(d.reason, DecisionReason::GlobalMimeDefault);
        assert_eq!(d.app_id.unwrap().as_str(), "org.legeos.Viewer");

        // A file called .pdf that is actually an executable does not inherit the PDF handler.
        let d = engine.resolve(
            Path::new("/tmp/report.pdf"),
            "application/x-executable",
            None,
        );
        assert_eq!(d.reason, DecisionReason::AskUser);

        // Generic content falls back to the extension rule.
        let d = engine.resolve(
            Path::new("/tmp/notes.pdf"),
            "application/octet-stream",
            None,
        );
        assert_eq!(d.reason, DecisionReason::GenericExtensionFallback);
        assert_eq!(d.app_id.unwrap().as_str(), "org.example.Editor");

        // Explicit override wins over the detected type.
        engine
            .set_extension_default(
                ".PDF",
                ExtensionRule {
                    app_id: "org.example.Editor".parse().unwrap(),
                    override_specific_mime: true,
                },
            )
            .unwrap();
        let d = engine.resolve(Path::new("/tmp/report.pdf"), "application/pdf", None);
        assert_eq!(d.reason, DecisionReason::ExplicitExtensionOverride);
    }

    #[test]
    fn project_defaults_win_and_unsupported_defaults_are_rejected() {
        let mut engine = engine();
        engine
            .set_mime_default("application/pdf", "org.legeos.Viewer".parse().unwrap())
            .unwrap();
        engine
            .set_project_mime_default(
                "thesis",
                "application/pdf",
                "org.example.Browser".parse().unwrap(),
            )
            .unwrap();

        let d = engine.resolve(Path::new("a.pdf"), "application/pdf", Some("thesis"));
        assert_eq!(d.reason, DecisionReason::ProjectMimeDefault);
        assert_eq!(d.app_id.unwrap().as_str(), "org.example.Browser");

        let d = engine.resolve(Path::new("a.pdf"), "application/pdf", Some("other"));
        assert_eq!(d.reason, DecisionReason::GlobalMimeDefault);

        assert!(
            engine
                .set_mime_default("video/mp4", "org.legeos.Viewer".parse().unwrap())
                .is_err()
        );
        assert!(
            engine
                .set_mime_default("bogus", "org.legeos.Viewer".parse().unwrap())
                .is_err()
        );
        assert!(
            engine
                .set_extension_default(
                    "../x",
                    ExtensionRule {
                        app_id: "org.legeos.Viewer".parse().unwrap(),
                        override_specific_mime: false
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn wildcard_support_counts() {
        let mut engine = engine();
        engine
            .set_mime_default("image/tiff", "org.legeos.Viewer".parse().unwrap())
            .unwrap();
        let d = engine.resolve(Path::new("scan.tif"), "image/tiff", None);
        assert_eq!(d.app_id.unwrap().as_str(), "org.legeos.Viewer");
    }

    #[test]
    fn stale_defaults_are_pruned() {
        let catalog = StaticCatalog::with(&[("org.legeos.Viewer", &["application/pdf"])]);
        let mut engine = AssociationEngine::new(
            AssociationDb::default(),
            Arc::clone(&catalog) as Arc<dyn AppCatalog>,
        );
        engine
            .set_mime_default("application/pdf", "org.legeos.Viewer".parse().unwrap())
            .unwrap();
        catalog.0.write().clear(); // "uninstall"
        let removed = engine.prune_stale();
        assert_eq!(removed.len(), 1);
        assert!(engine.database().mime_defaults.is_empty());
    }

    #[test]
    fn scheme_resolution_has_no_extension_fallback() {
        let mut engine = engine();
        engine
            .set_mime_default(
                "x-scheme-handler/https",
                "org.example.Browser".parse().unwrap(),
            )
            .unwrap();
        let d = engine.resolve_scheme("https", None);
        assert_eq!(d.reason, DecisionReason::GlobalMimeDefault);
        assert_eq!(d.app_id.unwrap().as_str(), "org.example.Browser");

        let d = engine.resolve_scheme("magnet", None);
        assert_eq!(d.reason, DecisionReason::AskUser);
    }

    #[test]
    fn forget_removes_exactly_the_named_rule() {
        let mut engine = engine();
        engine
            .set_mime_default("application/pdf", "org.legeos.Viewer".parse().unwrap())
            .unwrap();
        engine
            .set_extension_default(
                "pdf",
                ExtensionRule {
                    app_id: "org.legeos.Viewer".parse().unwrap(),
                    override_specific_mime: true,
                },
            )
            .unwrap();
        engine
            .set_project_mime_default(
                "thesis",
                "application/pdf",
                "org.example.Browser".parse().unwrap(),
            )
            .unwrap();

        assert!(engine.forget_mime_default("application/pdf").is_some());
        assert!(engine.forget_mime_default("application/pdf").is_none());
        assert!(engine.forget_extension_default(".PDF").is_some());
        assert!(
            engine
                .forget_project_mime_default("thesis", "application/pdf")
                .is_some()
        );
        assert!(engine.database().mime_defaults.is_empty());
        assert!(engine.database().extension_defaults.is_empty());
    }
}
