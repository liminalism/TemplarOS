//! Bounded local compositor-owner control protocol. The daemon brokers policy; it does not own previews or idle clocks.
use crate::{
    display::{DisplayId, DisplayObservation, DisplayProfile},
    input::{InputConfiguration, InputId, InputObservation},
    settings::{Capability, Revision},
    wallpaper::{WallpaperImage, WallpaperObservation},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CONTROL_SOCKET_NAME: &str = "templar-comp-control-v1.sock";
pub const MAX_CONTROL_FRAME: usize = 131_072;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityOutput {
    pub id: DisplayId,
    pub enabled: bool,
    pub internal: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityObservation {
    pub revision: Revision,
    pub display_revision: Revision,
    pub input_revision: Revision,
    pub activity_serial: u64,
    pub idle_elapsed_ms: u64,
    pub session_active: bool,
    /// A lock request alone is not protection; suspend must use fresh current-generation coverage.
    pub locked: bool,
    pub lock_generation: Option<u64>,
    pub protected_covered: bool,
    pub idle: bool,
    pub blanked: bool,
    pub inhibited: bool,
    pub idle_timeout_ms: Option<u64>,
    pub outputs: Vec<ActivityOutput>,
    pub idle_control: Capability,
    pub input_control: Capability,
    pub display_control: Capability,
}

/// Kernel-credential native Wayland windows; X11 PID properties are not authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabWindow {
    pub window_id: u64,
    pub peer_pid: u32,
    pub binding: Uuid,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabWindows {
    pub owner: Uuid,
    pub windows: Vec<TabWindow>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabCountUpdate {
    pub window_id: u64,
    pub peer_pid: u32,
    pub binding: Uuid,
    pub count: u32,
    pub generation: u64,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ControlRequest {
    Displays,
    ValidateDisplays {
        expected: Revision,
        profile: DisplayProfile,
    },
    PreviewDisplays {
        expected: Revision,
        profile: DisplayProfile,
    },
    RestoreConfirmedDisplays {
        expected: Revision,
        profile: DisplayProfile,
    },
    ConfirmDisplays {
        expected: Revision,
        token: Uuid,
    },
    RevertDisplays {
        token: Uuid,
    },
    SetPanelClosed {
        expected: Revision,
        closed: bool,
        allow_headless: bool,
    },
    Identify {
        expected: Revision,
        outputs: Vec<DisplayId>,
        calibration: bool,
    },
    Inputs,
    ConfigureInput {
        expected: Revision,
        id: InputId,
        configuration: InputConfiguration,
    },
    ResetInput {
        expected: Revision,
        id: InputId,
    },
    Wallpaper,
    SubscribeWallpaper,
    /// The frame must carry exactly one sealed raster descriptor via SCM_RIGHTS.
    SetWallpaper {
        image: WallpaperImage,
    },
    ClearWallpaper {
        revision: Revision,
        output: Option<DisplayId>,
    },
    Activity,
    SubscribeActivity,
    SubscribeTabWindows,
    SetTabCounts {
        owner: Uuid,
        counts: Vec<TabCountUpdate>,
    },
    SetIdleTimeout {
        expected: Revision,
        timeout_ms: Option<u64>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "observation", rename_all = "snake_case")]
pub enum ControlResponse {
    Displays(DisplayObservation),
    Inputs(InputObservation),
    Activity(ActivityObservation),
    Wallpaper(WallpaperObservation),
    TabWindows(TabWindows),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlError {
    pub code: String,
    pub message: String,
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ControlError {}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlReply {
    pub result: Result<ControlResponse, ControlError>,
}
