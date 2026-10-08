//! Explicit work identity is distinct from the application instance that started it.
use std::{fmt, path::PathBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::{AppId, InstanceId, ResourceClass};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkloadId(Uuid);

impl WorkloadId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn parse(text: &str) -> Option<Self> { Uuid::parse_str(text).ok().map(Self) }
    pub const fn uuid(self) -> Uuid { self.0 }
    pub fn compact(self) -> uuid::fmt::Simple { self.0.simple() }
}
impl Default for WorkloadId { fn default() -> Self { Self::new() } }
impl fmt::Display for WorkloadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(formatter) }
}

/// Governor key for a managed cgroup. The explicit identity kind is separate metadata.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceId(Uuid);
impl ResourceId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }
    pub fn parse(text: &str) -> Option<Self> { Uuid::parse_str(text).ok().map(Self) }
    pub const fn from_uuid(uuid: Uuid) -> Self { Self(uuid) }
    pub const fn uuid(self) -> Uuid { self.0 }

    /// Stable identity of an externally managed unit incarnation, not an application grant.
    pub fn external_unit(name: &str, inode: u64) -> Self {
        Self(Uuid::new_v5(&Uuid::from_u128(u128::from(inode)), name.as_bytes()))
    }
}
impl Default for ResourceId { fn default() -> Self { Self::new() } }
impl From<InstanceId> for ResourceId { fn from(id: InstanceId) -> Self { Self(id.uuid()) } }
impl From<WorkloadId> for ResourceId { fn from(id: WorkloadId) -> Self { Self(id.uuid()) } }
impl fmt::Display for ResourceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(formatter) }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadClass {
    Interactive,
    #[default]
    BackgroundCompute,
}
impl WorkloadClass {
    pub const fn as_str(self) -> &'static str {
        match self { Self::Interactive => "interactive", Self::BackgroundCompute => "background-compute" }
    }
    pub const fn resource_class(self) -> ResourceClass {
        match self { Self::Interactive => ResourceClass::Interactive, Self::BackgroundCompute => ResourceClass::Background }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkloadRecord {
    pub id: WorkloadId,
    /// None is explicitly unknown, never inferred from a process name or window title.
    pub spawned_by: Option<InstanceId>,
    pub parent_application: Option<AppId>,
    pub systemd_unit: String,
    pub control_group: PathBuf,
    /// Declared executable summary only, not secret-bearing arguments or identity evidence.
    pub command_summary: String,
    pub class: WorkloadClass,
}

/// Resource classification is observation, not application permission evidence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ResourceIdentity {
    Application { app_id: AppId },
    Workload { workload_id: WorkloadId, spawned_by: Option<InstanceId>, class: WorkloadClass },
    Flatpak { claimed_app_id: String },
}

impl ResourceIdentity {
    pub fn application(&self) -> Option<&AppId> {
        match self { Self::Application { app_id } => Some(app_id), _ => None }
    }
}
impl fmt::Display for ResourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Application { app_id } => app_id.fmt(formatter),
            Self::Workload { workload_id, .. } => write!(formatter, "workload {workload_id}"),
            Self::Flatpak { claimed_app_id } => write!(formatter, "Flatpak scope claim {claimed_app_id}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceMetadata {
    pub identity: ResourceIdentity,
    pub control_group: PathBuf,
}
