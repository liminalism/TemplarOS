//! Sanitized toplevel snapshots for the shell: no PIDs, no raw claims — enough to render the
//! dock, switcher and Window Shelf with no local heuristics (planning doc 02 §7).

use crate::{Layer, Rect, WindowId};
use serde::{Deserialize, Serialize};
use templar_types::{AppId, InstanceId};

/// A window group's identity as the shell sees it (`templar_shell_toplevel_v1.group`). The
/// compositor owns groups; this is a view. `accent` is 0x00RRGGBB, shared by every member, so
/// the band stripe, the outline on the workspace and the tile's edge on Home are one colour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: u64,
    pub name: String,
    pub accent: u32,
}

/// A verified per-window application tab count, bound to exactly one compositor
/// toplevel by sessiond (`templar_shell_toplevel_v1.tab_count`, version 14).
/// Count-only metadata: it authorizes no focus, close or capture, and carries no
/// URLs, tab names or terminal text. Absence (`None`) means unknown — unsupported,
/// disconnected or unverified — and must never render as a fabricated `0`/`1`.
/// A terminal pane/split is not another tab; a browser tab group is not one tab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCount {
    /// Tabs belonging to this OS window, inactive ones included. Always `>= 1`;
    /// a verified count of one shows `1`.
    pub count: u32,
    /// Invalidates older counts on adapter restart or window rebind; the shell
    /// keeps only the newest generation per window.
    pub generation: u64,
    /// Which adapter proved the binding (e.g. `"kitty"`, `"browser"`).
    pub provenance: String,
}

/// What the shell receives about one window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellToplevel {
    pub window_id: WindowId,
    pub canonical_app_id: Option<AppId>,
    /// Untrusted client claim, display only. Never used for identity, actions or task merging.
    #[serde(default)]
    pub claimed_app_id: Option<String>,
    pub instance_id: Option<InstanceId>,
    pub title: String,
    pub verified_identity: bool,
    pub activated: bool,
    /// Zero-based activation recency among mapped application windows; zero is newest.
    pub mru_rank: u32,
    pub fullscreen: bool,
    pub shelved: bool,
    /// Filling its output's work area at the client's request (`xdg_toplevel.set_maximized`).
    pub maximized: bool,
    /// The window's on-screen rectangle in the one global logical space. Sent to the trusted
    /// shell only: the Home Field zooms a tile out of, and a parked window back into, this rect,
    /// and it sizes preview buffers from it before any capture exists.
    pub geometry: Rect,
    pub layer: Layer,
    /// The window group this window belongs to, if any (planning doc
    /// `docs/planning/window-groups-and-application-integration.md` §2).
    pub group: Option<GroupInfo>,
    /// Verified application tab count for exactly this window, if any adapter has
    /// proven the binding. `None` is unknown, not zero.
    #[serde(default)]
    pub tab_count: Option<TabCount>,
}

impl TabCount {
    /// A verified count, or `None` when there is nothing honest to show. A count of
    /// zero is never a verified tab count — a window with no tabs is unknown, not
    /// empty — and an empty provenance names no adapter that proved the binding.
    pub fn verified(count: u32, generation: u64, provenance: String) -> Option<TabCount> {
        if count == 0 || provenance.is_empty() {
            return None;
        }
        Some(TabCount {
            count,
            generation,
            provenance,
        })
    }

    /// Whether `next` supersedes `self`: a newer generation always wins; within one
    /// generation the provenance must agree, so two adapters can never fight over a
    /// window by alternating counts.
    pub fn superseded_by(&self, next: &TabCount) -> bool {
        next.generation > self.generation
            || (next.generation == self.generation && next.provenance == self.provenance)
    }
}

impl ShellToplevel {
    pub fn icon_app_id(&self) -> Option<&str> {
        self.canonical_app_id
            .as_ref()
            .map(AppId::as_str)
            .or(self.claimed_app_id.as_deref())
    }
    /// Whether this window is filling its output — the shell's half of
    /// [`crate::WindowRecord::covers_screen`], and the one predicate behind every "there is a
    /// window over the bottom edge" rule the shell has: the machine cluster is not drawn at all
    /// while one is up, the switcher collapses to its hint until the pointer asks for the edge,
    /// and Home's window previews stop because nothing of Home can be seen.
    #[must_use]
    pub fn covers_screen(&self) -> bool {
        !self.shelved && (self.fullscreen || self.maximized)
    }
}

/// How the dock groups windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockGrouping {
    ByApplication,
    ByInstance,
    SeparateWindows,
}

/// Grouping key for one toplevel under `policy`. The canonical `AppId` / `InstanceId` come from
/// verified identity, so the dock needs no local `WM_CLASS`/Flatpak-ID heuristics of its own.
pub fn grouping_key(window: &ShellToplevel, policy: DockGrouping) -> String {
    match policy {
        DockGrouping::ByApplication => window.canonical_app_id.as_ref().map_or_else(
            || format!("unknown-window-{}", window.window_id.0),
            ToString::to_string,
        ),
        DockGrouping::ByInstance => window.instance_id.map_or_else(
            || format!("unknown-window-{}", window.window_id.0),
            |id| id.to_string(),
        ),
        DockGrouping::SeparateWindows => format!("window-{}", window.window_id.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevel() -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(7),
            canonical_app_id: Some("org.templaros.Files".parse().unwrap()),
            claimed_app_id: None,
            instance_id: None,
            title: String::new(),
            verified_identity: true,
            activated: false,
            mru_rank: 0,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
            group: None,
            tab_count: None,
        }
    }

    #[test]
    fn grouping_keys() {
        let toplevel = toplevel();
        assert_eq!(
            grouping_key(&toplevel, DockGrouping::ByApplication),
            "org.templaros.Files"
        );
        assert_eq!(
            grouping_key(&toplevel, DockGrouping::ByInstance),
            "unknown-window-7"
        );
        assert_eq!(
            grouping_key(&toplevel, DockGrouping::SeparateWindows),
            "window-7"
        );
    }

    #[test]
    fn tab_count_zero_or_anonymous_is_unknown_never_a_badge() {
        assert_eq!(TabCount::verified(0, 1, "kitty".to_owned()), None);
        assert_eq!(TabCount::verified(3, 1, String::new()), None);
        let one = TabCount::verified(1, 1, "kitty".to_owned()).expect("a verified one shows");
        assert_eq!(one.count, 1);
    }

    #[test]
    fn tab_count_generations_order_and_provenance_cannot_fight() {
        let old = TabCount::verified(2, 1, "kitty".to_owned()).expect("verified");
        let restart = TabCount::verified(2, 2, "kitty".to_owned()).expect("verified");
        let same = TabCount::verified(3, 1, "kitty".to_owned()).expect("verified");
        let rival = TabCount::verified(3, 1, "browser".to_owned()).expect("verified");
        assert!(old.superseded_by(&restart));
        assert!(!restart.superseded_by(&old));
        assert!(old.superseded_by(&same));
        assert!(!old.superseded_by(&rival));
    }

    #[test]
    fn snapshots_without_tab_counts_stay_unknown() {
        let json = serde_json::json!({
            "window_id": 7u64,
            "canonical_app_id": "org.templaros.Files",
            "title": "",
            "verified_identity": true,
            "activated": false,
            "mru_rank": 0u32,
            "fullscreen": false,
            "shelved": false,
            "maximized": false,
            "geometry": {"x": 0, "y": 0, "width": 640, "height": 480},
            "layer": "Normal",
            "group": null,
        });
        let toplevel: ShellToplevel = serde_json::from_value(json).expect("old snapshots parse");
        assert_eq!(toplevel.tab_count, None);
    }

    #[test]
    fn by_instance_prefers_instance_over_app_id() {
        let mut toplevel = toplevel();
        let instance = InstanceId::new();
        toplevel.instance_id = Some(instance);
        assert_eq!(
            grouping_key(&toplevel, DockGrouping::ByInstance),
            instance.to_string()
        );
    }
}
