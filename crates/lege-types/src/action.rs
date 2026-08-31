//! Every user-facing action, declared once.
//!
//! LegeOS already commits to each capability being reachable from the shell, the CLI and the
//! keyboard (`docs/architecture/accessibility.md`: "everything the shell shows is available
//! through `lege` too"). Maintained by hand across three implementations that is a discipline;
//! nothing stops them drifting, and a cheat sheet written separately is out of date the first
//! time a binding moves. Here the declaration is the single source: [`MANIFEST`] carries the
//! human label, the accelerator, the CLI verb and whether the action is destructive, and each
//! surface renders rows from it rather than hard-coding its own.
//!
//! **A chord is an accelerator, never the only route.** Every entry that has one must also have
//! a menu or region that reaches it, because a keyboard is not the only way a person uses this
//! system (`docs/research/omarchy-review.md` §5). [`ActionDecl::chords`] being empty is normal
//! and correct; a chord with no pointer path is a bug.
//!
//! This table *declares*; it does not dispatch. `lege-comp` keeps its own fixed keysym table
//! (`compositor/lege-comp/src/keybindings.rs`) because the recovery chords must keep working
//! with the shell — and the whole session bus — dead, so the compositor must never look anything
//! up to know what `Ctrl+Alt+Space` does. The two are held in agreement by a test, not by a
//! runtime lookup.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::provider::ProviderRole;

/// A key an accelerator can name. Deliberately not xkb keysyms: this crate is linked into the
/// compositor's render thread, the daemons and the CLI alike, and only one of those has an xkb
/// context. The compositor maps its own keysyms onto these when checking agreement.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Key {
    /// A printable key, held lower-case. Matching is case-insensitive.
    Character(char),
    Escape,
    Space,
    Tab,
    Enter,
    Delete,
    Backspace,
    Left,
    Right,
    Up,
    Down,
    /// A function key, by number. F1 is the conventional route to contextual help.
    Function(u8),
    /// The logo key pressed with nothing else — a gesture, not a combination. Press reveals
    /// Home immediately and holding keeps it up momentarily; a short tap latches the toggle.
    /// Tracked by `SuperTapTracker` in the compositor; named here so the cheat sheet can show
    /// the primary route to Home rather than only its fallbacks.
    LogoTap,
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Character(character) => {
                write!(formatter, "{}", character.to_ascii_uppercase())
            }
            Self::Escape => formatter.write_str("Escape"),
            Self::Space => formatter.write_str("Space"),
            Self::Tab => formatter.write_str("Tab"),
            Self::Enter => formatter.write_str("Enter"),
            Self::Delete => formatter.write_str("Delete"),
            Self::Backspace => formatter.write_str("Backspace"),
            Self::Left => formatter.write_str("Left"),
            Self::Right => formatter.write_str("Right"),
            Self::Up => formatter.write_str("Up"),
            Self::Down => formatter.write_str("Down"),
            Self::Function(number) => write!(formatter, "F{number}"),
            Self::LogoTap => formatter.write_str("Super"),
        }
    }
}

/// Which modifiers an accelerator requires. All four must match exactly — a chord that does not
/// name `logo` is not reachable while the logo key is held, which is what keeps `Ctrl+Alt+Space`
/// from firing during a `Super` chord.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize,
)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
    };
    pub const CTRL: Modifiers = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    pub const ALT: Modifiers = Modifiers {
        alt: true,
        ..Modifiers::NONE
    };
    pub const LOGO: Modifiers = Modifiers {
        logo: true,
        ..Modifiers::NONE
    };
    pub const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        ..Modifiers::NONE
    };
    pub const ALT_SHIFT: Modifiers = Modifiers {
        alt: true,
        shift: true,
        ..Modifiers::NONE
    };
    pub const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        shift: true,
        ..Modifiers::NONE
    };
}

impl fmt::Display for Modifiers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.logo, "Super"),
        ] {
            if held {
                write!(formatter, "{name}+")?;
            }
        }
        Ok(())
    }
}

/// One keyboard accelerator.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct Chord {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Chord {
    pub const fn new(modifiers: Modifiers, key: Key) -> Chord {
        Chord { modifiers, key }
    }

    /// A bare key with no modifiers.
    pub const fn bare(key: Key) -> Chord {
        Chord::new(Modifiers::NONE, key)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key == Key::LogoTap {
            return formatter.write_str("Super (tap or hold)");
        }
        write!(formatter, "{}{}", self.modifiers, self.key)
    }
}

/// How a menu groups actions. Ordering is the display order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionCategory {
    /// Moving between windows and Home.
    Navigation,
    /// Acting on one window.
    Window,
    /// Editing the Home Field's layout.
    Field,
    /// Status instruments and their controls.
    System,
    /// Ending or suspending the session.
    Session,
}

impl ActionCategory {
    pub const ALL: [ActionCategory; 5] = [
        Self::Navigation,
        Self::Window,
        Self::Field,
        Self::System,
        Self::Session,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Window => "window",
            Self::Field => "field",
            Self::System => "system",
            Self::Session => "session",
        }
    }

    /// The heading a menu shows for this group.
    pub fn title(self) -> &'static str {
        match self {
            Self::Navigation => "Navigation",
            Self::Window => "Window",
            Self::Field => "Home Field",
            Self::System => "System",
            Self::Session => "Session",
        }
    }
}

impl fmt::Display for ActionCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// When an action is offered. Data, not a closure, so the same declaration can be evaluated by
/// the shell against live state, by the CLI against a bus connection, and by a test against
/// nothing at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "requires")]
pub enum Requires {
    /// Always offered.
    Always,
    /// Needs the trusted-shell compositor connection (`lege_shell_v1`).
    TrustedShell,
    /// Needs a tile to be focused on the field.
    FocusedTile,
    /// Needs at least one tile on the shelf — a stash to undo.
    ShelvedTile,
    /// Needs a live window — a session tile, or the activated window.
    Window,
    /// Needs a selected provider for this role to be installed and launchable.
    Provider(ProviderRole),
}

/// One user-facing action, declared once for every surface that offers it.
///
/// [`Serialize`] but deliberately not `Deserialize`: this is a compile-time table, so the only
/// direction that means anything is *out* — `lege ... --json` emitting the manifest, or a
/// generated cheat sheet. There is no such thing as an action read in from elsewhere; that would
/// be the third-party shell extension the first-release scope rules out.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ActionDecl {
    /// Stable dotted identifier. Never shown to a person; never reused for another meaning.
    pub id: &'static str,
    /// What a menu, a region label or a cheat sheet calls this.
    pub label: &'static str,
    pub category: ActionCategory,
    /// Accelerators, in the order a cheat sheet should list them. Empty is normal.
    pub chords: &'static [Chord],
    /// The `lege` subcommand path that performs the same thing, if one does.
    pub cli_verb: Option<&'static str>,
    /// Whether the action needs a deliberate second step before it happens. Destructive and
    /// session-ending actions set this so no surface can fire one on a stray click.
    pub confirm: bool,
    pub requires: Requires,
}

impl ActionDecl {
    /// The accelerator a cheat sheet shows first, if any.
    pub fn primary_chord(&self) -> Option<Chord> {
        self.chords.first().copied()
    }
}

/// Every declared action. Adding a row here is what makes an action exist for the menu, the
/// cheat sheet and the consistency checks; wiring it up in a surface is a separate step, and
/// `xtask check-actions` is what notices when only one of the two happened.
pub const MANIFEST: &[ActionDecl] = &[
    // ---- Navigation -------------------------------------------------------------------
    ActionDecl {
        id: "field.help",
        label: "Show Home help",
        category: ActionCategory::Navigation,
        chords: &[Chord::bare(Key::Function(1))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "home.reveal",
        label: "Go home",
        category: ActionCategory::Navigation,
        // The bare tap is the primary route (`ROADMAP.md` M2: "a bare Super tap is the primary
        // route between windows and Home"); the other two exist because not every keyboard has
        // a logo key and this must work on all of them.
        chords: &[
            Chord::bare(Key::LogoTap),
            Chord::new(Modifiers::CTRL_ALT, Key::Character('h')),
            Chord::new(Modifiers::LOGO, Key::Character('d')),
        ],
        cli_verb: None,
        confirm: false,
        requires: Requires::TrustedShell,
    },
    ActionDecl {
        id: "window.cycle-forward",
        label: "Next window",
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::ALT, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.cycle-backward",
        label: "Previous window",
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::ALT_SHIFT, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.group-next",
        label: "Next window in this group",
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO, Key::Right)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.group-previous",
        label: "Previous window in this group",
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO, Key::Left)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    // ---- Window -----------------------------------------------------------------------
    // One-press reversible arrangements: the first preset retains the prior layout as the
    // baseline, switching presets recomputes from it, and restore returns to it exactly
    // (`lege_window::arrangement`). Buttons only, by design — presets are for auditioning with
    // a pointer, and the Super chord space stays free for held-gesture semantics.
    ActionDecl {
        id: "windows.arrange.focus",
        label: "Arrange windows: focus",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.halves",
        label: "Arrange windows: halves",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.thirds",
        label: "Arrange windows: thirds",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.quarters",
        label: "Arrange windows: grid",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.main-and-stack",
        label: "Arrange windows: main and stack",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.cushion",
        label: "Arrange windows: cushion",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "pane.new",
        label: "New shortcut pane",
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::TrustedShell,
    },
    ActionDecl {
        id: "windows.restore-arrangement",
        label: "Restore window arrangement",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.show-all",
        label: "Show every window",
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Space)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "window.escape-fullscreen",
        label: "Escape fullscreen or modal",
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Escape)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "window.park",
        label: "Park this window",
        category: ActionCategory::Window,
        // Compositor-owned so the one-workspace substitute remains available with the shell
        // dead, alongside show-all and the fallback switcher.
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Character('p'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.close",
        label: "Close this window",
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        // A window may hold unsaved work and the compositor cannot know: never one click.
        confirm: true,
        requires: Requires::Window,
    },
    // ---- Home Field -------------------------------------------------------------------
    ActionDecl {
        id: "field.undo",
        label: "Undo the last change",
        category: ActionCategory::Field,
        chords: &[Chord::new(Modifiers::CTRL, Key::Character('z'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "field.move",
        label: "Move this tile",
        category: ActionCategory::Field,
        chords: &[Chord::bare(Key::Space)],
        cli_verb: None,
        confirm: false,
        requires: Requires::FocusedTile,
    },
    ActionDecl {
        id: "field.resize",
        label: "Resize this tile",
        category: ActionCategory::Field,
        chords: &[Chord::bare(Key::Character('r'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::FocusedTile,
    },
    ActionDecl {
        id: "field.stash",
        label: "Stash this tile on the shelf",
        category: ActionCategory::Field,
        chords: &[Chord::bare(Key::Delete)],
        cli_verb: None,
        confirm: false,
        requires: Requires::FocusedTile,
    },
    // Stash's inverse. Without it the shelf was a one-way door: `field.undo` reversed a stash
    // only until the next change, and nothing else on any surface could reach a shelved tile.
    ActionDecl {
        id: "field.restore",
        label: "Bring shelved tiles back",
        category: ActionCategory::Field,
        chords: &[Chord::new(Modifiers::CTRL_SHIFT, Key::Character('r'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::ShelvedTile,
    },
    // Deliberately destructive, so it confirms: it discards an arrangement the user built, and
    // unlike every other field action it is not undoable — the stored document and its undo log
    // go together. It is the only way to adopt a changed default layout, because `merge` is
    // add-only and will never move a tile that already has a place.
    ActionDecl {
        id: "field.reset",
        label: "Reset the field to its default layout",
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: None,
        confirm: true,
        requires: Requires::Always,
    },
    // ---- System -----------------------------------------------------------------------
    ActionDecl {
        id: "network.open-control",
        label: "Network",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::NetworkStatusUi),
    },
    ActionDecl {
        id: "launcher.launch",
        label: "Launch an application",
        category: ActionCategory::System,
        // No accelerator: which application to start is a choice, not a chord. The launcher
        // widget's rows and the Home search bar are the pointer and keyboard routes to it.
        chords: &[],
        cli_verb: Some("app launch"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "launcher.search",
        label: "Find an application",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "launcher.show-all",
        label: "Show all",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "notifications.clear",
        label: "Clear notifications",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: true,
        requires: Requires::Always,
    },
    // ---- Session ----------------------------------------------------------------------
    // Volume and backlight: the two hardware levels a person changes several times a day. No
    // chords on any of them — the keyboards that have these keys route them through the
    // compositor's own table, and inventing a second binding here would put two different things
    // on one key. What these declarations buy is the widget region and the `lege` verb, which is
    // what makes the capability reachable without those keys at all.
    ActionDecl {
        id: "volume.decrease",
        label: "Quieter",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status quieter"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "volume.increase",
        label: "Louder",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status louder"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "volume.mute",
        label: "Mute",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status mute"),
        // Not destructive: muting is its own undo, and a confirmation on the control people reach
        // for fastest — when something starts making noise — would defeat the point of it.
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "brightness.decrease",
        label: "Dimmer",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status dimmer"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "brightness.increase",
        label: "Brighter",
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status brighter"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "trash.empty",
        label: "Empty Trash",
        category: ActionCategory::System,
        // No chord, deliberately: nothing that cannot be undone is one keystroke away.
        chords: &[],
        cli_verb: Some("trash empty"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.lock",
        label: "Lock the screen",
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session lock"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.suspend",
        label: "Suspend",
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session suspend"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.logout",
        label: "Log out",
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session logout"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.reboot",
        label: "Restart",
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session reboot"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.poweroff",
        label: "Shut down",
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session poweroff"),
        confirm: true,
        requires: Requires::Always,
    },
];

/// The declaration with this id.
pub fn action(id: &str) -> Option<&'static ActionDecl> {
    MANIFEST.iter().find(|declaration| declaration.id == id)
}

/// Every action in one category, in manifest order.
pub fn in_category(category: ActionCategory) -> impl Iterator<Item = &'static ActionDecl> {
    MANIFEST
        .iter()
        .filter(move |declaration| declaration.category == category)
}

/// The action `chord` performs, if any. Character keys match case-insensitively so a declaration
/// never has to name both cases.
pub fn action_for_chord(chord: Chord) -> Option<&'static ActionDecl> {
    MANIFEST.iter().find(|declaration| {
        declaration
            .chords
            .iter()
            .any(|declared| chords_match(*declared, chord))
    })
}

fn chords_match(declared: Chord, pressed: Chord) -> bool {
    if declared.modifiers != pressed.modifiers {
        return false;
    }
    match (declared.key, pressed.key) {
        (Key::Character(a), Key::Character(b)) => a.eq_ignore_ascii_case(&b),
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_id_is_unique() {
        let mut seen = BTreeSet::new();
        for declaration in MANIFEST {
            assert!(
                seen.insert(declaration.id),
                "duplicate action id {:?}",
                declaration.id
            );
        }
    }

    #[test]
    fn no_chord_is_claimed_twice() {
        let mut seen: Vec<(Chord, &str)> = Vec::new();
        for declaration in MANIFEST {
            for chord in declaration.chords {
                if let Some((_, owner)) = seen
                    .iter()
                    .find(|(existing, _)| chords_match(*existing, *chord))
                {
                    panic!(
                        "chord {chord} is claimed by both {owner:?} and {:?}",
                        declaration.id
                    );
                }
                seen.push((*chord, declaration.id));
            }
        }
    }

    #[test]
    fn every_cli_verb_is_unique() {
        let mut seen = BTreeSet::new();
        for verb in MANIFEST.iter().filter_map(|d| d.cli_verb) {
            assert!(seen.insert(verb), "duplicate CLI verb {verb:?}");
        }
    }

    #[test]
    fn ids_are_dotted_lowercase_so_they_read_the_same_everywhere() {
        for declaration in MANIFEST {
            assert!(
                declaration
                    .id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '-'),
                "action id {:?} is not dotted lower-case",
                declaration.id
            );
            assert!(
                declaration.id.contains('.'),
                "action id {:?} needs a namespace prefix",
                declaration.id
            );
        }
    }

    #[test]
    fn character_chords_are_declared_lower_case_and_match_either_case() {
        for declaration in MANIFEST {
            for chord in declaration.chords {
                if let Key::Character(character) = chord.key {
                    assert!(
                        character.is_ascii_lowercase(),
                        "{:?} declares {character:?}; declare the lower-case form",
                        declaration.id
                    );
                }
            }
        }
        let upper = Chord::new(Modifiers::CTRL, Key::Character('Z'));
        assert_eq!(action_for_chord(upper).map(|d| d.id), Some("field.undo"));
    }

    #[test]
    fn a_destructive_action_is_never_offered_in_one_step() {
        for id in [
            "window.close",
            "session.logout",
            "session.reboot",
            "session.poweroff",
        ] {
            assert!(action(id).is_some_and(|d| d.confirm), "{id} must confirm");
        }
        // …and a harmless one is not made annoying.
        assert!(action("field.undo").is_some_and(|d| !d.confirm));
    }

    #[test]
    fn lookups_find_what_the_surfaces_ask_for() {
        assert_eq!(
            action("session.poweroff").map(|d| d.label),
            Some("Shut down")
        );
        assert_eq!(action("nope.missing"), None);
        assert_eq!(
            action_for_chord(Chord::new(Modifiers::CTRL_ALT, Key::Space)).map(|d| d.id),
            Some("window.show-all")
        );
        // A chord that differs only by a modifier is a different chord.
        assert_eq!(
            action_for_chord(Chord::bare(Key::Space)).map(|d| d.id),
            Some("field.move")
        );
        assert_eq!(
            in_category(ActionCategory::Session).count(),
            5,
            "the session menu's rows"
        );
    }

    #[test]
    fn chords_render_the_way_a_cheat_sheet_should_show_them() {
        assert_eq!(
            Chord::new(Modifiers::CTRL_ALT, Key::Space).to_string(),
            "Ctrl+Alt+Space"
        );
        assert_eq!(
            Chord::new(Modifiers::ALT_SHIFT, Key::Tab).to_string(),
            "Alt+Shift+Tab"
        );
        assert_eq!(
            Chord::new(Modifiers::CTRL, Key::Character('z')).to_string(),
            "Ctrl+Z"
        );
        assert_eq!(Chord::bare(Key::LogoTap).to_string(), "Super (tap or hold)");
        assert_eq!(
            Chord::new(Modifiers::LOGO, Key::Left).to_string(),
            "Super+Left"
        );
    }
}
