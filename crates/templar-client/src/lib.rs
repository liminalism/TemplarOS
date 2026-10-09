//! Client library for TemplarOS services and the SDK first-party applications link.
//!
//! * [`proxies`] — typed zbus proxies for `org.templaros.*` interfaces.
//! * [`status`] — volume, backlight and battery wire models.
//! * [`sdk`] — what an application needs: its instance identity from the environment, the
//!   checkpoint protocol, history annotations.

#![deny(unsafe_code)]

pub mod appearance;
pub mod compositor_control;
pub mod devices;
pub mod file_index;
pub mod files;
pub mod proxies;
pub mod sdk;
pub mod secret_fd;
pub mod status;
pub mod tree_source;
pub mod wallpaper;

pub use templar_types::dbus as names;
