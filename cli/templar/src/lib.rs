//! Library half of the `templar` CLI: the doctor engine, reusable by `tests/distro-checks`
//! (image-time) and `tests/hardware` (matrix).

#![forbid(unsafe_code)]

pub mod account;
pub mod archive;
pub mod bus;
pub mod dev;
pub mod doctor;
pub mod files;
pub mod format;
pub mod garden_session;
pub mod history;
pub mod humanize;
pub mod lifecycle;
pub mod notify;
pub mod open;
pub mod power;
pub mod project;
pub mod providers;
pub mod res;
pub mod settings;
pub mod status;
pub mod terminal;
pub mod update;
pub mod wallpaper;
pub mod workload;
