//! Fetching Home's D-Bus data: compact drawer text, structured directory snapshots and brokered
//! file opening, shared between `--print` and the graphical Home Surface.
//!
//! `Drawer::WindowShelf` is deliberately not handled here: its data comes from the live
//! `lege_shell_v1` connection (`crate::wayland_bridge`), not a D-Bus session, and the callers
//! already have their own toplevel snapshot.

use lege_client::proxies::{
    AppIdentityProxy, AssociationsProxy, FilesBrowseProxy, FilesOperationsProxy, FilesRecentsProxy,
    FilesSearchProxy, FilesVolumesProxy, HomeFieldProxy, LauncherProxy, LifecycleProxy,
    NotificationsProxy, ProjectsProxy,
};
use lege_field::{FieldDocument, FieldOp};
use std::fmt::Write as _;

use crate::drawers;
use crate::drawers::standard_paths::StandardDrawer;
use crate::file_browser::{BrowserRoot, BrowserSources, DirectoryListing, RootKind};
use crate::home_surface::Drawer;

/// Connect to the session bus, or explain why not in one line.
pub async fn session() -> Result<zbus::Connection, String> {
    zbus::Connection::session()
        .await
        .map_err(|error| format!("service unavailable: no session bus ({error})"))
}

/// Whether a LegeOS session daemon is answering on this machine's session bus.
///
/// Only tests ask this, and they ask because they describe what a drawer prints when *nothing*
/// answers — which can only be observed when nothing does. A developer running the session,
/// installed or nested via `.agent/scratch/nested-session.sh`, has a live `org.legeos.Session`, and
/// those drawers then return real content. That is the service working, not a regression, and
/// failing the build over it sends the next person hunting a bug that is not there.
pub async fn a_session_daemon_is_answering() -> bool {
    let Ok(connection) = session().await else {
        return false;
    };
    let Ok(proxy) = zbus::fdo::DBusProxy::new(&connection).await else {
        return false;
    };
    proxy
        .name_has_owner("org.legeos.Session".try_into().expect("a valid bus name"))
        .await
        .unwrap_or(false)
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
    let search = match FilesSearchProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return unavailable(standard.title(), &error.into()),
    };
    let mut rendered =
        match drawers::standard_paths::render(&search, standard, &home, user_dirs.as_deref()).await
        {
            Ok(rendered) => rendered,
            Err(error) => return unavailable(standard.title(), &error),
        };
    let browse = match FilesBrowseProxy::new(connection).await {
        Ok(proxy) => proxy,
        Err(error) => return unavailable(standard.title(), &error.into()),
    };
    let parsed_user_dirs = user_dirs
        .as_deref()
        .map(|text| drawers::standard_paths::parse_user_dirs(text, &home))
        .unwrap_or_default();
    for directory in drawers::standard_paths::directories_for(standard, &parsed_user_dirs, &home) {
        let json = match browse
            .list_directory(&directory.to_string_lossy(), false, 12)
            .await
        {
            Ok(json) => json,
            Err(error) => {
                write!(
                    &mut rendered,
                    "\n  could not read {}: {error}",
                    directory.display()
                )
                .expect("writing to a String cannot fail");
                continue;
            }
        };
        let listing: DirectoryListing = match serde_json::from_str(&json) {
            Ok(listing) => listing,
            Err(error) => {
                write!(
                    &mut rendered,
                    "\n  malformed directory data for {}: {error}",
                    directory.display()
                )
                .expect("writing to a String cannot fail");
                continue;
            }
        };
        if listing.entries.is_empty() {
            rendered.push_str("\n  (empty)");
        }
        for entry in listing.entries {
            write!(
                &mut rendered,
                "\n  {:<7} {:<12} {}",
                if entry.is_dir() { "folder" } else { "file" },
                if entry.is_dir() {
                    "—".to_owned()
                } else {
                    crate::format::bytes(entry.size_bytes)
                },
                entry.name
            )
            .expect("writing to a String cannot fail");
        }
        if listing.truncated {
            write!(&mut rendered, "\n  … more than {} entries", listing.limit)
                .expect("writing to a String cannot fail");
        }
    }
    rendered
}

/// Every D-Bus-backed drawer's text in one call, for the Home Surface's background refresh.
/// `Drawer::WindowShelf` is absent — see the module doc.
/// Every installed application with what a launcher needs to offer it
/// (`AppIdentity1.ListApplications`).
///
/// `None` — not an empty list — when the session bus or sessiond is absent, because the two mean
/// opposite things to the person looking at the launcher: "nothing is installed" versus "I cannot
/// see what is installed". The widget says which.
pub async fn fetch_apps() -> Option<Vec<crate::app_search::AppEntry>> {
    let connection = session().await.ok()?;
    let proxy = AppIdentityProxy::new(&connection).await.ok()?;
    let rows = proxy.list_applications().await.ok()?;
    Some(
        rows.into_iter()
            .filter_map(|row| {
                let (id, name, desktop_file_id, icon, categories, keywords, no_display) = row;
                Some(crate::app_search::AppEntry {
                    id: id.parse().ok()?,
                    name,
                    desktop_file_id,
                    icon,
                    categories,
                    keywords,
                    no_display,
                })
            })
            .collect(),
    )
}

/// The Session widget's content: whether this machine will actually log out, restart, shut down
/// or suspend, asked of `Lifecycle1` (which asks logind).
///
/// A widget that cannot reach the service says so rather than offering controls that fail on
/// click — see `crate::surface::widgets`.
pub async fn fetch_session_widget() -> crate::surface::widgets::WidgetSnapshot {
    use crate::surface::widgets::WidgetSnapshot;
    let Ok(connection) = session().await else {
        return WidgetSnapshot::unavailable("Session — no session bus");
    };
    let Ok(proxy) = LifecycleProxy::new(&connection).await else {
        return WidgetSnapshot::unavailable("Session — Lifecycle1 unavailable");
    };
    let mut detail = Vec::new();
    let mut refused = Vec::new();
    let mut reachable = false;
    for (label, action, verdict) in [
        ("Shut down", "session.poweroff", proxy.can_power_off().await),
        ("Restart", "session.reboot", proxy.can_reboot().await),
        ("Suspend", "session.suspend", proxy.can_suspend().await),
    ] {
        match verdict {
            Ok(verdict) => {
                reachable = true;
                // logind's own vocabulary, kept distinct: "challenge" is not "no".
                let meaning = match verdict.as_str() {
                    "yes" => "ready",
                    "no" => "refused by policy",
                    "na" => "not supported",
                    "challenge" => "needs authentication",
                    other => other,
                };
                // Only a settled no. "challenge" means logind would authenticate the user, which
                // is a working control with a step in front of it, not a refused one.
                if matches!(verdict.as_str(), "no" | "na") {
                    refused.push(action.to_owned());
                }
                detail.push(format!("{label} — {meaning}"));
            }
            Err(error) => tracing::debug!(%error, %label, "Lifecycle1 query failed"),
        }
    }
    if !reachable {
        return WidgetSnapshot::unavailable("Session — logind unreachable");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "signed in".to_owned());
    WidgetSnapshot {
        headline: user,
        detail,
        available: true,
        refused,
    }
}

/// Active notifications as a stable Home widget snapshot. The service owns ordering, expiry and
/// dismissal; Home only turns its returned JSON into a count and a few lines.
pub async fn fetch_notifications_widget() -> crate::surface::widgets::WidgetSnapshot {
    use crate::surface::widgets::WidgetSnapshot;
    let Ok(connection) = session().await else {
        return WidgetSnapshot::unavailable("Notifications — no session bus");
    };
    let Ok(proxy) = NotificationsProxy::new(&connection).await else {
        return WidgetSnapshot::unavailable("Notifications — service unavailable");
    };
    let Ok(rows) = proxy.list_active().await else {
        return WidgetSnapshot::unavailable("Notifications — service unavailable");
    };
    let detail = rows
        .iter()
        .take(3)
        .filter_map(|row| serde_json::from_str::<serde_json::Value>(row).ok())
        .map(|row| {
            let app = row["app_name"].as_str().unwrap_or("Application");
            let summary = row["summary"].as_str().unwrap_or("Notification");
            format!("{app} — {summary}")
        })
        .collect();
    WidgetSnapshot {
        headline: match rows.len() {
            0 => "All caught up".to_owned(),
            1 => "1 notification".to_owned(),
            count => format!("{count} notifications"),
        },
        detail,
        available: true,
        refused: Vec::new(),
    }
}

/// `Projects1.List`, or none when sessiond is absent.
pub async fn fetch_projects() -> Vec<crate::field_model::ProjectInfo> {
    let Ok(connection) = session().await else {
        return Vec::new();
    };
    let Ok(proxy) = ProjectsProxy::new(&connection).await else {
        return Vec::new();
    };
    proxy
        .list()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(id, name, _root, is_open)| crate::field_model::ProjectInfo { id, name, is_open })
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

/// `HomeField1.Reset`: discard the stored layout and return the fresh document that replaces it.
/// `None` when the service is unreachable — there is nothing sensible to do locally, because the
/// point of a reset is that the *stored* document goes away.
pub async fn reset_field(output: &str, cols: u16, rows: u16) -> Option<FieldDocument> {
    let connection = session().await.ok()?;
    let proxy = HomeFieldProxy::new(&connection).await.ok()?;
    match proxy.reset(output, cols, rows).await {
        Ok(json) => serde_json::from_str(&json).ok(),
        Err(error) => {
            tracing::warn!(%error, "HomeField1.Reset refused");
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

/// The same XDG places and mounted volumes the standalone manager renders. `Files.Browse1`
/// owns the source list; Home only maps its structured roots onto drawer starting points.
pub async fn fetch_browser_sources() -> BrowserSources {
    let fallback = local_browser_sources();
    let Ok(connection) = session().await else {
        return fallback;
    };
    let Ok(proxy) = FilesBrowseProxy::new(&connection).await else {
        return fallback;
    };
    let Ok(json) = proxy.roots().await else {
        return fallback;
    };
    let Ok(shared) = serde_json::from_str::<lege_client::files::BrowserSources>(&json) else {
        return fallback;
    };
    let mut sources = BrowserSources::default();
    let home = drawers::standard_paths::home_dir();
    let mut drive_start: Option<std::path::PathBuf> = None;
    let mut drive_depth = 0;
    for root in shared.roots {
        if root.kind == lege_client::files::RootKind::Drive
            && home
                .as_ref()
                .is_some_and(|home| home.starts_with(&root.path))
        {
            let depth = root.path.components().count();
            if depth >= drive_depth {
                drive_depth = depth;
                drive_start = Some(root.path.clone());
            }
        }
        match root.key.as_str() {
            "downloads" => {
                sources.starts.insert(Drawer::Downloads, root.path.clone());
            }
            "documents" => {
                sources.starts.insert(Drawer::Documents, root.path.clone());
            }
            "pictures" => {
                sources.starts.insert(Drawer::Media, root.path.clone());
            }
            _ => {}
        }
        let detail = root.volume.as_ref().map_or_else(
            || {
                if root.available {
                    "Standard folder".to_owned()
                } else {
                    "Not present".to_owned()
                }
            },
            |volume| {
                format!(
                    "{} free of {} · {}{}",
                    crate::format::bytes(volume.free_bytes),
                    crate::format::bytes(volume.total_bytes),
                    volume.fs_type,
                    if volume.read_only {
                        " · read-only"
                    } else {
                        ""
                    }
                )
            },
        );
        sources.roots.push(BrowserRoot {
            label: root.label,
            path: root.path,
            detail,
            kind: match root.kind {
                lege_client::files::RootKind::Place => RootKind::Place,
                lege_client::files::RootKind::Drive => RootKind::Drive,
            },
        });
    }
    if drive_start.is_none() {
        drive_start = sources
            .roots
            .iter()
            .find(|root| root.kind == RootKind::Drive)
            .map(|root| root.path.clone());
    }
    if let Some(start) = drive_start {
        sources.starts.insert(Drawer::Drives, start);
    }
    sources
}

fn local_browser_sources() -> BrowserSources {
    let Some(home) = drawers::standard_paths::home_dir() else {
        return BrowserSources::default();
    };
    let user_dirs = drawers::standard_paths::read_user_dirs(&home)
        .map(|text| drawers::standard_paths::parse_user_dirs(&text, &home))
        .unwrap_or_default();
    let downloads =
        drawers::standard_paths::directories_for(StandardDrawer::Downloads, &user_dirs, &home);
    let documents =
        drawers::standard_paths::directories_for(StandardDrawer::Documents, &user_dirs, &home);
    let media = drawers::standard_paths::directories_for(StandardDrawer::Media, &user_dirs, &home);
    let mut sources = BrowserSources::default();
    sources.roots.push(BrowserRoot {
        label: "Home".to_owned(),
        path: home,
        detail: "Personal files".to_owned(),
        kind: RootKind::Place,
    });
    add_place(&mut sources, Drawer::Downloads, "Downloads", &downloads);
    add_place(&mut sources, Drawer::Documents, "Documents", &documents);
    for (index, path) in media.iter().enumerate() {
        let label = ["Pictures", "Music", "Videos"]
            .get(index)
            .copied()
            .unwrap_or("Media");
        sources.roots.push(BrowserRoot {
            label: label.to_owned(),
            path: path.clone(),
            detail: "Media folder".to_owned(),
            kind: RootKind::Place,
        });
    }
    if let Some(path) = media.first() {
        sources.starts.insert(Drawer::Media, path.clone());
    }
    sources
}

fn add_place(
    sources: &mut BrowserSources,
    drawer: Drawer,
    label: &str,
    paths: &[std::path::PathBuf],
) {
    if let Some(path) = paths.first() {
        sources.starts.insert(drawer, path.clone());
    }
    for path in paths {
        sources.roots.push(BrowserRoot {
            label: label.to_owned(),
            path: path.clone(),
            detail: "Standard folder".to_owned(),
            kind: RootKind::Place,
        });
    }
}

/// Ask `lege-filed` for one real directory snapshot.
pub async fn browse_directory(
    path: &std::path::Path,
    show_hidden: bool,
) -> Result<DirectoryListing, String> {
    let connection = session().await?;
    let proxy = FilesBrowseProxy::new(&connection)
        .await
        .map_err(|error| format!("file service unavailable: {error}"))?;
    let json = proxy
        .list_directory(&path.to_string_lossy(), show_hidden, 500)
        .await
        .map_err(|error| format!("could not list {}: {error}", path.display()))?;
    serde_json::from_str(&json)
        .map_err(|error| format!("malformed directory data for {}: {error}", path.display()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenFileResult {
    pub app_id: String,
    pub mime: String,
    pub instance: String,
}

#[async_trait::async_trait]
pub trait BrowserAssociations: Send + Sync {
    async fn resolve_file(&self, path: &str) -> Result<(String, String, String), String>;
}

#[async_trait::async_trait]
impl BrowserAssociations for AssociationsProxy<'_> {
    async fn resolve_file(&self, path: &str) -> Result<(String, String, String), String> {
        self.resolve_file(path, "")
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
pub trait BrowserLauncher: Send + Sync {
    async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String>;
}

#[async_trait::async_trait]
impl BrowserLauncher for LauncherProxy<'_> {
    async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String> {
        self.launch(app_id, arguments, "")
            .await
            .map_err(|error| error.to_string())
    }
}

/// Resolve first, launch second. An empty association is a visible result, never permission to
/// bypass the broker with `xdg-open` or a shell command.
pub async fn brokered_file_open(
    associations: &impl BrowserAssociations,
    launcher: &impl BrowserLauncher,
    path: &std::path::Path,
) -> Result<OpenFileResult, String> {
    let target = path.to_string_lossy().into_owned();
    let (app_id, mime, reason) = associations
        .resolve_file(&target)
        .await
        .map_err(|error| format!("could not resolve {}: {error}", path.display()))?;
    if app_id.is_empty() {
        return Err(format!(
            "No application is associated with {} ({}; {reason})",
            path.display(),
            if mime.is_empty() {
                "unknown type"
            } else {
                &mime
            }
        ));
    }
    let instance = launcher
        .launch(&app_id, vec![target])
        .await
        .map_err(|error| format!("could not open {} with {app_id}: {error}", path.display()))?;
    Ok(OpenFileResult {
        app_id,
        mime,
        instance,
    })
}

pub async fn open_file(path: &std::path::Path) -> Result<OpenFileResult, String> {
    let connection = session().await?;
    let associations = AssociationsProxy::new(&connection)
        .await
        .map_err(|error| format!("association service unavailable: {error}"))?;
    let launcher = LauncherProxy::new(&connection)
        .await
        .map_err(|error| format!("launcher unavailable: {error}"))?;
    let result = brokered_file_open(&associations, &launcher, path).await?;
    if let Ok(recents) = FilesRecentsProxy::new(&connection).await {
        let _ = recents
            .record(
                &path.to_string_lossy(),
                &result.app_id,
                "opened",
                &result.mime,
            )
            .await;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeAssociations((String, String, String));

    #[async_trait::async_trait]
    impl BrowserAssociations for FakeAssociations {
        async fn resolve_file(&self, _path: &str) -> Result<(String, String, String), String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeLauncher(Mutex<Vec<(String, Vec<String>)>>);

    #[async_trait::async_trait]
    impl BrowserLauncher for FakeLauncher {
        async fn launch(&self, app_id: &str, arguments: Vec<String>) -> Result<String, String> {
            self.0.lock().unwrap().push((app_id.to_owned(), arguments));
            Ok("instance-1".to_owned())
        }
    }

    #[tokio::test]
    async fn the_bus_free_drawer_renders_without_any_session_bus() {
        let network = render_drawer(Drawer::Network, 10).await;
        assert!(network.contains("not wired to a service yet"));
    }

    #[tokio::test]
    async fn every_bus_backed_drawer_degrades_readably_with_no_bus() {
        if a_session_daemon_is_answering().await {
            eprintln!(
                "skipped: org.legeos.Session is answering on this bus, so the no-service path \
                 cannot be observed here"
            );
            return;
        }
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

    #[tokio::test]
    async fn brokered_file_open_resolves_then_launches_the_canonical_app() {
        let associations = FakeAssociations((
            "org.example.Reader".into(),
            "application/pdf".into(),
            "global-mime-default".into(),
        ));
        let launcher = FakeLauncher::default();
        let result = brokered_file_open(
            &associations,
            &launcher,
            std::path::Path::new("/home/test/paper.pdf"),
        )
        .await
        .unwrap();
        assert_eq!(result.app_id, "org.example.Reader");
        assert_eq!(result.instance, "instance-1");
        assert_eq!(
            *launcher.0.lock().unwrap(),
            vec![(
                "org.example.Reader".into(),
                vec!["/home/test/paper.pdf".into()]
            )]
        );
    }

    #[tokio::test]
    async fn brokered_file_open_never_bypasses_an_unresolved_association() {
        let associations = FakeAssociations((
            String::new(),
            "application/x-unknown".into(),
            "ask-user".into(),
        ));
        let launcher = FakeLauncher::default();
        let error = brokered_file_open(
            &associations,
            &launcher,
            std::path::Path::new("/home/test/unknown.bin"),
        )
        .await
        .unwrap_err();
        assert!(error.contains("No application is associated"));
        assert!(error.contains("ask-user"));
        assert!(launcher.0.lock().unwrap().is_empty());
    }
}
