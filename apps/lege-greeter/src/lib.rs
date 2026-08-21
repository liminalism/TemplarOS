//! `lege-greeter`: a greetd JSON-IPC client (`crate::protocol`, `crate::client`, `crate::login`)
//! with a pixelkit full-screen login surface (`crate::screen`, `crate::surface`) and session
//! discovery (`crate::sessions`). See `src/main.rs`'s module doc for how the one binary this crate
//! builds decides, at startup, whether it needs to start its own compositor first.
//!
//! ADR-0010, as amended by ADR-0012: greetd owns PAM; this crate links no authentication code of
//! its own — no PAM, no shadow, no setuid — only greetd's own IPC protocol, implemented directly
//! (`crate::protocol`'s doc explains why not as a dependency on the existing `greetd_ipc` crate).

pub mod client;
pub mod login;
pub mod protocol;
pub mod screen;
pub mod sessions;
pub mod surface;
