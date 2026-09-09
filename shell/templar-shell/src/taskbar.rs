//! The bottom bar's entries: [`templar_window::task::units`] with the shell's names on it.
//!
//! The grouping is not computed here. `task::units` is the one task model — the same function the
//! compositor runs over the same records, so the island, Taskfield, `Alt+Tab` and the launcher
//! cannot disagree about what is running or in what order. This module adds only what that crate
//! deliberately has no business knowing: the display name (`field_model::short_name`,
//! `group_title`), the icon key, and the preview key each window's capture is filed under.
//!
//! **An entry is never named after a window.** A one-window application used to wear its window's
//! title, which is how a chip came to read `/mnt/Samsung980_1TB` — and it is the same redundancy
//! the bar spent a campaign removing: the window wears its own name on its own tag. The name of a
//! chip is the name of the *application*; the window titles live on the fan above it.

use templar_window::task::{TaskKey, TaskUnit};
use templar_window::{ShellToplevel, WindowId};

/// One window of an entry, as the fan needs it.
///
/// `preview` is what this window's capture is filed under
/// ([`crate::field_model::session_identifier`]) — carried rather than derived because the bar
/// never sees a `ShellToplevel`, and the key belongs to one function, not to two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryWindow {
    pub id: WindowId,
    pub title: String,
    pub preview: String,
}

/// One thing on the bar to switch to: a task unit, dressed for drawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The unit itself — its key, its windows in activation order, and which of them are parked.
    /// Every question about *what is running* is asked of this and never of the fields beside it.
    pub unit: TaskUnit,
    /// The name a person reads: the group's title, the application's short name, or an
    /// unidentified window's own title, which is all such a window has.
    pub label: String,
    /// The application whose icon this entry wears, when it is one.
    pub app_id: Option<String>,
    /// Every window in the unit, most recently activated first, parked ones included.
    pub windows: Vec<EntryWindow>,
    /// Whether the activated window is one of these.
    pub active: bool,
}

impl Entry {
    /// `group:<id>` for a window group, the canonical application id otherwise — which is also
    /// the key an icon is filed under, and the `key` `open_taskfield` carries.
    #[must_use]
    pub fn key(&self) -> String {
        self.unit.key.wire()
    }

    /// The group's accent; `None` for an application.
    #[must_use]
    pub fn accent(&self) -> Option<u32> {
        self.unit.accent
    }

    /// How many of the entry's windows are on screen: the number the chip's badge shows, and what
    /// decides whether a click activates, fans, or hands over to Taskfield.
    #[must_use]
    pub fn visible(&self) -> usize {
        self.unit.visible()
    }

    /// Whether every window here is parked — what makes the whole chip read as dimmed. A group
    /// with one member still open is not parked: something of it is on screen.
    #[must_use]
    pub fn all_parked(&self) -> bool {
        self.unit.all_shelved()
    }

    #[must_use]
    pub fn is_parked(&self, window: WindowId) -> bool {
        self.unit.shelved.contains(&window)
    }

    /// The one window a `ChipAction::Direct` click goes to: the most recently activated one that
    /// is on screen, or — when the whole unit is parked — the most recent of the parked ones,
    /// which the caller unparks first.
    #[must_use]
    pub fn most_recent(&self) -> Option<WindowId> {
        self.unit
            .visible_windows()
            .next()
            .or_else(|| self.unit.windows.first().copied())
    }
}

/// Every entry, in [`templar_window::task::units`]'s order: explicit groups first, then
/// applications in first-seen order. Activating never reorders them — the chips are spatial
/// memory.
#[must_use]
pub fn entries(toplevels: &[ShellToplevel]) -> Vec<Entry> {
    templar_window::task::units(toplevels)
        .into_iter()
        .map(|unit| {
            let windows: Vec<EntryWindow> = unit
                .windows
                .iter()
                .filter_map(|id| toplevels.iter().find(|top| top.window_id == *id))
                .map(entry_window)
                .collect();
            let active = toplevels
                .iter()
                .any(|top| top.activated && unit.windows.contains(&top.window_id));
            Entry {
                label: label_of(&unit),
                app_id: match &unit.key {
                    TaskKey::App(app) => Some(app.to_string()),
                    _ => None,
                },
                windows,
                active,
                unit,
            }
        })
        .collect()
}

/// The name to draw. `TaskUnit::label` is deliberately raw — `templar-window` has neither the
/// display-name rules nor the icon cache — so the prettifying happens here, once.
fn label_of(unit: &TaskUnit) -> String {
    match &unit.key {
        TaskKey::Group(_) => crate::field_model::group_title(&unit.label, unit.windows.len()),
        TaskKey::App(app) => crate::field_model::short_name(app),
        // Nothing but the window's own title exists for a window whose identity did not resolve,
        // and it must never be merged with anything, so its title is its name.
        TaskKey::Unknown(_) => title_or_untitled(&unit.label),
    }
}

fn entry_window(toplevel: &ShellToplevel) -> EntryWindow {
    EntryWindow {
        id: toplevel.window_id,
        title: title_or_untitled(&toplevel.title),
        preview: preview_key(toplevel),
    }
}

/// What this window's capture is filed under — the same key Home's session tiles use, so the
/// bar and Home read one cache rather than two.
fn preview_key(toplevel: &ShellToplevel) -> String {
    crate::field_model::session_identifier(toplevel, None)
}

fn title_or_untitled(title: &str) -> String {
    if title.is_empty() {
        "(untitled)".to_owned()
    } else {
        title.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_window::{GroupInfo, Layer, Rect};

    fn window(id: u64, app: &str, title: &str, group: Option<u64>) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: false,
            mru_rank: u32::try_from(id).unwrap_or(u32::MAX),
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 100, 100),
            layer: Layer::Normal,
            group: group.map(|group_id| GroupInfo {
                group_id,
                name: String::new(),
                accent: 0x0011_2233,
            }),
        }
    }

    #[test]
    fn groups_come_first_then_one_entry_per_application() {
        let mut editor = window(3, "org.example.Editor", "notes", None);
        editor.activated = true;
        let toplevels = vec![
            window(1, "org.templaros.Files", "Downloads", None),
            window(2, "org.templaros.Files", "Pictures", None),
            editor,
            window(4, "org.example.Term", "sh", Some(9)),
            window(5, "org.example.Browser", "docs", Some(9)),
        ];
        let entries = entries(&toplevels);
        let keys: Vec<String> = entries.iter().map(Entry::key).collect();
        // Groups first, then applications in *first-seen* order — the order `task::units` fixes
        // and activation never disturbs. Not key order: a chip is spatial memory, and sorting by
        // name makes the strip resort itself the moment an application opens.
        assert_eq!(
            keys,
            vec!["group:9", "org.templaros.Files", "org.example.Editor"]
        );
        assert_eq!(entries[0].windows.len(), 2);
        // The hover row looks a preview up by this key, and Home files it under that same one:
        // one cache, one formula, whichever branch of `entries` built the window.
        for entry in &entries {
            for window in &entry.windows {
                let toplevel = toplevels
                    .iter()
                    .find(|t| t.window_id == window.id)
                    .expect("every entry window is a toplevel");
                assert_eq!(
                    window.preview,
                    crate::field_model::session_identifier(toplevel, None)
                );
            }
        }
        assert_eq!(entries[0].label, "2 windows");
        assert_eq!(entries[0].accent(), Some(0x0011_2233));
        // Two windows of one application are one entry, named for the application.
        assert_eq!(entries[1].label, "Files");
        assert_eq!(entries[1].visible(), 2);
        assert!(!entries[1].active);
        // And a one-window application is named for the application too — never for its window.
        // A chip that wore its window's title read "/mnt/Samsung980_1TB" on a terminal.
        assert_eq!(entries[2].label, "Editor");
        assert_eq!(entries[2].most_recent(), Some(WindowId(3)));
        assert!(entries[2].active);
    }

    #[test]
    fn grouped_windows_never_double_as_application_entries() {
        let toplevels = vec![
            window(4, "org.example.Term", "sh", Some(9)),
            window(7, "org.example.Browser", "docs", Some(9)),
        ];
        let entries = entries(&toplevels);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key(), "group:9");
        assert_eq!(entries[0].label, "2 windows");
    }

    #[test]
    fn a_parked_window_keeps_its_place_on_the_bar() {
        let mut parked = window(1, "org.templaros.Files", "Downloads", None);
        parked.shelved = true;
        let toplevels = vec![
            parked,
            window(2, "org.example.Term", "sh", Some(9)),
            window(3, "org.example.Browser", "docs", Some(9)),
        ];
        let entries = entries(&toplevels);
        let files = entries
            .iter()
            .find(|e| e.key() == "org.templaros.Files")
            .unwrap();
        assert!(files.all_parked(), "a parked window is still on the bar");
        assert!(files.is_parked(WindowId(1)));
        let group = entries.iter().find(|e| e.key() == "group:9").unwrap();
        assert!(group.unit.shelved.is_empty());
        assert!(!group.all_parked());
    }

    /// Nothing on the workspace may be missing from the bar. The bar is the only place a window
    /// exists once Home stopped placing tiles for running windows, so a window that is in the
    /// snapshot and in no entry is a window the user can only reach by moving the ones on top of
    /// it — which is exactly the complaint this test exists for.
    ///
    /// Note what it does *not* claim: one entry per window. Two windows of one application are
    /// deliberately one chip
    /// (`@templaros.decision.one-persistent-bottom-bar-tags-not-bands/2`), with its members in
    /// `Entry::windows` for the row above the bar. Every window is *accounted for*, exactly once.
    #[test]
    fn every_unshelved_client_window_is_in_exactly_one_entry() {
        let mut unresolved = window(1, "org.example.Term", "sh", None);
        // An identity `appd` could not resolve: `grouping_key` gives each such window a key of
        // its own (`unknown-window-<id>`) rather than one shared bucket.
        unresolved.canonical_app_id = None;
        unresolved.verified_identity = false;
        let mut also_unresolved = window(2, "org.example.Term", "sh", None);
        also_unresolved.canonical_app_id = None;
        also_unresolved.verified_identity = false;
        let mut parked = window(3, "org.templaros.Files", "Downloads", None);
        parked.shelved = true;
        // A group with one member on screen and one parked: still a group, not a lone window.
        let mut parked_member = window(6, "org.example.Browser", "docs", Some(9));
        parked_member.shelved = true;
        let toplevels = vec![
            unresolved,
            also_unresolved,
            parked,
            window(4, "org.templaros.Files", "Pictures", None),
            window(5, "org.example.Term", "sh", Some(9)),
            parked_member,
            // XWayland windows arrive on the same layer through the same records; an X11 client
            // that never got an app id is the `None` case above.
            window(7, "org.example.Xterm", "xterm", None),
        ];
        let entries = entries(&toplevels);
        let mut seen: Vec<WindowId> = entries
            .iter()
            .flat_map(|entry| entry.windows.iter().map(|window| window.id))
            .collect();
        let count = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            count,
            "a window appears in two entries: {entries:?}"
        );
        assert_eq!(
            seen,
            toplevels.iter().map(|t| t.window_id).collect::<Vec<_>>(),
            "every window in the snapshot is on the bar: {entries:?}"
        );
        // Two windows whose identity did not resolve are two chips, not one bucket.
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.key().starts_with("unknown-window-"))
                .count(),
            2
        );
        let group = entries.iter().find(|e| e.key() == "group:9").unwrap();
        assert_eq!(group.windows.len(), 2, "a parked member is still a member");
        assert!(!group.all_parked());
    }

    /// The chips are spatial memory: using a window must never move the chip it belongs to, and
    /// must never move anybody else's. `task::units` fixes the order at first sight — group id
    /// order, then the order the windows arrive in, which the bridge sorts by window id — and
    /// recency lives *inside* a unit, where the fan reads it.
    #[test]
    fn using_a_window_never_reorders_the_bar() {
        let resting = vec![
            window(1, "b.app", "x", None),
            window(2, "a.app", "y", None),
            window(3, "c.app", "z", Some(2)),
        ];
        let mut used = resting.clone();
        used[1].activated = true;
        used[1].mru_rank = 0;
        used[0].mru_rank = 9;
        let keys = |tops: &[ShellToplevel]| -> Vec<String> {
            entries(tops).iter().map(Entry::key).collect()
        };
        assert_eq!(keys(&resting), keys(&used));
        assert_eq!(keys(&resting), vec!["group:2", "b.app", "a.app"]);
    }

    #[test]
    fn windows_inside_an_entry_follow_semantic_mru() {
        let mut older = window(1, "org.example.Browser", "older", None);
        older.mru_rank = 4;
        let mut newest = window(2, "org.example.Browser", "newest", None);
        newest.mru_rank = 0;
        let entry = entries(&[older, newest]).pop().unwrap();
        assert_eq!(
            entry
                .windows
                .iter()
                .map(|window| window.id)
                .collect::<Vec<_>>(),
            vec![WindowId(2), WindowId(1)]
        );
    }
}
