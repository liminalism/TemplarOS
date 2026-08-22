//! Dock rendering: grouping by canonical identity, not local `WM_CLASS` heuristics.
//!
//! `lege_window::shell::{ShellToplevel, DockGrouping, grouping_key}` is the whole policy
//! (planning doc 02 §7 end-to-end opening flow); this module only turns one grouped snapshot
//! into the lines a text dock prints, or (`crate::surface::dock_screen`) the entries a graphical
//! one draws. Takes `&[ShellToplevel]` directly, the same seam as [`crate::drawers::window_shelf`]:
//! in the running system that vector comes from the compositor's private `lege_shell_v1` protocol
//! (`crate::wayland_bridge`), so tests construct it directly instead of standing up a fake
//! transport.

use lege_window::{DockGrouping, ShellToplevel, WindowId, grouping_key};
use std::collections::BTreeMap;

/// One dock entry: a group's key, its windows' ids (for activating one) and titles, both in
/// encounter order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockEntry {
    pub key: String,
    pub window_count: usize,
    pub titles: Vec<String>,
    /// Every window in this group, in the same order as `titles`. Unused by the text renderer
    /// below (a line of text has nothing to click); `crate::surface::dock_screen` activates
    /// `window_ids[0]` — the group's first-encountered window — when its icon is clicked.
    pub window_ids: Vec<WindowId>,
}

/// Group `toplevels` under `policy` and return one [`DockEntry`] per group, ordered by key.
pub fn group(toplevels: &[ShellToplevel], policy: DockGrouping) -> Vec<DockEntry> {
    let mut groups: BTreeMap<String, Vec<&ShellToplevel>> = BTreeMap::new();
    for toplevel in toplevels {
        groups
            .entry(grouping_key(toplevel, policy))
            .or_default()
            .push(toplevel);
    }
    groups
        .into_iter()
        .map(|(key, windows)| DockEntry {
            key,
            window_count: windows.len(),
            titles: windows
                .iter()
                .map(|window| {
                    if window.title.is_empty() {
                        "(untitled)".to_owned()
                    } else {
                        window.title.clone()
                    }
                })
                .collect(),
            window_ids: windows.iter().map(|window| window.window_id).collect(),
        })
        .collect()
}

/// One line per group: `"{key}  {n} window(s)  — {titles}"`.
pub fn render(toplevels: &[ShellToplevel], policy: DockGrouping) -> String {
    let entries = group(toplevels, policy);
    if entries.is_empty() {
        return "Dock — no windows".to_owned();
    }
    entries
        .into_iter()
        .map(|entry| {
            let plural = if entry.window_count == 1 { "" } else { "s" };
            format!(
                "{}  {} window{plural}  — {}",
                entry.key,
                entry.window_count,
                entry.titles.join("; ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::InstanceId;
    use lege_window::{Layer, WindowId};

    fn toplevel(id: u64, app: &str, instance: Option<InstanceId>, title: &str) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().unwrap()),
            instance_id: instance,
            title: title.to_owned(),
            verified_identity: true,
            activated: false,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: lege_window::Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
        }
    }

    #[test]
    fn by_application_merges_every_instance_of_one_app() {
        let editor_a = toplevel(1, "org.legeos.Editor", Some(InstanceId::new()), "report.md");
        let editor_b = toplevel(2, "org.legeos.Editor", Some(InstanceId::new()), "notes.md");
        let viewer = toplevel(3, "org.legeos.Viewer", None, "scan.pdf");
        let windows = [editor_a, editor_b, viewer];

        let entries = group(&windows, DockGrouping::ByApplication);
        assert_eq!(entries.len(), 2);
        let editor_group = entries
            .iter()
            .find(|entry| entry.key == "org.legeos.Editor")
            .unwrap();
        assert_eq!(editor_group.window_count, 2);
        assert_eq!(editor_group.titles, vec!["report.md", "notes.md"]);
    }

    #[test]
    fn by_instance_splits_the_same_app_into_separate_groups() {
        let instance_one = InstanceId::new();
        let instance_two = InstanceId::new();
        let windows = [
            toplevel(1, "org.legeos.Editor", Some(instance_one), "a"),
            toplevel(2, "org.legeos.Editor", Some(instance_two), "b"),
        ];
        let entries = group(&windows, DockGrouping::ByInstance);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.window_count == 1));
    }

    #[test]
    fn separate_windows_never_merges_anything() {
        let windows = [
            toplevel(1, "org.legeos.Editor", Some(InstanceId::new()), "a"),
            toplevel(2, "org.legeos.Editor", Some(InstanceId::new()), "b"),
        ];
        let entries = group(&windows, DockGrouping::SeparateWindows);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn an_empty_title_renders_as_untitled() {
        let windows = [toplevel(1, "org.legeos.Editor", None, "")];
        let text = render(&windows, DockGrouping::ByApplication);
        assert!(text.contains("(untitled)"));
    }

    #[test]
    fn no_windows_is_a_clear_empty_line_not_a_blank_string() {
        assert_eq!(
            render(&[], DockGrouping::ByApplication),
            "Dock — no windows"
        );
    }

    #[test]
    fn the_rendered_line_names_the_group_and_counts_windows() {
        let windows = [
            toplevel(1, "org.legeos.Editor", Some(InstanceId::new()), "a"),
            toplevel(2, "org.legeos.Editor", Some(InstanceId::new()), "b"),
        ];
        let text = render(&windows, DockGrouping::ByApplication);
        assert_eq!(text, "org.legeos.Editor  2 windows  — a; b");
    }
}
