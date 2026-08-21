//! Long-running or once-at-startup work that is not a direct answer to a D-Bus call: importing
//! `.desktop` files, running autostart, relaying checkpoint requests, ingesting crashes, and
//! hosting notifications only while selected.

pub mod autostart;
pub mod checkpoint_relay;
pub mod crash_ingest;
pub mod desktop_import;
pub mod notification_provider;
