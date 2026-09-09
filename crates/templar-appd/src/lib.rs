//! `appd`: canonical application identity, launch brokering and instance tracking.
//!
//! Identity rule for a normal launch (planning doc 02 §3):
//! 1. the registry creates the instance;
//! 2. the process starts in a dedicated systemd unit and cgroup named
//!    `app-templaros-<AppId>@<InstanceId>.service`;
//! 3. the compositor obtains the client PID and asks which registered instance owns that
//!    PID's cgroup;
//! 4. client-supplied `app_id` / `WM_CLASS` are checked for consistency; a mismatch is logged
//!    but cannot replace the verified identity.
//!
//! This crate is a library hosted by `templar-sessiond`; it owns no process of its own.

#![forbid(unsafe_code)]

pub mod dbus;
pub mod desktop_entry;
pub mod identity;
pub mod launcher;
pub mod manifests;
pub mod store;
pub mod systemd1;
pub mod variants;

pub use identity::{IdentityResolution, InstanceRecord, Registry};
pub use templar_types::IdentityConfidence;
pub use variants::{LogicalIndex, LogicalSource, PersistedLogical};
