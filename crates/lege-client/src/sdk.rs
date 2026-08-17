//! The application SDK: identity from the environment and the checkpoint protocol.

use lege_types::{AppId, InstanceId, checkpoint::CheckpointRequest, dbus::env};

/// The identity `appd` gave this process at launch, if it was launched through LegeOS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchIdentity {
    pub app_id: AppId,
    pub instance_id: InstanceId,
}

impl LaunchIdentity {
    /// Read `LEGEOS_APP_ID` / `LEGEOS_INSTANCE_ID`. `None` when started outside the broker
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
) -> lege_types::checkpoint::CheckpointResponse {
    use lege_types::checkpoint::CheckpointResponse as R;
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

#[cfg(test)]
mod tests {
    use super::*;
    use lege_types::checkpoint::CheckpointResponse;
    use std::time::Duration;

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
}
