//! `org.legeos.Resources1` (`interfaces/dbus/org.legeos.Resources1.xml`).
//!
//! The governor never needs this interface to *contain* a runaway application (ADR-0002): every
//! method here either serves a read-only snapshot the sampling loop already published
//! ([`crate::state::SharedState::report`]) or writes enrichment/settings the loop picks up on its
//! next tick. No method blocks on, or is blocked by, the sampling loop.

use crate::policy::Stage;
use crate::state::SharedState;
use lege_types::InstanceId;
use std::sync::Arc;
use zbus::fdo;

/// `org.legeos.Resources1` at `/org/legeos/Resources`.
#[derive(Debug)]
pub struct ResourcesApi {
    shared: Arc<SharedState>,
}

impl ResourcesApi {
    pub fn new(shared: Arc<SharedState>) -> Self {
        Self { shared }
    }
}

#[zbus::interface(name = "org.legeos.Resources1")]
impl ResourcesApi {
    /// Current pressure level and whether the governor is still observe-only.
    async fn status(&self) -> (String, bool) {
        let level = self
            .shared
            .report
            .read()
            .as_ref()
            .map_or(crate::policy::PressureLevel::Normal, |report| report.level);
        (level.to_string(), self.shared.stage() == Stage::ObserveOnly)
    }

    /// Full status as JSON (empty object before the first tick has run).
    async fn report(&self) -> String {
        match self.shared.report.read().as_ref() {
            Some(report) => serde_json::to_string(report).unwrap_or_else(|_| "{}".to_owned()),
            None => "{}".to_owned(),
        }
    }

    /// Why `instance_id` would (or would not) be chosen next, in words and in numbers.
    async fn explain_instance(&self, instance_id: &str) -> fdo::Result<String> {
        let id = InstanceId::parse(instance_id).ok_or_else(|| {
            fdo::Error::InvalidArgs(format!("not a valid instance id: {instance_id}"))
        })?;

        let explanation = match self.shared.report.read().as_ref() {
            Some(report) => report.explain(id),
            None => {
                return serde_json::to_string(&NoReportYet { instance_id: id })
                    .map_err(|error| fdo::Error::Failed(error.to_string()));
            }
        };
        serde_json::to_string(&explanation).map_err(|error| fdo::Error::Failed(error.to_string()))
    }

    /// Containment stage: `observe-only|trim|throttle|checkpoint|terminate`.
    async fn stage(&self) -> String {
        self.shared.stage().to_string()
    }

    /// Move to a containment stage.
    async fn set_stage(&self, stage: &str) -> fdo::Result<()> {
        let parsed = Stage::parse(stage)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown stage: {stage}")))?;
        tracing::info!(stage = parsed.as_str(), "containment stage changed");
        self.shared.set_stage(parsed);
        Ok(())
    }

    /// Compositor enrichment: which instance the user is actually looking at.
    async fn set_instance_state(
        &self,
        instance_id: &str,
        foreground: bool,
        visible: bool,
    ) -> fdo::Result<()> {
        let id = parse_instance_id(instance_id)?;
        let mut enrichment = self.shared.enrichment.write();
        let entry = enrichment.entry(id).or_default();
        entry.foreground = foreground;
        entry.visible = visible;
        Ok(())
    }

    /// Session-daemon enrichment: protection, disposability, checkpoint support and whether the
    /// application currently holds unsaved work.
    // The four-bool signature mirrors `org.legeos.Resources1.SetInstanceHints` exactly — a fixed
    // D-Bus contract another agent is coding against — rather than something to redesign here.
    #[allow(clippy::fn_params_excessive_bools)]
    async fn set_instance_hints(
        &self,
        instance_id: &str,
        protected: bool,
        disposable: bool,
        supports_checkpoint: bool,
        has_unsaved_work: bool,
    ) -> fdo::Result<()> {
        let id = parse_instance_id(instance_id)?;
        let mut enrichment = self.shared.enrichment.write();
        let entry = enrichment.entry(id).or_default();
        entry.protected = protected;
        entry.disposable = disposable;
        entry.supports_checkpoint = supports_checkpoint;
        entry.has_unsaved_work = has_unsaved_work;
        Ok(())
    }

    /// An instance ended; drop its record and any temporary limits (applied by the loop on its
    /// next tick — see `crate::state`'s module doc comment).
    async fn forget_instance(&self, instance_id: &str) -> fdo::Result<()> {
        let id = parse_instance_id(instance_id)?;
        self.shared.enrichment.write().remove(&id);
        self.shared.pending_forgets.lock().insert(id);
        Ok(())
    }

    /// Recorded traces, newest first, as JSON lines.
    async fn recent_traces(&self, limit: u32) -> String {
        self.shared.traces.lock().recent(limit as usize).join("\n")
    }

    /// Pressure level changed.
    #[zbus(signal)]
    pub async fn pressure_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        level: String,
        report_json: String,
    ) -> zbus::Result<()>;

    /// One decided action (`applied` is false in observe-only mode and at stages below it).
    #[zbus(signal)]
    pub async fn pressure_action(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        kind: String,
        instance_id: String,
        applied: bool,
        reason: String,
        detail_json: String,
    ) -> zbus::Result<()>;

    /// A stall the memory governor does not own.
    #[zbus(signal)]
    pub async fn non_memory_stall(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        kind: String,
        detail_json: String,
    ) -> zbus::Result<()>;
}

fn parse_instance_id(text: &str) -> fdo::Result<InstanceId> {
    InstanceId::parse(text)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("not a valid instance id: {text}")))
}

#[derive(serde::Serialize)]
struct NoReportYet {
    instance_id: InstanceId,
}
