//! The bottom bar's entries, computed from the compositor's toplevel list and nothing else.
//!
//! Window groups first, one entry each in the group's accent — a group is one unit of work, so it
//! is one thing to switch to. Then one entry per application over the loose windows, bucketed by
//! canonical identity through [`crate::dock::group`]; an application with one window *is* that
//! window. Pure, on the same seam as [`crate::dock`]: in the running system the toplevels come
//! from `templar_shell_v1` (`crate::wayland_bridge`), and tests construct them directly.

use std::collections::BTreeMap;

use templar_window::{DockGrouping, Layer, ShellToplevel, WindowId};

/// One window of an entry, as the hover row needs it.
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

/// One thing on the bar to switch to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// `group:<id>` for a window group; the canonical application id otherwise, which is also
    /// the key an icon is filed under.
    pub key: String,
    pub label: String,
    /// The group's accent; `None` for an application.
    pub accent: Option<u32>,
    /// The application whose icon this entry wears, when it is one.
    pub app_id: Option<String>,
    /// Every window in the entry, in encounter order.
    pub windows: Vec<EntryWindow>,
    /// Which of [`Self::windows`] are parked on the shelf.
    ///
    /// Parking used to mean a tile appeared on Home; Home now holds only the user's own things,
    /// so the bar is the only place a parked window still exists. A parked entry is drawn dimmed
    /// and a click on it brings the window back before activating it.
    pub parked: Vec<WindowId>,
    /// Whether the activated window is one of these.
    pub active: bool,
}

impl Entry {
    /// The one window a click goes straight to, when there is exactly one.
    pub fn sole_window(&self) -> Option<WindowId> {
        match self.windows.as_slice() {
            [only] => Some(only.id),
            _ => None,
        }
    }

    /// Whether every window here is parked — what makes the whole chip read as dimmed. A group
    /// with one member still open is not parked: something of it is on screen.
    pub fn all_parked(&self) -> bool {
        !self.windows.is_empty() && self.parked.len() == self.windows.len()
    }

    pub fn is_parked(&self, window: WindowId) -> bool {
        self.parked.contains(&window)
    }
}

/// Groups first in group-id order (the order `field_model::merge_window_groups` uses, for the
/// same reason: it is stable under any reordering of the input), then applications in key order.
pub fn entries(toplevels: &[ShellToplevel]) -> Vec<Entry> {
    let mut groups: BTreeMap<u64, Vec<&ShellToplevel>> = BTreeMap::new();
    let mut loose: Vec<ShellToplevel> = Vec::new();
    for toplevel in toplevels
        .iter()
        .filter(|t| matches!(t.layer, Layer::Normal | Layer::AppOnTop))
    {
        match &toplevel.group {
            Some(info) => groups.entry(info.group_id).or_default().push(toplevel),
            None => loose.push(toplevel.clone()),
        }
    }
    let mut out = Vec::new();
    for (id, members) in groups {
        let info = members[0].group.as_ref().expect("bucketed by its group");
        out.push(Entry {
            key: format!("group:{id}"),
            label: crate::field_model::group_title(&info.name, members.len()),
            accent: Some(info.accent),
            app_id: None,
            windows: members.iter().map(|member| entry_window(member)).collect(),
            parked: members
                .iter()
                .filter(|member| member.shelved)
                .map(|member| member.window_id)
                .collect(),
            active: members.iter().any(|member| member.activated),
        });
    }
    for entry in crate::dock::group(&loose, DockGrouping::ByApplication) {
        let windows: Vec<EntryWindow> = entry
            .window_ids
            .iter()
            .zip(entry.titles.iter())
            .map(|(id, title)| EntryWindow {
                id: *id,
                title: title.clone(),
                // The key is a function of the window, and `dock::group` has already dropped the
                // toplevel it came from; the list is a few dozen windows at most.
                preview: loose
                    .iter()
                    .find(|toplevel| toplevel.window_id == *id)
                    .map(preview_key)
                    .unwrap_or_default(),
            })
            .collect();
        let label = match windows.as_slice() {
            [only] => only.title.clone(),
            _ => entry.key.parse::<templar_types::AppId>().map_or_else(
                |_| entry.key.clone(),
                |id| crate::field_model::short_name(&id),
            ),
        };
        let active = loose
            .iter()
            .any(|t| t.activated && entry.window_ids.contains(&t.window_id));
        let parked = loose
            .iter()
            .filter(|t| t.shelved && entry.window_ids.contains(&t.window_id))
            .map(|t| t.window_id)
            .collect();
        out.push(Entry {
            key: entry.key.clone(),
            label,
            accent: None,
            app_id: Some(entry.key),
            windows,
            parked,
            active,
        });
    }
    out
}

fn entry_window(toplevel: &ShellToplevel) -> EntryWindow {
    EntryWindow {
        id: toplevel.window_id,
        title: title_of(toplevel),
        preview: preview_key(toplevel),
    }
}

/// What this window's capture is filed under — the same key Home's session tiles use, so the
/// bar and Home read one cache rather than two.
fn preview_key(toplevel: &ShellToplevel) -> String {
    crate::field_model::session_identifier(toplevel, None)
}

fn title_of(toplevel: &ShellToplevel) -> String {
    if toplevel.title.is_empty() {
        "(untitled)".to_owned()
    } else {
        toplevel.title.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_window::{GroupInfo, Rect};

    fn window(id: u64, app: &str, title: &str, group: Option<u64>) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: None,
            title: title.to_owned(),
            verified_identity: true,
            activated: false,
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
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["group:9", "org.example.Editor", "org.templaros.Files"]
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
        assert_eq!(entries[0].accent, Some(0x0011_2233));
        // A one-window application is that window.
        assert_eq!(entries[1].label, "notes");
        assert_eq!(entries[1].sole_window(), Some(WindowId(3)));
        assert!(entries[1].active);
        // Two windows of one application are one entry, named for the application.
        assert_eq!(entries[2].label, "Files");
        assert_eq!(entries[2].sole_window(), None);
        assert!(!entries[2].active);
    }

    #[test]
    fn grouped_windows_never_double_as_application_entries_and_shell_layers_are_skipped() {
        let mut home = window(7, "org.templaros.Shell", "Home", None);
        home.layer = Layer::HomeSurface;
        let toplevels = vec![window(4, "org.example.Term", "sh", Some(9)), home];
        let entries = entries(&toplevels);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "group:9");
        assert_eq!(entries[0].label, "1 window");
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
            .find(|e| e.key == "org.templaros.Files")
            .unwrap();
        assert!(files.all_parked(), "a parked window is still on the bar");
        assert!(files.is_parked(WindowId(1)));
        let group = entries.iter().find(|e| e.key == "group:9").unwrap();
        assert!(group.parked.is_empty());
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
                .filter(|entry| entry.key.starts_with("unknown-window-"))
                .count(),
            2
        );
        let group = entries.iter().find(|e| e.key == "group:9").unwrap();
        assert_eq!(group.windows.len(), 2, "a parked member is still a member");
        assert!(!group.all_parked());
    }

    #[test]
    fn order_is_stable_under_input_reordering() {
        let a = vec![
            window(1, "b.app", "x", None),
            window(2, "a.app", "y", None),
            window(3, "c.app", "z", Some(2)),
        ];
        let mut b = a.clone();
        b.reverse();
        assert_eq!(entries(&a), entries(&b));
    }
}
