//! Session policy domains hosted by `templar-sessiond` (ADR-0001).
//!
//! Each module is a self-contained, I/O-free policy core with its own types; the daemon wires
//! them to D-Bus interfaces and to the shared state store. Rule of the house: **system behaviour
//! belongs in services; tray icons and panels are views.**

#![forbid(unsafe_code)]

pub mod autostart;
pub mod change_log;
pub mod file_view;
pub mod first_run;
pub mod history;
pub mod homefield;
pub mod homegarden;
pub mod location;
pub mod notifications;
pub mod power;
pub mod projects;
pub mod providers;
pub mod settings;
