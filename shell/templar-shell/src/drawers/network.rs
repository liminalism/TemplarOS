//! Network drawer — and why it has nothing to show yet.
//!
//! Planning doc 01 §1 lists a Network drawer of "network shares and remote machines". Nothing in
//! `interfaces/dbus/` answers that question: `templar-filed` enumerates *mounted* volumes
//! (`Files.Volumes1`, which is the Drives drawer, and which does include an already-mounted NFS
//! or SMB filesystem), but no service discovers shares, lists remote machines, or mounts them.
//! `gvfs`/`udisks2` are the usual sources for that and neither has a TemplarOS policy owner yet.
//!
//! Rather than invent shares, this drawer says plainly that it is not wired to a service. That
//! is the honest state and it is one line to change once a `Network1`-shaped interface exists —
//! whereas a fabricated list would look like a working feature and be found out only in use.
//!
//! Already-mounted remote filesystems are *not* silently omitted: they appear in Drives, and the
//! text below points there so the user is not left thinking the shell cannot see them at all.

use crate::drawers::DrawerFeed;

/// The Network drawer's content: a statement of what is and is not available.
pub fn render() -> String {
    feed().to_text()
}

pub fn feed() -> DrawerFeed {
    // Not `unavailable`: nothing failed to answer, there is simply no interface yet, and dimming
    // the tile as if a service were down would misreport the situation.
    DrawerFeed::new(
        "not wired to a service yet",
        vec![
            "(no TemplarOS interface discovers network shares or remote machines; already-mounted \
             remote filesystems appear in the Drives drawer)"
                .to_owned(),
        ],
    )
}
