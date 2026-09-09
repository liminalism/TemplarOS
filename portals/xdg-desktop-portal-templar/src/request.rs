//! `org.freedesktop.impl.portal.Request`: the object every backend method exports at its
//! caller-supplied `handle` path for the duration of one call.
//!
//! None of our four interfaces ever keeps a request open across an `await` point that outlives
//! the method call itself (no interactive dialog exists yet — see each interface module's `Ui`
//! seam), so `Close` has nothing to interrupt; it is still exported, because a well-behaved
//! frontend may call it defensively, and an unknown method on a path it just received would be a
//! spec violation. No `org.freedesktop.impl.portal.Session` object is used anywhere in this
//! backend: that pattern belongs to stateful portals (ScreenCast, RemoteDesktop, Inhibit), none
//! of which this backend implements.

/// Numeric response codes shared by every backend method here
/// (`org.freedesktop.impl.portal.Request` callers' convention).
pub mod response {
    /// The request is carried out.
    pub const SUCCESS: u32 = 0;
    /// The user cancelled the interaction, or no UI was available to ask.
    pub const CANCELLED: u32 = 1;
    /// The interaction ended some other way.
    #[allow(dead_code)]
    pub const OTHER: u32 = 2;
}

/// The `Close` method every exported Request object must answer.
#[derive(Debug, Default)]
pub struct RequestObject;

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestObject {
    /// Ends the user interaction. A no-op here: our methods complete before returning, so there
    /// is never an in-flight interaction left for `Close` to interrupt.
    async fn close(&self) {}
}

/// Export a fresh [`RequestObject`] at `handle` for the lifetime of one backend call, then
/// unregister it once `body` resolves — so a caller inspecting the object between the method
/// call and its reply finds a real (if inert) `Request`, and nothing is leaked afterwards.
pub async fn with_request<T, F, Fut>(
    connection: &zbus::Connection,
    handle: &zbus::zvariant::OwnedObjectPath,
    body: F,
) -> zbus::fdo::Result<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let registered = connection
        .object_server()
        .at(handle.clone(), RequestObject)
        .await
        .map_err(|error| {
            zbus::fdo::Error::Failed(format!("exporting Request at {handle}: {error}"))
        })?;
    let result = body().await;
    if registered {
        let _ = connection
            .object_server()
            .remove::<RequestObject, _>(handle.clone())
            .await;
    }
    Ok(result)
}

/// Convert any zvariant-compatible value into an owned `v` for an `a{sv}` results map — the
/// small piece every backend method needs and `zbus::zvariant::OwnedValue` does not implement
/// `From` for directly (`String` notably), so it goes through the borrowed `Value` first.
pub fn owned_value<'v>(value: impl Into<zbus::zvariant::Value<'v>>) -> zbus::zvariant::OwnedValue {
    value
        .into()
        .try_to_owned()
        .expect("converting a primitive/string/collection value to OwnedValue never fails")
}
