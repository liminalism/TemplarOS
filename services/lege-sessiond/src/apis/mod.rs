//! D-Bus faces of the session-core domains, one module per interface. Each wraps a pure
//! `lege-session-core` type behind `Arc<RwLock<_>>`, persists through `lege-state` in the same
//! transaction as any history/undo-log write, and, where the interface documents one, emits a
//! `Changed` signal afterward.

pub mod background;
pub mod history;
pub mod homefield;
pub mod lifecycle;
pub mod location;
pub mod notifications;
pub mod power;
pub mod projects;
pub mod providers;
pub mod settings;

// Takes `anyhow::Error` by value so it can be passed directly as `.map_err(failed)`.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn failed(error: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}
