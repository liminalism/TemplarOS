//! `org.legeos.HomeField1`: the Home Field's document and undo, persisted in `lege-state`
//! (`lege_session_core::homefield`). Every change is also a history event, so the shell's
//! drag-and-drop shows up in `History1` like every other change the session makes.

use crate::apis::failed;
use lege_session_core::{
    history::{Event, EventId, EventKind, HistoryStore, SqliteHistory},
    homefield,
};
use lege_state::Store;
use std::sync::Arc;
use zbus::{fdo, object_server::SignalEmitter};

#[derive(Debug)]
pub struct HomeFieldApi {
    store: Store,
    history: Arc<SqliteHistory>,
}

impl HomeFieldApi {
    pub fn new(store: Store, history: Arc<SqliteHistory>) -> Self {
        Self { store, history }
    }

    fn record(&self, message: String) {
        let event = Event {
            id: EventId::new(),
            at_ms: lege_state::now_ms(),
            kind: EventKind::SettingChange,
            app_id: None,
            instance_id: None,
            message,
            cause: Some("home-field".into()),
            actions: Vec::new(),
            // Undo goes through `HomeField1.Undo`, which keeps the exact inverse; History1's
            // own cross-domain undo has no applier for the field and does not need one.
            reversible: false,
        };
        if let Err(error) = self.history.append(event) {
            tracing::warn!(%error, "failed to record a home field history event");
        }
    }
}

fn to_json(doc: &lege_field::FieldDocument) -> fdo::Result<String> {
    serde_json::to_string(doc).map_err(|error| fdo::Error::Failed(error.to_string()))
}

#[zbus::interface(name = "org.legeos.HomeField1")]
impl HomeFieldApi {
    async fn get(&self, output: &str, cols: u16, rows: u16) -> fdo::Result<String> {
        if output.is_empty() {
            return Err(fdo::Error::InvalidArgs("output must not be empty".into()));
        }
        let doc = homefield::load_or_create(&self.store, output, cols, rows).map_err(failed)?;
        to_json(&doc)
    }

    async fn apply(
        &self,
        output: &str,
        op_json: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<u64> {
        let doc = homefield::apply_json(&self.store, output, op_json)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;
        let label =
            serde_json::from_str::<lege_field::FieldOp>(op_json).map_or("CHANGE", |op| op.label());
        self.record(format!(
            "Home Field on {output}: {} (revision {}).",
            label.to_lowercase(),
            doc.revision
        ));
        let _ = emitter.changed(output.to_owned(), doc.revision).await;
        Ok(doc.revision)
    }

    async fn undo(
        &self,
        output: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        match homefield::undo(&self.store, output).map_err(failed)? {
            Some(doc) => {
                self.record(format!(
                    "Home Field on {output}: change undone (revision {}).",
                    doc.revision
                ));
                let _ = emitter.changed(output.to_owned(), doc.revision).await;
                to_json(&doc)
            }
            None => Ok(String::new()),
        }
    }

    async fn get_revision(&self, output: &str) -> fdo::Result<u64> {
        Ok(homefield::load(&self.store, output)
            .map_err(failed)?
            .map_or(0, |doc| doc.revision))
    }

    #[zbus(signal)]
    async fn changed(
        emitter: &SignalEmitter<'_>,
        output: String,
        revision: u64,
    ) -> zbus::Result<()>;
}
