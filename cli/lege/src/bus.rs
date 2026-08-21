//! Session/system bus connections with clear errors: a service that is not running must produce
//! one readable sentence, never a panic or a `zbus` debug dump.

use anyhow::{Context, Result};

/// Connect to the session bus, for `org.legeos.Session` / `org.legeos.Resources` /
/// `org.legeos.Files`.
pub async fn session() -> Result<zbus::Connection> {
    zbus::Connection::session()
        .await
        .context("connecting to the session bus (is a LegeOS session running?)")
}

/// Connect to the system bus, for `org.legeos.Host`.
pub async fn system() -> Result<zbus::Connection> {
    zbus::Connection::system()
        .await
        .context("connecting to the system bus")
}
