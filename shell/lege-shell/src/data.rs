//! Fetching every D-Bus-backed drawer's already-formatted text, shared between `--print` (the
//! binary) and the Home Surface's background refresh loop (`surface::data`).
//!
//! This is exactly the dispatch `main.rs` had before the graphical surface existed — moved here,
//! unchanged in behaviour, so the GUI does not have to either duplicate it or shell out to the
//! text renderer as a subprocess. `Drawer::WindowShelf` is deliberately not handled here: its data
//! comes from the live `lege_shell_v1` connection (`crate::wayland_bridge`), not a D-Bus session,
//! and the two callers of this module already have their own toplevel snapshot to hand to
//! `crate::drawers::window_shelf::render` / `crate::dock::render` directly.

use lege_client::proxies::{
    FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy, FilesVolumesProxy, ProjectsProxy,
};

use crate::drawers;
use crate::drawers::standard_paths::StandardDrawer;
use crate::home_surface::Drawer;

/// Connect to the session bus, or explain why not in one line.
pub async fn session() -> Result<zbus::Connection, String> {
    zbus::Connection::session()
        .await
        .map_err(|error| format!("service unavailable: no session bus ({error})"))
}

/// `service unavailable: <why>` — the single line a drawer prints instead of its contents.
pub fn unavailable(what: &str, error: &anyhow::Error) -> String {
    format!("{what} — service unavailable: {error:#}")
}

/// Render one D-Bus-backed drawer's text, exactly as `lege-shell --print <drawer>` does.
///
/// `drawer` must not be [`Drawer::WindowShelf`] — see the module doc for why that one is the
/// caller's own responsibility. [`Drawer::Network`] needs no bus at all and is answered directly.
pub async fn render_drawer(drawer: Drawer, limit: u32) -> String {
    if drawer == Drawer::Network {
        return drawers::network::render();
    }

    let connection = match session().await {
        Ok(connection) => connection,
        Err(message) => return format!("{} — {message}", drawer.name()),
    };

    match drawer {
        Drawer::Recent => match FilesRecentsProxy::new(&connection).await {
            Ok(proxy) => drawers::recent::render(&proxy, limit)
                .await
                .unwrap_or_else(|error| unavailable("Recent", &error)),
            Err(error) => unavailable("Recent", &error.into()),
        },
        Drawer::Drives => match FilesVolumesProxy::new(&connection).await {
            Ok(proxy) => drawers::volumes::render(&proxy)
                .await
                .unwrap_or_else(|error| unavailable("Drives", &error)),
            Err(error) => unavailable("Drives", &error.into()),
        },
        Drawer::Operations => match FilesOperationsProxy::new(&connection).await {
            Ok(proxy) => drawers::operations::render(&proxy, limit)
                .await
                .unwrap_or_else(|error| unavailable("Operations", &error)),
            Err(error) => unavailable("Operations", &error.into()),
        },
        Drawer::Trash => match FilesOperationsProxy::new(&connection).await {
            Ok(proxy) => drawers::trash::render(&proxy, limit)
                .await
                .unwrap_or_else(|error| unavailable("Trash", &error)),
            Err(error) => unavailable("Trash", &error.into()),
        },
        Drawer::Projects => match ProjectsProxy::new(&connection).await {
            Ok(proxy) => drawers::projects::render(&proxy)
                .await
                .unwrap_or_else(|error| unavailable("Projects", &error)),
            Err(error) => unavailable("Projects", &error.into()),
        },
        Drawer::Downloads | Drawer::Documents | Drawer::Media => {
            let standard = match drawer {
                Drawer::Downloads => StandardDrawer::Downloads,
                Drawer::Documents => StandardDrawer::Documents,
                _ => StandardDrawer::Media,
            };
            render_standard(&connection, standard).await
        }
        // Handled above, before the connection was made.
        Drawer::Network => unreachable!(),
        Drawer::WindowShelf => unreachable!("see this module's doc: not this module's job"),
    }
}

async fn render_standard(connection: &zbus::Connection, standard: StandardDrawer) -> String {
    let Some(home) = drawers::standard_paths::home_dir() else {
        return format!(
            "{} — service unavailable: $HOME is not set",
            standard.title()
        );
    };
    let user_dirs = drawers::standard_paths::read_user_dirs(&home);
    match FilesSearchProxy::new(connection).await {
        Ok(proxy) => drawers::standard_paths::render(&proxy, standard, &home, user_dirs.as_deref())
            .await
            .unwrap_or_else(|error| unavailable(standard.title(), &error)),
        Err(error) => unavailable(standard.title(), &error.into()),
    }
}

/// Every D-Bus-backed drawer's text in one call, for the Home Surface's background refresh.
/// `Drawer::WindowShelf` is absent — see the module doc.
pub async fn fetch_all(limit: u32) -> Vec<(Drawer, String)> {
    let mut out = Vec::with_capacity(Drawer::DEFAULT.len() - 1);
    for drawer in Drawer::DEFAULT {
        if drawer == Drawer::WindowShelf {
            continue;
        }
        out.push((drawer, render_drawer(drawer, limit).await));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_bus_free_drawer_renders_without_any_session_bus() {
        let network = render_drawer(Drawer::Network, 10).await;
        assert!(network.contains("not wired to a service yet"));
    }

    #[tokio::test]
    async fn every_bus_backed_drawer_degrades_readably_with_no_bus() {
        for drawer in Drawer::DEFAULT {
            if matches!(drawer, Drawer::Network | Drawer::WindowShelf) {
                continue;
            }
            let text = render_drawer(drawer, 5).await;
            assert!(
                text.contains("unavailable"),
                "{drawer:?} did not degrade readably: {text}"
            );
        }
    }

    #[tokio::test]
    async fn fetch_all_covers_every_drawer_except_the_window_shelf() {
        let all = fetch_all(5).await;
        assert_eq!(all.len(), Drawer::DEFAULT.len() - 1);
        assert!(!all.iter().any(|(drawer, _)| *drawer == Drawer::WindowShelf));
    }
}
