//! `associationd`: the single source of truth for "which application opens this?".
//!
//! One engine backs file-manager double-clicks, shell search results, "Open with", URI schemes,
//! the XDG OpenURI/OpenFile portal and the compatibility export to `mimeapps.list`
//! (planning doc 02 §4). Detected MIME type is stronger than an ordinary extension rule; a
//! mislabelled executable does not inherit the handler for its misleading suffix.
//!
//! Library crate hosted by `lege-sessiond`.

#![forbid(unsafe_code)]

pub mod dbus;
pub mod detector;
pub mod engine;
pub mod export;

pub use engine::{AssociationDb, AssociationEngine, DecisionReason, ExtensionRule, OpenDecision};
