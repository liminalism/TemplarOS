//! Apply governor actions to cgroups (or only log them in observe-only mode).

use crate::{cgroup::Cgroup, policy::Action};
use anyhow::{Context, Result};
use lege_types::InstanceId;
use std::collections::HashMap;

/// A tracked application instance.
#[derive(Clone, Debug)]
pub struct ManagedInstance {
    pub cgroup: Cgroup,
    /// Original policy before temporary pressure overrides.
    pub configured_memory_high: Option<u64>,
}

/// Executor mode. Observe-only is the default until traces have been reviewed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    ObserveOnly,
    Enforce,
}

/// Apply one action. Cooperative requests (trim, checkpoint) are forwarded through the session
/// daemon; the return value says whether anything was written.
pub fn apply_action(
    instances: &HashMap<InstanceId, ManagedInstance>,
    action: &Action,
    mode: Mode,
) -> Result<bool> {
    tracing::info!(?action, ?mode, "resource action");

    if mode == Mode::ObserveOnly {
        return Ok(false);
    }

    match action {
        Action::TrimCaches { instance_id } => {
            // Forwarded as a CheckpointRequest::PrepareForPressure through sessiond; nothing to
            // write here.
            tracing::debug!(%instance_id, "cache trim requested");
            Ok(false)
        }
        Action::SetMemoryHigh { instance_id, bytes } => {
            instance(instances, *instance_id)?
                .cgroup
                .set_memory_high(Some(*bytes))?;
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
            Ok(false)
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
            let managed = instance(instances, *instance_id)?;
            managed
                .cgroup
                .set_memory_high(managed.configured_memory_high)?;
            Ok(true)
        }
    }
}

fn instance(
    instances: &HashMap<InstanceId, ManagedInstance>,
    id: InstanceId,
) -> Result<&ManagedInstance> {
    instances
        .get(&id)
        .with_context(|| format!("unknown managed instance {id}"))
}
