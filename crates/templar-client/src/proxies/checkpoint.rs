//! Proxy for the checkpoint protocol an *application instance* serves for itself
//! (`org.templaros.Checkpoint1`).
//!
//! Inverted from every other proxy here: the caller is `templar-sessiond`, relaying the governor's
//! request, and the server is an ordinary application. The bus name is derived from the instance
//! rather than looked up — see [`templar_types::dbus::checkpoint`].
//!
//! Kept in sync with `interfaces/dbus/org.templaros.Checkpoint1.xml`.

use zbus::proxy;

#[proxy(
    interface = "org.templaros.Checkpoint1",
    default_path = "/org/templaros/Checkpoint"
)]
pub trait Checkpoint {
    /// The JSON encoding of `templar_types::checkpoint::CheckpointRequest`, answered with the JSON
    /// encoding of `CheckpointResponse`. A `refused` response is a legitimate answer, not a
    /// D-Bus error.
    fn request(&self, request_json: &str) -> zbus::Result<String>;
}
