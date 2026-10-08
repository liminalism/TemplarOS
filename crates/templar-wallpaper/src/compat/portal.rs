//! The desktop owner decodes and presents portal wallpaper. This adapter never
//! decodes, and a successful Response means acceptance, not verified presentation.
use super::{ApplyStatus, Backend, Capabilities, Request, path_bytes};
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::fmt::Write;
use std::os::fd::AsFd;
use std::time::Duration;
use zbus::zvariant::{Fd, OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.portal.Wallpaper";

pub async fn inspect() -> Result<Capabilities> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::Proxy::new(&connection, DESTINATION, PATH, INTERFACE).await?;
        let version: u32 = proxy.get_property("version").await.context("desktop owner exposes no wallpaper portal")?;
        Ok(capabilities(version))
    }).await.context("wallpaper portal initialization deadline expired")?
}
fn capabilities(version: u32) -> Capabilities {
    Capabilities { backend: Backend::Portal, available: true, exact_sizing: false, reduced_buffers: false,
        resident_connection: false, indexed_storage: false, presentation_verified: false,
        detail: format!("wallpaper portal v{version}: desktop-owner acceptance only; exact sizing, per-output geometry, palette and residency limits are unavailable; no local decode; byte counts are local only, desktop-owner memory is unobserved") }
}

pub async fn apply(request: &Request) -> Result<ApplyStatus> {
    crate::config::validate(&request.config)?;
    if request.output.is_some() { bail!("wallpaper portal has no per-output selector"); }
    if !request.config.outputs.is_empty() { bail!("wallpaper portal cannot apply per-output overrides"); }
    // Sizing/budgets remain the native JSON vocabulary, but cannot be passed to
    // this protocol. Capabilities/status explicitly report no residency guarantee.
    let mut selector = crate::selection::SourceSelector::new();
    let source = request.select(&mut selector)?;
    let file = File::open(&source).with_context(|| format!("opening {}", source.display()))?;
    let initialization_deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let connection = tokio::time::timeout_at(initialization_deadline, zbus::Connection::session())
        .await.context("wallpaper portal initialization deadline expired")??;
    let owner = tokio::time::timeout_at(initialization_deadline,
        zbus::Proxy::new(&connection, DESTINATION, PATH, INTERFACE))
        .await.context("wallpaper portal initialization deadline expired")??;
    let version: u32 = tokio::time::timeout_at(initialization_deadline, owner.get_property("version"))
        .await.context("wallpaper portal initialization deadline expired")?
        .context("desktop owner exposes no wallpaper portal")?;
    super::print_json(&capabilities(version))?;
    let sender = connection.unique_name().context("session bus supplied no unique sender")?
        .as_str().trim_start_matches(':').replace('.', "_");
    // A fresh token makes the predicted request object unique even in a reused
    // bus connection. Subscribe before the method call to avoid a fast response race.
    let mut nonce = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let mut token = String::from("templar_");
    for byte in nonce { write!(&mut token, "{byte:02x}")?; }
    let predicted = format!("{PATH}/request/{sender}/{token}");
    let response_proxy = tokio::time::timeout_at(initialization_deadline,
        zbus::Proxy::new(&connection, DESTINATION, predicted.as_str(), "org.freedesktop.portal.Request"))
        .await.context("wallpaper portal initialization deadline expired")??;
    let mut responses = tokio::time::timeout_at(initialization_deadline, response_proxy.receive_signal("Response"))
        .await.context("wallpaper portal initialization deadline expired")??;
    let options: HashMap<&str, Value<'_>> = HashMap::from([
        ("handle_token", Value::from(token.as_str())),
        ("show-preview", Value::from(false)),
        ("set-on", Value::from("background")),
    ]);
    let returned: OwnedObjectPath = tokio::time::timeout(Duration::from_secs(30),
        owner.call("SetWallpaperFile", &("", Fd::from(file.as_fd()), options)))
        .await.context("wallpaper portal method deadline expired")??;
    if returned.as_str() != predicted { bail!("wallpaper portal returned an unexpected request path; cannot safely observe its response"); }
    let signal = match tokio::time::timeout(Duration::from_secs(120), responses.next()).await {
        Ok(Some(signal)) => signal,
        Ok(None) => bail!("wallpaper portal response stream closed"),
        Err(_) => {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                let _: Result<(), zbus::Error> = response_proxy.call("Close", &()).await;
            }).await;
            bail!("wallpaper portal request timed out after 120 seconds");
        }
    };
    let (code, _results): (u32, HashMap<String, OwnedValue>) = signal.body().deserialize()?;
    match code {
        0 => Ok(ApplyStatus { backend: Backend::Portal, accepted: true, presented: false, source: path_bytes(&source),
            buffer_bytes: 0, retained_bytes: 0,
            error: None }),
        1 => bail!("wallpaper portal request was cancelled by the owner/user"),
        code => bail!("wallpaper portal owner rejected the request (response {code})"),
    }
}
