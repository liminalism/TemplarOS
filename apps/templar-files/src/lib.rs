//! Standalone TemplarOS Files application.

#![forbid(unsafe_code)]

pub mod actions;
pub mod app;
pub mod backend;
pub mod format;
pub mod model;
pub mod surface;

mod fonts;

pub use app::{run_graphical, run_print};
