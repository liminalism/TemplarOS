//! `org.templaros.Resources1` (`interfaces/dbus/org.templaros.Resources1.xml`).
//!
//! The governor never needs this interface to *contain* a runaway application (ADR-0002): every
//! method here either serves a read-only snapshot the sampling loop already published
//! ([`crate::state::SharedState::report`]) or writes enrichment/settings the loop picks up on its
//! next tick. No method blocks on, or is blocked by, the sampling loop.

use crate::policy::Stage;
use crate::state::SharedState;
use std::sync::Arc;
use templar_types::ResourceId;
use zbus::fdo;

/// `org.templaros.Resources1` at `/org/templaros/Resources`.
#[derive(Debug)]
pub struct ResourcesApi {
    shared: Arc<SharedState>,
}

impl ResourcesApi {
    pub fn new(shared: Arc<SharedState>) -> Self {
        Self { shared }
    }
}

#[zbus::interface(name = "org.templaros.Resources1")]
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
            Some(report) => report.applications_json().unwrap_or_else(|_| "{}".to_owned()),
            None => "{}".to_owned(),
        }
    }

    /// Full governor target set, including explicit work and observed Flatpak scopes.
    async fn targets(&self) -> fdo::Result<String> {
        match self.shared.report.read().as_ref() {
            Some(report) => serde_json::to_string(report).map_err(|error| fdo::Error::Failed(error.to_string())),
            None => Ok("{}".to_owned()),
        }
    }

    /// Kernel PID membership, never an application ID claimed by a window.
    async fn resolve_pid(&self, pid: u32) -> fdo::Result<String> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let group = text.lines().find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| fdo::Error::Failed("PID has no unified cgroup".into()))?;
        let group = std::path::Path::new(group);
        let report = self.shared.report.read();
        Ok(report.as_ref().and_then(|report| report.instances.iter()
            .filter(|target| group.starts_with(&target.metadata.control_group))
            .max_by_key(|target| target.metadata.control_group.components().count()))
            .map(|target| target.instance_id.to_string()).unwrap_or_default())
    }

    async fn explain_target(&self, resource_id: &str) -> fdo::Result<String> {
        let id = ResourceId::parse(resource_id)
            .ok_or_else(|| fdo::Error::InvalidArgs("invalid resource UUID".into()))?;
        match self.shared.report.read().as_ref() {
            Some(report) => serde_json::to_string(&report.explain(id)).map_err(|error| fdo::Error::Failed(error.to_string())),
            None => serde_json::to_string(&NoReportYet::new(id, self.shared.stage())).map_err(|error| fdo::Error::Failed(error.to_string())),
        }
    }

    /// Why `instance_id` would (or would not) be chosen next, in words and in numbers.
    async fn explain_instance(&self, instance_id: &str) -> fdo::Result<String> {
        let id = ResourceId::parse(instance_id).ok_or_else(|| {
            fdo::Error::InvalidArgs(format!("not a valid instance id: {instance_id}"))
        })?;

        let explanation = match self.shared.report.read().as_ref() {
            Some(report) => report.explain(id),
            None => {
                return serde_json::to_string(&NoReportYet::new(id, self.shared.stage()))
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
        entry.window_state_observed = true;
        Ok(())
    }

    /// Session-daemon enrichment: protection, disposability, checkpoint support and whether the
    /// application currently holds unsaved work.
    // The four-bool signature mirrors `org.templaros.Resources1.SetInstanceHints` exactly — a fixed
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
        // Deliberately asymmetric: this method can *raise* an unsaved-work claim but not clear
        // one. Its callers set four facts at once, so every one of them has to supply a value
        // for all four — and the callers that know about protection and disposability know
        // nothing about unsaved work. `Projects1` passes `false` here when a project opens or
        // closes, which used to erase a claim the checkpoint relay had just made. `false` from
        // this method therefore means "I have nothing to say about it", which is what its
        // callers actually mean; `SetInstanceWork` is the method that can say the work is saved,
        // and the claim's own expiry stops a stuck one lasting the session.
        if has_unsaved_work {
            entry.set_unsaved_work(true, crate::trace::now_ms());
        }
        Ok(())
    }

    /// Unsaved work and checkpoint freshness, with the times they were true.
    ///
    /// Separate from `SetInstanceHints` because a claim that any caller can erase, and that
    /// carries no time, is not a claim the governor can act on: see the asymmetry note above and
    /// the interface XML.
    async fn set_instance_work(
        &self,
        instance_id: &str,
        has_unsaved_work: bool,
        as_of_ms: i64,
        checkpoint_ok: bool,
        checkpoint_at_ms: i64,
    ) -> fdo::Result<()> {
        let id = parse_instance_id(instance_id)?;
        let now_ms = crate::trace::now_ms();
        let mut enrichment = self.shared.enrichment.write();
        let entry = enrichment.entry(id).or_default();
        // Zero means "now": a producer that has the fact but not a clock reading for it should
        // not have to invent one, and 1970 would read as an infinitely stale claim.
        entry.set_work(
            has_unsaved_work,
            (as_of_ms > 0).then_some(as_of_ms),
            checkpoint_ok,
            (checkpoint_at_ms > 0).then_some(checkpoint_at_ms),
            now_ms,
        );
        Ok(())
    }

    /// Attention and intent: when the user last touched this instance, whether they pinned it,
    /// and whether its project is important.
    ///
    /// An instance nobody reports is scored exactly as it was before this existed. A zero
    /// timestamp means "never reported", not 1970.
    async fn set_instance_activity(
        &self,
        instance_id: &str,
        last_interaction_ms: i64,
        user_pinned: bool,
        project_important: bool,
    ) -> fdo::Result<()> {
        let id = parse_instance_id(instance_id)?;
        let mut enrichment = self.shared.enrichment.write();
        let entry = enrichment.entry(id).or_default();
        entry.last_interaction_ms = (last_interaction_ms > 0).then_some(last_interaction_ms);
        entry.user_pinned = user_pinned;
        entry.project_important = project_important;
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

    /// The sampled target set changed; subscribe before reading Targets or resolving PIDs.
    #[zbus(signal)]
    pub async fn targets_changed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;

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

fn parse_instance_id(text: &str) -> fdo::Result<ResourceId> {
    ResourceId::parse(text)
        .ok_or_else(|| fdo::Error::InvalidArgs(format!("not a valid instance id: {text}")))
}

#[derive(serde::Serialize)]
struct NoReportYet {
    instance_id: ResourceId,
    known: bool,
    level: &'static str,
    stage: Stage,
    candidate_count: usize,
    would_be_selected: bool,
    summary: &'static str,
}

impl NoReportYet {
    fn new(instance_id: ResourceId, stage: Stage) -> Self {
        Self { instance_id, known: false, level: "unknown", stage, candidate_count: 0,
            would_be_selected: false, summary: "The governor has not completed a sampling tick yet." }
    }
}
