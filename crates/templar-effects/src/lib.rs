//! The TemplarOS effect library: WGSL shaders and the pure planning behind them.
//!
//! * [`essence`] — the transition for minimize, restore and the desktop button: a window
//!   deconstructs into essence that flows through the gaps between the windows that stay, to
//!   its Home card or into the Homegarden, and comes back the same way. Planned once per
//!   trigger on a bounded coarse field; per frame, one uniform write and two draws.
//! * [`surface`] — materials for the few surfaces that wear one (notifications, the login
//!   screen, Home's ground): glass, etched glass, stained glass, stone, glassy stone, metal
//!   sheen and rough wood.
//! * [`bar`] — pointer response for a bottom-bar plate: a quiet glass sheen that sweeps on
//!   hover, a bounded wobble and refraction while the pointer is engaged, and a click that
//!   shrinks the plate and washes a ring across it.
//! * [`noise`] — the one small tileable noise texture the shaders sample.
//! * [`shaders`] — the WGSL sources and their entry points.
//!
//! This crate owns no GPU state and has no dependencies. Hosts: `tools/effects-sandbox` (a
//! standalone wgpu sandbox, the place to tune all of this) today; the compositor later, where
//! `essence` replaces the park motion in `templar-comp/src/motion.rs`.

#![forbid(unsafe_code)]
// Grid code indexes several parallel per-cell arrays by one cell index.
#![allow(
    clippy::needless_range_loop,
    clippy::similar_names,
    clippy::many_single_char_names
)]

pub mod bar;
pub mod essence;
pub mod geometry;
pub mod noise;
pub mod shaders;
pub mod surface;

pub use bar::{Gesture, Optics, Plate};
pub use essence::{Direction, Mote, Plan, Planner, Request, Run, Sink, Style, Timing};
pub use geometry::Rect;
pub use surface::{Material, Surface};
