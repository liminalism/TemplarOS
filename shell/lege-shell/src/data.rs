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
    AppIdentityProxy, FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy, FilesVolumesProxy,
    HomeFieldProxy, ProjectsProxy,
};
use lege_field::{FieldDocument, FieldOp};
use lege_types::AppId;

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
/// Every installed application's canonical id (`AppIdentity1.list_apps`), or none when the
/// session bus or sessiond is absent — the Home Field then shows only windows and drawers.
pub async fn fetch_apps() -> Vec<AppId> {
    let Ok(connection) = session().await else {
        return Vec::new();
    };
    let Ok(proxy) = AppIdentityProxy::new(&connection).await else {
        return Vec::new();
    };
    proxy
        .list_apps()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| id.parse().ok())
        .collect()
}

/// `HomeField1.Get`: the persisted field for `output`, created at `cols`×`rows` if new. `None`
/// when sessiond is absent — the shell then keeps an in-session document instead.
pub async fn fetch_field(output: &str, cols: u16, rows: u16) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = proxy.get(output, cols, rows).await.ok()?;
    serde_json::from_str(&json).ok()
}

/// `HomeField1.Apply`: persist one op; the new revision, or `None` when refused or unreachable.
pub async fn apply_field_op(output: &str, op: &FieldOp) -> Option<u64> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = serde_json::to_string(op).ok()?;
    match proxy.apply(output, &json).await {
        Ok(revision) => Some(revision),
        Err(error) => {
            tracing::warn!(%error, "HomeField1.Apply refused");
            None
        }
    }
}

/// `HomeField1.Undo`: the document after undoing the last op, or `None` if nothing to undo or
/// the service is unreachable.
pub async fn undo_field(output: &str) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    let json = proxy.undo(output).await.ok()?;
    if json.is_empty() {
        return None;
    }
    serde_json::from_str(&json).ok()
}

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
