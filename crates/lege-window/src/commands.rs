//! Commands only the trusted shell connection may submit, and the activation grants that gate
//! stealing focus (planning doc 02 §6).
//!
//! Identity (`CachedIdentity`) says *who owns* a window; this module says *whether a request to
//! activate it is allowed*. Measuring "recent input" needs real input-event timestamps, which
//! live in the fork / `lege-comp-integration`, not here — [`ActivationGrant`] only records their
//! verdict, so it stays testable without a real input stack.

use crate::{
    IdentityCache, Rect, WindowId, WindowService,
    geometry::{move_into_view, scatter_layout},
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
            WindowCommand::Activate { window_id, grant } => {
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
        let before = service.window(id).unwrap().clone();
        service
            .handle_command(
                WindowCommand::RevealHome {
                    reveal: RevealHome::Show,
                },
                trusted(0),
            )
            .unwrap();
        assert_eq!(
            service.window(id).unwrap(),
            &before,
            "a reveal is a view, not a state"
        );

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
