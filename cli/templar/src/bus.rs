//! Session/system bus connections with clear errors: a service that is not running must produce
//! one readable sentence, never a panic or a `zbus` debug dump.

use anyhow::{Context, Result};

/// Connect to the session bus, for `org.templaros.Session` / `org.templaros.Resources` /
/// `org.templaros.Files`.
pub async fn session() -> Result<zbus::Connection> {
    zbus::Connection::session()
        .await
        .context("connecting to the session bus (is a TemplarOS session running?)")
}

/// Connect to the system bus, for `org.templaros.Host`.
pub async fn system() -> Result<zbus::Connection> {
    zbus::Connection::system()
        .await
        .context("connecting to the system bus")
}
