//! Backend Request lifetime: Close, caller loss and a bounded interaction deadline.
use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use tokio::sync::watch;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub mod response {
    pub const SUCCESS: u32 = 0;
    pub const CANCELLED: u32 = 1;
    pub const OTHER: u32 = 2;
}

pub type Reply = (u32, HashMap<String, OwnedValue>);
pub fn failed(message: String) -> Reply {
    tracing::error!(%message, "portal infrastructure failure");
    (
        response::OTHER,
        HashMap::from([("error".to_owned(), owned_value(message))]),
    )
}

#[derive(Debug)]
pub struct RequestObject(watch::Sender<bool>);
#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    async fn close(&self) {
        self.0.send_replace(true);
    }
}

pub async fn with_request<F, Fut>(
    connection: &zbus::Connection,
    handle: &OwnedObjectPath,
    caller: Option<&zbus::names::UniqueName<'_>>,
    body: F,
) -> zbus::fdo::Result<Reply>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Reply>,
{
    let (close, mut closed) = watch::channel(false);
    let registered = connection
        .object_server()
        .at(handle.clone(), RequestObject(close))
        .await
        .map_err(|error| zbus::fdo::Error::Failed(format!("exporting Request: {error}")))?;
    if !registered {
        return Err(zbus::fdo::Error::InvalidArgs(
            "request handle is already in use".into(),
        ));
    }
    // Subscribe before checking ownership so a disconnect cannot fall between
    // the snapshot and the watch. No unmanaged/background task per request.
    let result = async {
        let proxy = zbus::fdo::DBusProxy::new(connection).await?;
        let mut owners = proxy.receive_name_owner_changed().await?;
        if let Some(caller) = caller {
            if !proxy.name_has_owner(zbus::names::BusName::from(caller.clone())).await? {
                return Ok((response::CANCELLED, HashMap::new()));
            }
        }
        let caller_lost = async {
            while let Some(signal) = owners.next().await {
                let args = signal.args()?;
                if caller.is_some_and(|caller| args.name().as_str() == caller.as_str())
                    && args.new_owner().is_none() {
                    return Ok::<(), zbus::Error>(());
                }
            }
            Err(zbus::Error::Failure("caller watch closed".into()))
        };
        tokio::select! {
            biased;
            _ = closed.changed() => Ok((response::CANCELLED, HashMap::new())),
            lost = caller_lost => Ok(match lost {
                Ok(()) => (response::CANCELLED, HashMap::new()),
                Err(error) => failed(format!("caller watch failed: {error}")),
            }),
            () = tokio::time::sleep(Duration::from_secs(300)) => Ok(failed("portal request timed out".into())),
            result = body() => Ok(result),
        }
    }.await;
    // Dropping the selected-out UI future wakes and closes exactly its window.
    connection
        .object_server()
        .remove::<RequestObject, _>(handle.clone())
        .await
        .map_err(|error| zbus::fdo::Error::Failed(format!("retiring Request: {error}")))?;
    result
}

pub fn owned_value<'v>(value: impl Into<zbus::zvariant::Value<'v>>) -> OwnedValue {
    value
        .into()
        .try_to_owned()
        .expect("converting a primitive/string/collection value to OwnedValue never fails")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn infrastructure_failure_is_not_cancellation() {
        let (code, results) = failed("display disconnected".into());
        assert_eq!(code, response::OTHER);
        assert_eq!(
            String::try_from(results["error"].clone()).unwrap(),
            "display disconnected"
        );
    }
}
