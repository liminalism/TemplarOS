//! Apply governor actions to cgroups — or decide not to, when the current [`Stage`] does not yet
//! permit it (planning doc 02 §8's staged enablement ladder).
//!
//! `ManagedInstance` (and everything else here) is deliberately loop-thread-local, never shared
//! with the D-Bus interface: the bus reads a published [`crate::state::Report`] snapshot instead
//! (see `crate::state`'s module doc comment for why).

use crate::{
    cgroup::Cgroup,
    policy::{Action, Stage},
};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::{collections::HashMap, time::Instant};
use templar_types::{ResourceId, ResourceIdentity, ResourceMetadata};

/// A tracked application instance, private to the sampling loop.
#[derive(Clone, Debug)]
pub struct ManagedInstance {
    pub metadata: Arc<ResourceMetadata>,
    pub cgroup: Cgroup,
    /// Original policy before temporary pressure overrides. Application units are launched with
    /// no static `MemoryHigh=` (`templar-appd`'s launcher), so "restore" always means "max" today —
    /// this field is the seam for a future manifest-declared limit.
    pub configured_memory_high: Option<u64>,
    /// The `memory.high` value the governor itself last wrote via `Action::SetMemoryHigh`, if
    /// any override is currently outstanding. `None` means the governor has never touched (or
    /// has already restored) this instance's `memory.high` — `Action::RestoreDynamicLimits` must
    /// only ever write when this is `Some`, never unconditionally: it is decided for every
    /// running instance on every Normal-pressure tick, and a governor that writes regardless of
    /// this flag would touch `memory.high` on every application, every tick, forever — including
    /// in observe-only, and including clobbering a future manifest- or drop-in-declared limit it
    /// never set in the first place.
    pub active_override: Option<u64>,
    /// `(memory_current, sampled_at)` from the previous tick, for the growth-rate calculation.
    pub last_memory_sample: Option<(u64, Instant)>,
    /// Whether `memory.oom.group=1` has been applied (item 3: applied once at registration, not
    /// re-written every tick).
    pub oom_group_applied: bool,
    pub first_seen: Instant,
}

impl ManagedInstance {
    pub fn new(identity: ResourceIdentity, cgroup: Cgroup, now: Instant) -> Self {
        let metadata = Arc::new(ResourceMetadata {
            identity,
            control_group: cgroup.control_group_path(),
        });
        Self {
            metadata,
            cgroup,
            configured_memory_high: None,
            active_override: None,
            last_memory_sample: None,
            oom_group_applied: false,
            first_seen: now,
        }
    }

    /// Growth in bytes/second since the last sample, given `current` at `now`; also updates the
    /// stored sample. Zero on the very first sample (no prior point to compare against).
    pub fn record_sample(&mut self, current: u64, now: Instant) -> i64 {
        let growth = self.last_memory_sample.map_or(0, |(previous, at)| {
            let seconds = now.saturating_duration_since(at).as_secs_f64().max(0.001);
            ((current as f64 - previous as f64) / seconds) as i64
        });
        self.last_memory_sample = Some((current, now));
        growth
    }
}

/// Apply one action if `stage` permits it (`Action::minimum_stage`); otherwise it is a no-op that
/// still returns `Ok(false)` so the caller can log/signal it as decided-but-not-applied. `Ok(true)`
/// means a cgroup write (or, for `TrimCaches`/`RequestCheckpoint`, the cooperative forward to
/// sessiond that the `PressureAction` signal itself constitutes) actually happened.
///
/// `RestoreDynamicLimits` is the one action whose `minimum_stage` is `Stage::ObserveOnly` (it is
/// always "at or above" the current stage) — so the *only* thing that can make it a no-op is
/// `ManagedInstance::active_override` being `None`. That guard, not the stage check, is what
/// keeps observe-only from writing to any cgroup at all.
pub fn apply_action(
    instances: &mut HashMap<ResourceId, ManagedInstance>,
    action: &Action,
    stage: Stage,
) -> Result<bool> {
    if action.minimum_stage() > stage {
        tracing::debug!(?action, %stage, "decided but not performed: above the current stage");
        return Ok(false);
    }

    match action {
        Action::TrimCaches { instance_id } => {
            // Forwarded as a CheckpointRequest::PrepareForPressure through sessiond, which
            // subscribes to `PressureAction{kind: "trim-caches", applied: true, ..}`; nothing to
            // write here.
            tracing::debug!(%instance_id, "cache trim requested");
            Ok(true)
        }
        Action::SetMemoryHigh { instance_id, bytes } => {
            let managed = instance_mut(instances, *instance_id)?;
            managed.cgroup.set_memory_high(Some(*bytes))?;
            managed.active_override = Some(*bytes);
            Ok(true)
        }
        Action::Reclaim { instance_id, bytes } => {
            if *bytes > 0 {
                instance(instances, *instance_id)?
                    .cgroup
                    .request_reclaim(*bytes)?;
                return Ok(true);
            }
            Ok(false)
        }
        Action::RequestCheckpoint {
            instance_id,
            deadline,
        } => {
            tracing::debug!(%instance_id, ?deadline, "checkpoint requested");
            Ok(true)
        }
        Action::KillCgroup {
            instance_id,
            reason,
        } => {
            tracing::warn!(%instance_id, %reason, "terminating application group");
            instance(instances, *instance_id)?.cgroup.kill_all()?;
            Ok(true)
        }
        Action::RestoreDynamicLimits { instance_id } => {
            let managed = instance_mut(instances, *instance_id)?;
            if managed.active_override.is_none() {
                // Nothing the governor itself set is outstanding: this is the every-tick,
                // every-application case at Normal pressure. Decided, never performed.
                return Ok(false);
            }
            managed
                .cgroup
                .set_memory_high(managed.configured_memory_high)?;
            managed.active_override = None;
            Ok(true)
        }
    }
}

fn instance(
    instances: &HashMap<ResourceId, ManagedInstance>,
    id: ResourceId,
) -> Result<&ManagedInstance> {
    instances
        .get(&id)
        .with_context(|| format!("unknown managed instance {id}"))
}

fn instance_mut(
    instances: &mut HashMap<ResourceId, ManagedInstance>,
    id: ResourceId,
) -> Result<&mut ManagedInstance> {
    instances
        .get_mut(&id)
        .with_context(|| format!("unknown managed instance {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::Cgroup;
    use std::{fs, time::Duration};

    fn cgroup_fixture(name: &str) -> Cgroup {
        let dir = std::env::temp_dir().join(format!(
            "templar-resourced-executor-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for file in ["memory.high", "memory.max", "memory.reclaim", "cgroup.kill"] {
            fs::write(dir.join(file), "0\n").unwrap();
        }
        Cgroup::from_directory(dir)
    }

    fn app_id() -> ResourceIdentity {
        ResourceIdentity::Application {
            app_id: "org.templaros.Test".parse().unwrap(),
        }
    }

    #[test]
    fn actions_below_the_stage_are_not_performed() {
        let id = ResourceId::new();
        let cgroup = cgroup_fixture("below");
        let dir = cgroup.directory().to_path_buf();
        let mut instances = HashMap::new();
        instances.insert(id, ManagedInstance::new(app_id(), cgroup, Instant::now()));

        let applied = apply_action(
            &mut instances,
            &Action::SetMemoryHigh {
                instance_id: id,
                bytes: 1024,
            },
            Stage::Trim, // SetMemoryHigh needs Throttle
        )
        .unwrap();
        assert!(!applied);
        assert_eq!(instances[&id].active_override, None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn actions_at_or_above_the_stage_write_the_cgroup() {
        let id = ResourceId::new();
        let cgroup = cgroup_fixture("above");
        let dir = cgroup.directory().to_path_buf();
        let mut instances = HashMap::new();
        instances.insert(id, ManagedInstance::new(app_id(), cgroup, Instant::now()));

        let applied = apply_action(
            &mut instances,
            &Action::SetMemoryHigh {
                instance_id: id,
                bytes: 4096,
            },
            Stage::Throttle,
        )
        .unwrap();
        assert!(applied);
        assert_eq!(
            fs::read_to_string(dir.join("memory.high")).unwrap().trim(),
            "4096"
        );
        assert_eq!(instances[&id].active_override, Some(4096));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_dynamic_limits_writes_when_an_override_is_active() {
        let id = ResourceId::new();
        let cgroup = cgroup_fixture("restore");
        let dir = cgroup.directory().to_path_buf();
        let mut managed = ManagedInstance::new(app_id(), cgroup, Instant::now());
        managed.configured_memory_high = Some(2048);
        managed.active_override = Some(9999); // the governor previously overrode this instance
        let mut instances = HashMap::new();
        instances.insert(id, managed);

        let applied = apply_action(
            &mut instances,
            &Action::RestoreDynamicLimits { instance_id: id },
            Stage::ObserveOnly, // RestoreDynamicLimits is always at or above the stage
        )
        .unwrap();
        assert!(applied);
        assert_eq!(
            fs::read_to_string(dir.join("memory.high")).unwrap().trim(),
            "2048"
        );
        assert_eq!(instances[&id].active_override, None);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The defect this guards against: `RestoreDynamicLimits` is decided for every running
    /// instance on every Normal-pressure tick (`Governor::decide`), regardless of stage. Without
    /// this guard, `templar-resourced` would `open(O_NONBLOCK) + write` `memory.high` into every
    /// `app.slice` cgroup once per second forever, even in observe-only, and even clobber a
    /// `MemoryHigh=` nothing but the application's own unit ever set.
    #[test]
    fn restore_dynamic_limits_is_a_noop_without_an_active_override() {
        let id = ResourceId::new();
        let cgroup = cgroup_fixture("restore-noop");
        let dir = cgroup.directory().to_path_buf();
        let before = fs::read_to_string(dir.join("memory.high")).unwrap();
        let mut instances = HashMap::new();
        instances.insert(id, ManagedInstance::new(app_id(), cgroup, Instant::now()));

        // Try it at every stage, including full enforcement: the guard is about the absence of
        // an override, not about the stage.
        for stage in Stage::ALL {
            let applied = apply_action(
                &mut instances,
                &Action::RestoreDynamicLimits { instance_id: id },
                stage,
            )
            .unwrap();
            assert!(!applied, "no active override at stage {stage}");
        }

        let after = fs::read_to_string(dir.join("memory.high")).unwrap();
        assert_eq!(before, after, "memory.high must be untouched");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_instance_is_an_error_not_a_panic() {
        let mut instances = HashMap::new();
        let result = apply_action(
            &mut instances,
            &Action::KillCgroup {
                instance_id: ResourceId::new(),
                reason: "test".into(),
            },
            Stage::Terminate,
        );
        assert!(result.is_err());
    }

    #[test]
    fn record_sample_is_zero_on_the_first_reading_then_tracks_growth() {
        let mut managed = ManagedInstance::new(app_id(), cgroup_fixture("growth"), Instant::now());
        let dir = managed.cgroup.directory().to_path_buf();
        let t0 = Instant::now();
        assert_eq!(managed.record_sample(100, t0), 0);
        let t1 = t0 + Duration::from_secs(1);
        let growth = managed.record_sample(200, t1);
        assert_eq!(growth, 100);
        let _ = fs::remove_dir_all(&dir);
    }
}
