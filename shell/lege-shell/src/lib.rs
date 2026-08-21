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

#![forbid(unsafe_code)]

pub mod dock;
pub mod drawers;
pub mod format;
pub mod home_surface;
pub mod pressure;
pub mod settings;
