//! The greeter as a real full-screen window — pixelkit driving the same `crate::login`/
//! `crate::sessions`/`crate::screen` models a headless test exercises with no window at all.

pub mod app;
pub mod fonts;
pub mod window;

pub use app::run;
