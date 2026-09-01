//! Library half of the `lege` CLI: the doctor engine, reusable by `tests/distro-checks`
//! (image-time) and `tests/hardware` (matrix).

#![forbid(unsafe_code)]

pub mod bus;
pub mod dev;
pub mod doctor;
pub mod files;
pub mod format;
pub mod history;
pub mod humanize;
pub mod lifecycle;
pub mod open;
pub mod power;
pub mod project;
pub mod providers;
pub mod res;
pub mod settings;
pub mod status;
pub mod update;
