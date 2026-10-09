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
pub mod task;
pub mod taskfield;

pub use arrangement::{ArrangementPreset, preset_rects};
pub use commands::{
    ActivationGrant, CommandContext, CommandEffect, CommandError, RevealHome, WindowCommand,
};
pub use geometry::{
    Corner, Direction, Output, PhysicalPoint, Point, RESIZE_SQUARE, RecoveryAffordance, Rect,
    ResizeEdges, TAB_ACTIVATION_DEPTH, TAB_CONTROL_GAP, TAB_CONTROL_SIZE, TAB_EXIT_GRACE_MS,
    TAB_HYSTERESIS, TAB_REVEAL_GRACE_MS, TAB_TITLE_MAX_WIDTH, TAB_TITLE_MIN_WIDTH, TabControl,
    TabPlacement, TabPolicy, WindowLayout, bounding_rect, is_beside, is_recoverable,
    logical_to_physical, move_into_view, physical_to_logical, place_beside, recovery_affordance,
    recovery_band, repair_window, resize_edges_at, resize_rect, scatter_layout,
};
pub use group::{GROUP_ACCENTS, GROUP_STRIPE_WIDTH, GroupId, GroupRecord, accent_for};
pub use layers::{ClientKind, Layer, LayerError, LayerStack, StackPosition, may_request_layer};
pub use service::{
    CachedIdentity, Decoration, EscapeAction, IdentityCache, Presentation, TabDeadlineFired,
    WindowId, WindowRecord, WindowRole, WindowService, WindowStacking,
};
pub use shell::{DockGrouping, GroupInfo, ShellToplevel, TabCount, grouping_key};
pub use switch::SwitchRing;
pub use task::{ChipAction, TaskKey, TaskUnit};
pub use taskfield::{Card, Section, TaskFilter, TaskfieldState};

// Re-exported so `WindowClaims::default()` and friends stay reachable as `templar_window::WindowClaims`
// for crates that otherwise only need this crate.
pub use templar_types::WindowClaims;

#[cfg(test)]
mod test_support;
