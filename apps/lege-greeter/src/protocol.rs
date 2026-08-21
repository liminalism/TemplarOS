//! greetd's JSON IPC protocol: a length-prefixed JSON message on the UNIX socket named by the
//! `GREETD_SOCK` environment variable (ADR-0010, ADR-0012 — "It authenticates by speaking
//! greetd's JSON IPC over its socket, so greetd owns PAM and the greeter links no authentication
//! code of its own").
//!
//! Implemented directly from `greetd-ipc(7)` (installed on the development machine as part of the
//! `greetd` package; `man 7 greetd-ipc`), not by depending on the `greetd_ipc` crate: the ADR's
//! whole point is that this greeter carries no authentication machinery of its own, and pulling in
//! someone else's implementation of the one wire format that stands between a typed password and
//! the PAM stack is exactly the kind of code this crate should own and test itself, not vendor
//! sight-unseen. `greetd_ipc` is also GPL-3.0-only, which would entangle this MPL-2.0 crate's
//! licence (`deny.toml`, ADR-0008) for what is, in the end, four struct definitions and a length
//! prefix. The shapes here were cross-checked field-for-field against the `greetd_ipc` crate that
//! happens to be present in the development machine's cargo registry cache
//! (`greetd_ipc-0.10.3/src/lib.rs`) — read for reference only, never linked — and the framing
//! against `man 7 greetd-ipc`'s own hexdump example (see `write_frame_matches_the_manual_pages_hexdump_example`
//! below).
//!
//! # Wire format
//!
//! ```text
//! +--------------------+-----------------------+
//! | length: u32, native | JSON payload: UTF-8   |
//! +--------------------+-----------------------+
//! ```
//!
//! `greetd-ipc(7)`: "`<payload-length>` is a 32-bit integer in native byte order" — not
//! network-order, because both ends of this socket always run on the same machine.
//!
//! # What must never happen here
//!
//! [`Request::PostAuthMessageResponse`] carries the text the user just typed, which is a password
//! far more often than not. Its `Debug` impl is written by hand below specifically to never format
//! that text — see its doc comment and `post_auth_message_response_response_never_reaches_debug_output`
//! in the test module.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

/// A request from the greeter to greetd. Internally tagged `type`, snake_case
/// (`greetd-ipc(7)`'s REQUESTS table).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Initiates a login attempt for `username`.
    CreateSession { username: String },
    /// Answers the last `auth_message`. `response` is the text the user typed — often a password —
    /// or `None` for an informative message that needs no answer.
    PostAuthMessageResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<String>,
    },
    /// Starts the session `create_session`/`post_auth_message_response` just authenticated, once
    /// one of them has returned `success`.
    StartSession {
        cmd: Vec<String>,
        #[serde(default)]
        env: Vec<String>,
    },
    /// Aborts the session under configuration. Never required after an `error` response —
    /// `greetd-ipc(7)`: "the session is cancelled automatically on error" — but always safe to
    /// send anyway (Escape, in this greeter's UI).
    CancelSession,
}

/// A hand-written `Debug` — never derived — so that whatever the user typed into the field
/// answering an `auth_message` can never reach a log line, a panic message or a test failure's
/// assertion output. `username` is not a secret (it is typed in the clear, in an unmasked field)
/// and is shown normally; `response`'s *content* never is, only whether one was sent at all.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::CreateSession { username } => f
                .debug_struct("CreateSession")
                .field("username", username)
                .finish(),
            Request::PostAuthMessageResponse { response } => f
                .debug_struct("PostAuthMessageResponse")
                .field("response", &response.as_ref().map(|_| "<redacted>"))
                .finish(),
            Request::StartSession { cmd, env } => f
                .debug_struct("StartSession")
                .field("cmd", cmd)
                .field("env", env)
                .finish(),
            Request::CancelSession => f.write_str("CancelSession"),
        }
    }
}

/// `greetd-ipc(7)`'s error-response enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorType {
    /// Authentication failed — "not a fatal error, and is likely caused by incorrect
    /// credentials. Handle as appropriate" (`greetd-ipc(7)`): retry, do not exit.
    AuthError,
    /// Anything else; `description` says what.
    Error,
}

/// `greetd-ipc(7)`'s auth-message-type enum — what kind of answer, if any, an `auth_message`
/// wants. Both `Info` and `Error` here are greetd/PAM being informative, not the transport-level
/// [`ErrorType`] above; a greeter must not assume how many of any of these arrive, or in what
/// order (`greetd-ipc(7)`: "There are no limits on the number and type of messages that may be
/// required for authentication to succeed, and a greeter should not make any assumptions").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    /// The answer should be shown as typed.
    Visible,
    /// The answer should be masked — a password, almost always.
    Secret,
    /// No answer needed beyond acknowledging it.
    Info,
    /// Likewise no answer needed; PAM reporting a problem.
    Error,
}

/// A response from greetd. Internally tagged `type`, snake_case (`greetd-ipc(7)`'s RESPONSES
/// table). Safe to derive `Debug` for: every field here originates at greetd/PAM, not from
/// anything the user typed into this greeter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error {
        error_type: ErrorType,
        description: String,
    },
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

/// Everything that can go wrong reading or writing one frame. Safe to derive `Debug` for — an I/O
/// or parse failure never carries user-typed content.
#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    /// The peer closed the connection (cleanly or otherwise) mid-frame — greetd exited, or the
    /// socket was never valid to begin with.
    ConnectionClosed,
    /// A frame declared a length past [`MAX_FRAME_LEN`] — refused before the read even starts, so
    /// a corrupt or hostile length prefix cannot make this process allocate on the peer's say-so.
    FrameTooLarge(usize),
    /// The frame's bytes were not valid UTF-8 JSON, or did not match [`Request`]/[`Response`]'s
    /// shape.
    Malformed(serde_json::Error),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(error) => write!(f, "i/o error: {error}"),
            ProtocolError::ConnectionClosed => write!(f, "greetd closed the connection"),
            ProtocolError::FrameTooLarge(len) => {
                write!(
                    f,
                    "frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte limit"
                )
            }
            ProtocolError::Malformed(error) => write!(f, "malformed frame: {error}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> ProtocolError {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            ProtocolError::ConnectionClosed
        } else {
            ProtocolError::Io(error)
        }
    }
}

/// Refused before allocating: greetd's real payloads (a username, a short prompt, a session
/// command line) are at most a few hundred bytes. A garbage or hostile length prefix — four
/// arbitrary bytes read as a `u32` — must not be trusted to size a `Vec` before anything else has
/// been validated.
const MAX_FRAME_LEN: u32 = 1024 * 1024;

/// Serialize `value` as one length-prefixed JSON frame and write it whole.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), ProtocolError> {
    let body = serde_json::to_vec(value).map_err(ProtocolError::Malformed)?;
    // `body.len()` cannot realistically exceed `u32::MAX` for anything this protocol serializes
    // (see `MAX_FRAME_LEN`'s doc); the conversion still uses `try_from` rather than `as` so a
    // future caller who serializes something enormous gets a typed error, not a silently
    // truncated length prefix that corrupts the framing of every message after it.
    let len = u32::try_from(body.len()).map_err(|_| ProtocolError::FrameTooLarge(body.len()))?;
    writer.write_all(&len.to_ne_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read one length-prefixed JSON frame and deserialize it.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> Result<T, ProtocolError> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes)?;
    let len = u32::from_ne_bytes(len_bytes);
    if len > MAX_FRAME_LEN {
        return Err(ProtocolError::FrameTooLarge(len as usize));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(ProtocolError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `man 7 greetd-ipc`'s own worked example: `create_session` for user "me", framed as a
    /// 4-byte native-endian length followed by the JSON payload. The manual page's hexdump
    /// pretty-prints that JSON with spaces after `:`/`,` (a 44-byte, 0x2c body);
    /// `serde_json::to_vec`'s compact form is the same JSON with the insignificant whitespace
    /// removed (41 bytes) — equally valid on the wire, since JSON whitespace carries no meaning.
    /// What this test checks is the actual invariant the manual page is demonstrating: the length
    /// prefix names exactly how many bytes of JSON follow, and those bytes parse back to the same
    /// request the example describes.
    #[test]
    fn write_frame_matches_the_manual_pages_hexdump_example() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            &Request::CreateSession {
                username: "me".to_owned(),
            },
        )
        .unwrap();

        let declared_len = u32::from_ne_bytes(buffer[..4].try_into().unwrap());
        let body = &buffer[4..];
        assert_eq!(
            declared_len as usize,
            body.len(),
            "the length prefix names exactly the body's own length"
        );

        let parsed: serde_json::Value = serde_json::from_slice(body).unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(r#"{"type": "create_session", "username": "me"}"#).unwrap();
        assert_eq!(
            parsed, expected,
            "same JSON value as the manual page's example, modulo whitespace"
        );
    }

    #[test]
    fn a_frame_round_trips_through_a_plain_buffer() {
        let mut buffer = Vec::new();
        let request = Request::StartSession {
            cmd: vec!["lege-session".to_owned()],
            env: vec!["XDG_SESSION_TYPE=wayland".to_owned()],
        };
        write_frame(&mut buffer, &request).unwrap();

        let mut cursor = &buffer[..];
        let read_back: Request = read_frame(&mut cursor).unwrap();
        assert_eq!(read_back, request);
        assert!(
            cursor.is_empty(),
            "the whole frame was consumed, nothing left over"
        );
    }

    #[test]
    fn every_request_variant_round_trips_with_the_documented_json_shape() {
        let cases: Vec<(Request, &str)> = vec![
            (
                Request::CreateSession {
                    username: "alice".to_owned(),
                },
                r#"{"type":"create_session","username":"alice"}"#,
            ),
            (
                Request::PostAuthMessageResponse {
                    response: Some("hunter2".to_owned()),
                },
                r#"{"type":"post_auth_message_response","response":"hunter2"}"#,
            ),
            (
                // `response: None` is dropped entirely, matching an info/error auth message that
                // "does not need to be set" (`greetd-ipc(7)`) rather than serialized as `null`.
                Request::PostAuthMessageResponse { response: None },
                r#"{"type":"post_auth_message_response"}"#,
            ),
            (
                Request::StartSession {
                    cmd: vec!["lege-session".to_owned()],
                    env: vec![],
                },
                r#"{"type":"start_session","cmd":["lege-session"],"env":[]}"#,
            ),
            (Request::CancelSession, r#"{"type":"cancel_session"}"#),
        ];
        for (request, expected_json) in cases {
            assert_eq!(serde_json::to_string(&request).unwrap(), expected_json);
            let parsed: Request = serde_json::from_str(expected_json).unwrap();
            assert_eq!(parsed, request);
        }
    }

    #[test]
    fn every_response_variant_parses_from_the_documented_json_shape() {
        let cases: Vec<(&str, Response)> = vec![
            (r#"{"type":"success"}"#, Response::Success),
            (
                r#"{"type":"error","error_type":"auth_error","description":"bad password"}"#,
                Response::Error {
                    error_type: ErrorType::AuthError,
                    description: "bad password".to_owned(),
                },
            ),
            (
                r#"{"type":"error","error_type":"error","description":"pam trouble"}"#,
                Response::Error {
                    error_type: ErrorType::Error,
                    description: "pam trouble".to_owned(),
                },
            ),
            (
                r#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password:"}"#,
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Secret,
                    auth_message: "Password:".to_owned(),
                },
            ),
            (
                r#"{"type":"auth_message","auth_message_type":"visible","auth_message":"Username:"}"#,
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Visible,
                    auth_message: "Username:".to_owned(),
                },
            ),
            (
                r#"{"type":"auth_message","auth_message_type":"info","auth_message":"one moment"}"#,
                Response::AuthMessage {
                    auth_message_type: AuthMessageType::Info,
                    auth_message: "one moment".to_owned(),
                },
            ),
        ];
        for (json, expected) in cases {
            let parsed: Response = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn a_frame_declaring_more_than_the_limit_is_refused_before_it_allocates() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&(MAX_FRAME_LEN + 1).to_ne_bytes());
        // Deliberately no body at all — if `read_frame` tried to allocate and read it, this test
        // would hang or fail on a short read instead of returning `FrameTooLarge` immediately.
        let mut cursor = &buffer[..];
        let result: Result<Response, _> = read_frame(&mut cursor);
        assert!(matches!(result, Err(ProtocolError::FrameTooLarge(_))));
    }

    #[test]
    fn a_frame_truncated_mid_header_is_reported_as_connection_closed_not_a_panic() {
        let mut cursor: &[u8] = &[0x01, 0x00]; // two bytes of a four-byte length, then nothing
        let result: Result<Response, _> = read_frame(&mut cursor);
        assert!(matches!(result, Err(ProtocolError::ConnectionClosed)));
    }

    #[test]
    fn a_frame_truncated_mid_body_is_reported_as_connection_closed_not_a_panic() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&10u32.to_ne_bytes()); // promises 10 bytes
        buffer.extend_from_slice(b"{\"type\":\""); // delivers 9, then EOF
        let mut cursor = &buffer[..];
        let result: Result<Response, _> = read_frame(&mut cursor);
        assert!(matches!(result, Err(ProtocolError::ConnectionClosed)));
    }

    #[test]
    fn a_well_framed_but_non_json_body_is_malformed_not_a_panic() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&4u32.to_ne_bytes());
        buffer.extend_from_slice(b"\xff\xff\xff\xff"); // four bytes, not UTF-8, not JSON
        let mut cursor = &buffer[..];
        let result: Result<Response, _> = read_frame(&mut cursor);
        assert!(matches!(result, Err(ProtocolError::Malformed(_))));
    }

    #[test]
    fn a_well_formed_json_object_of_the_wrong_shape_is_malformed_not_a_panic() {
        let mut buffer = Vec::new();
        write_frame(
            &mut buffer,
            &serde_json::json!({"type": "not_a_real_response_type"}),
        )
        .unwrap();
        let mut cursor = &buffer[..];
        let result: Result<Response, _> = read_frame(&mut cursor);
        assert!(matches!(result, Err(ProtocolError::Malformed(_))));
    }

    /// The exact check the module doc promises: format every `Request` variant that can carry
    /// what a user typed, with a distinctive secret in it, and confirm the secret substring is
    /// nowhere in the `Debug` output — not the derived form (which this type deliberately does
    /// not use), and not a hand-written one that forgot a variant.
    #[test]
    fn post_auth_message_response_response_never_reaches_debug_output() {
        const SECRET: &str = "hunter2-do-not-log-me";
        let request = Request::PostAuthMessageResponse {
            response: Some(SECRET.to_owned()),
        };
        let debug_output = format!("{request:?}");
        assert!(
            !debug_output.contains(SECRET),
            "the secret leaked into Debug output: {debug_output}"
        );
        assert!(debug_output.contains("redacted"));

        // The `None` case (an info/error message answered with no text) still debug-prints
        // cleanly, so a log line can still distinguish "sent an empty answer" from "sent one".
        let empty = Request::PostAuthMessageResponse { response: None };
        assert_eq!(
            format!("{empty:?}"),
            r"PostAuthMessageResponse { response: None }"
        );
    }

    #[test]
    fn a_username_is_not_treated_as_secret() {
        // Usernames are typed in the clear into an unmasked field; only the auth *response* is
        // redacted. This is what makes `CreateSession`'s Debug output actually useful for
        // diagnosing "why didn't create_session start" without touching anything secret.
        let request = Request::CreateSession {
            username: "alice".to_owned(),
        };
        assert_eq!(
            format!("{request:?}"),
            r#"CreateSession { username: "alice" }"#
        );
    }
}
