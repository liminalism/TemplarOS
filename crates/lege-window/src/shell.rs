//! Sanitized toplevel snapshots for the shell: no PIDs, no raw claims — enough to render the
//! dock, switcher and Window Shelf with no local heuristics (planning doc 02 §7).

use crate::{Layer, Rect, WindowId};
use lege_types::{AppId, InstanceId};
use serde::{Deserialize, Serialize};

/// A window group's identity as the shell sees it (`lege_shell_toplevel_v1.group`). The
/// compositor owns groups; this is a view. `accent` is 0x00RRGGBB, shared by every member, so
/// the band stripe, the outline on the workspace and the tile's edge on Home are one colour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub group_id: u64,
    pub name: String,
    pub accent: u32,
}

/// What the shell receives about one window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellToplevel {
    pub window_id: WindowId,
    pub canonical_app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub title: String,
    pub verified_identity: bool,
    pub activated: bool,
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
}

impl ShellToplevel {
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
            canonical_app_id: Some("org.legeos.Files".parse().unwrap()),
            instance_id: None,
            title: String::new(),
            verified_identity: true,
            activated: false,
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry: Rect::new(0, 0, 640, 480),
            layer: Layer::Normal,
            group: None,
        }
    }

    #[test]
    fn grouping_keys() {
        let toplevel = toplevel();
        assert_eq!(
            grouping_key(&toplevel, DockGrouping::ByApplication),
            "org.legeos.Files"
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
