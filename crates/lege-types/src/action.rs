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
    /// The backtick key, left of 1 on most layouts. Not a [`Key::Character`]: it has no case to
    /// match insensitively, and the one chord that uses it (`Super+grave`, the drop-down
    /// terminal) means the *key*, whichever character the layout puts on it.
    Grave,
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
            Self::Grave => formatter.write_str("`"),
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
    pub const LOGO_SHIFT: Modifiers = Modifiers {
        logo: true,
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
    /// What a menu or cheat sheet calls this. A complete phrase; the category is not repeated.
    pub label: &'static str,
    /// What a widget region or compact chrome calls this, when the tile's own heading already
    /// supplies the noun. `None` means [`Self::label`] is short enough to use in both places.
    pub control: Option<&'static str>,
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

    /// The word a control on a tile uses. Falls back to [`Self::label`].
    pub fn control_label(&self) -> &'static str {
        self.control.unwrap_or(self.label)
    }
}

/// Every declared action. Adding a row here is what makes an action exist for the menu, the
/// cheat sheet and the consistency checks; wiring it up in a surface is a separate step, and
/// `xtask check-actions` is what notices when only one of the two happened.
pub const MANIFEST: &[ActionDecl] = &[
    // ---- Navigation -------------------------------------------------------------------
    ActionDecl {
        id: "field.help",
        label: "Help",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::bare(Key::Function(1))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "home.reveal",
        label: "Home",
        control: None,
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
        id: "home.pin-app",
        label: "Keep on Home",
        control: None,
        category: ActionCategory::Navigation,
        // No chord: it applies to *this* application, and a payload-free chord has no way to say
        // which. The pointer route is the session tile's own menu row, which knows.
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    // Alt+Tab moves inside the focused window's *group*; with no group anywhere the loose set is
    // the group, so this is the switcher it has always been (planning doc §2 rules 3-4).
    ActionDecl {
        id: "window.cycle-forward",
        label: "Next window",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::ALT, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.cycle-backward",
        label: "Previous window",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::ALT_SHIFT, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    // Super+Tab crosses between groups, and there is deliberately no Ctrl+Tab and no setting:
    // Ctrl+Tab is the tab-cycling chord in every browser, terminal and editor, and a
    // compositor-reserved chord wins (planning doc §2.1).
    ActionDecl {
        id: "window.group-cycle-forward",
        label: "Next window group",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.group-cycle-backward",
        label: "Previous window group",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO_SHIFT, Key::Tab)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    // The spatial step: the next window to the right or left of this one, by geometry
    // (`lege_window::geometry::contiguous_window`). It stores nothing and means nothing — which
    // is exactly why it is *not* called a group any more: a window group is an explicit, stored,
    // visible thing (`window.group`), and one word cannot mean both.
    ActionDecl {
        id: "window.step-right",
        label: "Step right",
        control: None,
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO, Key::Right)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.step-left",
        label: "Step left",
        control: None,
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
        label: "Focus",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.halves",
        label: "Halves",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.thirds",
        label: "Thirds",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.quarters",
        label: "Grid",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.main-and-stack",
        label: "Main and stack",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "windows.arrange.cushion",
        label: "Cushion",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "pane.new",
        label: "New shortcut pane",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::TrustedShell,
    },
    ActionDecl {
        id: "windows.restore-arrangement",
        label: "Restore arrangement",
        control: None,
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.show-all",
        label: "Show all windows",
        control: None,
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Space)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "window.escape-fullscreen",
        label: "Leave fullscreen",
        control: None,
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Escape)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "window.park",
        label: "Park",
        control: None,
        category: ActionCategory::Window,
        // Compositor-owned so the one-workspace substitute remains available with the shell
        // dead, alongside show-all and the fallback switcher.
        chords: &[Chord::new(Modifiers::CTRL_ALT, Key::Character('p'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    // ---- Window groups ----------------------------------------------------------------
    // A window group is an explicit set of windows that park, return and stack as one
    // (`docs/planning/window-groups-and-application-integration.md` §2). Every row here is
    // reached from Home: grouping is a drop of one window's tile on another's, and the rest are
    // the group tile's own controls and menu rows. The compositor owns the chords, and attaches
    // them to these same ids: `Super+G` groups the focused window with the previously focused
    // one, `Super+Shift+G` takes it out, and dropping one window's band on another's band is the
    // same gesture as the tile drop, on the workspace instead of on Home.
    ActionDecl {
        id: "window.group",
        label: "Group these windows",
        control: Some("Group"),
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::LOGO, Key::Character('g'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.ungroup",
        label: "Take out of group",
        control: Some("Ungroup"),
        category: ActionCategory::Window,
        chords: &[Chord::new(Modifiers::LOGO_SHIFT, Key::Character('g'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.activate-group",
        label: "Raise the whole group",
        control: Some("Raise"),
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.shelve-group",
        label: "Park the whole group",
        control: Some("Park"),
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.unshelve-group",
        label: "Bring the whole group back",
        control: Some("Return"),
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    ActionDecl {
        id: "window.rename-group",
        label: "Rename group",
        control: Some("Rename"),
        category: ActionCategory::Window,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Window,
    },
    // The drop-down terminal (§4.6). `Super+grave`, because a bare ` or ~ is a typed character
    // everywhere — the home directory, Markdown, every shell — and a compositor grab on one would
    // take it from every application. A second press parks the terminal; it never closes it.
    // There is no `Ctrl+Alt+T`; this is the one terminal chord.
    ActionDecl {
        id: "terminal.toggle",
        label: "Show or hide the terminal",
        control: Some("Terminal"),
        category: ActionCategory::Navigation,
        chords: &[Chord::new(Modifiers::LOGO, Key::Grave)],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "window.close",
        label: "Close",
        control: None,
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
        label: "Undo",
        control: None,
        category: ActionCategory::Field,
        chords: &[Chord::new(Modifiers::CTRL, Key::Character('z'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "field.move",
        label: "Move",
        control: None,
        category: ActionCategory::Field,
        chords: &[Chord::bare(Key::Space)],
        cli_verb: None,
        confirm: false,
        requires: Requires::FocusedTile,
    },
    ActionDecl {
        id: "field.resize",
        label: "Resize",
        control: None,
        category: ActionCategory::Field,
        chords: &[Chord::bare(Key::Character('r'))],
        cli_verb: None,
        confirm: false,
        requires: Requires::FocusedTile,
    },
    ActionDecl {
        id: "field.stash",
        label: "Stash",
        control: None,
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
        label: "Restore from shelf",
        control: None,
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
        label: "Reset Home",
        control: None,
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
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::NetworkStatusUi),
    },
    ActionDecl {
        id: "launcher.launch",
        label: "Launch",
        control: None,
        category: ActionCategory::System,
        // No accelerator: which application to start is a choice, not a chord. The launcher
        // widget's rows and the Home search bar are the pointer and keyboard routes to it.
        chords: &[],
        cli_verb: Some("app launch"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "app.launch-variant",
        label: "Start this copy",
        control: None,
        category: ActionCategory::System,
        // No accelerator, for the same reason `launcher.launch` has none: *which* copy is a
        // choice, and a chord cannot carry one. The pointer route is the application tile's own
        // menu, which lists the copies that exist.
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "launcher.search",
        label: "Apps",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "launcher.show-all",
        label: "Show all applications",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "notifications.clear",
        label: "Clear notifications",
        control: Some("Clear"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: true,
        requires: Requires::Always,
    },
    // ---- Session ----------------------------------------------------------------------
    // Ordinary file manipulation inside Home's file surfaces. No chords here, deliberately, and
    // the reason is not timidity: these are surface-local accelerators handled by the file
    // surface that has focus (`shell/lege-shell/src/surface/file_browser.rs`), not compositor
    // chords. Ctrl+C means "copy this file" over a folder listing and something else entirely
    // over a terminal, so reserving it globally would be wrong — and the compositor's fixed
    // keysym table, which must keep working with the whole session bus dead, has no business
    // carrying a paste. What these declarations buy is the pointer route on the verb row and one
    // agreed name per verb, so the Home surface and the Files app cannot call the same thing two
    // different things.
    ActionDecl {
        id: "files.open",
        label: "Open",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    // The one file verb that needs an application the person chose rather than one LegeOS
    // ships: whichever terminal fills the exclusive `terminal` role, started in the folder
    // being looked at. With no terminal installed the row is not offered, which is what
    // `Requires::Provider` says.
    ActionDecl {
        id: "files.open-terminal",
        label: "Open terminal here",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::Terminal),
    },
    // ---- Projects ---------------------------------------------------------------------
    // Entering a project and restoring one are deliberately two actions, not one with a
    // surprise: `Projects1.SetCurrent` selects the context and starts nothing, and
    // `Projects1.Open` starts the applications the project remembers. A person who wanted the
    // first and got the second has had eight windows opened at them.
    ActionDecl {
        id: "project.enter",
        label: "Enter project",
        control: Some("Enter"),
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("project current"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "project.restore",
        label: "Restore workspace",
        control: Some("Restore"),
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("project open"),
        confirm: false,
        requires: Requires::Always,
    },
    // The three tools, each a provider role rather than a path copied into a project: "open this
    // project in the editor" has to keep meaning something after the person changes editors.
    // Every one of them opens at the project's real root, which is the service's answer and not
    // a directory this shell inferred.
    ActionDecl {
        id: "project.open-terminal",
        label: "Open terminal at project",
        control: Some("Terminal"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::Terminal),
    },
    ActionDecl {
        id: "project.open-files",
        label: "Open project folder",
        control: Some("Files"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::FileManager),
    },
    ActionDecl {
        id: "project.open-editor",
        label: "Open project in editor",
        control: Some("Editor"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::CodeEditor),
    },
    // Creating a project from a folder you are already looking at, which is where the root comes
    // from — never a directory invented under ~/Projects for a name. Files only: the Home Field
    // has no selected real directory to mean.
    // A Place's tools (@legeos.decision.homegarden-activity-grown-places): the same three roles
    // a project offers, for any folder Home has grown or the user placed. Resolved through
    // Providers1 at press time; Home never names kitty or pcmanfm-qt.
    ActionDecl {
        id: "place.open-terminal",
        label: "Open terminal here",
        control: Some("Terminal"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::Terminal),
    },
    ActionDecl {
        id: "place.open-files",
        label: "Open folder",
        control: Some("Files"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::FileManager),
    },
    ActionDecl {
        id: "place.open-editor",
        label: "Open in editor",
        control: Some("Edit"),
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::CodeEditor),
    },
    // Manual gardening: the constraints the tender obeys. Pin/keep/hide/forget act on the
    // ledger's record of a Place; the tile itself follows at the next tending moment.
    ActionDecl {
        id: "garden.pin",
        label: "Pin here",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden pin"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.unpin",
        label: "Unpin",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden unpin"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.keep",
        label: "Keep this size and place",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden keep"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.hide",
        label: "Hide from Home",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden hide"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.forget",
        label: "Remove from Home",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden forget"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.why",
        label: "Why is this here?",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden why"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.tend",
        label: "Tend Home now",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden tend"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "garden.forget-all",
        label: "Forget every unpinned place",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("garden clear"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "project.create-here",
        label: "Create project here",
        control: None,
        category: ActionCategory::Field,
        chords: &[],
        cli_verb: Some("project create"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.select-all",
        label: "Select all",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.sort",
        label: "Sort by…",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.show-hidden",
        label: "Show hidden",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    // The way out of the folder you are in and into the search that covers the machine. Like
    // "Open terminal here" it needs an application the person chose, so it is not offered when
    // no file-search provider is installed.
    ActionDecl {
        id: "files.search-here",
        label: "Search here…",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Provider(ProviderRole::FileSearch),
    },
    ActionDecl {
        id: "files.copy",
        label: "Copy",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.cut",
        label: "Cut",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.paste",
        label: "Paste",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.trash",
        label: "Move to Trash",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        // Not confirmed, and that is the point of a trash: trashing is the reversible half of
        // removal and it reports the undo that puts it back. A confirmation on the reversible
        // action would train people to dismiss the one on `trash.empty`, which is not reversible.
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.rename",
        label: "Rename",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.new-folder",
        label: "New folder",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.properties",
        label: "Properties",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "files.copy-path",
        label: "Copy path",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    // The way in to everything the Settings widget only reports. The widget is a readout with two
    // or three figures on it; changing anything beyond a step belongs in a window with room to
    // explain itself, and this is the one declared route from the glance to that window.
    ActionDecl {
        id: "settings.open",
        label: "Open Settings",
        control: Some("Open"),
        category: ActionCategory::System,
        // No chord: it opens a window, and nothing that opens a window is a keystroke away here.
        chords: &[],
        // No `lege` verb either — `lege-settings` is an application a person launches, and a CLI
        // verb whose whole effect is "start a graphical program" is not a capability, it is a
        // shortcut for one.
        cli_verb: None,
        confirm: false,
        requires: Requires::Always,
    },
    // Volume and backlight: the two hardware levels a person changes several times a day. No
    // chords on any of them — the keyboards that have these keys route them through the
    // compositor's own table, and inventing a second binding here would put two different things
    // on one key. What these declarations buy is the widget region and the `lege` verb, which is
    // what makes the capability reachable without those keys at all.
    ActionDecl {
        id: "volume.decrease",
        label: "Quieter",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status quieter"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "volume.increase",
        label: "Louder",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status louder"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "volume.mute",
        label: "Mute",
        control: None,
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
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status dimmer"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "brightness.increase",
        label: "Brighter",
        control: None,
        category: ActionCategory::System,
        chords: &[],
        cli_verb: Some("status brighter"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "trash.empty",
        label: "Empty Trash",
        control: Some("Empty"),
        category: ActionCategory::System,
        // No chord, deliberately: nothing that cannot be undone is one keystroke away.
        chords: &[],
        cli_verb: Some("trash empty"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.lock",
        label: "Lock",
        control: None,
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session lock"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.suspend",
        label: "Suspend",
        control: None,
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session suspend"),
        confirm: false,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.logout",
        label: "Log out",
        control: None,
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session logout"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.reboot",
        label: "Restart",
        control: None,
        category: ActionCategory::Session,
        chords: &[],
        cli_verb: Some("session reboot"),
        confirm: true,
        requires: Requires::Always,
    },
    ActionDecl {
        id: "session.poweroff",
        label: "Shut down",
        control: None,
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
    fn a_control_is_the_short_form_when_the_tile_already_names_the_noun() {
        let settings = action("settings.open").expect("declared");
        assert_eq!(settings.label, "Open Settings");
        assert_eq!(settings.control_label(), "Open");
        let lock = action("session.lock").expect("declared");
        assert_eq!(lock.control_label(), lock.label);
        assert_eq!(lock.label, "Lock");
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
