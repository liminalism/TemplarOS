//! Drawers as live views over the services, per `docs/planning/01-initial-shape.md`'s Home
//! Surface table. Each module: a small async client trait (implemented for real directly on the
//! matching `lege_client::proxies::*Proxy` type, and by a fake in its own tests), and a `render`
//! function that turns the service's answer into the lines the drawer shows.
//!
//! `crates/lege-window` (`shell.rs`, `layers.rs`) and this crate's own [`crate::home_surface`]
//! own the *set* of drawers; nothing here invents a new one.

pub mod network;
pub mod operations;
pub mod projects;
pub mod recent;
pub mod standard_paths;
pub mod trash;
pub mod volumes;
pub mod window_shelf;
