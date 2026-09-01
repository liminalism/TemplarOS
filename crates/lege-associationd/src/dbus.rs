//! D-Bus interface `org.legeos.Associations1`.
//!
//! Every mutating method validates against the in-memory [`AssociationEngine`] first, persists
//! (with history + the undo log) in one `lege-state` transaction, and only *then* applies the
//! mutation in memory — never the other order, which could leave the in-memory engine ahead of a
//! database write that failed. `mimeapps.list` is regenerated atomically after every mutation
//! (`docs/architecture/associations.md`: import once, then own it).

use crate::{
    detector::MimeDetector,
    engine::{AssociationEngine, ExtensionRule},
    export,
    store::{self, ForgetScope},
};
use lege_state::Store;
use lege_types::{AppCatalog, AppId};
use parking_lot::RwLock;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zbus::{fdo, object_server::SignalEmitter};

/// `org.legeos.Associations1`
pub struct AssociationApi {
    engine: Arc<RwLock<AssociationEngine>>,
    detector: Arc<dyn MimeDetector>,
    store: Store,
    catalog: Arc<dyn AppCatalog>,
    mimeapps_path: PathBuf,
}

impl std::fmt::Debug for AssociationApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssociationApi")
            .field("mimeapps_path", &self.mimeapps_path)
            .finish_non_exhaustive()
    }
}

impl AssociationApi {
    pub fn new(
        engine: Arc<RwLock<AssociationEngine>>,
        detector: Arc<dyn MimeDetector>,
        store: Store,
        catalog: Arc<dyn AppCatalog>,
        mimeapps_path: PathBuf,
    ) -> Self {
        Self {
            engine,
            detector,
            store,
            catalog,
            mimeapps_path,
        }
    }

    /// Best-effort: a failure to regenerate `mimeapps.list` (e.g. a read-only home during
    /// recovery) must not fail the association change itself, which is already durably
    /// persisted in `lege-state`.
    fn regenerate_mimeapps(&self) {
        let engine = self.engine.read();
        if let Err(error) =
            export::export_mimeapps(engine.database(), &*self.catalog, &self.mimeapps_path)
        {
            tracing::warn!(%error, path = %self.mimeapps_path.display(), "failed to regenerate mimeapps.list");
        }
    }
}

#[zbus::interface(name = "org.legeos.Associations1")]
impl AssociationApi {
    /// Returns `(canonical_app_id, detected_mime, reason)`; an empty app ID means "ask the user".
    async fn resolve_file(
        &self,
        path: &str,
        project_id: &str,
    ) -> fdo::Result<(String, String, String)> {
        let path = Path::new(path);
        let mime = self
            .detector
            .detect(path)
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;

        let decision = self
            .engine
            .read()
            .resolve(path, &mime, nonempty(project_id).as_deref());

        Ok((
            decision.app_id.map(|id| id.to_string()).unwrap_or_default(),
            decision.detected_mime,
            decision.reason.as_str().to_owned(),
        ))
    }

    /// Returns `(canonical_app_id, detected_mime, reason)` for a URI scheme
    /// (`x-scheme-handler/<scheme>`); an empty app ID means "ask the user".
    async fn resolve_uri(
        &self,
        uri: &str,
        project_id: &str,
    ) -> fdo::Result<(String, String, String)> {
        let scheme = uri.split_once(':').map_or(uri, |(scheme, _)| scheme);
        let decision = self
            .engine
            .read()
            .resolve_scheme(scheme, nonempty(project_id).as_deref());
        Ok((
            decision.app_id.map(|id| id.to_string()).unwrap_or_default(),
            decision.detected_mime,
            decision.reason.as_str().to_owned(),
        ))
    }

    /// Set the global default handler for a MIME type.
    async fn set_mime_default(
        &self,
        mime: &str,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let logical = self.catalog.logical_app_of(&app_id);
        self.engine
            .read()
            .validate_mime_default(mime, &app_id)
            .map_err(failed)?;
        self.store
            .transaction(|tx| store::set_mime_default_within(tx, mime, &app_id, logical.as_ref()))
            .map_err(failed)?;
        self.engine.write().apply_mime_default(mime, app_id);
        self.regenerate_mimeapps();
        let _ = emitter.defaults_changed("mime", mime.to_owned()).await;
        Ok(())
    }

    /// Set (or override) the rule for a file extension.
    async fn set_extension_default(
        &self,
        extension: &str,
        app_id: &str,
        override_specific_mime: bool,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let app_id = parse_app_id(app_id)?;
        let logical = self.catalog.logical_app_of(&app_id);
        let rule = ExtensionRule {
            app_id,
            override_specific_mime,
        };
        self.engine
            .read()
            .validate_extension_default(&rule)
            .map_err(failed)?;
        self.store
            .transaction(|tx| {
                store::set_extension_default_within(tx, extension, &rule, logical.as_ref())
            })
            .map_err(failed)?;
        let normalized = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        self.engine
            .write()
            .apply_extension_default(normalized.clone(), rule);
        self.regenerate_mimeapps();
        let _ = emitter.defaults_changed("extension", normalized).await;
        Ok(())
    }

    /// Set the default for a MIME type inside one project only.
    async fn set_project_default(
        &self,
        project_id: &str,
        mime: &str,
        app_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        if project_id.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "project_id must not be empty".into(),
            ));
        }
        let app_id = parse_app_id(app_id)?;
        let logical = self.catalog.logical_app_of(&app_id);
        self.engine
            .read()
            .validate_project_mime_default(project_id, mime, &app_id)
            .map_err(failed)?;
        self.store
            .transaction(|tx| {
                store::set_project_mime_default_within(
                    tx,
                    project_id,
                    mime,
                    &app_id,
                    logical.as_ref(),
                )
            })
            .map_err(failed)?;
        self.engine
            .write()
            .apply_project_mime_default(project_id, mime, app_id);
        self.regenerate_mimeapps();
        let _ = emitter
            .defaults_changed("project-mime", format!("{project_id}:{mime}"))
            .await;
        Ok(())
    }

    /// Current global defaults as `(mime, app_id)` pairs.
    async fn list_mime_defaults(&self) -> Vec<(String, String)> {
        self.engine
            .read()
            .database()
            .mime_defaults
            .iter()
            .map(|(mime, app)| (mime.clone(), app.to_string()))
            .collect()
    }

    /// Explain, in one sentence, why `path` would (or would not) open with a particular
    /// application right now — the same resolution `ResolveFile` uses, phrased for a person.
    async fn explain(&self, path: &str, project_id: &str) -> fdo::Result<String> {
        let path_ref = Path::new(path);
        let mime = self
            .detector
            .detect(path_ref)
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let decision = self
            .engine
            .read()
            .resolve(path_ref, &mime, nonempty(project_id).as_deref());
        Ok(explain_decision(path, &mime, &decision))
    }

    /// Remove a rule. `scope` is `mime`, `extension` or `project-mime`; for `project-mime`,
    /// `project_id` must be set and `key` is the MIME type. Returns whether anything was removed.
    async fn forget(
        &self,
        scope: &str,
        key: &str,
        project_id: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<bool> {
        let (forget_scope, project_id) = match scope {
            "mime" => (ForgetScope::Mime, None),
            "extension" => (ForgetScope::Extension, None),
            "project-mime" if !project_id.is_empty() => {
                (ForgetScope::ProjectMime, Some(project_id))
            }
            "project-mime" => {
                return Err(fdo::Error::InvalidArgs(
                    "project-mime scope requires project_id".into(),
                ));
            }
            other => {
                return Err(fdo::Error::InvalidArgs(format!(
                    "unknown forget scope {other:?}"
                )));
            }
        };
        let key = if forget_scope == ForgetScope::Extension {
            key.trim().trim_start_matches('.').to_ascii_lowercase()
        } else {
            key.to_owned()
        };

        let removed = self
            .store
            .transaction(|tx| store::forget_within(tx, forget_scope, &key, project_id))
            .map_err(failed)?;

        if removed.is_some() {
            {
                let mut engine = self.engine.write();
                match forget_scope {
                    ForgetScope::Mime => {
                        engine.forget_mime_default(&key);
                    }
                    ForgetScope::Extension => {
                        engine.forget_extension_default(&key);
                    }
                    ForgetScope::ProjectMime => {
                        engine.forget_project_mime_default(project_id.unwrap_or_default(), &key);
                    }
                }
            }
            self.regenerate_mimeapps();
            let _ = emitter.defaults_changed(scope, key).await;
        }
        Ok(removed.is_some())
    }

    /// Defaults changed: `scope` is `mime`, `extension` or `project-mime`; `key` names what
    /// changed (a MIME type, an extension, or `<project_id>:<mime>`).
    #[zbus(signal)]
    async fn defaults_changed(
        signal_emitter: &SignalEmitter<'_>,
        scope: &str,
        key: String,
    ) -> zbus::Result<()>;
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_app_id(app_id: &str) -> fdo::Result<AppId> {
    app_id
        .parse()
        .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))
}

// Takes `anyhow::Error` by value so it can be passed directly as `.map_err(failed)`.
#[allow(clippy::needless_pass_by_value)]
fn failed(error: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(error.to_string())
}

fn explain_decision(path: &str, mime: &str, decision: &crate::engine::OpenDecision) -> String {
    let app = decision.app_id.as_ref().map_or("nothing", |id| id.as_str());
    match decision.reason {
        crate::engine::DecisionReason::ProjectMimeDefault => {
            format!(
                "{path} (detected as {mime}) opens with {app}: this project's default for {mime}."
            )
        }
        crate::engine::DecisionReason::ExplicitExtensionOverride => {
            format!(
                "{path} opens with {app}: an explicit rule always opens this extension with it, overriding the detected type ({mime})."
            )
        }
        crate::engine::DecisionReason::GlobalMimeDefault => {
            format!(
                "{path} (detected as {mime}) opens with {app}: the default application for {mime}."
            )
        }
        crate::engine::DecisionReason::GenericExtensionFallback => {
            format!(
                "{path} opens with {app}: its content looks generic ({mime}), so the rule for its extension was used instead."
            )
        }
        crate::engine::DecisionReason::VariantSubstituted => {
            format!(
                "{path} (detected as {mime}) opens with {app}: the copy of that application you had chosen is no longer installed, so another copy of it was used."
            )
        }
        crate::engine::DecisionReason::AskUser => {
            format!("{path} (detected as {mime}) has no handler set; you will be asked.")
        }
    }
}
