//! Window service: a library inside the compositor core (planning doc 02 §6).
//!
//! * a cached client of `appd` (never blocking D-Bus on the render thread);
//! * the owner of window geometry and identity records;
//! * the z-order authority for typed overlay layers;
//! * a producer of sanitized toplevel information for the shell;
//! * the authority that enforces **one workspace** and window recoverability.
//!
//! No workspace ID appears anywhere here. The first implementation has exactly one scene and
//! therefore cannot accidentally move a window to an invisible workspace.

#![forbid(unsafe_code)]

pub mod arrangement;
pub mod commands;
pub mod geometry;
pub mod group;
pub mod layers;
mod service;
mod shell;
mod switch;

pub use arrangement::{ArrangementPreset, preset_rects};
pub use commands::{
    ActivationGrant, CommandContext, CommandEffect, CommandError, RevealHome, WindowCommand,
};
pub use geometry::{
    Output, PhysicalPoint, Point, RecoveryAffordance, Rect, ResizeEdges, bounding_rect,
    client_area, frame_around, is_beside, is_recoverable, logical_to_physical, move_into_view,
    physical_to_logical, place_beside, recovery_affordance, repair_window, resize_edges_at,
    resize_rect, scatter_layout, tag_strip,
};
pub use group::{GROUP_ACCENTS, GroupId, GroupRecord, accent_for};
pub use layers::{ClientKind, Layer, LayerError, LayerStack, StackPosition, may_request_layer};
pub use service::{
    CachedIdentity, Decoration, EscapeAction, IdentityCache, WindowId, WindowRecord, WindowRole,
    WindowService,
};
pub use shell::{DockGrouping, GroupInfo, ShellToplevel, grouping_key};
pub use switch::SwitchRing;

// Re-exported so `WindowClaims::default()` and friends stay reachable as `templar_window::WindowClaims`
// for crates that otherwise only need this crate.
pub use templar_types::WindowClaims;

#[cfg(test)]
mod test_support;
