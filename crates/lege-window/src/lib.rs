//! Window service: a library inside the compositor core (planning doc 02 §6).
//!
//! * a cached client of `appd` (never blocking D-Bus on the render thread);
//! * the owner of window geometry and identity records;
//! * a producer of sanitized toplevel information for the shell;
//! * the authority that enforces **one workspace** and window recoverability.
//!
//! No workspace ID appears anywhere here. The first implementation has exactly one scene and
//! therefore cannot accidentally move a window to an invisible workspace.

#![forbid(unsafe_code)]

pub mod geometry;
pub mod layers;
mod service;

pub use geometry::{Output, Point, Rect, is_recoverable, recovery_band, repair_window};
pub use layers::Layer;
pub use service::{
    ActivationGrant, CachedIdentity, DockGrouping, IdentityCache, ShellToplevel, WindowCommand,
    WindowId, WindowRecord, WindowService, grouping_key,
};
