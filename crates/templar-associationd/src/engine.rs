//! Association rule engine (pure; no I/O).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use templar_types::{AppCatalog, AppId};

/// An explicit per-extension rule.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionRule {
    pub app_id: AppId,

    /// False by default. When false, the extension rule is only used if content detection
    /// returned a generic type such as `application/octet-stream`.
    #[serde(default)]
    pub override_specific_mime: bool,
}

/// The typed association database (persisted by `templar-state`; exported to `mimeapps.list`).
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
    /// The stored default named a copy of an application that is no longer installed, and another
    /// copy of the same application answered instead. Said out loud rather than silently: the file
    /// opened, and the person should be able to find out why it opened in the Flatpak this time.
    VariantSubstituted,
    AskUser,
}

impl DecisionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectMimeDefault => "project-mime-default",
            Self::ExplicitExtensionOverride => "explicit-extension-override",
            Self::GlobalMimeDefault => "global-mime-default",
            Self::GenericExtensionFallback => "generic-extension-fallback",
            Self::VariantSubstituted => "variant-substituted",
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
    /// change, to bring the in-memory engine back in sync with `templar-state` without needing a
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
            && let Some(stored) = self
                .db
                .project_mime_defaults
                .get(project_id)
                .and_then(|rules| rules.get(detected_mime))
            && let Some(handler) =
                self.live_handler(stored, |id| self.app_supports(id, detected_mime))
        {
            return decision(
                handler.app_id.clone(),
                detected_mime,
                handler.reason(DecisionReason::ProjectMimeDefault),
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
            && let Some(handler) = self.live_handler(&rule.app_id, |id| self.app_exists(id))
        {
            return decision(
                handler.app_id.clone(),
                detected_mime,
                handler.reason(DecisionReason::ExplicitExtensionOverride),
            );
        }

        if let Some(stored) = self.db.mime_defaults.get(detected_mime)
            && let Some(handler) =
                self.live_handler(stored, |id| self.app_supports(id, detected_mime))
        {
            return decision(
                handler.app_id.clone(),
                detected_mime,
                handler.reason(DecisionReason::GlobalMimeDefault),
            );
        }

        let generic_type = matches!(detected_mime, "application/octet-stream" | "text/plain");
        if generic_type
            && let Some(rule) = extension_rule
            && let Some(handler) = self.live_handler(&rule.app_id, |id| self.app_exists(id))
        {
            return decision(
                handler.app_id.clone(),
                detected_mime,
                handler.reason(DecisionReason::GenericExtensionFallback),
            );
        }

        OpenDecision {
            app_id: None,
            detected_mime: detected_mime.to_owned(),
            reason: DecisionReason::AskUser,
        }
    }

    /// Every installed application that declares support for `mime`, sorted by id.
    ///
    /// This is exactly the set [`Self::validate_mime_default`] will accept, so a chooser built on
    /// it cannot offer a default the engine would then refuse. Sorted rather than left in catalog
    /// order because catalog order is unspecified, and a list of applications that reshuffles
    /// itself between two readings of the same page is one a person cannot use.
    pub fn handlers(&self, mime: &str) -> Vec<AppId> {
        let mut handlers: Vec<AppId> = self
            .catalog
            .manifests()
            .into_iter()
            .filter(|manifest| manifest.supports_mime(mime))
            .map(|manifest| manifest.id.clone())
            .collect();
        handlers.sort();
        handlers.dedup();
        handlers
    }

    /// Set the global default for a MIME type. The application must declare support for it.
    pub fn set_mime_default(&mut self, mime: &str, app_id: AppId) -> Result<()> {
        self.validate_mime_default(mime, &app_id)?;
        self.apply_mime_default(mime, app_id);
        Ok(())
    }

    /// Check whether `app_id` may become `mime`'s default, without mutating anything. Split out
    /// from [`Self::set_mime_default`] so a D-Bus-backed caller can validate, persist to
    /// `templar-state`, and only then apply in-memory — never the reverse order, which could leave
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

    /// The copy that will actually open this, given a stored choice that may name a copy which has
    /// since been uninstalled.
    ///
    /// A person who chose one copy gets that copy for as long as it exists — the stored id is
    /// tried first and unchanged. Only when it is gone does the search widen: through the alias
    /// table, then to a sibling copy of the same application. Replacing the deb with the Flatpak
    /// should not make every PDF stop opening.
    fn live_handler(&self, stored: &AppId, test: impl Fn(&AppId) -> bool) -> Option<Substituted> {
        if test(stored) {
            return Some(Substituted {
                app_id: stored.clone(),
                substituted: false,
            });
        }
        let resolved = self.catalog.resolve_alias(stored);
        if resolved != *stored && test(&resolved) {
            return Some(Substituted {
                app_id: resolved,
                substituted: true,
            });
        }
        self.catalog
            .sibling_variants(&resolved)
            .into_iter()
            .find(|sibling| test(sibling))
            .map(|app_id| Substituted {
                app_id,
                substituted: true,
            })
    }
}

/// A resolved handler, and whether it is the copy the person actually chose.
struct Substituted {
    app_id: AppId,
    substituted: bool,
}

impl Substituted {
    fn reason(&self, original: DecisionReason) -> DecisionReason {
        if self.substituted {
            DecisionReason::VariantSubstituted
        } else {
            original
        }
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
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use templar_types::{AppAliases, AppManifest, ResourceClass};

    #[derive(Default)]
    pub(crate) struct StaticCatalog {
        pub manifests: RwLock<HashMap<AppId, Arc<AppManifest>>>,
        /// Old id -> current id, as the registry's alias table would answer.
        pub aliases: RwLock<HashMap<AppId, AppId>>,
        /// Variant -> the other copies of the same application, best first.
        pub siblings: RwLock<HashMap<AppId, Vec<AppId>>>,
    }

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
                    provenance: None,
                };
                catalog
                    .manifests
                    .write()
                    .insert(manifest.id.clone(), Arc::new(manifest));
            }
            Arc::new(catalog)
        }
    }

    impl AppCatalog for StaticCatalog {
        fn manifest(&self, id: &AppId) -> Option<Arc<AppManifest>> {
            self.manifests.read().get(id).cloned()
        }
        fn manifests(&self) -> Vec<Arc<AppManifest>> {
            self.manifests.read().values().cloned().collect()
        }
        fn resolve_alias(&self, stored: &AppId) -> AppId {
            self.aliases
                .read()
                .get(stored)
                .cloned()
                .unwrap_or_else(|| stored.clone())
        }
        fn sibling_variants(&self, id: &AppId) -> Vec<AppId> {
            self.siblings.read().get(id).cloned().unwrap_or_default()
        }
    }

    fn engine() -> AssociationEngine {
        let catalog = StaticCatalog::with(&[
            ("org.templaros.Viewer", &["application/pdf", "image/*"]),
            (
                "org.example.Browser",
                &["application/pdf", "text/html", "x-scheme-handler/https"],
            ),
            ("org.example.Editor", &["text/plain", "text/*"]),
        ]);
        AssociationEngine::new(AssociationDb::default(), catalog)
    }

    #[test]
    fn handlers_are_exactly_what_set_mime_default_will_accept() {
        let mut engine = engine();
        let handlers = engine.handlers("application/pdf");
        assert_eq!(
            handlers.iter().map(AppId::as_str).collect::<Vec<_>>(),
            vec!["org.example.Browser", "org.templaros.Viewer"],
            "sorted, so the same page read twice lists them the same way"
        );
        for app_id in handlers {
            engine
                .set_mime_default("application/pdf", app_id.clone())
                .unwrap_or_else(|error| panic!("{app_id} was offered but refused: {error}"));
        }
    }

    #[test]
    fn a_wildcard_declaration_makes_an_application_a_handler_for_the_types_it_covers() {
        // `org.example.Editor` declares `text/*`, so it handles text/plain and text/markdown
        // without naming either; a chooser that only matched literal strings would leave the one
        // application that can open the file out of its own list.
        let engine = engine();
        assert!(
            engine
                .handlers("text/markdown")
                .iter()
                .any(|id| id.as_str() == "org.example.Editor")
        );
    }

    #[test]
    fn a_type_nothing_declares_has_no_handlers() {
        assert!(
            engine()
                .handlers("application/x-nothing-declares-this")
                .is_empty()
        );
    }

    #[test]
    fn detected_mime_beats_extension_unless_overridden() {
        let mut engine = engine();
        engine
            .set_mime_default("application/pdf", "org.templaros.Viewer".parse().unwrap())
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
        assert_eq!(d.app_id.unwrap().as_str(), "org.templaros.Viewer");

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
            .set_mime_default("application/pdf", "org.templaros.Viewer".parse().unwrap())
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
                .set_mime_default("video/mp4", "org.templaros.Viewer".parse().unwrap())
                .is_err()
        );
        assert!(
            engine
                .set_mime_default("bogus", "org.templaros.Viewer".parse().unwrap())
                .is_err()
        );
        assert!(
            engine
                .set_extension_default(
                    "../x",
                    ExtensionRule {
                        app_id: "org.templaros.Viewer".parse().unwrap(),
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
            .set_mime_default("image/tiff", "org.templaros.Viewer".parse().unwrap())
            .unwrap();
        let d = engine.resolve(Path::new("scan.tif"), "image/tiff", None);
        assert_eq!(d.app_id.unwrap().as_str(), "org.templaros.Viewer");
    }

    #[test]
    fn stale_defaults_are_pruned() {
        let catalog = StaticCatalog::with(&[("org.templaros.Viewer", &["application/pdf"])]);
        let mut engine = AssociationEngine::new(
            AssociationDb::default(),
            Arc::clone(&catalog) as Arc<dyn AppCatalog>,
        );
        engine
            .set_mime_default("application/pdf", "org.templaros.Viewer".parse().unwrap())
            .unwrap();
        catalog.manifests.write().clear(); // "uninstall"
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
            .set_mime_default("application/pdf", "org.templaros.Viewer".parse().unwrap())
            .unwrap();
        engine
            .set_extension_default(
                "pdf",
                ExtensionRule {
                    app_id: "org.templaros.Viewer".parse().unwrap(),
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

    #[test]
    fn a_default_still_opens_when_the_chosen_copy_is_replaced_by_another_of_the_same_app() {
        // The person chose the distribution's Firefox. They then removed the package and kept the
        // Flatpak. Their PDFs should not stop opening, and the substitution should be sayable.
        let distro: AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let flatpak: AppId = "flatpak.org.mozilla.firefox".parse().unwrap();
        let catalog = StaticCatalog::with(&[("flatpak.org.mozilla.firefox", &["application/pdf"])]);
        catalog
            .siblings
            .write()
            .insert(distro.clone(), vec![flatpak.clone()]);

        let mut db = AssociationDb::default();
        db.mime_defaults
            .insert("application/pdf".into(), distro.clone());
        let engine = AssociationEngine::new(db, catalog);

        let decision = engine.resolve(Path::new("/tmp/a.pdf"), "application/pdf", None);
        assert_eq!(decision.app_id.as_ref(), Some(&flatpak));
        assert_eq!(decision.reason, DecisionReason::VariantSubstituted);
    }

    #[test]
    fn the_copy_a_person_chose_is_used_for_as_long_as_it_exists() {
        let distro: AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let flatpak: AppId = "flatpak.org.mozilla.firefox".parse().unwrap();
        let catalog = StaticCatalog::with(&[
            ("desktop-import.firefox.desktop", &["application/pdf"]),
            ("flatpak.org.mozilla.firefox", &["application/pdf"]),
        ]);
        catalog
            .siblings
            .write()
            .insert(distro.clone(), vec![flatpak]);

        let mut db = AssociationDb::default();
        db.mime_defaults
            .insert("application/pdf".into(), distro.clone());
        let engine = AssociationEngine::new(db, catalog);

        let decision = engine.resolve(Path::new("/tmp/a.pdf"), "application/pdf", None);
        assert_eq!(decision.app_id.as_ref(), Some(&distro));
        assert_eq!(
            decision.reason,
            DecisionReason::GlobalMimeDefault,
            "no substitution happened, so nothing should say one did"
        );
    }

    #[test]
    fn a_renamed_variant_resolves_through_its_alias_before_any_sibling_is_considered() {
        let old: AppId = "desktop-import.org.mozilla.firefox.desktop"
            .parse()
            .unwrap();
        let new: AppId = "flatpak.org.mozilla.firefox".parse().unwrap();
        let other: AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let catalog = StaticCatalog::with(&[
            ("flatpak.org.mozilla.firefox", &["application/pdf"]),
            ("desktop-import.firefox.desktop", &["application/pdf"]),
        ]);
        catalog.aliases.write().insert(old.clone(), new.clone());
        catalog.siblings.write().insert(new.clone(), vec![other]);

        let mut db = AssociationDb::default();
        db.mime_defaults.insert("application/pdf".into(), old);
        let engine = AssociationEngine::new(db, catalog);

        let decision = engine.resolve(Path::new("/tmp/a.pdf"), "application/pdf", None);
        assert_eq!(
            decision.app_id.as_ref(),
            Some(&new),
            "the same copy under its new id, not a different copy"
        );
        assert_eq!(decision.reason, DecisionReason::VariantSubstituted);
    }

    #[test]
    fn no_sibling_that_handles_the_type_still_means_ask() {
        let distro: AppId = "desktop-import.firefox.desktop".parse().unwrap();
        let catalog = StaticCatalog::with(&[("org.example.Editor", &["text/plain"])]);
        catalog
            .siblings
            .write()
            .insert(distro.clone(), vec!["org.example.Editor".parse().unwrap()]);
        let mut db = AssociationDb::default();
        db.mime_defaults.insert("application/pdf".into(), distro);
        let engine = AssociationEngine::new(db, catalog);

        let decision = engine.resolve(Path::new("/tmp/a.pdf"), "application/pdf", None);
        assert_eq!(decision.reason, DecisionReason::AskUser);
        assert!(decision.app_id.is_none());
    }
}
