//! D-Bus interface `org.legeos.Associations1`.

use crate::{detector::MimeDetector, engine::AssociationEngine};
use parking_lot::RwLock;
use std::{path::Path, sync::Arc};
use zbus::fdo;

/// `org.legeos.Associations1`
pub struct AssociationApi {
    engine: Arc<RwLock<AssociationEngine>>,
    detector: Arc<dyn MimeDetector>,
}

impl std::fmt::Debug for AssociationApi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AssociationApi")
            .finish_non_exhaustive()
    }
}

impl AssociationApi {
    pub fn new(engine: Arc<RwLock<AssociationEngine>>, detector: Arc<dyn MimeDetector>) -> Self {
        Self { engine, detector }
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

        let decision =
            self.engine
                .read()
                .resolve(path, &mime, (!project_id.is_empty()).then_some(project_id));

        Ok((
            decision.app_id.map(|id| id.to_string()).unwrap_or_default(),
            decision.detected_mime,
            decision.reason.as_str().to_owned(),
        ))
    }

    /// Set the global default handler for a MIME type.
    async fn set_mime_default(&self, mime: &str, app_id: &str) -> fdo::Result<()> {
        let app_id = app_id
            .parse()
            .map_err(|error: lege_types::AppIdError| fdo::Error::InvalidArgs(error.to_string()))?;
        self.engine
            .write()
            .set_mime_default(mime, app_id)
            .map_err(|error| fdo::Error::Failed(error.to_string()))
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
}
