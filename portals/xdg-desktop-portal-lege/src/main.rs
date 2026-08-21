//! `xdg-desktop-portal-lege` — one backend, all clients of the same services:
//! FileChooser → `org.legeos.Files` (shared last-directory policy), AppChooser/OpenURI →
//! `org.legeos.Associations1`, Settings → `org.legeos.Settings1`, Background → `AppIdentity1` +
//! `Resources1`.
//!
//! Real freedesktop portal interfaces, matched against the actual specification (fetched from
//! `flatpak/xdg-desktop-portal`'s `data/org.freedesktop.impl.portal.*.xml`), not new
//! `org.legeos.*` XML — see each interface module for what is genuinely implemented versus a
//! modelled seam awaiting the shell's toolkit (`file_chooser::FileChooserUi`,
//! `app_chooser::AppChooserUi`, `background::BackgroundUi`).

mod app_chooser;
mod background;
mod clients;
mod file_chooser;
mod request;
mod settings;

use anyhow::{Context, Result};
use clap::Parser;
use clients::{LiveAppState, LiveAssociations, LiveRecents, LiveSettings};
use std::{future::pending, sync::Arc, time::Duration};
use zbus::connection;

pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.legeos";
pub const PATH: &str = "/org/freedesktop/portal/desktop";

/// LegeOS portal backend.
#[derive(Debug, Parser)]
#[command(name = "xdg-desktop-portal-lege", version, about)]
struct Cli {
    /// Claim the name and idle without serving interfaces (smoke test).
    #[arg(long)]
    idle: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    let connection = connection::Builder::session()?
        .name(BUS_NAME)?
        .build()
        .await
        .context("claiming the portal backend name")?;

    if cli.idle {
        tracing::info!(
            name = BUS_NAME,
            "portal backend idling (--idle, no interfaces served)"
        );
        pending::<()>().await;
        return Ok(());
    }

    serve(&connection)
        .await
        .context("serving the portal backend interfaces")?;
    tracing::info!(
        name = BUS_NAME,
        "portal backend ready: FileChooser, AppChooser, Settings, Background"
    );
    pending::<()>().await;
    Ok(())
}

/// Wire every interface onto `connection` and start the background forwarders. Split out from
/// `main` so a future integration test can call it directly against a private connection.
async fn serve(connection: &zbus::Connection) -> Result<()> {
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
                ui: Arc::new(file_chooser::NoUiAvailable),
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
                ui: Arc::new(app_chooser::NoUiAvailable),
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
                ui: Arc::new(background::NoUiAvailable),
            },
        )
        .await
        .context("serving Background")?;

    settings::spawn_change_forwarder(connection.clone()).await;
    background::spawn_change_poller(connection.clone(), app_state, Duration::from_secs(5)).await;

    Ok(())
}
