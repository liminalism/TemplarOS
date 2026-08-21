//! Typed zbus proxies for every `org.legeos.*` interface, one module per owning daemon.
//!
//! `interfaces/dbus/*.xml` is the contract; these proxies are its Rust side and
//! `cargo xtask check-interfaces` fails the build when the two drift apart. Everything is
//! re-exported flat, so `lege_client::proxies::AppIdentityProxy` keeps working.

pub mod files;
pub mod host;
pub mod resources;
pub mod session;

pub use files::*;
pub use host::*;
pub use resources::*;
pub use session::*;
