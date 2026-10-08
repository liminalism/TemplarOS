//! Installed-session activation, performed once before the compositor enters its render loop.
//! Direct/nested runs never call this: their private display must not replace a login's environment.

use anyhow::{Context, Result, ensure};
use std::{collections::HashMap, io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};

/// Publish the listener that actually exists to both activation owners and the session launcher.
/// A failure prevents readiness; it never announces a guessed or unreachable display.
pub async fn publish_session(socket: &str) -> Result<PathBuf> {
    ensure!(socket.starts_with("wayland-") && socket[8..].bytes().all(|byte| byte.is_ascii_digit()) && socket.len() > 8,
        "installed Wayland socket must be wayland-N");
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is absent")?);
    ensure!(runtime.join(socket).exists(), "actual Wayland socket is absent");
    let environment = HashMap::from([
        ("WAYLAND_DISPLAY", socket),
        ("DISPLAY", ""),
        ("XDG_CURRENT_DESKTOP", "TemplarOS"),
        ("XDG_SESSION_DESKTOP", "templaros"),
        ("XDG_SESSION_TYPE", "wayland"),
    ]);
    tokio::time::timeout(Duration::from_secs(5), async {
        let connection = zbus::Connection::session().await.context("connecting activation bus")?;
        let dbus = zbus::Proxy::new(&connection, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").await?;
        dbus.call::<_, _, ()>("UpdateActivationEnvironment", &(&environment,)).await.context("publishing D-Bus activation environment")?;
        let manager = zbus::Proxy::new(&connection, "org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager").await?;
        let entries: Vec<String> = environment.iter().map(|(key, value)| format!("{key}={value}")).collect();
        manager.call::<_, _, ()>("SetEnvironment", &(entries,)).await.context("publishing user-manager activation environment")?;
        Ok::<(), anyhow::Error>(())
    }).await.context("publishing session activation timed out")??;
    let directory = runtime.join("templaros");
    std::fs::create_dir_all(&directory).context("creating compositor runtime directory")?;
    let pid = std::process::id();
    let destination = directory.join("compositor-display.json");
    let temporary = directory.join(format!(".compositor-display-{pid}"));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)
        .context("creating compositor display publication")?;
    let result = (|| {
        write!(file, "{{\"schema\":1,\"compositor_pid\":{pid},\"socket\":\"{socket}\"}}\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &destination)?;
        Ok::<(), std::io::Error>(())
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result.context("publishing actual compositor display")?;
    Ok(destination)
}
