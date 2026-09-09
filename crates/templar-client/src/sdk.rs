//! The application SDK: identity from the environment and the checkpoint protocol.
//!
//! [`serve_checkpoint`] is the other half of `org.templaros.Checkpoint1`
//! (`interfaces/dbus/org.templaros.Checkpoint1.xml`): inverted from every other `org.templaros.*`
//! interface, this one is *served by the application*, not by a daemon. `templar-sessiond` relays
//! the governor's request through [`templar_client::proxies::CheckpointProxy`](crate::proxies::CheckpointProxy)
//! to whichever bus name the instance claimed for itself
//! (`templar_types::dbus::checkpoint::instance_bus_name`); this module is the receiving end a
//! first-party application links to answer it.

use templar_types::{
    AppId, InstanceId,
    checkpoint::{CheckpointRequest, CheckpointResponse},
    dbus::{checkpoint as checkpoint_dbus, env},
};

/// The identity `appd` gave this process at launch, if it was launched through TemplarOS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchIdentity {
    pub app_id: AppId,
    pub instance_id: InstanceId,
}

impl LaunchIdentity {
    /// Read `TEMPLAROS_APP_ID` / `TEMPLAROS_INSTANCE_ID`. `None` when started outside the broker
    /// (the application still works; it is just tracked with lower confidence).
    pub fn from_env() -> Option<Self> {
        let app_id = std::env::var(env::APP_ID).ok()?.parse().ok()?;
        let instance_id = InstanceId::parse(&std::env::var(env::INSTANCE_ID).ok()?)?;
        Some(Self {
            app_id,
            instance_id,
        })
    }
}

/// What an application must implement to take part in pressure containment without losing work.
pub trait CheckpointHandler {
    /// Drop caches, release what can be recreated. Return bytes released (best effort).
    fn prepare_for_pressure(&mut self) -> u64;

    /// Persist restorable state; return an opaque token and whether unsaved work exists.
    fn checkpoint_now(&mut self) -> Result<(String, bool), String>;

    /// Restore from a token produced by an earlier `checkpoint_now`.
    fn restore(&mut self, token: &str) -> Result<(), String>;
}

/// Dispatch a request to a handler; used by the (future) D-Bus glue and by tests.
pub fn dispatch<H: CheckpointHandler>(
    handler: &mut H,
    request: &CheckpointRequest,
    stored_token: Option<&str>,
) -> templar_types::checkpoint::CheckpointResponse {
    use templar_types::checkpoint::CheckpointResponse as R;
    match request {
        CheckpointRequest::PrepareForPressure { instance } => R::Prepared {
            instance: *instance,
            released_bytes: handler.prepare_for_pressure(),
        },
        CheckpointRequest::CheckpointNow { instance, .. } => match handler.checkpoint_now() {
            Ok((token, has_unsaved_work)) => R::CheckpointComplete {
                instance: *instance,
                token,
                has_unsaved_work,
            },
            Err(reason) => R::Refused {
                instance: *instance,
                reason,
            },
        },
        CheckpointRequest::RestoreInstance { instance, .. } => {
            match stored_token.map(|t| handler.restore(t)) {
                Some(Ok(())) => R::Prepared {
                    instance: *instance,
                    released_bytes: 0,
                },
                Some(Err(reason)) => R::Refused {
                    instance: *instance,
                    reason,
                },
                None => R::Refused {
                    instance: *instance,
                    reason: "no checkpoint token".into(),
                },
            }
        }
    }
}

/// Why [`serve_checkpoint`] could not start.
#[derive(Debug, thiserror::Error)]
pub enum ServeCheckpointError {
    /// No `TEMPLAROS_APP_ID`/`TEMPLAROS_INSTANCE_ID` in the environment: this process was not started
    /// through the broker, so it has no instance identity and therefore no bus name to claim.
    /// This is the documented legacy case (`docs/protocols/checkpoint.md`: "legacy applications
    /// are governed with less confidence"), not a fault — callers should treat it as "do not
    /// serve the checkpoint protocol," not as an error to surface to the user.
    #[error("not launched through TemplarOS: no instance identity in the environment")]
    NotLaunchedThroughTemplarOs,

    /// Claiming the instance bus name, or exporting the object, failed.
    #[error("could not serve org.templaros.Checkpoint1: {0}")]
    Zbus(#[from] zbus::Error),
}

/// The `org.templaros.Checkpoint1` object a served instance exports at
/// [`checkpoint_dbus::PATH`]. Holds the caller's [`CheckpointHandler`] plus the one piece of
/// state the wire protocol itself does not carry: the token from this process's own most recent
/// [`CheckpointResponse::CheckpointComplete`], consulted when a later `restore-instance` request
/// arrives with no token of its own (the current [`CheckpointRequest::RestoreInstance`] variant
/// does not carry one — restoring a *different*, freshly launched process from an older
/// instance's token is therefore not yet answerable by this SDK; only "resume within the same
/// still-running process" is).
#[derive(Debug)]
struct CheckpointServer<H> {
    identity: LaunchIdentity,
    handler: H,
    last_token: Option<String>,
}

#[zbus::interface(name = "org.templaros.Checkpoint1")]
impl<H> CheckpointServer<H>
where
    H: CheckpointHandler + Send + Sync + 'static,
{
    /// The JSON encoding of a [`CheckpointRequest`], answered with the JSON encoding of a
    /// [`CheckpointResponse`]. A `refused` response is a legitimate answer (the instance is
    /// mid-transaction and cannot save right now) returned as ordinary method output, never as a
    /// D-Bus error; a D-Bus error here means the request itself could not even be understood.
    async fn request(&mut self, request_json: &str) -> zbus::fdo::Result<String> {
        let response = match serde_json::from_str::<CheckpointRequest>(request_json) {
            Ok(request) => {
                let response = dispatch(&mut self.handler, &request, self.last_token.as_deref());
                if let CheckpointResponse::CheckpointComplete { token, .. } = &response {
                    self.last_token = Some(token.clone());
                }
                response
            }
            Err(error) => CheckpointResponse::Refused {
                instance: self.identity.instance_id,
                reason: format!("malformed checkpoint request: {error}"),
            },
        };
        serde_json::to_string(&response)
            .map_err(|error| zbus::fdo::Error::Failed(format!("encoding response: {error}")))
    }
}

/// Claim this instance's checkpoint bus name
/// (`templar_types::dbus::checkpoint::instance_bus_name`) on `connection` and serve
/// `org.templaros.Checkpoint1` there for `handler`, so pressure containment can save this
/// application's work instead of only killing it.
///
/// Reads [`LaunchIdentity::from_env`] itself: an application started outside the broker has no
/// instance identity and nothing to claim, so this returns
/// [`ServeCheckpointError::NotLaunchedThroughTemplarOs`] rather than inventing one. `connection` is
/// a parameter (not established here) so tests can serve over a private peer-to-peer pair
/// instead of a real session bus.
pub async fn serve_checkpoint<H>(
    connection: &zbus::Connection,
    handler: H,
) -> Result<(), ServeCheckpointError>
where
    H: CheckpointHandler + Send + Sync + 'static,
{
    let identity =
        LaunchIdentity::from_env().ok_or(ServeCheckpointError::NotLaunchedThroughTemplarOs)?;
    let bus_name = checkpoint_dbus::instance_bus_name(identity.instance_id);
    connection
        .object_server()
        .at(
            checkpoint_dbus::PATH,
            CheckpointServer {
                identity,
                handler,
                last_token: None,
            },
        )
        .await?;
    connection.request_name(bus_name).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use templar_types::checkpoint::CheckpointResponse;

    struct Editor {
        dirty: bool,
        saved: Option<String>,
    }

    impl CheckpointHandler for Editor {
        fn prepare_for_pressure(&mut self) -> u64 {
            1024
        }
        fn checkpoint_now(&mut self) -> Result<(String, bool), String> {
            self.saved = Some("state-v1".into());
            Ok(("state-v1".into(), self.dirty))
        }
        fn restore(&mut self, token: &str) -> Result<(), String> {
            if token == "state-v1" {
                Ok(())
            } else {
                Err("unknown token".into())
            }
        }
    }

    #[test]
    fn protocol_round_trip() {
        let mut editor = Editor {
            dirty: true,
            saved: None,
        };
        let instance = InstanceId::new();
        let r = dispatch(
            &mut editor,
            &CheckpointRequest::PrepareForPressure { instance },
            None,
        );
        assert_eq!(
            r,
            CheckpointResponse::Prepared {
                instance,
                released_bytes: 1024
            }
        );

        let r = dispatch(
            &mut editor,
            &CheckpointRequest::CheckpointNow {
                instance,
                deadline: Duration::from_secs(3),
            },
            None,
        );
        let CheckpointResponse::CheckpointComplete {
            token,
            has_unsaved_work,
            ..
        } = r
        else {
            panic!("{r:?}")
        };
        assert!(has_unsaved_work);

        let r = dispatch(
            &mut editor,
            &CheckpointRequest::RestoreInstance {
                instance,
                previous: instance,
            },
            Some(&token),
        );
        assert!(matches!(r, CheckpointResponse::Prepared { .. }));
    }

    fn server(handler: Editor) -> CheckpointServer<Editor> {
        CheckpointServer {
            identity: LaunchIdentity {
                app_id: "org.templaros.Test".parse().unwrap(),
                instance_id: InstanceId::new(),
            },
            handler,
            last_token: None,
        }
    }

    #[tokio::test]
    async fn request_round_trips_json_and_remembers_the_token_for_a_later_restore() {
        let instance = InstanceId::new();
        let mut server = server(Editor {
            dirty: true,
            saved: None,
        });
        server.identity.instance_id = instance;

        let checkpoint_json = serde_json::to_string(&CheckpointRequest::CheckpointNow {
            instance,
            deadline: Duration::from_secs(3),
        })
        .unwrap();
        let response_json = server.request(&checkpoint_json).await.unwrap();
        let response: CheckpointResponse = serde_json::from_str(&response_json).unwrap();
        let CheckpointResponse::CheckpointComplete {
            has_unsaved_work, ..
        } = response
        else {
            panic!("{response_json}")
        };
        assert!(has_unsaved_work);
        assert_eq!(server.last_token.as_deref(), Some("state-v1"));

        // A restore with no token of its own on the wire still succeeds: the server remembered
        // the token from the checkpoint it just completed.
        let restore_json = serde_json::to_string(&CheckpointRequest::RestoreInstance {
            instance,
            previous: instance,
        })
        .unwrap();
        let response_json = server.request(&restore_json).await.unwrap();
        let response: CheckpointResponse = serde_json::from_str(&response_json).unwrap();
        assert!(matches!(response, CheckpointResponse::Prepared { .. }));
    }

    #[tokio::test]
    async fn malformed_request_json_is_a_refusal_not_a_dbus_error() {
        let mut server = server(Editor {
            dirty: false,
            saved: None,
        });
        let response_json = server.request("not json").await.unwrap();
        let response: CheckpointResponse = serde_json::from_str(&response_json).unwrap();
        let CheckpointResponse::Refused { reason, .. } = response else {
            panic!("{response_json}")
        };
        assert!(reason.contains("malformed"), "{reason}");
    }

    #[tokio::test]
    async fn serving_without_a_launch_identity_is_refused_before_touching_the_connection() {
        // The test process is normally not launched through the broker, so neither
        // TEMPLAROS_APP_ID nor TEMPLAROS_INSTANCE_ID is set — exactly the documented legacy case.
        // Run from a terminal inside a TemplarOS session, both *are* set (the terminal was
        // launched through the broker and its children inherit the identity), and the premise
        // does not hold: skip rather than fail, the same way the shell's no-service tests skip
        // when the session daemon is answering. Environment variables are process-wide, so
        // unsetting them here would race every other test reading them.
        if std::env::var(env::APP_ID).is_ok() || std::env::var(env::INSTANCE_ID).is_ok() {
            eprintln!("skipped: this process carries a TemplarOS launch identity");
            return;
        }

        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let guid = zbus::Guid::generate();
        let (client_conn, _server_conn) = tokio::try_join!(
            zbus::connection::Builder::unix_stream(client).p2p().build(),
            zbus::connection::Builder::unix_stream(server)
                .server(guid)
                .unwrap()
                .p2p()
                .build(),
        )
        .unwrap();

        let result = serve_checkpoint(
            &client_conn,
            Editor {
                dirty: false,
                saved: None,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(ServeCheckpointError::NotLaunchedThroughTemplarOs)
        ));
    }
}
