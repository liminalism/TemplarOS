//! `resourced`: the desktop-aware resource governor (planning docs 01 §5, 02 §5).
//!
//! Policy is **pure** ([`policy::Governor`]) and separated from cgroup writes
//! ([`executor`]), so it is unit-testable and can run observe-only. The daemon starts
//! observe-only: it records pressure, identifies which applications *would* have been throttled,
//! and lets false positives be inspected before it is allowed to kill anything
//! ([`policy::Stage`], `crate::daemon`'s staged enablement ladder).
//!
//! This is a separate, small, protected process (ADR-0002): it must keep ticking under exactly
//! the conditions that stall a daemon doing SQLite, fsync or D-Bus work. Emergency actions never
//! need a D-Bus round-trip — launch units are self-describing
//! (`app-legeos-<AppId>@<InstanceId>.service`), so identity comes from the cgroup path alone
//! ([`scan`]). `org.legeos.Resources1` ([`dbus`]) carries only *enrichment* and *observation*,
//! served from a task strictly separate from the sampling loop ([`daemon`]).

pub mod cgroup;
pub mod config;
pub mod daemon;
pub mod dbus;
pub mod executor;
pub mod nonmemory;
pub mod policy;
pub mod protect;
pub mod psi;
pub mod replay;
pub mod scan;
pub mod state;
pub mod trace;

pub use policy::{Action, AppSample, Governor, GovernorConfig, PressureLevel, Stage};
