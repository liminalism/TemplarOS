//! Window records, identity cache boundary, drag handling and shell-facing types.

use crate::geometry::{Output, Point, Rect, repair_window};
use lege_types::{AppId, InstanceId, WindowClaims};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct WindowId(pub u64);

/// What the compositor currently believes about a window's owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIdentity {
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub verified: bool,
    pub claim_mismatch: bool,
}

impl CachedIdentity {
    pub const UNKNOWN: Self = Self {
        app_id: None,
        instance_id: None,
        verified: false,
        claim_mismatch: false,
    };
}

/// Boundary between the render thread and `appd`. Implemented in `lege-comp-integration`.
pub trait IdentityCache: Send + Sync {
    /// Must not perform blocking D-Bus I/O on the compositor render thread.
    fn resolve_cached(&self, pid: u32, claims: &WindowClaims) -> Option<CachedIdentity>;

    /// Schedule an asynchronous refresh from `appd`; the answer arrives through
    /// [`WindowService::update_identity`].
    fn request_resolution(&self, window_id: WindowId, pid: u32, claims: WindowClaims);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowRecord {
    pub id: WindowId,
    pub peer_pid: u32,
    pub claims: WindowClaims,
    pub identity: CachedIdentity,
    pub title: String,
    pub geometry: Rect,
    pub activated: bool,
    pub fullscreen: bool,
    /// Explicit minimisation: the window goes to the visible Window Shelf, never vanishes.
    pub shelved: bool,
}

#[derive(Clone, Copy, Debug)]
struct DragState {
    window_id: WindowId,
    anchor_in_window: Point,
}

/// The single-scene window service.
#[derive(Debug)]
pub struct WindowService<I> {
    identity: I,
    outputs: Vec<Output>,
    windows: HashMap<WindowId, WindowRecord>,
    next_window_id: u64,
    drag: Option<DragState>,
}

impl<I: IdentityCache> WindowService<I> {
    pub fn new(identity: I, outputs: Vec<Output>) -> Self {
        Self {
            identity,
            outputs,
            windows: HashMap::new(),
            next_window_id: 1,
            drag: None,
        }
    }

    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    /// A new toplevel appeared. Geometry is validated against the current topology rather
    /// than trusted blindly; identity is resolved from cache and refreshed asynchronously.
    pub fn on_toplevel_mapped(
        &mut self,
        peer_pid: u32,
        claims: WindowClaims,
        title: String,
        requested_geometry: Rect,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self.next_window_id.saturating_add(1);

        let identity = self
            .identity
            .resolve_cached(peer_pid, &claims)
            .unwrap_or(CachedIdentity::UNKNOWN);
        let geometry = repair_window(requested_geometry, &self.outputs);

        self.windows.insert(
            id,
            WindowRecord {
                id,
                peer_pid,
                claims: claims.clone(),
                identity,
                title,
                geometry,
                activated: false,
                fullscreen: false,
                shelved: false,
            },
        );

        self.identity.request_resolution(id, peer_pid, claims);
        id
    }

    pub fn update_identity(&mut self, window_id: WindowId, identity: CachedIdentity) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.identity = identity;
        }
    }

    pub fn on_toplevel_destroyed(&mut self, window_id: WindowId) {
        self.windows.remove(&window_id);
        if self.drag.is_some_and(|drag| drag.window_id == window_id) {
            self.drag = None;
        }
    }

    /// Outputs changed (hotplug, scale, resume). Every window is re-validated; the returned
    /// list is what must be re-configured on the clients.
    pub fn on_outputs_changed(&mut self, outputs: Vec<Output>) -> Vec<(WindowId, Rect)> {
        self.outputs = outputs;
        let mut changed = Vec::new();
        for window in self.windows.values_mut() {
            let repaired = repair_window(window.geometry, &self.outputs);
            if repaired != window.geometry {
                window.geometry = repaired;
                changed.push((window.id, repaired));
            }
        }
        changed
    }

    pub fn begin_drag(&mut self, window_id: WindowId, pointer_global: Point) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        self.drag = Some(DragState {
            window_id,
            anchor_in_window: Point {
                x: pointer_global.x.saturating_sub(window.geometry.x),
                y: pointer_global.y.saturating_sub(window.geometry.y),
            },
        });
        true
    }

    /// Pointer and window geometry remain in one global logical coordinate space. The anchor
    /// is never recalculated when crossing an output with a different scale factor.
    pub fn update_drag(&mut self, pointer_global: Point) -> Option<(WindowId, Rect)> {
        let drag = self.drag?;
        let window = self.windows.get_mut(&drag.window_id)?;
        window.geometry.x = pointer_global.x.saturating_sub(drag.anchor_in_window.x);
        window.geometry.y = pointer_global.y.saturating_sub(drag.anchor_in_window.y);
        Some((window.id, window.geometry))
    }

    /// Drag ended: the invariant is enforced once, at the end.
    pub fn finish_drag(&mut self) -> Option<(WindowId, Rect)> {
        let drag = self.drag.take()?;
        let window = self.windows.get_mut(&drag.window_id)?;
        window.geometry = repair_window(window.geometry, &self.outputs);
        Some((window.id, window.geometry))
    }

    pub fn set_shelved(&mut self, window_id: WindowId, shelved: bool) {
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.shelved = shelved;
        }
    }

    pub fn window(&self, window_id: WindowId) -> Option<&WindowRecord> {
        self.windows.get(&window_id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &WindowRecord> {
        self.windows.values()
    }

    /// Sanitized view for the shell (dock, switcher, window shelf).
    pub fn shell_toplevels(&self) -> Vec<ShellToplevel> {
        let mut list: Vec<ShellToplevel> = self
            .windows
            .values()
            .map(|window| ShellToplevel {
                window_id: window.id,
                canonical_app_id: window.identity.app_id.clone(),
                instance_id: window.identity.instance_id,
                title: window.title.clone(),
                verified_identity: window.identity.verified,
                activated: window.activated,
                shelved: window.shelved,
            })
            .collect();
        list.sort_by_key(|toplevel| toplevel.window_id);
        list
    }
}

/// What the shell receives about a window: no PIDs, no raw claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellToplevel {
    pub window_id: WindowId,
    pub canonical_app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub title: String,
    pub verified_identity: bool,
    pub activated: bool,
    pub shelved: bool,
}

/// How the dock groups windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockGrouping {
    ByApplication,
    ByInstance,
    SeparateWindows,
}

/// Grouping key for one toplevel under `policy`.
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

/// A compositor-issued permission to steal focus (xdg-activation semantics).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationGrant {
    pub token: String,
    pub requesting_instance: Option<InstanceId>,
    pub target_instance: Option<InstanceId>,
    pub expires_at_monotonic_ns: u64,
}

/// Commands only the trusted shell connection may submit over the private protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    Activate {
        window_id: WindowId,
        grant: ActivationGrant,
    },
    Close {
        window_id: WindowId,
    },
    MoveToVisibleArea {
        window_id: WindowId,
    },
    Shelve {
        window_id: WindowId,
    },
    Unshelve {
        window_id: WindowId,
    },
    ShowAllWindows,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeCache {
        requests: Mutex<Vec<(WindowId, u32)>>,
    }

    impl IdentityCache for FakeCache {
        fn resolve_cached(&self, _pid: u32, _claims: &WindowClaims) -> Option<CachedIdentity> {
            None
        }
        fn request_resolution(&self, window_id: WindowId, pid: u32, _claims: WindowClaims) {
            self.requests.lock().unwrap().push((window_id, pid));
        }
    }

    fn outputs() -> Vec<Output> {
        vec![Output {
            name: "eDP-1".into(),
            logical: Rect::new(0, 0, 1920, 1080),
            scale_milli: 1000,
        }]
    }

    #[test]
    fn mapping_repairs_geometry_and_requests_identity() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            4242,
            WindowClaims::default(),
            "x".into(),
            Rect::new(9000, 9000, 400, 300),
        );
        let window = service.window(id).unwrap();
        assert!(crate::is_recoverable(window.geometry, service.outputs()));
        assert_eq!(window.identity, CachedIdentity::UNKNOWN);
        assert_eq!(
            service.identity.requests.lock().unwrap().as_slice(),
            &[(id, 4242)]
        );
    }

    #[test]
    fn drag_keeps_anchor_and_repairs_on_finish() {
        let mut service = WindowService::new(FakeCache::default(), outputs());
        let id = service.on_toplevel_mapped(
            1,
            WindowClaims::default(),
            String::new(),
            Rect::new(100, 100, 400, 300),
        );
        assert!(service.begin_drag(id, Point { x: 150, y: 120 }));
        let (_, during) = service.update_drag(Point { x: 5000, y: 5000 }).unwrap();
        assert_eq!((during.x, during.y), (4950, 4980)); // anchor preserved, no clamping mid-drag
        let (_, after) = service.finish_drag().unwrap();
        assert!(crate::is_recoverable(after, service.outputs()));
    }

    #[test]
    fn output_removal_migrates_windows() {
        let two = vec![
            Output {
                name: "eDP-1".into(),
                logical: Rect::new(0, 0, 1920, 1080),
                scale_milli: 1000,
            },
            Output {
                name: "HDMI-1".into(),
                logical: Rect::new(1920, 0, 2560, 1440),
                scale_milli: 1000,
            },
        ];
        let mut service = WindowService::new(FakeCache::default(), two.clone());
        let id = service.on_toplevel_mapped(
            1,
            WindowClaims::default(),
            String::new(),
            Rect::new(3000, 200, 800, 600),
        );
        let changed = service.on_outputs_changed(vec![two[0].clone()]);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, id);
        assert!(crate::is_recoverable(changed[0].1, service.outputs()));
    }

    #[test]
    fn grouping_keys() {
        let toplevel = ShellToplevel {
            window_id: WindowId(7),
            canonical_app_id: Some("org.legeos.Files".parse().unwrap()),
            instance_id: None,
            title: String::new(),
            verified_identity: true,
            activated: false,
            shelved: false,
        };
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
}
