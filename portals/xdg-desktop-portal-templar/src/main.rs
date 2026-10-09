//! `xdg-desktop-portal-templar` — one backend, all clients of the same services:
//! FileChooser → `org.templaros.Files` (shared last-directory policy), AppChooser →
//! `org.templaros.Associations1` + `org.templaros.AppIdentity1`, Settings → `org.templaros.Settings1`,
//! Background → `AppIdentity1` + `Resources1`.
//!
//! Four interfaces, and only those four: the ones `templaros.portal` claims. **There is no OpenURI
//! backend here, and there is no such thing to write** — `org.freedesktop.impl.portal.OpenURI`
//! is not an interface the portal frontend defines or calls. `xdg-desktop-portal` implements
//! `org.freedesktop.portal.OpenURI` itself: it finds the default handler through
//! GAppInfo/`mimeapps.list` (which `templar-associationd` generates), asks *our* `AppChooser` only
//! when it has to ask, and then launches the chosen desktop entry in its own scope. So "open
//! this downloaded file" is served here by AppChooser, and the agreement between TemplarOS
//! defaults and what the frontend picks lives in that exported `mimeapps.list`.
//!
//! Real freedesktop portal interfaces, matched against the actual specification (fetched from
//! `flatpak/xdg-desktop-portal`'s `data/org.freedesktop.impl.portal.*.xml`), not new
//! `org.templaros.*` XML — see each interface module for what is genuinely implemented versus a
//! modelled seam (`file_chooser::FileChooserUi`, `app_chooser::AppChooserUi`,
//! `background::BackgroundUi`), each of which has a native Pixelkit implementation used whenever
//! there is a display to show it on.

mod app_chooser;
mod background;
mod browser;
mod clients;
mod dialog;
mod file_chooser;
mod host;
mod parent;
mod request;
mod settings;
mod wire;

use anyhow::{Context, Result};
use clap::Parser;
use clients::{LiveAppState, LiveAssociations, LiveRecents, LiveSettings};
use std::{future::pending, sync::Arc, time::Duration};
use zbus::connection;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.templaros";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// TemplarOS portal backend.
#[derive(Debug, Parser)]
#[command(name = "xdg-desktop-portal-templar", version, about)]
struct Cli {
    /// Claim the name and idle without serving interfaces (smoke test).
    #[arg(long)]
    idle: bool,
    /// Owned request-local X11 UI process; never claims a portal backend name.
    #[arg(long, hide = true)]
    x11_dialog: bool,
}

fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    let cli = Cli::parse();
    if cli.x11_dialog {
        tracing_subscriber::fmt()
            .with_ansi(std::io::stderr().is_terminal())
            .with_writer(std::io::stderr)
            .init();
        return host::run_x11_dialog();
    }
    // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    // Say which build is running, for `templar dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep xdg-desktop-portal-templar from starting.
    if let Err(error) = templar_build::write_stamp("xdg-desktop-portal-templar") {
        tracing::warn!(%error, "could not write the build stamp");
    }
    let runtime = tokio::runtime::Runtime::new()?;
    if cli.idle || !display_available() {
        return runtime.block_on(backend(cli.idle, None));
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    pixelkit_windowing::run_window_service(
        |waker| {
            runtime.spawn(async move {
                if let Err(error) = backend(false, Some(host::Client::new(sender, waker))).await {
                    tracing::error!(%error, "portal backend infrastructure failed");
                    std::process::exit(1);
                }
            });
        },
        move || host::receive(&receiver),
    )
    .map_err(|error| anyhow::anyhow!("portal graphical host: {error}"))
}

async fn backend(idle: bool, ui_host: Option<host::Client>) -> Result<()> {
    let connection = connection::Builder::session()?
        .build()
        .await
        .context("connecting to the portal bus")?;

    if idle {
        connection.request_name(BUS_NAME).await?;
        tracing::info!(
            name = BUS_NAME,
            "portal backend idling (--idle, no interfaces served)"
        );
        pending::<()>().await;
        return Ok(());
    }

    serve(&connection, ui_host)
        .await
        .context("serving the portal backend interfaces")?;
    // Publish readiness only after every interface is registered. The frontend
    // can call immediately after activation reports name ownership.
    connection
        .request_name(BUS_NAME)
        .await
        .context("claiming the portal backend name")?;
    tracing::info!(
        name = BUS_NAME,
        "portal backend ready: FileChooser, AppChooser, Settings, Background"
    );
    pending::<()>().await;
    Ok(())
}

/// Environment is only an intent to connect; event-loop/window failures are
/// reported separately and never translated into user cancellation.
fn display_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Wire every interface onto `connection` and start the background forwarders. Split out from
/// `main` so a future integration test can call it directly against a private connection.
async fn serve(connection: &zbus::Connection, ui_host: Option<host::Client>) -> Result<()> {
    let file_chooser_ui: Arc<dyn file_chooser::FileChooserUi> = if let Some(host) = &ui_host {
        Arc::new(file_chooser::NativeUi(host.clone()))
    } else {
        Arc::new(file_chooser::NoUiAvailable)
    };
    let app_chooser_ui: Arc<dyn app_chooser::AppChooserUi> = if let Some(host) = &ui_host {
        Arc::new(app_chooser::NativeUi(host.clone()))
    } else {
        Arc::new(app_chooser::NoUiAvailable)
    };
    let background_ui: Arc<dyn background::BackgroundUi> = if let Some(host) = &ui_host {
        Arc::new(background::NativeUi(host.clone()))
    } else {
        Arc::new(background::NoUiAvailable)
    };

    let recents: Arc<dyn clients::RecentsClient> = Arc::new(LiveRecents(connection.clone()));
    let associations: Arc<dyn clients::AssociationsClient> =
        Arc::new(LiveAssociations(connection.clone()));
    let session_settings: Arc<dyn clients::SessionSettingsClient> =
        Arc::new(LiveSettings(connection.clone()));
    let app_state: Arc<dyn clients::AppStateClient> = Arc::new(LiveAppState(connection.clone()));

    connection
        .object_server()
        .at(
            PATH,
            file_chooser::FileChooserApi {
                recents,
                ui: file_chooser_ui,
            },
        )
        .await
        .context("serving FileChooser")?;
    connection
        .object_server()
        .at(
            PATH,
            app_chooser::AppChooserApi {
                associations,
                apps: Arc::clone(&app_state),
                ui: app_chooser_ui,
            },
        )
        .await
        .context("serving AppChooser")?;
    connection
        .object_server()
        .at(
            PATH,
            settings::SettingsApi {
                settings: session_settings,
            },
        )
        .await
        .context("serving Settings")?;
    connection
        .object_server()
        .at(
            PATH,
            background::BackgroundApi {
                app_state: Arc::clone(&app_state),
                ui: background_ui,
            },
        )
        .await
        .context("serving Background")?;

    settings::spawn_change_forwarder(connection.clone()).await;
    background::spawn_change_poller(connection.clone(), app_state, Duration::from_secs(5)).await;

    Ok(())
}
