//! Plain-text clipboard for TemplarOS.
//!
//! The file clipboard (`templar-shell`'s `clipboard` plus `data_device` transport, and
//! `templar_types::uri_list`) already carries path sets between Home, the Files app and every
//! GTK or Qt application on the machine. What was missing is the same treatment for *text*: a
//! search string, a copied path, a line out of a dialog. That text travels as `text/plain`
//! between native Wayland clients and as `UTF8_STRING`/`TEXT`/`STRING` across the XWayland
//! bridge, and every side spells the offer slightly differently — which is what this crate
//! normalises.
//!
//! Nothing here talks to a compositor. This is the model and the encoding, both pure and both
//! tested without a clipboard; the transports stay where they already are:
//! `templar-shell`'s `wayland_bridge` (seat selection), the future `org.freedesktop.portal.Clipboard`
//! backend, and the compositor's data-control wiring. Mirrors the split `data_device.rs`
//! documents: the encodings are got exactly right with no live seat, and everything that
//! genuinely needs one is confined to the caller.
//!
//! [`TextClipboard`] decides *what* a text copy means; [`TextPayload`] renders the bytes each
//! offered MIME type serves; [`choose_target`] picks what to ask a foreign offer for.

#![forbid(unsafe_code)]

pub mod text;

pub use text::{
    STRING, TEXT, TEXT_PLAIN, TEXT_PLAIN_UTF8, TextClipboard, TextPayload, TextTarget, UTF8_STRING,
    choose_target, from_transfer,
};
