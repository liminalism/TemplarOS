//! Shared TemplarOS vocabulary.
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
pub mod appearance;
pub mod checkpoint;
pub mod compositor_control;
pub mod dbus;
pub mod display;
pub mod file_sort;
pub mod identity;
pub mod input;
pub mod native_path;
pub mod provider;
pub mod settings;
pub mod unit;
pub mod uri_list;
pub mod variant;
pub mod wallpaper;
pub mod workload;

pub use action::{ActionCategory, ActionDecl, Chord, Key, Modifiers, Requires};
pub use app::{
    AppAliases, AppCatalog, AppId, AppIdError, AppManifest, InstanceId, ResourceClass, WindowClaims,
};
pub use identity::IdentityConfidence;
pub use provider::{ProviderCapability, ProviderRole};
pub use settings::SettingsPage;
pub use variant::{
    AppOriginKind, AppProvenance, AppVariantId, LaunchMechanism, LogicalAppId, TrustTier,
    UpdateOwner,
};
pub use workload::{
    ResourceId, ResourceIdentity, ResourceMetadata, WorkloadClass, WorkloadId, WorkloadRecord,
};
