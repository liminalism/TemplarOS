//! One action vocabulary for visible controls and keyboard accelerators.

use pixelkit_shell::{KeyInput, Modifiers};
use std::path::PathBuf;

use crate::model::SelectionModifiers;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ActionId {
    Back,
    Forward,
    Up,
    Refresh,
    Location,
    ToggleHidden,
    Open,
    Copy,
    CopyPath,
    Cut,
    Paste,
    Rename,
    Trash,
    /// Open the trash: a listing that is not a directory.
    OpenTrash,
    /// Put the selected trashed items back where they came from.
    Restore,
    /// Remove the selected trashed items for good. No undo.
    DeletePermanently,
    /// Empty every trash. No undo.
    EmptyTrash,
    Undo,
    Properties,
    CancelOperation,
    SelectPrevious,
    SelectNext,
    SelectFirst,
    SelectLast,
    SelectAll,
    ClearSelection,
    /// Cycle the primary sort field (name, type, size, date) — shared contract order.
    CycleSort,
    ToggleSortDirection,
    /// Switch between the detail list and the thumbnail grid.
    ToggleView,
}

/// The actions represented by visible toolbar buttons. Tests require each to have a shortcut.
pub const VISIBLE_ACTIONS: &[ActionId] = &[
    ActionId::Back,
    ActionId::Forward,
    ActionId::Up,
    ActionId::Refresh,
    ActionId::Location,
    ActionId::ToggleHidden,
    ActionId::Open,
    ActionId::Copy,
    ActionId::CopyPath,
    ActionId::Cut,
    ActionId::Paste,
    ActionId::Rename,
    ActionId::Trash,
    ActionId::OpenTrash,
    ActionId::Undo,
    ActionId::Properties,
    ActionId::CycleSort,
    ActionId::ToggleSortDirection,
    ActionId::ToggleView,
];

/// The actions the toolbar offers *instead* while the trash is open. Separate from
/// [`VISIBLE_ACTIONS`] because they are not operations on a directory and most of that toolbar
/// means nothing here: there is no paste into the trash, and no rename of something deleted.
pub const TRASH_ACTIONS: &[ActionId] = &[
    ActionId::Restore,
    ActionId::DeletePermanently,
    ActionId::EmptyTrash,
];

impl ActionId {
    /// Whether performing this cannot be undone, so a surface must ask first.
    pub fn destructive(self) -> bool {
        matches!(self, Self::DeletePermanently | Self::EmptyTrash)
    }

    /// What a button or a confirmation calls it.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenTrash => "Trash",
            Self::Restore => "Restore",
            Self::DeletePermanently => "Delete permanently",
            Self::EmptyTrash => "Empty trash",
            Self::Trash => "Move to Trash",
            other => other.fallback_label(),
        }
    }

    fn fallback_label(self) -> &'static str {
        match self {
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::Up => "Up",
            Self::Refresh => "Refresh",
            Self::Location => "Location",
            Self::ToggleHidden => "Hidden",
            Self::Open => "Open",
            Self::Copy => "Copy",
            Self::CopyPath => "Copy path",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::Rename => "Rename",
            Self::Undo => "Undo",
            Self::Properties => "Properties",
            Self::CycleSort => "Sort",
            Self::ToggleSortDirection => "Direction",
            Self::ToggleView => "View",
            _ => "Action",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Action(ActionId, SelectionModifiers),
    Navigate(PathBuf),
    OpenWindow(PathBuf),
    Bookmark {
        path: PathBuf,
        enabled: bool,
    },
    PasteInto(PathBuf),
    ContextMenu {
        at: (i32, i32),
        target: MenuTarget,
    },
    /// Place the folder on Home as a clickable object, through `HomeField1`.
    AddToHome(PathBuf),
    /// Make this directory a real `Projects1` project, rooted at the directory itself.
    ///
    /// Deliberately not the same thing as [`Command::AddToHome`]: creating a project is a
    /// session fact, and pinning one on Home is a layout choice. Doing both from one row would
    /// make it impossible to have either without the other.
    CreateProject(PathBuf),
    Select {
        index: usize,
        modifiers: SelectionModifiers,
        activate_if_selected: bool,
    },
    ConfirmRename(String),
    ConfirmLocation(String),
    DismissDialog,
    /// The thumbnail grid reported which entry range it can see; the app asks the backend for
    /// the bounded batch the model derives from it.
    ThumbnailsVisible(std::ops::Range<usize>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuTarget {
    Entry(usize),
    Ground,
    Bookmark(PathBuf),
}

pub fn shortcut(key: &KeyInput, modifiers: Modifiers) -> Option<ActionId> {
    if modifiers.control && modifiers.shift && matches!(key, KeyInput::Character('c' | 'C')) {
        return Some(ActionId::CopyPath);
    }
    if modifiers.alt && matches!(key, KeyInput::Enter) {
        return Some(ActionId::Properties);
    }
    // Shift+Delete is the long-standing "skip the trash" gesture; with Control as well it is the
    // whole trash. Both are matched before the table below, which cannot see Shift.
    if modifiers.shift && matches!(key, KeyInput::Delete) {
        return Some(if modifiers.control {
            ActionId::EmptyTrash
        } else {
            ActionId::DeletePermanently
        });
    }
    match (key, modifiers.control, modifiers.alt) {
        (KeyInput::Character('l' | 'L'), true, false) => Some(ActionId::Location),
        (KeyInput::Character('h' | 'H'), true, false) => Some(ActionId::ToggleHidden),
        (KeyInput::Character('c' | 'C'), true, false) => Some(ActionId::Copy),
        (KeyInput::Character('x' | 'X'), true, false) => Some(ActionId::Cut),
        (KeyInput::Character('v' | 'V'), true, false) => Some(ActionId::Paste),
        (KeyInput::Character('z' | 'Z'), true, false) => Some(ActionId::Undo),
        (KeyInput::Character('t' | 'T'), true, false) => Some(ActionId::OpenTrash),
        (KeyInput::Character('r' | 'R'), true, false) => Some(ActionId::Restore),
        (KeyInput::Character('a' | 'A'), true, false) => Some(ActionId::SelectAll),
        (KeyInput::Character('s' | 'S'), true, false) => Some(ActionId::CycleSort),
        (KeyInput::Character('d' | 'D'), true, false) => Some(ActionId::ToggleSortDirection),
        (KeyInput::Character('g' | 'G'), true, false) => Some(ActionId::ToggleView),
        (KeyInput::Left, false, true) => Some(ActionId::Back),
        (KeyInput::Right, false, true) => Some(ActionId::Forward),
        (KeyInput::Backspace, false, false) => Some(ActionId::Up),
        (KeyInput::Function(2), false, false) => Some(ActionId::Rename),
        (KeyInput::Function(5), false, false) => Some(ActionId::Refresh),
        (KeyInput::Delete, false, false) => Some(ActionId::Trash),
        (KeyInput::Enter, false, false) => Some(ActionId::Open),
        (KeyInput::Up, false, false) => Some(ActionId::SelectPrevious),
        (KeyInput::Down, false, false) => Some(ActionId::SelectNext),
        (KeyInput::Home, false, false) => Some(ActionId::SelectFirst),
        (KeyInput::End, false, false) => Some(ActionId::SelectLast),
        (KeyInput::Escape, false, false) => Some(ActionId::ClearSelection),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn interaction_every_visible_action_has_a_keyboard_route() {
        let candidates = [
            (
                KeyInput::Left,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Right,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
            (KeyInput::Backspace, Modifiers::default()),
            (KeyInput::Function(5), Modifiers::default()),
            (
                KeyInput::Character('l'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('h'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (KeyInput::Enter, Modifiers::default()),
            (
                KeyInput::Character('c'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('c'),
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('x'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('v'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (KeyInput::Function(2), Modifiers::default()),
            (KeyInput::Delete, Modifiers::default()),
            (
                KeyInput::Character('t'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('z'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Enter,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('s'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('d'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
            (
                KeyInput::Character('g'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                },
            ),
        ];
        let mapped: Vec<ActionId> = candidates
            .iter()
            .filter_map(|(key, modifiers)| shortcut(key, *modifiers))
            .collect();
        for action in VISIBLE_ACTIONS {
            assert!(mapped.contains(action), "no keyboard route for {action:?}");
        }
    }

    #[test]
    fn interaction_every_trash_action_has_a_keyboard_route_and_says_it_is_destructive() {
        assert_eq!(
            shortcut(
                &KeyInput::Character('r'),
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                }
            ),
            Some(ActionId::Restore)
        );
        assert_eq!(
            shortcut(
                &KeyInput::Delete,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                }
            ),
            Some(ActionId::DeletePermanently)
        );
        assert_eq!(
            shortcut(
                &KeyInput::Delete,
                Modifiers {
                    shift: true,
                    control: true,
                    ..Modifiers::default()
                }
            ),
            Some(ActionId::EmptyTrash)
        );
        // Trashing stays reversible and unconfirmed; the two that cannot be undone say so.
        assert!(!ActionId::Trash.destructive());
        assert!(!ActionId::Restore.destructive());
        assert!(ActionId::DeletePermanently.destructive());
        assert!(ActionId::EmptyTrash.destructive());
    }

    #[test]
    fn interaction_plain_arrows_and_enter_keep_list_navigation_complete() {
        assert_eq!(
            shortcut(&KeyInput::Up, Modifiers::default()),
            Some(ActionId::SelectPrevious)
        );
        assert_eq!(
            shortcut(&KeyInput::Down, Modifiers::default()),
            Some(ActionId::SelectNext)
        );
        assert_eq!(
            shortcut(&KeyInput::Enter, Modifiers::default()),
            Some(ActionId::Open)
        );
    }

    #[test]
    fn properties_and_copy_path_have_distinct_visible_shortcuts() {
        assert!(VISIBLE_ACTIONS.contains(&ActionId::Properties));
        assert!(VISIBLE_ACTIONS.contains(&ActionId::CopyPath));
        assert_eq!(
            shortcut(
                &KeyInput::Enter,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                }
            ),
            Some(ActionId::Properties)
        );
        assert_eq!(
            shortcut(
                &KeyInput::Character('c'),
                Modifiers {
                    control: true,
                    shift: true,
                    ..Modifiers::default()
                }
            ),
            Some(ActionId::CopyPath)
        );
    }
}
