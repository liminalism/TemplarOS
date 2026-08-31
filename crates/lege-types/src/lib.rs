//! Shared LegeOS vocabulary.
//!
//! One application instance has exactly one canonical [`AppId`], one [`InstanceId`], one cgroup,
//! one resource record, and any number of windows. Every service refers to that same instance
//! rather than rediscovering the application independently. A Wayland `app_id`, an X11 `WM_CLASS`,
//! a desktop-file ID, a Flatpak ID and an executable path are **aliases or evidence**, never
//! separate identities.
//!
//! This crate is dependency-light on purpose: it is linked into the compositor, the daemons,
//! the shell, the CLI and first-party applications alike.

#![forbid(unsafe_code)]

pub mod action;
pub mod app;
pub mod checkpoint;
pub mod dbus;
pub mod file_sort;
pub mod identity;
pub mod provider;
pub mod unit;

pub use action::{ActionCategory, ActionDecl, Chord, Key, Modifiers, Requires};
pub use app::{
    AppAliases, AppCatalog, AppId, AppIdError, AppManifest, InstanceId, ResourceClass, WindowClaims,
};
pub use identity::IdentityConfidence;
pub use provider::{ProviderCapability, ProviderRole};
