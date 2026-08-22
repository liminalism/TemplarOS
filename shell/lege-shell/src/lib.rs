//! The Home Surface as a model over the services, and its text renderer.
//!
//! `lege-shell` is split into a library and a thin binary for the same reason `cli/lege` is: the
//! part worth testing is the model — what each drawer asks the services for, and how the answer
//! reads — and none of it should need a process, a session bus or a compositor to exercise. The
//! binary is argument parsing and dispatch over what lives here.
//!
//! No UI toolkit is linked. libcosmic is a large git dependency and the first real surface
//! belongs to a later milestone; what this crate owns now is the part a toolkit would merely
//! render. Where a producer genuinely does not exist yet — the compositor's private
//! `lege_shell_v1` protocol has no client anywhere in this workspace — the consumer is built and
//! tested against the real type, and says so rather than pretending.

// Everything except `wayland_bridge` stays unsafe-free, as it always has (M1's `#![forbid]`
// weakened to `#![deny]` only because `deny` — unlike `forbid` — can be locally lifted, which
// `wayland_bridge`'s module doc explains the one, narrow need for: winit hands out a Wayland
// window only as a raw pointer, and reaching the compositor's private protocol on that window's
// own connection has no safe API for it).
#![deny(unsafe_code)]

pub mod data;
pub mod dock;
pub mod drawers;
pub mod field_model;
pub mod format;
pub mod home_surface;
pub mod icons;
pub mod pressure;
pub mod protocol;
pub mod settings;
pub mod surface;
pub mod wayland_bridge;
