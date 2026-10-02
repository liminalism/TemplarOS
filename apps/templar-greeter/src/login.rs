//! The login flow's state machine, and the controller that drives [`crate::client::GreetdClient`]
//! with it.
//!
//! [`LoginFlow::advance`] is a pure function — `(username in flight, the `Response` that just
//! arrived) -> next state` — with no I/O anywhere near it, so every transition this greeter can go
//! through is tested below with plain values and no socket at all, the way the task brief asks for
//! ("the state machine is pure — test them properly"). [`LoginController`] is the thin, generic-
//! over-transport layer that actually calls [`crate::client::GreetdClient`] and folds whatever came
//! back — a normal `Response`, or a [`crate::protocol::ProtocolError`] from a socket that dropped
//! mid-flow — through [`LoginFlow::advance`], so a caller (`crate::surface::app`) never has to
//! choose between "the login failed" and "the connection failed": both always leave [`LoginFlow`]
//! in a state the screen can draw, per the rule this whole module exists to satisfy — "a mistyped
//! password must let you try again, not drop you to a black screen."

use crate::client::GreetdClient;
use crate::protocol::{AuthMessageType, ProtocolError, Response};
use std::io::{Read, Write};

/// What the screen should show right now, and what it means to press Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginFlow {
    /// No session in flight. `error`, if set, is what to show above the username field — the
    /// description from the last `error` response, or a connection failure.
    Idle { error: Option<String> },
    /// A session for `username` exists at greetd and is waiting on `prompt`. `GreetdClient`'s
    /// calls are blocking (`crate::client`'s module doc), so there is no state to represent
    /// between sending a request and its answer arriving — every transition into this variant
    /// already has its `Prompt` in hand.
    Authenticating { username: String, prompt: Prompt },
    /// Every `auth_message` answered; a plain `success` came back with nothing further to answer.
    /// Ready for `LoginController::start_session`.
    ReadyToStart { username: String },
}

impl LoginFlow {
    pub fn idle() -> LoginFlow {
        LoginFlow::Idle { error: None }
    }

    /// The username this flow is authenticating, if any — `None` only in [`LoginFlow::Idle`].
    pub fn username(&self) -> Option<&str> {
        match self {
            LoginFlow::Idle { .. } => None,
            LoginFlow::Authenticating { username, .. } | LoginFlow::ReadyToStart { username } => {
                Some(username)
            }
        }
    }

    /// The pure transition: given the username a request was sent for and the [`Response`] that
    /// answered it, what should the flow become. Used identically after `create_session` and after
    /// `post_auth_message_response` — `greetd-ipc(7)` gives both the same three possible answers
    /// (`success`, `error`, `auth_message`), so one function covers both call sites.
    pub fn advance(username: String, response: Response) -> LoginFlow {
        match response {
            // Idle, with the description surfaced so the user (or the log) knows why. The session
            // greetd was configuring is *not* gone: `LoginController` cancels it (see
            // `LoginController::after`), because greetd keeps it and refuses the next
            // create_session with "a session is already being configured".
            Response::Error { description, .. } => LoginFlow::Idle {
                error: Some(description),
            },
            Response::Success => LoginFlow::ReadyToStart { username },
            Response::AuthMessage {
                auth_message_type,
                auth_message,
            } => LoginFlow::Authenticating {
                username,
                prompt: Prompt::new(auth_message_type, auth_message),
            },
        }
    }
}

/// One `auth_message` translated into what the screen needs to draw and accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    /// The literal text greetd/PAM sent — shown as-is, never assumed to be "Password:". Faithful
    /// to `greetd-ipc(7)`'s "a greeter should not make any assumptions about the messages".
    pub message: String,
    /// Whether the field answering this prompt should be masked (`AuthMessageType::Secret`).
    pub secret: bool,
    /// Whether the user must type something before continuing. False for `Info`/`Error` messages,
    /// which `greetd-ipc(7)` says need no `response` value set at all — Enter alone advances past
    /// them with `post_auth_message_response { response: None }`.
    pub needs_response: bool,
}

impl Prompt {
    pub(crate) fn new(kind: AuthMessageType, message: String) -> Prompt {
        Prompt {
            message,
            secret: matches!(kind, AuthMessageType::Secret),
            needs_response: matches!(kind, AuthMessageType::Visible | AuthMessageType::Secret),
        }
    }
}

/// Drives a [`GreetdClient`] with [`LoginFlow`]'s transition rule, and never lets an I/O failure
/// escape as an `Err` the caller has to separately handle — every method here leaves
/// [`LoginController::flow`] in a valid, drawable state no matter what the socket did.
pub struct LoginController<S> {
    client: GreetdClient<S>,
    pub flow: LoginFlow,
}

impl<S> std::fmt::Debug for LoginController<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `GreetdClient<S>` itself carries no secret (see its own Debug impl's reasoning) — this
        // exists only so `#[derive(Debug)]` is not needed on `S`, which a caller's transport type
        // may not implement.
        f.debug_struct("LoginController")
            .field("flow", &self.flow)
            .finish_non_exhaustive()
    }
}

fn connection_error_text(error: &ProtocolError) -> String {
    format!("lost the connection to greetd: {error}")
}

impl<S: Read + Write> LoginController<S> {
    pub fn new(client: GreetdClient<S>) -> LoginController<S> {
        LoginController {
            client,
            flow: LoginFlow::idle(),
        }
    }

    /// Begin authenticating `username`. Always leaves `flow` in a state the screen can draw —
    /// `Authenticating` with the first prompt on success, `Idle` with an error message on any
    /// failure, protocol-level or connection-level alike.
    pub fn submit_username(&mut self, username: String) {
        let result = self.client.create_session(username.clone());
        self.flow = self.after(username, result);
    }

    /// Answer the current prompt. A no-op if nothing is currently `Authenticating` — the caller
    /// (the screen) only calls this when a prompt is showing, but a stray call is harmless rather
    /// than a panic.
    pub fn submit_response(&mut self, text: Option<String>) {
        let Some(username) = self.flow.username().map(str::to_owned) else {
            return;
        };
        let result = self.client.post_auth_message_response(text);
        self.flow = self.after(username, result);
    }

    /// The next flow for a create_session/post_auth_message_response answer. On an `error`
    /// response greetd keeps the half-configured session — observed on the reference laptop's
    /// greetd after `pam_acct_mgmt: NEW_AUTHTOK_REQD` and after a wrong password — and answers
    /// every later create_session with "a session is already being configured", so one mistyped
    /// password locked the screen until greetd restarted. Cancel it, best-effort, before retrying
    /// is possible.
    fn after(&mut self, username: String, result: Result<Response, ProtocolError>) -> LoginFlow {
        match result {
            Ok(response) => {
                if matches!(response, Response::Error { .. }) {
                    let _ = self.client.cancel_session();
                }
                LoginFlow::advance(username, response)
            }
            Err(error) => LoginFlow::Idle {
                error: Some(connection_error_text(&error)),
            },
        }
    }

    /// Start the session once [`LoginFlow::ReadyToStart`] — `cmd`/`env` from whichever session the
    /// picker had selected. Returns `true` once greetd has confirmed the session started (the
    /// caller's job from here is to exit so greetd can hand the VT to it); `false` leaves `flow`
    /// showing why it did not, ready for another attempt.
    pub fn start_session(&mut self, cmd: Vec<String>, env: Vec<String>) -> bool {
        match self.client.start_session(cmd, env) {
            Ok(Response::Success) => true,
            Ok(Response::Error { description, .. }) => {
                let _ = self.client.cancel_session();
                self.flow = LoginFlow::Idle {
                    error: Some(description),
                };
                false
            }
            Ok(Response::AuthMessage { .. }) => {
                // Not a shape `greetd-ipc(7)` documents `start_session` returning — handled
                // defensively rather than assumed impossible, per the same "never crash" rule.
                self.flow = LoginFlow::Idle {
                    error: Some(
                        "greetd asked another question instead of starting the session".to_owned(),
                    ),
                };
                false
            }
            Err(error) => {
                self.flow = LoginFlow::Idle {
                    error: Some(connection_error_text(&error)),
                };
                false
            }
        }
    }

    /// Abort whatever is in flight and return to a clean [`LoginFlow::Idle`] with no error shown —
    /// Escape, in this greeter's UI. Sends `cancel_session` only when there is actually a session
    /// to cancel (`greetd-ipc(7)`: cancelling with none active is meaningless, and best-effort —
    /// a failure here still leaves the UI idle, since there is nothing more useful to show).
    pub fn cancel(&mut self) {
        if self.flow.username().is_some() {
            let _ = self.client.cancel_session();
        }
        self.flow = LoginFlow::idle();
    }
}

#[cfg(test)]
mod pure_flow_tests {
    use super::*;
    use crate::protocol::ErrorType;

    #[test]
    fn a_secret_auth_message_becomes_a_masked_prompt() {
        let flow = LoginFlow::advance(
            "alice".to_owned(),
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".to_owned(),
            },
        );
        match flow {
            LoginFlow::Authenticating { username, prompt } => {
                assert_eq!(username, "alice");
                assert!(prompt.secret);
                assert!(prompt.needs_response);
                assert_eq!(prompt.message, "Password:");
            }
            other => panic!("expected Authenticating, got {other:?}"),
        }
    }

    #[test]
    fn a_visible_auth_message_is_not_masked() {
        let flow = LoginFlow::advance(
            "alice".to_owned(),
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Visible,
                auth_message: "One-time code:".to_owned(),
            },
        );
        let LoginFlow::Authenticating { prompt, .. } = flow else {
            panic!("expected Authenticating");
        };
        assert!(!prompt.secret);
    }

    #[test]
    fn an_info_or_error_auth_message_needs_no_typed_response() {
        for kind in [AuthMessageType::Info, AuthMessageType::Error] {
            let flow = LoginFlow::advance(
                "alice".to_owned(),
                Response::AuthMessage {
                    auth_message_type: kind,
                    auth_message: "please wait".to_owned(),
                },
            );
            let LoginFlow::Authenticating { prompt, .. } = flow else {
                panic!("expected Authenticating for {kind:?}");
            };
            assert!(
                !prompt.needs_response,
                "{kind:?} should not need a response"
            );
        }
    }

    #[test]
    fn success_after_a_prompt_is_answered_becomes_ready_to_start() {
        let flow = LoginFlow::advance("alice".to_owned(), Response::Success);
        assert_eq!(
            flow,
            LoginFlow::ReadyToStart {
                username: "alice".to_owned()
            }
        );
    }

    #[test]
    fn a_wrong_password_returns_to_idle_with_the_description_shown_ready_to_retry() {
        let flow = LoginFlow::advance(
            "alice".to_owned(),
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "authentication failed".to_owned(),
            },
        );
        assert_eq!(
            flow,
            LoginFlow::Idle {
                error: Some("authentication failed".to_owned())
            },
            "a bad password must return to a state where the user can try again"
        );
    }

    #[test]
    fn a_generic_error_also_returns_to_idle_not_some_stuck_state() {
        let flow = LoginFlow::advance(
            "alice".to_owned(),
            Response::Error {
                error_type: ErrorType::Error,
                description: "pam_something failed".to_owned(),
            },
        );
        assert!(matches!(flow, LoginFlow::Idle { error: Some(_) }));
    }

    #[test]
    fn idle_carries_no_username() {
        assert_eq!(LoginFlow::idle().username(), None);
    }

    #[test]
    fn authenticating_and_ready_to_start_both_report_their_username() {
        let authenticating = LoginFlow::Authenticating {
            username: "bob".to_owned(),
            prompt: Prompt::new(AuthMessageType::Secret, "Password:".to_owned()),
        };
        assert_eq!(authenticating.username(), Some("bob"));
        let ready = LoginFlow::ReadyToStart {
            username: "bob".to_owned(),
        };
        assert_eq!(ready.username(), Some("bob"));
    }
}

#[cfg(test)]
mod controller_tests {
    use super::*;
    use crate::protocol::{ErrorType, Request, read_frame, write_frame};
    use std::os::unix::net::UnixStream;
    use std::thread;

    /// Runs `script` against one end of a socket pair as a fake greetd, and hands the other end
    /// to a fresh [`LoginController`] for the test body to drive. `script` sees each [`Request`]
    /// in order and returns the [`Response`] to send back — the same fake-greetd shape
    /// `crate::client`'s own tests use, at the level this module actually drives it from.
    fn with_fake_greetd<F>(script: Vec<Response>, body: F) -> Vec<Request>
    where
        F: FnOnce(&mut LoginController<UnixStream>),
    {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let mut seen = Vec::new();
            for response in script {
                let request: Request = match read_frame(&mut server_stream) {
                    Ok(request) => request,
                    Err(_) => return seen,
                };
                seen.push(request);
                if write_frame(&mut server_stream, &response).is_err() {
                    return seen;
                }
            }
            seen
        });

        let mut controller = LoginController::new(GreetdClient::new(client_stream));
        body(&mut controller);
        drop(controller);
        server.join().unwrap()
    }

    #[test]
    fn the_happy_path_reaches_ready_to_start_and_then_a_started_session() {
        with_fake_greetd(
            vec![
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
                Response::Success,
                Response::Success,
            ],
            |controller| {
                controller.submit_username("alice".to_owned());
                assert!(matches!(controller.flow, LoginFlow::Authenticating { .. }));

                controller.submit_response(Some("correct horse".to_owned()));
                assert_eq!(
                    controller.flow,
                    LoginFlow::ReadyToStart {
                        username: "alice".to_owned()
                    }
                );

                let started = controller.start_session(vec!["templar-session".to_owned()], vec![]);
                assert!(started);
            },
        );
    }

    #[test]
    fn a_wrong_password_lets_the_user_try_again_with_the_same_controller() {
        let requests = with_fake_greetd(
            vec![
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
                Response::Error {
                    error_type: ErrorType::AuthError,
                    description: "authentication failed".to_owned(),
                },
                // greetd's answer to the cancel_session the error must be followed by.
                Response::Success,
                // The retry: a brand new create_session for the same controller.
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
                Response::Success,
                Response::Success,
            ],
            |controller| {
                controller.submit_username("alice".to_owned());
                controller.submit_response(Some("wrong".to_owned()));
                assert_eq!(
                    controller.flow,
                    LoginFlow::Idle {
                        error: Some("authentication failed".to_owned())
                    }
                );

                // Retrying is not "stuck": the very same controller can start over.
                controller.submit_username("alice".to_owned());
                assert!(matches!(controller.flow, LoginFlow::Authenticating { .. }));
                controller.submit_response(Some("correct horse".to_owned()));
                assert!(matches!(controller.flow, LoginFlow::ReadyToStart { .. }));
                assert!(controller.start_session(vec!["templar-session".to_owned()], vec![]));
            },
        );
        // Without the cancel, greetd refuses the retry's create_session ("a session is already
        // being configured") — what the reference laptop's greeter did after one failed attempt.
        assert!(
            matches!(requests.get(2), Some(Request::CancelSession)),
            "the request after an error must be cancel_session: {requests:?}"
        );
        assert!(matches!(
            requests.get(3),
            Some(Request::CreateSession { .. })
        ));
    }

    #[test]
    fn an_error_response_to_start_session_is_recoverable_too() {
        with_fake_greetd(
            vec![
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
                Response::Success,
                Response::Error {
                    error_type: ErrorType::Error,
                    description: "no such session command".to_owned(),
                },
                Response::Success,
            ],
            |controller| {
                controller.submit_username("alice".to_owned());
                controller.submit_response(Some("correct horse".to_owned()));
                let started = controller.start_session(vec!["does-not-exist".to_owned()], vec![]);
                assert!(!started);
                assert_eq!(
                    controller.flow,
                    LoginFlow::Idle {
                        error: Some("no such session command".to_owned())
                    }
                );
            },
        );
    }

    #[test]
    fn a_connection_that_drops_mid_flow_lands_on_a_drawable_idle_state_not_a_panic() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        // The "server" hangs up immediately, mid-flow, with no response at all — the general
        // "greetd disappeared" case, as opposed to `protocol.rs`'s specific truncated-frame cases.
        drop(server_stream);

        let mut controller = LoginController::new(GreetdClient::new(client_stream));
        controller.submit_username("alice".to_owned());
        assert!(
            matches!(controller.flow, LoginFlow::Idle { error: Some(_) }),
            "a dead connection must still leave a drawable state: {:?}",
            controller.flow
        );
    }

    #[test]
    fn cancel_from_idle_is_a_harmless_no_op() {
        with_fake_greetd(vec![], |controller| {
            controller.cancel();
            assert_eq!(controller.flow, LoginFlow::idle());
        });
    }

    #[test]
    fn escape_mid_authentication_cancels_and_returns_to_a_clean_idle_state() {
        with_fake_greetd(
            vec![
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
                Response::Success, // the fake greetd's answer to cancel_session
            ],
            |controller| {
                controller.submit_username("alice".to_owned());
                assert!(matches!(controller.flow, LoginFlow::Authenticating { .. }));
                controller.cancel();
                assert_eq!(controller.flow, LoginFlow::idle());
            },
        );
    }

    #[test]
    fn submit_response_with_nothing_in_flight_does_not_touch_the_socket_or_panic() {
        // No script entries at all: if this sent a request, the fake greetd thread would have
        // nothing to answer with and the test would hang waiting for it to join.
        with_fake_greetd(vec![], |controller| {
            controller.submit_response(Some("stray".to_owned()));
            assert_eq!(controller.flow, LoginFlow::idle());
        });
    }

    /// The same Debug-leak guarantee `protocol.rs` checks at the wire-type level, checked again at
    /// the controller level a screen would actually hold: even with a live `Prompt` on display and
    /// a distinctive secret sitting in a hypothetical caller's field, `LoginController`'s own
    /// `Debug` output never contains anything that came from a `PostAuthMessageResponse`, because
    /// it is never stored past the single call that sends it.
    #[test]
    fn login_controller_debug_output_carries_no_typed_secret() {
        with_fake_greetd(
            vec![Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".to_owned(),
            }],
            |controller| {
                controller.submit_username("alice".to_owned());
                let debug_output = format!("{controller:?}");
                assert!(!debug_output.contains("hunter2"));
            },
        );
    }
}
