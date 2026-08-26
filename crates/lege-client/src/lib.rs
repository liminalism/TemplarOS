//! Client library for LegeOS services and the SDK first-party applications link.
//!
//! * [`proxies`] — typed zbus proxies for `org.legeos.*` interfaces.
//! * [`sdk`] — what an application needs: its instance identity from the environment, the
//!   checkpoint protocol, history annotations.

#![forbid(unsafe_code)]

pub mod files;
pub mod proxies;
pub mod sdk;

pub use lege_types::dbus as names;
