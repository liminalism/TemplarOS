//! Commands only the trusted shell connection may submit, and the activation grants that gate
//! stealing focus (planning doc 02 §6).
//!
//! Identity (`CachedIdentity`) says *who owns* a window; this module says *whether a request to
//! activate it is allowed*. Measuring "recent input" needs real input-event timestamps, which
//! live in the fork / `lege-comp-integration`, not here — [`ActivationGrant`] only records their
//! verdict, so it stays testable without a real input stack.

use crate::{
    IdentityCache, Rect, WindowId, WindowService,
    arrangement::ArrangementPreset,
    geometry::{move_into_view, scatter_layout},
    group::GroupId,
    service::EscapeAction,
};
use lege_types::InstanceId;
use std::fmt;

/// A compositor-issued, single-use permission to steal focus (xdg-activation semantics).
/// "Single-use" is enforced here, not merely asserted: `WindowService` remembers every spent
/// `token` until it would have expired anyway (`try_spend_activation_token`), so replaying an
/// identical grant within its validity window fails with `CommandError::GrantAlreadyUsed`
/// instead of activating the window a second time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationGrant {
    pub token: String,
    pub requesting_instance: Option<InstanceId>,
    /// If set, the grant is only good for a window owned by this instance.
    pub target_instance: Option<InstanceId>,
    pub expires_at_monotonic_ns: u64,
    /// Whether the issuer observed recent user input before minting this grant.
    pub had_recent_input_evidence: bool,
}

/// Commands only the trusted shell connection may submit over the private protocol, about
/// *client windows*. Layer surfaces the shell owns itself (its overlay chrome, notifications,
/// the Home Surface) go through [`WindowService::set_layer`] directly with
/// [`crate::layers::ClientKind::TrustedShell`] instead — those are shell surfaces, not commands
/// about someone else's window.
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
    /// `None` escapes the currently activated fullscreen/modal window, if any.
    EscapeFullscreenOrModal {
        window_id: Option<WindowId>,
    },
    /// The compositor-owned recovery view: every window, shelved ones included, scattered
    /// across the outputs. Ends any "go home" reveal.
    ShowAllWindows,
    /// `xdg_toplevel.set_maximized` / `unset_maximized`, or the same from the shell.
    SetMaximized {
        window_id: WindowId,
        maximized: bool,
    },
    /// "Go home": hide every client window so the shell's Home Surface shows, or bring them
    /// back. A view change only — see [`WindowService::set_home_revealed`].
    RevealHome {
        reveal: RevealHome,
    },
    /// Arrange every eligible window per the named preset
    /// ([`WindowService::arrange_windows`]): the first request retains the pre-preset layout as
    /// the baseline, and switching presets recomputes from that baseline and the current
    /// eligible set — user-invoked arrangement, never a standing tiling policy.
    ArrangeWindows {
        preset: ArrangementPreset,
    },
    /// Return every surviving window exactly to the retained baseline
    /// ([`WindowService::restore_arrangement`]), repaired only where the output topology makes
    /// the old rectangle unreachable.
    RestoreArrangement,
    /// Put `b` in `a`'s group — making one if neither has one, merging if both do — and set `b`
    /// down beside `a` (`WindowService::group` then `WindowService::place_beside`). Grouping
    /// places; it never stacks, tabs or folds windows together (planning doc §2 rule 6).
    Group {
        a: WindowId,
        b: WindowId,
    },
    /// Take one window out of its group; the group dissolves if that leaves fewer than two.
    Ungroup {
        window_id: WindowId,
    },
    /// Activate `window_id`, which raises its whole group with it. The grant is checked exactly
    /// as [`WindowCommand::Activate`]'s is — a group is not a way around focus stealing.
    ActivateGroup {
        window_id: WindowId,
        grant: ActivationGrant,
    },
    /// Park every member of this window's group under one shelf rank, so the shelf lists the
    /// group as a unit.
    ShelveGroup {
        window_id: WindowId,
    },
    /// Bring every shelved member of this window's group back.
    UnshelveGroup {
        window_id: WindowId,
    },
    /// Name this window's group (empty clears the name).
    SetGroupName {
        window_id: WindowId,
        name: String,
    },
}

/// What [`WindowCommand::RevealHome`] should do to the reveal that may or may not be in effect.
/// `Toggle` is resolved inside `handle_command`, so a keybinding needs no state of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RevealHome {
    Show,
    Hide,
    Toggle,
}

/// Time and trust context a command is evaluated under.
#[derive(Clone, Copy, Debug)]
pub struct CommandContext {
    /// Only the connection the compositor itself launched as the shell may pass `true`.
    pub trusted_shell: bool,
    pub now_monotonic_ns: u64,
}

/// What applying a command changed, for the caller to turn into protocol events / configures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandEffect {
    Activated {
        window_id: WindowId,
    },
    Closed {
        window_id: WindowId,
    },
    Moved {
        window_id: WindowId,
        geometry: Rect,
    },
    Shelved {
        window_id: WindowId,
    },
    Unshelved {
        window_id: WindowId,
        geometry: Rect,
    },
    Escaped {
        action: EscapeAction,
    },
    ShowAllWindows {
        layout: Vec<(WindowId, Rect)>,
    },
    /// The geometry to configure the client with. Absent when the window was already in the
    /// requested state, so nothing needs to be sent.
    Maximized {
        window_id: WindowId,
        geometry: Option<Rect>,
    },
    HomeRevealed {
        revealed: bool,
    },
    /// The windows an arrangement (or its restore) moved, with the geometry to configure.
    Arranged {
        moves: Vec<(WindowId, Rect)>,
    },
    /// Two windows are now one group. `geometry` is where the joining window was set down, absent
    /// when it was already beside its target and nothing moved.
    Grouped {
        group: GroupId,
        window_id: WindowId,
        geometry: Option<Rect>,
    },
    /// A window left `group`. The group may no longer exist — it dissolves below two members —
    /// so the caller re-reads membership rather than assuming it survived.
    Ungrouped {
        group: GroupId,
        window_id: WindowId,
    },
    /// Every window a group-wide shelve parked.
    GroupShelved {
        windows: Vec<WindowId>,
    },
    /// Every window a group-wide unshelve brought back, with the geometry to configure.
    GroupUnshelved {
        windows: Vec<(WindowId, Rect)>,
    },
    /// A group's name changed; the shell re-reads it from the next `group` event.
    GroupRenamed {
        group: GroupId,
    },
}

/// Why a command was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandError {
    /// Only the trusted shell connection may submit any [`WindowCommand`].
    Untrusted,
    UnknownWindow,
    GrantExpired,
    GrantMissingInputEvidence,
    GrantInstanceMismatch,
    /// The grant's token already activated a window once; xdg-activation tokens are single-use.
    GrantAlreadyUsed,
    NothingToEscape,
    /// A group request named two windows that cannot be grouped: the same window twice, or one
    /// that is not an application window (shell chrome, the Home Surface).
    NotGroupable,
    /// A group request named a window that is in no group.
    NotGrouped,
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Untrusted => "only the trusted shell connection may submit window commands",
            Self::UnknownWindow => "no such window",
            Self::GrantExpired => "activation grant has expired",
            Self::GrantMissingInputEvidence => "activation grant lacks recent-input evidence",
            Self::GrantInstanceMismatch => "activation grant does not name this window's instance",
            Self::GrantAlreadyUsed => "activation grant has already been used",
            Self::NothingToEscape => "no fullscreen or modal window to escape",
            Self::NotGroupable => "these two windows cannot be grouped",
            Self::NotGrouped => "that window is in no group",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CommandError {}

impl<I: IdentityCache> WindowService<I> {
    /// Apply one [`WindowCommand`]. `Close` only reports intent: the caller still sends the
    /// client an `xdg_toplevel.close`, and the window leaves the service through the resulting
    /// `on_toplevel_destroyed`, not here.
    pub fn handle_command(
        &mut self,
        command: WindowCommand,
        ctx: CommandContext,
    ) -> Result<CommandEffect, CommandError> {
        if !ctx.trusted_shell {
            return Err(CommandError::Untrusted);
        }

        match command {
            // One arm, deliberately: activating a grouped window already raises its whole group
            // (`WindowService::activate`), so `ActivateGroup` differs from `Activate` only in
            // which surface asked — and a group is not a way around the focus-stealing checks.
            WindowCommand::Activate { window_id, grant }
            | WindowCommand::ActivateGroup { window_id, grant } => {
                self.handle_activate(window_id, &grant, ctx.now_monotonic_ns)
            }
            WindowCommand::Close { window_id } => {
                self.window(window_id).ok_or(CommandError::UnknownWindow)?;
                Ok(CommandEffect::Closed { window_id })
            }
            WindowCommand::MoveToVisibleArea { window_id } => {
                let geometry = self
                    .window(window_id)
                    .ok_or(CommandError::UnknownWindow)?
                    .geometry;
                let moved = move_into_view(geometry, self.outputs());
                self.set_geometry(window_id, moved);
                Ok(CommandEffect::Moved {
                    window_id,
                    geometry: moved,
                })
            }
            WindowCommand::Shelve { window_id } => {
                if self.shelve(window_id) {
                    Ok(CommandEffect::Shelved { window_id })
                } else {
                    Err(CommandError::UnknownWindow)
                }
            }
            WindowCommand::Unshelve { window_id } => {
                let geometry = self
                    .unshelve(window_id)
                    .ok_or(CommandError::UnknownWindow)?;
                Ok(CommandEffect::Unshelved {
                    window_id,
                    geometry,
                })
            }
            WindowCommand::EscapeFullscreenOrModal { window_id } => self
                .escape(window_id)
                .map(|action| CommandEffect::Escaped { action })
                .ok_or(CommandError::NothingToEscape),
            WindowCommand::ShowAllWindows => {
                // "Show every window" means every window. The shelf is a shell surface, and the
                // shell is a restartable process that may be dead — which is exactly when this
                // command gets used — so a shelved window that stayed hidden here would be the
                // one kind of window this recovery view could not recover. Unshelving is an
                // honest, visible state change the user can undo by shelving again; it is not a
                // hidden side effect. A reveal ends too: the two views are mutually exclusive.
                let shelved: Vec<WindowId> = self
                    .windows()
                    .filter(|window| self.is_client_window(window.id) && window.shelved)
                    .map(|window| window.id)
                    .collect();
                for window_id in shelved {
                    self.unshelve(window_id);
                }
                self.set_home_revealed(false);
                let windows: Vec<(WindowId, Rect)> = self
                    .windows()
                    .filter(|window| self.is_client_window(window.id))
                    .map(|window| (window.id, window.geometry))
                    .collect();
                let layout = scatter_layout(&windows, self.outputs());
                Ok(CommandEffect::ShowAllWindows { layout })
            }
            WindowCommand::SetMaximized {
                window_id,
                maximized,
            } => {
                self.window(window_id).ok_or(CommandError::UnknownWindow)?;
                let geometry = self.set_maximized(window_id, maximized);
                Ok(CommandEffect::Maximized {
                    window_id,
                    geometry,
                })
            }
            WindowCommand::ArrangeWindows { preset } => Ok(CommandEffect::Arranged {
                moves: self.arrange_windows(preset),
            }),
            WindowCommand::RestoreArrangement => Ok(CommandEffect::Arranged {
                moves: self.restore_arrangement(),
            }),
            WindowCommand::Group { a, b } => self.apply_group(a, b),
            WindowCommand::Ungroup { window_id } => self.apply_ungroup(window_id),
            WindowCommand::ShelveGroup { window_id } => self.apply_shelve_group(window_id),
            WindowCommand::UnshelveGroup { window_id } => self.apply_unshelve_group(window_id),
            WindowCommand::SetGroupName { window_id, name } => {
                self.apply_set_group_name(window_id, name)
            }
            WindowCommand::RevealHome { reveal } => {
                let revealed = match reveal {
                    RevealHome::Show => true,
                    RevealHome::Hide => false,
                    RevealHome::Toggle => !self.home_revealed(),
                };
                self.set_home_revealed(revealed);
                Ok(CommandEffect::HomeRevealed { revealed })
            }
        }
    }

    /// [`WindowCommand::Group`]: group, then place. The two halves are one command because rule 6
    /// says they are one thing — a group whose windows were left overlapping wherever they
    /// happened to be would not read as one piece of work.
    fn apply_group(&mut self, a: WindowId, b: WindowId) -> Result<CommandEffect, CommandError> {
        let group = self.group(a, b).ok_or(CommandError::NotGroupable)?;
        let geometry = self.place_beside(b, a);
        Ok(CommandEffect::Grouped {
            group,
            window_id: b,
            geometry,
        })
    }

    fn apply_ungroup(&mut self, window_id: WindowId) -> Result<CommandEffect, CommandError> {
        self.window(window_id).ok_or(CommandError::UnknownWindow)?;
        let group = self.ungroup(window_id).ok_or(CommandError::NotGrouped)?;
        Ok(CommandEffect::Ungrouped { group, window_id })
    }

    fn apply_shelve_group(&mut self, window_id: WindowId) -> Result<CommandEffect, CommandError> {
        self.window(window_id).ok_or(CommandError::UnknownWindow)?;
        Ok(CommandEffect::GroupShelved {
            windows: self.shelve_group(window_id),
        })
    }

    fn apply_unshelve_group(&mut self, window_id: WindowId) -> Result<CommandEffect, CommandError> {
        self.window(window_id).ok_or(CommandError::UnknownWindow)?;
        Ok(CommandEffect::GroupUnshelved {
            windows: self.unshelve_group(window_id),
        })
    }

    fn apply_set_group_name(
        &mut self,
        window_id: WindowId,
        name: String,
    ) -> Result<CommandEffect, CommandError> {
        self.window(window_id).ok_or(CommandError::UnknownWindow)?;
        let group = self
            .set_group_name(window_id, Some(name))
            .ok_or(CommandError::NotGrouped)?;
        Ok(CommandEffect::GroupRenamed { group })
    }

    fn handle_activate(
        &mut self,
        window_id: WindowId,
        grant: &ActivationGrant,
        now_monotonic_ns: u64,
    ) -> Result<CommandEffect, CommandError> {
        let window = self.window(window_id).ok_or(CommandError::UnknownWindow)?;

        if now_monotonic_ns >= grant.expires_at_monotonic_ns {
            return Err(CommandError::GrantExpired);
        }
        if !grant.had_recent_input_evidence {
            return Err(CommandError::GrantMissingInputEvidence);
        }
        if let Some(target) = grant.target_instance
            && window.identity.instance_id != Some(target)
        {
            return Err(CommandError::GrantInstanceMismatch);
        }

        // Last, and only for an otherwise-valid grant: consuming the token here (rather than
        // before the checks above) means a malformed or mismatched replay is reported for its
        // real reason, not masked as "already used".
        if !self.try_spend_activation_token(
            &grant.token,
            grant.expires_at_monotonic_ns,
            now_monotonic_ns,
        ) {
            return Err(CommandError::GrantAlreadyUsed);
        }

        self.activate(window_id);
        Ok(CommandEffect::Activated { window_id })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientKind, Layer, Output, WindowClaims};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeCache {
        requests: Mutex<Vec<(WindowId, u32)>>,
    }

    impl IdentityCache for FakeCache {
        fn resolve_cached(
            &self,
            _pid: u32,
            _claims: &WindowClaims,
        ) -> Option<crate::CachedIdentity> {
            None
        }
        fn request_resolution(&self, window_id: WindowId, pid: u32, _claims: WindowClaims) {
            self.requests.lock().unwrap().push((window_id, pid));
        }
    }

    fn service_with_one_window() -> (WindowService<FakeCache>, WindowId) {
        let outputs = vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)];
        let mut service = WindowService::new(FakeCache::default(), outputs);
        let id = service.on_toplevel_mapped(
            1,
            ClientKind::Application,
            WindowClaims::default(),
            "win".into(),
            Rect::new(100, 100, 400, 300),
        );
        (service, id)
    }

    fn trusted(now: u64) -> CommandContext {
        CommandContext {
            trusted_shell: true,
            now_monotonic_ns: now,
        }
    }

    fn grant(expires_at: u64) -> ActivationGrant {
        ActivationGrant {
            token: "tok".into(),
            requesting_instance: None,
            target_instance: None,
            expires_at_monotonic_ns: expires_at,
            had_recent_input_evidence: true,
        }
    }

    #[test]
    fn untrusted_connections_cannot_submit_any_command() {
        let (mut service, id) = service_with_one_window();
        let ctx = CommandContext {
            trusted_shell: false,
            now_monotonic_ns: 0,
        };
        let result = service.handle_command(WindowCommand::Close { window_id: id }, ctx);
        assert_eq!(result, Err(CommandError::Untrusted));
    }

    #[test]
    fn activate_requires_unexpired_grant() {
        let (mut service, id) = service_with_one_window();
        let result = service.handle_command(
            WindowCommand::Activate {
                window_id: id,
                grant: grant(100),
            },
            trusted(200),
        );
        assert_eq!(result, Err(CommandError::GrantExpired));
    }

    #[test]
    fn activate_requires_recent_input_evidence() {
        let (mut service, id) = service_with_one_window();
        let mut grant = grant(1_000);
        grant.had_recent_input_evidence = false;
        let result = service.handle_command(
            WindowCommand::Activate {
                window_id: id,
                grant,
            },
            trusted(0),
        );
        assert_eq!(result, Err(CommandError::GrantMissingInputEvidence));
    }

    #[test]
    fn activate_rejects_a_grant_naming_a_different_instance() {
        let (mut service, id) = service_with_one_window();
        let mut grant = grant(1_000);
        grant.target_instance = Some(InstanceId::new());
        let result = service.handle_command(
            WindowCommand::Activate {
                window_id: id,
                grant,
            },
            trusted(0),
        );
        assert_eq!(result, Err(CommandError::GrantInstanceMismatch));
    }

    #[test]
    fn a_valid_grant_activates_the_window() {
        let (mut service, id) = service_with_one_window();
        let result = service.handle_command(
            WindowCommand::Activate {
                window_id: id,
                grant: grant(1_000),
            },
            trusted(0),
        );
        assert_eq!(result, Ok(CommandEffect::Activated { window_id: id }));
        assert!(service.window(id).unwrap().activated);
    }

    #[test]
    fn a_grant_cannot_be_replayed() {
        let (mut service, first_window) = service_with_one_window();
        let second_window = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );

        let first = service.handle_command(
            WindowCommand::Activate {
                window_id: first_window,
                grant: grant(1_000),
            },
            trusted(0),
        );
        assert_eq!(
            first,
            Ok(CommandEffect::Activated {
                window_id: first_window
            })
        );

        // Replay the identical grant (same token) against a *different* window, well before it
        // expires: every other check still passes, but the token was already spent, so this
        // must not steal activation away from `first_window`.
        let replay = service.handle_command(
            WindowCommand::Activate {
                window_id: second_window,
                grant: grant(1_000),
            },
            trusted(500),
        );
        assert_eq!(replay, Err(CommandError::GrantAlreadyUsed));
        assert!(service.window(first_window).unwrap().activated);
        assert!(!service.window(second_window).unwrap().activated);
    }

    #[test]
    fn spending_one_token_does_not_block_a_different_grant() {
        let (mut service, id) = service_with_one_window();
        let mut second_grant = grant(1_000);
        second_grant.token = "a-different-token".into();

        assert!(
            service
                .handle_command(
                    WindowCommand::Activate {
                        window_id: id,
                        grant: grant(1_000)
                    },
                    trusted(0)
                )
                .is_ok()
        );
        assert!(
            service
                .handle_command(
                    WindowCommand::Activate {
                        window_id: id,
                        grant: second_grant
                    },
                    trusted(0)
                )
                .is_ok()
        );
    }

    #[test]
    fn move_to_visible_area_uses_move_into_view() {
        let (mut service, id) = service_with_one_window();
        service.set_geometry(id, Rect::new(9000, 9000, 400, 300));
        let result = service.handle_command(
            WindowCommand::MoveToVisibleArea { window_id: id },
            trusted(0),
        );
        let CommandEffect::Moved { geometry, .. } = result.unwrap() else {
            panic!("expected Moved");
        };
        assert!(crate::is_recoverable(geometry, service.outputs()));
        // Fits entirely within the only output.
        assert_eq!(
            service.outputs()[0].logical.intersection(geometry),
            Some(geometry)
        );
    }

    #[test]
    fn shelve_then_unshelve_round_trips_through_commands() {
        let (mut service, id) = service_with_one_window();
        assert_eq!(
            service.handle_command(WindowCommand::Shelve { window_id: id }, trusted(0)),
            Ok(CommandEffect::Shelved { window_id: id })
        );
        assert!(service.window(id).unwrap().shelved);

        let result = service.handle_command(WindowCommand::Unshelve { window_id: id }, trusted(0));
        assert!(matches!(result, Ok(CommandEffect::Unshelved { .. })));
        assert!(!service.window(id).unwrap().shelved);
    }

    #[test]
    fn escape_with_nothing_to_escape_errors() {
        let (mut service, id) = service_with_one_window();
        let result = service.handle_command(
            WindowCommand::EscapeFullscreenOrModal {
                window_id: Some(id),
            },
            trusted(0),
        );
        assert_eq!(result, Err(CommandError::NothingToEscape));
    }

    #[test]
    fn show_all_windows_unshelves_everything_and_ends_a_reveal() {
        let (mut service, id) = service_with_one_window();
        let shelved = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            String::new(),
            Rect::new(0, 0, 100, 100),
        );
        service.shelve(shelved);
        let home = service.on_toplevel_mapped(
            3,
            ClientKind::TrustedShell,
            WindowClaims::default(),
            "Home Surface".into(),
            Rect::new(0, 0, 1920, 1080),
        );
        service
            .set_layer(home, Layer::HomeSurface, ClientKind::TrustedShell)
            .unwrap();
        service.set_home_revealed(true);

        let result = service.handle_command(WindowCommand::ShowAllWindows, trusted(0));
        let Ok(CommandEffect::ShowAllWindows { layout }) = result else {
            panic!("expected ShowAllWindows");
        };
        let mut ids: Vec<WindowId> = layout.iter().map(|(id, _)| *id).collect();
        ids.sort();
        assert_eq!(ids, vec![id, shelved]);
        assert!(!layout.iter().any(|(id, _)| *id == home));
        assert!(!service.window(shelved).unwrap().shelved);
        assert!(!service.home_revealed());
        for (id, rect) in layout {
            assert!(
                crate::is_recoverable(rect, service.outputs()),
                "{id:?} scattered somewhere unrecoverable: {rect:?}"
            );
        }
    }

    /// `switch-from-baseline` and restore: A→B is computed from the retained baseline (the
    /// preset geometry is a function of the window set and output, never of A's result), the
    /// baseline survives switching, and Restore returns every surviving window exactly.
    #[test]
    fn arrangement_presets_are_reversible() {
        let (mut service, first) = service_with_one_window();
        let second = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "two".into(),
            Rect::new(300, 200, 500, 400),
        );
        let third = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "three".into(),
            Rect::new(700, 300, 300, 300),
        );
        service
            .handle_command(
                WindowCommand::SetMaximized {
                    window_id: third,
                    maximized: true,
                },
                trusted(0),
            )
            .unwrap();
        let before: Vec<_> = [first, second, third]
            .iter()
            .map(|id| {
                let window = service.window(*id).unwrap();
                (*id, window.geometry, window.maximized)
            })
            .collect();

        let a = service
            .handle_command(
                WindowCommand::ArrangeWindows {
                    preset: crate::ArrangementPreset::Halves,
                },
                trusted(0),
            )
            .unwrap();
        assert!(matches!(a, CommandEffect::Arranged { ref moves } if moves.len() == 3));
        assert!(service.arrangement_active());

        // Switching to B from A equals applying B on a service that never saw A: geometry is
        // computed from the baseline and window set, not from A's result.
        let b = service
            .handle_command(
                WindowCommand::ArrangeWindows {
                    preset: crate::ArrangementPreset::Cushion,
                },
                trusted(0),
            )
            .unwrap();
        let mut fresh = {
            let (mut fresh, fresh_first) = service_with_one_window();
            assert_eq!(fresh_first, first);
            for (pid, title, rect) in [
                (2u32, "two", Rect::new(300, 200, 500, 400)),
                (3, "three", Rect::new(700, 300, 300, 300)),
            ] {
                let mapped = fresh.on_toplevel_mapped(
                    pid,
                    ClientKind::Application,
                    WindowClaims::default(),
                    title.into(),
                    rect,
                );
                assert_eq!(mapped, WindowId(u64::from(pid)));
            }
            fresh
                .handle_command(
                    WindowCommand::SetMaximized {
                        window_id: third,
                        maximized: true,
                    },
                    trusted(0),
                )
                .unwrap();
            fresh
        };
        let direct = fresh
            .handle_command(
                WindowCommand::ArrangeWindows {
                    preset: crate::ArrangementPreset::Cushion,
                },
                trusted(0),
            )
            .unwrap();
        assert_eq!(b, direct, "B is computed from the baseline, not from A");

        // Restore: every window exactly at its pre-arrangement geometry and maximize state.
        service
            .handle_command(WindowCommand::RestoreArrangement, trusted(0))
            .unwrap();
        for (id, geometry, maximized) in before {
            let window = service.window(id).unwrap();
            assert_eq!(window.geometry, geometry, "{id:?} geometry restored");
            assert_eq!(
                window.maximized, maximized,
                "{id:?} maximize state restored"
            );
        }
        assert!(!service.arrangement_active(), "the baseline is spent");
    }

    /// `dynamic-window-set`: a window closed between presses is ignored, a newly opened one
    /// joins only on the next explicit request, and Restore skips the departed without losing
    /// any survivor.
    #[test]
    fn arrangement_ignores_closed_windows_and_joins_new_ones_on_the_next_press() {
        let (mut service, first) = service_with_one_window();
        let second = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "two".into(),
            Rect::new(300, 200, 500, 400),
        );
        service
            .handle_command(
                WindowCommand::ArrangeWindows {
                    preset: crate::ArrangementPreset::Quarters,
                },
                trusted(0),
            )
            .unwrap();
        let arranged_second = service.window(second).unwrap().geometry;

        // A new window maps: nothing retiles until the next explicit request.
        let third = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "three".into(),
            Rect::new(700, 300, 300, 300),
        );
        assert_eq!(
            service.window(second).unwrap().geometry,
            arranged_second,
            "no background retiling on map"
        );
        let effect = service
            .handle_command(
                WindowCommand::ArrangeWindows {
                    preset: crate::ArrangementPreset::Quarters,
                },
                trusted(0),
            )
            .unwrap();
        let CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(
            moves.iter().any(|(id, _)| *id == third),
            "the new window joins on the explicit press"
        );

        // One closes; restore skips it and returns the survivors exactly.
        let first_before = Rect::new(100, 100, 400, 300);
        service.on_toplevel_destroyed(second);
        let effect = service
            .handle_command(WindowCommand::RestoreArrangement, trusted(0))
            .unwrap();
        let CommandEffect::Arranged { moves } = effect else {
            panic!("expected Arranged");
        };
        assert!(moves.iter().all(|(id, _)| *id != second));
        assert_eq!(service.window(first).unwrap().geometry, first_before);
    }

    #[test]
    fn reveal_home_toggles_shows_and_hides() {
        let (mut service, id) = service_with_one_window();
        let toggle = WindowCommand::RevealHome {
            reveal: RevealHome::Toggle,
        };
        assert_eq!(
            service.handle_command(toggle.clone(), trusted(0)),
            Ok(CommandEffect::HomeRevealed { revealed: true })
        );
        assert!(service.home_revealed());
        assert!(!service.is_visible(id));
        assert_eq!(
            service.handle_command(toggle, trusted(0)),
            Ok(CommandEffect::HomeRevealed { revealed: false })
        );
        assert!(service.is_visible(id));

        assert_eq!(
            service.handle_command(
                WindowCommand::RevealHome {
                    reveal: RevealHome::Show
                },
                trusted(0)
            ),
            Ok(CommandEffect::HomeRevealed { revealed: true })
        );
        assert_eq!(
            service.handle_command(
                WindowCommand::RevealHome {
                    reveal: RevealHome::Hide
                },
                trusted(0)
            ),
            Ok(CommandEffect::HomeRevealed { revealed: false })
        );
    }

    #[test]
    fn a_reveal_changes_no_window_and_ends_on_activate() {
        let (mut service, id) = service_with_one_window();
        // A maximized and a shelved window alongside the floating one, so the full-struct
        // equality below covers geometry, activation, maximize and shelf state at once —
        // momentary and latched reveals alike must be a pure view.
        let maximized = service.on_toplevel_mapped(
            2,
            ClientKind::Application,
            WindowClaims::default(),
            "max".into(),
            Rect::new(200, 200, 400, 300),
        );
        service
            .handle_command(
                WindowCommand::SetMaximized {
                    window_id: maximized,
                    maximized: true,
                },
                trusted(0),
            )
            .unwrap();
        let shelved = service.on_toplevel_mapped(
            3,
            ClientKind::Application,
            WindowClaims::default(),
            "shelf".into(),
            Rect::new(300, 300, 400, 300),
        );
        service
            .handle_command(WindowCommand::Shelve { window_id: shelved }, trusted(0))
            .unwrap();
        let before: Vec<_> = service.windows().cloned().collect();
        for reveal in [
            RevealHome::Show,
            RevealHome::Hide,
            RevealHome::Toggle,
            RevealHome::Show,
        ] {
            service
                .handle_command(WindowCommand::RevealHome { reveal }, trusted(0))
                .unwrap();
            let after: Vec<_> = service.windows().cloned().collect();
            assert_eq!(
                after, before,
                "a reveal is a view, not a state ({reveal:?})"
            );
        }

        let result = service.handle_command(
            WindowCommand::Activate {
                window_id: id,
                grant: grant(1_000),
            },
            trusted(0),
        );
        assert_eq!(result, Ok(CommandEffect::Activated { window_id: id }));
        assert!(
            !service.home_revealed(),
            "activating a window ends the reveal"
        );
        assert!(service.is_visible(id));
    }

    #[test]
    fn maximize_fills_the_usable_area_and_unmaximize_restores() {
        let (mut service, id) = service_with_one_window();
        let floating = service.window(id).unwrap().geometry;
        let result = service.handle_command(
            WindowCommand::SetMaximized {
                window_id: id,
                maximized: true,
            },
            trusted(0),
        );
        let Ok(CommandEffect::Maximized {
            geometry: Some(geometry),
            ..
        }) = result
        else {
            panic!("expected Maximized with a geometry: {result:?}");
        };
        assert_eq!(geometry, service.outputs()[0].usable());
        assert!(service.window(id).unwrap().maximized);

        // Already maximized: nothing to configure.
        let again = service.handle_command(
            WindowCommand::SetMaximized {
                window_id: id,
                maximized: true,
            },
            trusted(0),
        );
        assert_eq!(
            again,
            Ok(CommandEffect::Maximized {
                window_id: id,
                geometry: None
            })
        );

        let result = service.handle_command(
            WindowCommand::SetMaximized {
                window_id: id,
                maximized: false,
            },
            trusted(0),
        );
        let Ok(CommandEffect::Maximized {
            geometry: Some(geometry),
            ..
        }) = result
        else {
            panic!("expected Maximized with a geometry: {result:?}");
        };
        assert_eq!(geometry, floating);
        assert!(!service.window(id).unwrap().maximized);
    }
}
