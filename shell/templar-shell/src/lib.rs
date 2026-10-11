//! The Home Surface as a model over the services, and its text renderer.
//!
//! `templar-shell` is split into a library and a thin binary for the same reason `cli/templar` is: the
//! part worth testing is the model — what each drawer asks the services for, and how the answer
//! reads — and none of it should need a process, a session bus or a compositor to exercise. The
//! binary is argument parsing and dispatch over what lives here.
//!
//! The graphical surface uses PixelKit and the compositor's authenticated private
//! `templar_shell_v2` connection; text-only renderers retain service availability distinctions
//! rather than treating an unavailable producer as an empty desktop.

// Everything except `wayland_bridge` stays unsafe-free, as it always has (M1's `#![forbid]`
// weakened to `#![deny]` only because `deny` — unlike `forbid` — can be locally lifted, which
// `wayland_bridge`'s module doc explains the one, narrow need for: winit hands out a Wayland
// window only as a raw pointer, and reaching the compositor's private protocol on that window's
// own connection has no safe API for it).
#![deny(unsafe_code)]

pub mod app_search;
pub mod clipboard;
pub mod data;
pub mod data_device;
pub mod drawers;
pub mod field_model;
pub mod file_browser;
pub mod files_surface;
pub mod format;
pub mod garden;
pub mod home_surface;
pub mod icons;
pub mod launcher_model;
pub mod omnibox;
pub mod pressure;
pub mod previews;
pub mod protocol;
pub mod rail_model;
pub mod recents;
pub mod screenshot;
pub mod session_lock;
pub mod settings;
pub mod surface;
pub mod taskbar;
pub mod unlock;
pub mod wayland_bridge;
pub mod work_layout;
pub mod work_positions;
