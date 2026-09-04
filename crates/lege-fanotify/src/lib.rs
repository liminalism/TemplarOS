//! fanotify for the LegeOS file index: the measured alternative to one inotify watch per
//! directory.
//!
//! Recursive inotify cannot cover a home tree on the reference laptop — 189,089 directories after
//! the index's skip policy against a `fs.inotify.max_user_watches` of 250,074, of which the
//! index's `WatchBudget` deliberately takes half. fanotify's headline answer is
//! `FAN_MARK_FILESYSTEM`: one mark for a whole filesystem instead of one per directory. This crate
//! exists to find out whether that answer is available to LegeOS, and what the unprivileged
//! variant costs if it is not. The numbers, and the recommendation they support, are in
//! `docs/measurements/fanotify-2026-09-04.md`.
//!
//! # Why this is a separate crate
//!
//! `lege-file-index` is `#![forbid(unsafe_code)]` and that is worth keeping: it is the crate that
//! decides what the index believes about the filesystem. No crate in the registry parses
//! fanotify's `FAN_REPORT_DFID_NAME` info records — `rustix` does not implement fanotify at all
//! (`not_implemented!(fanotify_init)`), and `nix`'s binding stops at the fixed-size event header —
//! so the choice was a new dependency that would not do the job, or six `libc` calls written here.
//!
//! The `unsafe` exception CLAUDE.md asks crates to document is therefore: **six foreign calls, all
//! in [`sys`], each a single scalar-and-pointer syscall wrapper.** Everything with real logic —
//! decoding the packed event stream, mapping a file handle to a directory, reconstructing a rename
//! — is safe code over `&[u8]`, unit-tested against hand-encoded kernel layouts in [`event`].
//!
//! # What it plugs into
//!
//! [`source::FanotifySource`] implements `lege_file_index::watch::EventSource`, the same seam
//! `InotifySource` and `FakeSource` sit behind. It is **selectable, not default**: nothing in
//! `lege-filed` constructs it, and this crate is not in any service's dependency graph.

#![allow(
    unsafe_code,
    reason = "fanotify has no safe binding in the ecosystem; the exception is scoped to `sys`, \
              where each `unsafe` block is one syscall, and every parsing decision is safe code"
)]

pub mod event;
pub mod group;
pub mod source;
pub mod sys;

pub use event::{DirKey, Event, FidRecord, ParseError};
pub use group::{Config, Group, MarkScope, limits};
pub use source::FanotifySource;

/// Whether this process may open the group `FanotifySource` needs.
///
/// Cheap enough to call at startup: it opens a group and closes it again.
#[must_use]
pub fn unprivileged_fanotify_available() -> bool {
    Group::open(Config::default()).is_ok()
}
