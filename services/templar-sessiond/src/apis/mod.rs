//! D-Bus faces of the session-core domains, one module per interface. Each wraps a pure
//! `templar-session-core` type behind `Arc<RwLock<_>>`, persists through `templar-state` in the same
//! transaction as any history/undo-log write, and, where the interface documents one, emits a
//! `Changed` signal afterward.

pub mod appearance;
pub mod audio;
pub mod bluetooth;
pub mod device_prompt;
pub mod displays;
pub mod input;
pub mod background;
pub mod backlight;
pub mod battery;
pub mod history;
pub mod homefield;
pub mod homegarden;
pub mod lifecycle;
pub mod location;
pub mod notifications;
pub mod network;
pub mod power;
pub mod projects;
pub mod providers;
pub mod secret_store;
pub mod settings;
pub mod tab_counts;
pub mod wallpaper;

// Takes the original error by value so callers can use `.map_err(failed)` without boxing.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn failed(error: impl std::fmt::Display) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(error.to_string())
}
