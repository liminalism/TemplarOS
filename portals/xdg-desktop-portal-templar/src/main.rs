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
mod request;
mod settings;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    // Say which build is running, for `templar dev status`. Diagnostic only: a stamp that cannot be
    // written must never keep xdg-desktop-portal-templar from starting.
    if let Err(error) = templar_build::write_stamp("xdg-desktop-portal-templar") {
        tracing::warn!(%error, "could not write the build stamp");
    }
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

/// Whether a compositor is reachable. Without one, the Pixelkit dialogs have nothing to open
/// onto, so each interface falls back to its headless default — which declines rather than
/// fabricating an answer — instead of blocking a worker thread on a window that cannot appear.
fn display_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Wire every interface onto `connection` and start the background forwarders. Split out from
/// `main` so a future integration test can call it directly against a private connection.
async fn serve(connection: &zbus::Connection) -> Result<()> {
    let graphical = display_available();
    if !graphical {
        tracing::warn!(
            "no WAYLAND_DISPLAY or DISPLAY: portal dialogs will decline instead of prompting"
        );
    }
    let file_chooser_ui: Arc<dyn file_chooser::FileChooserUi> = if graphical {
        Arc::new(file_chooser::NativeUi)
    } else {
        Arc::new(file_chooser::NoUiAvailable)
    };
    let app_chooser_ui: Arc<dyn app_chooser::AppChooserUi> = if graphical {
        Arc::new(app_chooser::NativeUi)
    } else {
        Arc::new(app_chooser::NoUiAvailable)
    };
    let background_ui: Arc<dyn background::BackgroundUi> = if graphical {
        Arc::new(background::NativeUi)
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
