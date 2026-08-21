//! A thin, blocking client over [`crate::protocol`]: one call per `greetd-ipc(7)` request type,
//! each writing a [`Request`] frame and reading back the one [`Response`] frame that answers it.
//!
//! Deliberately dumb: `GreetdClient` does not know what a `success` or an `auth_message` *means*
//! for the login flow — that policy lives in [`crate::login`], which is pure and tested with no
//! socket at all. This module's own job is exactly the four lines `greetd-ipc(7)`'s REQUESTS table
//! lists, generic over any `Read + Write` transport so it can be driven by a real
//! `UnixStream` in production and an in-process pipe (or a fake greetd on the other end of a
//! [`std::os::unix::net::UnixStream::pair`]) in tests — see `crate::login`'s test module for the
//! fake-greetd harness this genericity exists for.

use std::env;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::protocol::{ProtocolError, Request, Response, read_frame, write_frame};

/// The environment variable greetd sets, naming the socket to connect to
/// (`greetd-ipc(7)`: "The path to the socket is stored in the environment variable GREETD_SOCK").
pub const GREETD_SOCK_VAR: &str = "GREETD_SOCK";

/// [`GREETD_SOCK_VAR`] was not set — this process was not started by greetd, or greetd's contract
/// with the greeter it starts was not honoured.
#[derive(Debug)]
pub struct MissingSocketEnv;

impl std::fmt::Display for MissingSocketEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{GREETD_SOCK_VAR} is not set — this binary must be run as greetd's default_session.command"
        )
    }
}

impl std::error::Error for MissingSocketEnv {}

/// A `greetd-ipc(7)` client over any duplex byte stream.
#[derive(Debug)]
pub struct GreetdClient<S> {
    stream: S,
}

impl GreetdClient<UnixStream> {
    /// Read [`GREETD_SOCK_VAR`] and connect to the socket it names.
    pub fn connect_from_env() -> anyhow::Result<GreetdClient<UnixStream>> {
        let path = env::var_os(GREETD_SOCK_VAR).ok_or(MissingSocketEnv)?;
        let stream = UnixStream::connect(PathBuf::from(path))?;
        Ok(GreetdClient::new(stream))
    }
}

impl<S: Read + Write> GreetdClient<S> {
    pub fn new(stream: S) -> GreetdClient<S> {
        GreetdClient { stream }
    }

    fn call(&mut self, request: &Request) -> Result<Response, ProtocolError> {
        write_frame(&mut self.stream, request)?;
        read_frame(&mut self.stream)
    }

    pub fn create_session(&mut self, username: String) -> Result<Response, ProtocolError> {
        self.call(&Request::CreateSession { username })
    }

    pub fn post_auth_message_response(
        &mut self,
        response: Option<String>,
    ) -> Result<Response, ProtocolError> {
        self.call(&Request::PostAuthMessageResponse { response })
    }

    pub fn start_session(
        &mut self,
        cmd: Vec<String>,
        env: Vec<String>,
    ) -> Result<Response, ProtocolError> {
        self.call(&Request::StartSession { cmd, env })
    }

    pub fn cancel_session(&mut self) -> Result<Response, ProtocolError> {
        self.call(&Request::CancelSession)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AuthMessageType, ErrorType};
    use std::thread;

    /// A minimal fake greetd: reads one [`Request`], hands it to `respond` for a canned
    /// [`Response`], writes it back, and repeats until the stream closes. Shared by every test
    /// below and by `crate::login`'s integration tests, so the framing exercised here is the exact
    /// framing `GreetdClient` uses, never a hand-simplified stand-in.
    fn fake_greetd(
        mut server: UnixStream,
        mut respond: impl FnMut(Request) -> Response + Send + 'static,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            loop {
                let request: Request = match read_frame(&mut server) {
                    Ok(request) => request,
                    Err(_) => return, // client hung up — the other end of a closed test pair
                };
                let response = respond(request);
                if write_frame(&mut server, &response).is_err() {
                    return;
                }
            }
        })
    }

    #[test]
    fn create_session_round_trips_over_a_real_socket_pair() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = fake_greetd(server_stream, |request| match request {
            Request::CreateSession { username } => Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: format!("Password for {username}:"),
            },
            other => panic!("unexpected request in this test: {other:?}"),
        });

        let mut client = GreetdClient::new(client_stream);
        let response = client.create_session("alice".to_owned()).unwrap();
        assert_eq!(
            response,
            Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password for alice:".to_owned(),
            }
        );

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn a_wrong_password_comes_back_as_an_auth_error_the_client_can_read() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = fake_greetd(server_stream, |request| match request {
            Request::CreateSession { .. } => Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".to_owned(),
            },
            Request::PostAuthMessageResponse { .. } => Response::Error {
                error_type: ErrorType::AuthError,
                description: "authentication failed".to_owned(),
            },
            other => panic!("unexpected request in this test: {other:?}"),
        });

        let mut client = GreetdClient::new(client_stream);
        client.create_session("alice".to_owned()).unwrap();
        let response = client
            .post_auth_message_response(Some("wrong-password".to_owned()))
            .unwrap();
        assert_eq!(
            response,
            Response::Error {
                error_type: ErrorType::AuthError,
                description: "authentication failed".to_owned(),
            }
        );

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn the_full_happy_path_ends_in_a_started_session() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = fake_greetd(server_stream, |request| match request {
            Request::CreateSession { .. } => Response::AuthMessage {
                auth_message_type: AuthMessageType::Secret,
                auth_message: "Password:".to_owned(),
            },
            Request::PostAuthMessageResponse { .. } | Request::StartSession { .. } => {
                Response::Success
            }
            other @ Request::CancelSession => {
                panic!("unexpected request in this test: {other:?}")
            }
        });

        let mut client = GreetdClient::new(client_stream);
        assert!(matches!(
            client.create_session("alice".to_owned()).unwrap(),
            Response::AuthMessage { .. }
        ));
        assert_eq!(
            client
                .post_auth_message_response(Some("correct-password".to_owned()))
                .unwrap(),
            Response::Success
        );
        assert_eq!(
            client
                .start_session(vec!["lege-session".to_owned()], vec![])
                .unwrap(),
            Response::Success
        );

        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn a_response_frame_the_peer_never_finishes_sending_is_reported_as_connection_closed_not_a_hang_or_panic()
     {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            // Consume the client's request (so its write succeeds) before promising a response
            // body and vanishing without ever sending it — the truncated/garbage-frame case the
            // IPC layer must survive.
            let _: Request = read_frame(&mut server_stream).unwrap();
            server_stream.write_all(&100u32.to_ne_bytes()).unwrap();
            // `server_stream` drops here, closing the connection mid-frame.
        });

        let mut client = GreetdClient::new(client_stream);
        let result = client.create_session("alice".to_owned());
        assert!(matches!(result, Err(ProtocolError::ConnectionClosed)));
    }

    #[test]
    fn a_peer_that_is_already_gone_before_anything_is_sent_is_reported_not_panicked_on() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        // No fake greetd at all on the other end — closed immediately, simulating a dead or
        // never-started greetd socket. The client's own outgoing write is what fails here (a
        // broken pipe), a different failure point than the mid-frame read truncation above, and
        // one `LoginController` (crate::login) must handle exactly as gracefully.
        drop(server_stream);

        let mut client = GreetdClient::new(client_stream);
        let result = client.create_session("alice".to_owned());
        assert!(
            result.is_err(),
            "a dead peer must be an error, not a panic or a hang"
        );
    }

    #[test]
    fn garbage_bytes_instead_of_a_frame_are_reported_not_panicked_on() {
        let (client_stream, mut server_stream) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            // Consume the client's outgoing request so the write side doesn't block, then answer
            // with four bytes of noise as if it were a length prefix followed by an equally
            // nonsensical body.
            let _: Request = read_frame(&mut server_stream).unwrap();
            server_stream.write_all(&[0xde, 0xad, 0xbe, 0xef]).unwrap();
            server_stream.write_all(&[0x00, 0x01, 0x02, 0x03]).unwrap();
        });

        let mut client = GreetdClient::new(client_stream);
        let result = client.create_session("alice".to_owned());
        assert!(result.is_err(), "garbage must be an error, not a panic");
    }

    #[test]
    fn connecting_without_greetd_sock_set_fails_with_a_clear_error_not_a_panic() {
        // SAFETY: this test does not spawn other threads that read the process environment, and
        // no other test in this crate sets/removes `GREETD_SOCK` — `cargo test`'s harness runs
        // each test's own body on a fresh thread, so a stray parallel reader race is the only risk
        // this could interact with, and none exists.
        #[allow(unsafe_code)]
        unsafe {
            env::remove_var(GREETD_SOCK_VAR);
        }
        let result = GreetdClient::connect_from_env();
        assert!(result.is_err());
    }
}
