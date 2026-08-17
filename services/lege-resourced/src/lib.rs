//! `resourced`: the desktop-aware resource governor (planning docs 01 §5, 02 §5).
//!
//! Policy is **pure** ([`policy::Governor`]) and separated from cgroup writes
//! ([`executor`]), so it is unit-testable and can run observe-only. The daemon starts
//! observe-only: it records pressure, identifies which applications *would* have been throttled,
//! and lets false positives be inspected before it is allowed to kill anything.
//!
//! This is a separate, small, protected process (ADR-0002): it must keep ticking under exactly
//! the conditions that stall a daemon doing SQLite, fsync or D-Bus work. Emergency actions never
//! need a D-Bus round-trip — launch units are self-describing
//! (`app-legeos-<AppId>@<InstanceId>.service`), so identity comes from the cgroup path alone.

pub mod cgroup;
pub mod executor;
pub mod policy;
pub mod psi;
pub mod scan;

pub use policy::{Action, AppSample, Governor, GovernorConfig, PressureLevel};
