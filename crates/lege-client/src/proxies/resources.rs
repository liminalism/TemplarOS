//! Proxies for `lege-resourced` (`org.legeos.Resources`).
//!
//! Kept in sync with `interfaces/dbus/org.legeos.Resources1.xml`
//! (`cargo xtask check-interfaces`).
//!
//! The governor needs none of this to contain a runaway application — unit names are
//! self-describing, so cgroup → instance works from `/sys/fs/cgroup` alone (ADR-0002). These
//! calls carry *enrichment* (who is on screen, what must never be a victim) and observation.

use zbus::proxy;

#[proxy(
    interface = "org.legeos.Resources1",
    default_service = "org.legeos.Resources",
    default_path = "/org/legeos/Resources"
)]
pub trait Resources {
    /// Current pressure level (`normal|elevated|severe|critical`) and observe-only flag.
    fn status(&self) -> zbus::Result<(String, bool)>;

    /// Full status as JSON: level, stage, MemAvailable, global PSI and per-instance samples
    /// with their victim scores.
    fn report(&self) -> zbus::Result<String>;

    /// Why this instance would (or would not) be chosen next.
    fn explain_instance(&self, instance_id: &str) -> zbus::Result<String>;

    /// Containment stage: `observe-only|trim|throttle|checkpoint|terminate`.
    fn stage(&self) -> zbus::Result<String>;

    /// Move to a containment stage.
    fn set_stage(&self, stage: &str) -> zbus::Result<()>;

    /// Compositor enrichment: which instance the user is actually looking at.
    fn set_instance_state(
        &self,
        instance_id: &str,
        foreground: bool,
        visible: bool,
    ) -> zbus::Result<()>;

    /// Session-daemon enrichment: protection, disposability and unsaved work.
    fn set_instance_hints(
        &self,
        instance_id: &str,
        protected: bool,
        disposable: bool,
        supports_checkpoint: bool,
        has_unsaved_work: bool,
    ) -> zbus::Result<()>;

    /// Unsaved work and checkpoint freshness, with the times they were true.
    ///
    /// Separate from `set_instance_hints` because that method sets four facts at once, so a
    /// caller that only knows about one of them has to make something up about the other three —
    /// and the something it makes up about unsaved work erases whatever the last caller said.
    /// Pass `0` for a timestamp to mean "now".
    fn set_instance_work(
        &self,
        instance_id: &str,
        has_unsaved_work: bool,
        as_of_ms: i64,
        checkpoint_ok: bool,
        checkpoint_at_ms: i64,
    ) -> zbus::Result<()>;

    /// An instance ended.
    fn forget_instance(&self, instance_id: &str) -> zbus::Result<()>;

    /// Recorded observe-only traces, newest first, as JSON lines.
    fn recent_traces(&self, limit: u32) -> zbus::Result<String>;

    /// Pressure level changed.
    #[zbus(signal)]
    fn pressure_changed(&self, level: String, report_json: String) -> zbus::Result<()>;

    /// One decided action (`applied` is false in observe-only mode).
    #[zbus(signal)]
    fn pressure_action(
        &self,
        kind: String,
        instance_id: String,
        applied: bool,
        reason: String,
        detail_json: String,
    ) -> zbus::Result<()>;

    /// A stall the memory governor does not own.
    #[zbus(signal)]
    fn non_memory_stall(&self, kind: String, detail_json: String) -> zbus::Result<()>;
}
