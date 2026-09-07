//! The restrained semantic grammar of drops
//! (`@legeos.decision.home-field-stable-bounded-semantic-grammar`): the source's kind and what it
//! is dropped on determine one named operation, or
//! plain placement. Small enough to learn, and announced in text before release — when no
//! operation is named, only placement changes.
//!
//! This crate does not know MIME types: whether an application accepts a file is something the
//! shell asks `Associations1` and passes in as [`Target::Tile::accepts_open`].

use crate::field::Cell;
use crate::tile::{TileId, TileKind};
use serde::{Deserialize, Serialize};

/// What the pointer's modifiers asked this drop to mean.
///
/// The grammar needs this for exactly one family of drops — a real file onto a real folder — and
/// takes it as a value rather than guessing, because the two answers move a person's data in
/// different ways and the difference must be announced before release, not discovered after.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DropModifiers {
    /// Copy rather than move. Ctrl on every desktop this project's users are arriving from.
    pub copy: bool,
}

impl DropModifiers {
    pub const MOVE: DropModifiers = DropModifiers { copy: false };
    pub const COPY: DropModifiers = DropModifiers { copy: true };
}

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
    /// A window group's tile (its frame or its interior).
    WindowGroup { id: TileId },
}

/// Where the lifted tile is coming *from*: bare field, or the inside of a container.
///
/// The grammar needs this for exactly one drop — a member dragged out of a window group onto the
/// bare field, which is how a window leaves its group — and takes it as a value for the same
/// reason [`DropModifiers`] is a value: the answer changes what the compositor is asked to do,
/// and the caller is the only one that knows where the tile was.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DropOrigin {
    /// The lifted tile is currently inside a window-group tile.
    pub in_window_group: bool,
}

impl DropOrigin {
    pub const LOOSE: DropOrigin = DropOrigin {
        in_window_group: false,
    };
    pub const IN_WINDOW_GROUP: DropOrigin = DropOrigin {
        in_window_group: true,
    };
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
    /// Two application tiles become an app group: a container of favourites on Home, with no
    /// project and no directory behind it.
    GroupIntoAppGroup {
        with: TileId,
    },
    /// An application tile dropped on an app group joins it.
    AddToAppGroup {
        group: TileId,
    },
    /// Put this window in that one's window group (`lege_shell_toplevel_v1.join_group`), making
    /// one of the two when there is none yet. `with` is either another session tile or a group
    /// tile; either way the compositor decides what the group becomes, and the Home tile follows
    /// on the next merge.
    JoinWindowGroup {
        with: TileId,
    },
    /// Take this window out of its window group (`lege_shell_toplevel_v1.leave_group`): a member
    /// dragged from inside the group's tile onto the bare field.
    LeaveWindowGroup,
    /// Copy the dropped item into the folder this tile binds. A real file operation: the shell
    /// hands it to `Files.Operations1` and the service, not the shell, decides what a conflict,
    /// a permission failure or a device boundary means.
    CopyInto {
        folder: TileId,
    },
    /// Move the dropped item into the folder this tile binds.
    MoveInto {
        folder: TileId,
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
            Self::GroupIntoAppGroup { .. } => Some("NEW APP GROUP"),
            Self::AddToAppGroup { .. } => Some("ADD TO GROUP"),
            Self::JoinWindowGroup { .. } => Some("GROUP THESE WINDOWS"),
            Self::LeaveWindowGroup => Some("TAKE OUT OF GROUP"),
            // Named in full, and named differently from each other, because these are the two
            // drops in the whole grammar that change a file rather than a relationship. "MOVE"
            // and "COPY" are not interchangeable words to somebody watching their only copy of
            // something leave the folder it was in.
            Self::CopyInto { .. } => Some("COPY INTO FOLDER"),
            Self::MoveInto { .. } => Some("MOVE INTO FOLDER"),
        }
    }

    /// Whether the operation changes a relationship rather than a position — what makes a
    /// drop worth announcing and, afterwards, undoing through the ledger.
    pub fn is_semantic(&self) -> bool {
        self.label().is_some()
    }

    /// Whether performing this touches the filesystem rather than the field document.
    ///
    /// The dividing line the shell routes on: a field operation is applied to the document and
    /// undone from the field's own log, while one of these is handed to `Files.Operations1` and
    /// undone through the journal. Both are undo; they are not the same undo, and the surface has
    /// to know which it is offering.
    pub fn is_file_operation(&self) -> bool {
        matches!(self, Self::CopyInto { .. } | Self::MoveInto { .. })
    }
}

/// The grammar table. `source` is the lifted tile's kind, `source_id` its id (so a tile is
/// never grouped with itself), `origin` says where the tile was lifted from, and `modifiers` is
/// what the pointer asked for — consulted only where the answer genuinely differs, which is a
/// real file onto a real folder.
pub fn operation_for(
    source_id: TileId,
    source: &TileKind,
    target: &Target,
    origin: DropOrigin,
    modifiers: DropModifiers,
) -> Option<Operation> {
    match target {
        // The one drop where where-it-came-from decides the meaning: a member dragged out of its
        // group tile onto bare field leaves the group. Onto bare field *only* — dropped inside
        // the group's own interior it is a plain rearrangement, and a group's tile is nowhere
        // near the shelf.
        Target::Empty {
            container: None,
            cell,
        } if origin.in_window_group && matches!(source, TileKind::ParkedSession { .. }) => {
            let _ = cell;
            Some(Operation::LeaveWindowGroup)
        }
        Target::Empty { container, cell } => Some(Operation::Relocate {
            container: *container,
            to: *cell,
        }),
        Target::Shelf => Some(Operation::Stash),
        // A window's tile dropped on a group's tile joins that group; nothing else has any
        // business inside a group, which holds windows and only windows.
        Target::WindowGroup { id } => {
            if *id == source_id {
                return None;
            }
            matches!(source, TileKind::ParkedSession { .. })
                .then_some(Operation::JoinWindowGroup { with: *id })
        }
        Target::Project { id } => {
            if *id == source_id {
                return None;
            }
            Some(match source {
                TileKind::Project { .. }
                | TileKind::WindowGroup { .. }
                | TileKind::AppGroup { .. } => return None,
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
                // Two windows, dropped one on the other: the gesture that makes a group. This is
                // the Home half of dropping one title band on another on the workspace, and it
                // takes the place the old "new project from these two" answer had for a pair of
                // session tiles — two windows put together are a window group, which is the
                // lighter of the two things and the one that was actually meant.
                (TileKind::ParkedSession { .. }, TileKind::ParkedSession { .. }) => {
                    Some(Operation::JoinWindowGroup { with: *id })
                }
                // Two applications put together are favourites kept together — an app group,
                // not a project: a project is a place work lives, with a directory behind it,
                // and nobody dropping one shortcut on another asked for a directory.
                (TileKind::App { .. }, TileKind::App { .. }) => {
                    Some(Operation::GroupIntoAppGroup { with: *id })
                }
                (TileKind::App { .. }, TileKind::AppGroup { .. }) => {
                    Some(Operation::AddToAppGroup { group: *id })
                }
                (TileKind::File { .. }, TileKind::App { .. }) if *accepts_open => {
                    Some(Operation::OpenWith { app: *id })
                }
                (TileKind::File { .. }, TileKind::Tray { .. }) => {
                    Some(Operation::AddReference { tray: *id })
                }
                // A real thing onto a real folder is a real file operation, and it is named:
                // this is where the grammar stopped short before, returning None so that no drop
                // could quietly move somebody's data while the invariant "an unnamed drop changes
                // only position" still held. It holds here too — both answers carry a label, and
                // which one is shown depends on the modifier the pointer is holding.
                (TileKind::File { .. } | TileKind::Folder { .. }, TileKind::Folder { .. }) => {
                    Some(if modifiers.copy {
                        Operation::CopyInto { folder: *id }
                    } else {
                        Operation::MoveInto { folder: *id }
                    })
                }
                // Everything else involving a folder still refuses rather than grouping: a folder
                // is not a loose tile to be swept into a project by accident, and dropping some
                // other kind of tile *onto* one has no file meaning at all. Projects and window
                // groups refuse for their own reason — a container never groups into another
                // tile. (A group tile as a *target* never reaches here: `Target::WindowGroup`
                // answers for it above, the way `Target::Project` does for a project.)
                (
                    TileKind::Folder { .. }
                    | TileKind::Project { .. }
                    | TileKind::WindowGroup { .. }
                    | TileKind::AppGroup { .. },
                    _,
                )
                | (
                    _,
                    TileKind::Folder { .. }
                    | TileKind::Project { .. }
                    | TileKind::WindowGroup { .. }
                    | TileKind::AppGroup { .. },
                ) => None,
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
                },
                DropOrigin::LOOSE,
                DropModifiers::default()
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
                },
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::AddReference { tray: TileId(3) })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &Target::Project { id: project },
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::PinTool { project })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &TileKind::ParkedSession {
                    app_id: None,
                    identifier: "w".into()
                },
                &Target::Project { id: project },
                DropOrigin::LOOSE,
                DropModifiers::default()
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
                },
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::Relocate {
                container: None,
                to: Cell::new(2, 2)
            })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &Target::Shelf,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::Stash)
        );
    }

    #[test]
    fn two_applications_make_an_app_group_and_a_third_joins_it() {
        let app = TileKind::App {
            app_id: "org.legeos.Files".parse().unwrap(),
        };
        let other = Target::Tile {
            id: TileId(2),
            kind: app.clone(),
            accepts_open: false,
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &other,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::GroupIntoAppGroup { with: TileId(2) })
        );
        let group = Target::Tile {
            id: TileId(3),
            kind: TileKind::AppGroup {
                group_id: "g".into(),
            },
            accepts_open: false,
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &group,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::AddToAppGroup { group: TileId(3) })
        );
        // A group never groups into a tile, and nothing but an application joins one.
        assert_eq!(
            operation_for(
                TileId(3),
                &TileKind::AppGroup {
                    group_id: "g".into(),
                },
                &other,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            None
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &group,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            None
        );
    }

    #[test]
    fn two_loose_tiles_group_into_a_new_project_but_never_a_tile_with_itself() {
        let app = TileKind::Widget {
            widget: "calendar".into(),
            scope: None,
        };
        let other = Target::Tile {
            id: TileId(2),
            kind: TileKind::System {
                probe: "drives".into(),
            },
            accepts_open: false,
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &other,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::GroupIntoNewProject { with: TileId(2) })
        );
        let itself = Target::Tile {
            id: TileId(1),
            kind: app.clone(),
            accepts_open: false,
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &itself,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            None
        );
        // A file on an app that does not open it is not "open"; it groups like anything else.
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &other,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::GroupIntoNewProject { with: TileId(2) })
        );
    }

    fn session(name: &str) -> TileKind {
        TileKind::ParkedSession {
            app_id: None,
            identifier: name.to_owned(),
        }
    }

    #[test]
    fn two_windows_dropped_together_make_a_window_group_not_a_project() {
        let target = Target::Tile {
            id: TileId(2),
            kind: session("b"),
            accepts_open: false,
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &session("a"),
                &target,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::JoinWindowGroup { with: TileId(2) })
        );
    }

    #[test]
    fn a_window_dropped_on_a_group_joins_it_and_nothing_else_may() {
        let group = Target::WindowGroup { id: TileId(9) };
        assert_eq!(
            operation_for(
                TileId(1),
                &session("a"),
                &group,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::JoinWindowGroup { with: TileId(9) })
        );
        // A group holds windows. A file, a folder, an application or another container dropped on
        // one has no meaning, and a refusal is the honest answer.
        for source in [
            file(),
            folder("/home/u/Notes"),
            TileKind::App {
                app_id: "org.legeos.Files".parse().unwrap(),
            },
            TileKind::WindowGroup {
                identifier: "3".into(),
            },
        ] {
            assert_eq!(
                operation_for(
                    TileId(1),
                    &source,
                    &group,
                    DropOrigin::LOOSE,
                    DropModifiers::default()
                ),
                None,
                "{source:?} onto a window group"
            );
        }
    }

    #[test]
    fn a_member_dragged_onto_the_bare_field_leaves_its_group() {
        let bare = Target::Empty {
            container: None,
            cell: Cell::new(3, 1),
        };
        assert_eq!(
            operation_for(
                TileId(1),
                &session("a"),
                &bare,
                DropOrigin::IN_WINDOW_GROUP,
                DropModifiers::default()
            ),
            Some(Operation::LeaveWindowGroup)
        );
        // The same drop from bare field is placement, as it always was.
        assert_eq!(
            operation_for(
                TileId(1),
                &session("a"),
                &bare,
                DropOrigin::LOOSE,
                DropModifiers::default()
            ),
            Some(Operation::Relocate {
                container: None,
                to: Cell::new(3, 1)
            })
        );
        // And rearranging inside the group's own interior is placement too: the tile has not
        // left anything.
        assert_eq!(
            operation_for(
                TileId(1),
                &session("a"),
                &Target::Empty {
                    container: Some(TileId(9)),
                    cell: Cell::new(0, 1)
                },
                DropOrigin::IN_WINDOW_GROUP,
                DropModifiers::default()
            ),
            Some(Operation::Relocate {
                container: Some(TileId(9)),
                to: Cell::new(0, 1)
            })
        );
    }

    #[test]
    fn both_group_answers_are_announced_before_release() {
        assert_eq!(
            Operation::JoinWindowGroup { with: TileId(2) }.label(),
            Some("GROUP THESE WINDOWS")
        );
        assert_eq!(
            Operation::LeaveWindowGroup.label(),
            Some("TAKE OUT OF GROUP")
        );
        assert!(Operation::LeaveWindowGroup.is_semantic());
        // Neither touches the filesystem: they change a relationship the compositor owns.
        assert!(!Operation::LeaveWindowGroup.is_file_operation());
        assert!(!Operation::JoinWindowGroup { with: TileId(2) }.is_file_operation());
    }

    fn folder(path: &str) -> TileKind {
        TileKind::Folder {
            path: PathBuf::from(path),
        }
    }

    fn onto(id: u64, kind: TileKind) -> Target {
        Target::Tile {
            id: TileId(id),
            kind,
            accepts_open: false,
        }
    }

    #[test]
    fn drag_drop_a_file_onto_a_folder_is_a_named_move_or_a_named_copy() {
        let target = onto(2, folder("/home/u/Documents"));
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &target,
                DropOrigin::LOOSE,
                DropModifiers::MOVE
            ),
            Some(Operation::MoveInto { folder: TileId(2) })
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &file(),
                &target,
                DropOrigin::LOOSE,
                DropModifiers::COPY
            ),
            Some(Operation::CopyInto { folder: TileId(2) })
        );
        // A folder is a file too: dropping one into another is the same operation.
        assert_eq!(
            operation_for(
                TileId(1),
                &folder("/home/u/Notes"),
                &target,
                DropOrigin::LOOSE,
                DropModifiers::MOVE
            ),
            Some(Operation::MoveInto { folder: TileId(2) })
        );
        // Never onto itself, whatever the modifier says.
        for modifiers in [DropModifiers::MOVE, DropModifiers::COPY] {
            assert_eq!(
                operation_for(
                    TileId(2),
                    &folder("/home/u/Documents"),
                    &onto(2, folder("/home/u/Documents")),
                    DropOrigin::LOOSE,
                    modifiers
                ),
                None
            );
        }
    }

    #[test]
    fn drag_drop_names_both_answers_before_release_and_never_confuses_them() {
        let copy = Operation::CopyInto { folder: TileId(2) };
        let moved = Operation::MoveInto { folder: TileId(2) };
        // Every file operation is announced: the invariant is that an unnamed drop changes only
        // position, and these change the filesystem.
        assert_eq!(copy.label(), Some("COPY INTO FOLDER"));
        assert_eq!(moved.label(), Some("MOVE INTO FOLDER"));
        assert_ne!(
            copy.label(),
            moved.label(),
            "a person watching their only copy leave a folder must be able to tell these apart"
        );
        assert!(copy.is_semantic() && moved.is_semantic());
        // And both are routed to the service, not to the field's own undo log.
        assert!(copy.is_file_operation() && moved.is_file_operation());
        for other in [
            Operation::Stash,
            Operation::AddReference { tray: TileId(3) },
            Operation::GroupIntoNewProject { with: TileId(3) },
            Operation::Relocate {
                container: None,
                to: Cell::new(0, 0),
            },
        ] {
            assert!(
                !other.is_file_operation(),
                "{other:?} must not be routed through Files.Operations1"
            );
        }
    }

    #[test]
    fn drag_drop_onto_a_shortcut_pane_still_only_adds_a_reference() {
        // A pane binds no directory, so there is no file operation to choose even when the
        // pointer asks for one: adding to it never relocates what it points at.
        let pane = onto(3, TileKind::Tray { tray_id: TrayId(1) });
        for modifiers in [DropModifiers::MOVE, DropModifiers::COPY] {
            assert_eq!(
                operation_for(TileId(1), &file(), &pane, DropOrigin::LOOSE, modifiers),
                Some(Operation::AddReference { tray: TileId(3) })
            );
        }
        assert!(!Operation::AddReference { tray: TileId(3) }.is_file_operation());
    }

    #[test]
    fn drag_drop_still_refuses_every_other_drop_involving_a_folder() {
        let app = TileKind::App {
            app_id: "org.legeos.Viewer".parse().unwrap(),
        };
        // A folder is not a loose tile to be swept into a project by accident...
        assert_eq!(
            operation_for(
                TileId(1),
                &folder("/home/u/Notes"),
                &onto(2, app.clone()),
                DropOrigin::LOOSE,
                DropModifiers::MOVE
            ),
            None
        );
        // ...and dropping something with no file meaning onto one is refused rather than named.
        assert_eq!(
            operation_for(
                TileId(1),
                &app,
                &onto(2, folder("/home/u/Notes")),
                DropOrigin::LOOSE,
                DropModifiers::COPY
            ),
            None
        );
        assert_eq!(
            operation_for(
                TileId(1),
                &TileKind::Widget {
                    widget: "battery".into(),
                    scope: None
                },
                &onto(2, folder("/home/u/Notes")),
                DropOrigin::LOOSE,
                DropModifiers::MOVE
            ),
            None
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
