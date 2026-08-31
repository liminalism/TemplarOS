//! The restrained semantic grammar of drops (`docs/homefield.md` "A restrained semantic
//! grammar"): the source's kind and what it is dropped on determine one named operation, or
//! plain placement. Small enough to learn, and announced in text before release — when no
//! operation is named, only placement changes.
//!
//! This crate does not know MIME types: whether an application accepts a file is something the
//! shell asks `Associations1` and passes in as [`Target::Tile::accepts_open`].

use crate::field::Cell;
use crate::tile::{TileId, TileKind};
use serde::{Deserialize, Serialize};

/// What a lifted tile is over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Empty field (or empty space inside the container `container`).
    Empty {
        container: Option<TileId>,
        cell: Cell,
    },
    /// The visible shelf.
    Shelf,
    /// Another tile, not a container. `accepts_open` is whether that tile is an application
    /// that opens the lifted file.
    Tile {
        id: TileId,
        kind: TileKind,
        accepts_open: bool,
    },
    /// A project container (its frame or its interior).
    Project { id: TileId },
}

/// One named, previewable operation. The shell turns it into field ops and service calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Relocate {
        container: Option<TileId>,
        to: Cell,
    },
    Stash,
    OpenWith {
        app: TileId,
    },
    AddReference {
        tray: TileId,
    },
    PinTool {
        project: TileId,
    },
    ParkSession {
        project: TileId,
    },
    ScopeWidget {
        project: TileId,
    },
    AddToProject {
        project: TileId,
    },
    /// Two loose tiles become a new project, sized to a quadrant.
    GroupIntoNewProject {
        with: TileId,
    },
}

impl Operation {
    /// The text shown on the target before release. `Relocate` has none: it is placement
    /// only, and the invariant is that an unnamed drop changes nothing but position.
    pub fn label(&self) -> Option<&'static str> {
        match self {
            Self::Relocate { .. } => None,
            Self::Stash => Some("STASH ON SHELF"),
            Self::OpenWith { .. } => Some("OPEN"),
            Self::AddReference { .. } => Some("ADD REFERENCE · files remain in place"),
            Self::PinTool { .. } => Some("PIN TOOL TO PROJECT"),
            Self::ParkSession { .. } => Some("PARK SESSION HERE"),
            Self::ScopeWidget { .. } => Some("SCOPE TO PROJECT"),
            Self::AddToProject { .. } => Some("ADD TO PROJECT"),
            Self::GroupIntoNewProject { .. } => Some("NEW PROJECT FROM THESE TWO"),
        }
    }

    /// Whether the operation changes a relationship rather than a position — what makes a
    /// drop worth announcing and, afterwards, undoing through the ledger.
    pub fn is_semantic(&self) -> bool {
        self.label().is_some()
    }
}

/// The grammar table. `source` is the lifted tile's kind, `source_id` its id (so a tile is
/// never grouped with itself).
pub fn operation_for(source_id: TileId, source: &TileKind, target: &Target) -> Option<Operation> {
    match target {
        Target::Empty { container, cell } => Some(Operation::Relocate {
            container: *container,
            to: *cell,
        }),
        Target::Shelf => Some(Operation::Stash),
        Target::Project { id } => {
            if *id == source_id {
                return None;
            }
            Some(match source {
                TileKind::Project { .. } => return None,
                TileKind::App { .. } => Operation::PinTool { project: *id },
                TileKind::ParkedSession { .. } => Operation::ParkSession { project: *id },
                TileKind::Widget { .. } => Operation::ScopeWidget { project: *id },
                TileKind::File { .. }
                | TileKind::Tray { .. }
                | TileKind::Folder { .. }
                | TileKind::System { .. } => Operation::AddToProject { project: *id },
            })
        }
        Target::Tile {
            id,
            kind,
            accepts_open,
        } => {
            if *id == source_id {
                return None;
            }
            match (source, kind) {
                (TileKind::File { .. }, TileKind::App { .. }) if *accepts_open => {
                    Some(Operation::OpenWith { app: *id })
                }
                (TileKind::File { .. }, TileKind::Tray { .. }) => {
                    Some(Operation::AddReference { tray: *id })
                }
                // A drop on or of a folder never silently groups: real file operations onto a
                // folder (copy/move) belong to the typed drag-drop of the file-operations item
                // and must be named there, not defaulted to here. Projects refuse grouping for
                // their own reason (a container never groups into another tile).
                (TileKind::Folder { .. } | TileKind::Project { .. }, _)
                | (_, TileKind::Folder { .. } | TileKind::Project { .. }) => None,
                _ => Some(Operation::GroupIntoNewProject { with: *id }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TrayId;
    use std::path::PathBuf;

    fn file() -> TileKind {
        TileKind::File {
            path: PathBuf::from("/tmp/paper.pdf"),
        }
    }

    #[test]
    fn the_table_matches_the_design_document() {
        let app = TileKind::App {
            app_id: "org.legeos.Viewer".parse().unwrap(),
        };
        let project = TileId(10);
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &Target::Tile {
                    id: TileId(2),
                    kind: app.clone(),
                    accepts_open: true
                }
            ),
            Some(Operation::OpenWith { app: TileId(2) })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &Target::Tile {
                    id: TileId(3),
                    kind: TileKind::Tray { tray_id: TrayId(1) },
                    accepts_open: false
                }
            ),
            Some(Operation::AddReference { tray: TileId(3) })
        );
        assert_eq!(
            operation_for(TileId(1), &app, &Target::Project { id: project }),
            Some(Operation::PinTool { project })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &TileKind::ParkedSession {
                    app_id: None,
                    identifier: "w".into()
                },
                &Target::Project { id: project }
            ),
            Some(Operation::ParkSession { project })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &Target::Empty {
                    container: None,
                    cell: Cell::new(2, 2)
                }
            ),
            Some(Operation::Relocate {
                container: None,
                to: Cell::new(2, 2)
            })
        );
        assert_eq!(
            operation_for(TileId(1), &file(), &Target::Shelf),
            Some(Operation::Stash)
        );
    }

    #[test]
    fn two_loose_tiles_group_into_a_new_project_but_never_a_tile_with_itself() {
        let app = TileKind::App {
            app_id: "org.legeos.Files".parse().unwrap(),
        };
        let other = Target::Tile {
            id: TileId(2),
            kind: app.clone(),
            accepts_open: false,
        };
        assert_eq!(
            operation_for(TileId(1), &app, &other),
            Some(Operation::GroupIntoNewProject { with: TileId(2) })
        );
        let itself = Target::Tile {
            id: TileId(1),
            kind: app.clone(),
            accepts_open: false,
        };
        assert_eq!(operation_for(TileId(1), &app, &itself), None);
        // A file on an app that does not open it is not "open"; it groups like anything else.
        assert_eq!(
            operation_for(TileId(1), &file(), &other),
            Some(Operation::GroupIntoNewProject { with: TileId(2) })
        );
    }

    #[test]
    fn only_relocate_has_no_label() {
        assert!(
            Operation::Relocate {
                container: None,
                to: Cell::new(0, 0)
            }
            .label()
            .is_none()
        );
        assert!(Operation::Stash.is_semantic());
        assert!(Operation::GroupIntoNewProject { with: TileId(2) }.is_semantic());
    }
}
