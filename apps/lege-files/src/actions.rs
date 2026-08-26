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
    Undo,
    Properties,
    CancelOperation,
    SelectPrevious,
    SelectNext,
    SelectFirst,
    SelectLast,
    SelectAll,
    ClearSelection,
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
    ActionId::Undo,
    ActionId::Properties,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Action(ActionId, SelectionModifiers),
    Navigate(PathBuf),
    Select {
        index: usize,
        modifiers: SelectionModifiers,
        activate_if_selected: bool,
    },
    ConfirmRename(String),
    ConfirmLocation(String),
    DismissDialog,
}

pub fn shortcut(key: &KeyInput, modifiers: Modifiers) -> Option<ActionId> {
    if modifiers.control && modifiers.shift && matches!(key, KeyInput::Character('c' | 'C')) {
        return Some(ActionId::CopyPath);
    }
    if modifiers.alt && matches!(key, KeyInput::Enter) {
        return Some(ActionId::Properties);
    }
    match (key, modifiers.control, modifiers.alt) {
        (KeyInput::Character('l' | 'L'), true, false) => Some(ActionId::Location),
        (KeyInput::Character('h' | 'H'), true, false) => Some(ActionId::ToggleHidden),
        (KeyInput::Character('c' | 'C'), true, false) => Some(ActionId::Copy),
        (KeyInput::Character('x' | 'X'), true, false) => Some(ActionId::Cut),
        (KeyInput::Character('v' | 'V'), true, false) => Some(ActionId::Paste),
        (KeyInput::Character('z' | 'Z'), true, false) => Some(ActionId::Undo),
        (KeyInput::Character('a' | 'A'), true, false) => Some(ActionId::SelectAll),
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
