//! Sanitized toplevel snapshots for the shell: no PIDs, no raw claims — enough to render the
//! dock, switcher and Window Shelf with no local heuristics (planning doc 02 §7).

use crate::{Layer, WindowId};
use lege_types::{AppId, InstanceId};
use serde::{Deserialize, Serialize};

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
    pub layer: Layer,
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
            layer: Layer::Normal,
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
