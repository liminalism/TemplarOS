//! The desktopless Home Surface: drawers, not icons on a wallpaper (planning doc 01 §1).
//! Files remain normal POSIX files at visible paths; a drawer is a directory, a mount, a saved
//! query, a tag or a project. `~/Desktop` stays as a compatibility directory but is not rendered.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Drawer {
    Recent,
    Projects,
    Downloads,
    Documents,
    Media,
    Drives,
    Network,
    Operations,
    Trash,
    /// Explicitly minimised windows live here — never silently vanished.
    WindowShelf,
}

impl Drawer {
    pub const DEFAULT: [Drawer; 10] = [
        Self::Recent,
        Self::Projects,
        Self::Downloads,
        Self::Documents,
        Self::Media,
        Self::Drives,
        Self::Network,
        Self::Operations,
        Self::Trash,
        Self::WindowShelf,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::Projects => "Projects",
            Self::Downloads => "Downloads",
            Self::Documents => "Documents",
            Self::Media => "Media",
            Self::Drives => "Drives",
            Self::Network => "Network",
            Self::Operations => "Operations",
            Self::Trash => "Trash",
            Self::WindowShelf => "Windows",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Recent => "Recently used files, applications and projects",
            Self::Projects => "Explicit working sets with files, terminals and application state",
            Self::Downloads => "Current transfers and recent downloads",
            Self::Documents => "Standard paths and saved searches",
            Self::Media => "Pictures, music, video and saved searches",
            Self::Drives => "Mounted volumes with free space, health and eject controls",
            Self::Network => "Network shares and remote machines",
            Self::Operations => "File copies, conversions and background jobs",
            Self::Trash => "Contents, size and restore actions",
            Self::WindowShelf => "Minimised windows, always visible and recoverable",
        }
    }
}
