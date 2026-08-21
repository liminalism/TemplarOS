//! `lege-shell` — everything decorative, restartable without terminating applications.
//!
//! This milestone builds the part that has to be right whatever renders it: the Home Surface as
//! a **model over the services**, plus a text renderer. Every drawer is a live view of an
//! `org.legeos.*` interface rather than state the shell keeps for itself (planning doc 01 §1),
//! the dock groups by canonical `AppId` through `lege_window::shell` so it needs no local
//! `WM_CLASS` heuristics, and the pressure overlay's wording is a pure function of one
//! `Resources1` signal.
//!
//! No UI toolkit is linked yet, deliberately: libcosmic is a large git dependency and the first
//! real surface belongs to a later milestone. `--print <drawer>` is what makes the model
//! exercisable and testable today, and it is also how `docs/architecture/accessibility.md`'s
//! "everything the shell shows is available through `lege` too" is met for the shell's own
//! surfaces while the graphical ones do not exist.
//!
//! A drawer whose service is not running prints one clear line saying so. The shell is the last
//! thing that should panic when a daemon is missing.

use anyhow::Result;
use clap::Parser;
use lege_client::proxies::{
    FilesOperationsProxy, FilesRecentsProxy, FilesSearchProxy, FilesVolumesProxy, ProjectsProxy,
    SettingsProxy,
};
use lege_window::ShellToplevel;

use lege_shell::{
    dock, drawers, drawers::standard_paths::StandardDrawer, home_surface::Drawer, pressure,
    settings,
};

/// LegeOS shell.
#[derive(Debug, Parser)]
#[command(name = "lege-shell", version, about)]
struct Cli {
    /// List the Home Surface drawers and exit.
    #[arg(long)]
    list_drawers: bool,

    /// Render one drawer as text and exit. Also accepts `dock` and `pressure`.
    #[arg(long, value_name = "DRAWER")]
    print: Option<String>,

    /// How many entries to show in the drawers that are lists.
    #[arg(long, default_value_t = 20)]
    limit: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    if cli.list_drawers {
        for drawer in Drawer::DEFAULT {
            println!("{:<12} {}", drawer.name(), drawer.description());
        }
        return Ok(());
    }

    if let Some(target) = &cli.print {
        println!("{}", print_target(target, cli.limit).await?);
        return Ok(());
    }

    tracing::info!(
        "lege-shell: no graphical surface in this milestone; use --print <drawer> or --list-drawers"
    );
    Ok(())
}

/// Every value `--print` accepts, for the error message when one is misspelled.
fn print_targets() -> Vec<String> {
    let mut targets: Vec<String> = Drawer::DEFAULT
        .iter()
        .map(|drawer| slug(*drawer).to_owned())
        .collect();
    targets.push("dock".to_owned());
    targets.push("pressure".to_owned());
    targets
}

/// The `--print` name of a drawer: its display name, lowercased, which is what a person would
/// type. `WindowShelf` renders as "Windows", so `windows` is its slug; `window-shelf` is accepted
/// too because that is what the enum is called.
fn slug(drawer: Drawer) -> &'static str {
    match drawer {
        Drawer::Recent => "recent",
        Drawer::Projects => "projects",
        Drawer::Downloads => "downloads",
        Drawer::Documents => "documents",
        Drawer::Media => "media",
        Drawer::Drives => "drives",
        Drawer::Network => "network",
        Drawer::Operations => "operations",
        Drawer::Trash => "trash",
        Drawer::WindowShelf => "windows",
    }
}

fn drawer_from_slug(target: &str) -> Option<Drawer> {
    let target = target.trim().to_ascii_lowercase();
    if target == "window-shelf" || target == "shelf" {
        return Some(Drawer::WindowShelf);
    }
    Drawer::DEFAULT
        .into_iter()
        .find(|drawer| slug(*drawer) == target)
}

/// Render one `--print` target, degrading to a readable line per drawer when its service is not
/// reachable rather than failing the whole command.
async fn print_target(target: &str, limit: u32) -> Result<String> {
    if target.eq_ignore_ascii_case("dock") {
        return Ok(render_dock().await);
    }
    if target.eq_ignore_ascii_case("pressure") {
        return Ok(render_pressure_glossary());
    }

    let Some(drawer) = drawer_from_slug(target) else {
        anyhow::bail!(
            "unknown drawer {target:?}; expected one of {}",
            print_targets().join(", ")
        );
    };
    Ok(render_drawer(drawer, limit).await)
}

/// Connect to the session bus, or explain why not in one line.
async fn session() -> std::result::Result<zbus::Connection, String> {
    zbus::Connection::session()
        .await
        .map_err(|error| format!("service unavailable: no session bus ({error})"))
}

/// `service unavailable: <why>` — the single line a drawer prints instead of its contents.
fn unavailable(what: &str, error: &anyhow::Error) -> String {
    format!("{what} — service unavailable: {error:#}")
}

async fn render_drawer(drawer: Drawer, limit: u32) -> String {
    // The Network drawer and the Window Shelf need no bus at all: one has no service to reach,
    // and the other's producer (the private `lege_shell_v1` protocol) does not exist yet.
    match drawer {
        Drawer::Network => return drawers::network::render(),
        Drawer::WindowShelf => return render_window_shelf(),
        _ => {}
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
        Drawer::Network | Drawer::WindowShelf => unreachable!(),
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

/// The Window Shelf's producer does not exist yet (see [`drawers::window_shelf`]), so there is
/// no vector of toplevels to render — say that rather than printing an empty shelf, which would
/// wrongly read as "nothing is minimised".
fn render_window_shelf() -> String {
    let none: [ShellToplevel; 0] = [];
    format!(
        "{}\n(the compositor's private lege_shell_v1 protocol has no client in this milestone, \
         so no window list is available yet)",
        drawers::window_shelf::render(&none)
    )
}

/// The dock, grouped by whatever `shell.dock.grouping` says. Same caveat as the Window Shelf:
/// the toplevel list has no producer yet, so this renders the empty dock and says why.
async fn render_dock() -> String {
    let policy = match session().await {
        Ok(connection) => match SettingsProxy::new(&connection).await {
            Ok(proxy) => settings::dock_grouping(&proxy).await,
            Err(_) => lege_window::DockGrouping::ByApplication,
        },
        Err(_) => lege_window::DockGrouping::ByApplication,
    };
    let none: [ShellToplevel; 0] = [];
    format!(
        "{}\ngrouping: {policy:?}\n(the compositor's private lege_shell_v1 protocol has no client \
         in this milestone, so no window list is available yet)",
        dock::render(&none, policy)
    )
}

/// What each pressure level means, for `--print pressure`.
fn render_pressure_glossary() -> String {
    ["normal", "elevated", "severe", "critical"]
        .into_iter()
        .map(|level| format!("{level:<9} {}", pressure::describe_level(level)))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_drawer_has_a_unique_print_slug() {
        let slugs: Vec<&str> = Drawer::DEFAULT.iter().map(|d| slug(*d)).collect();
        let mut unique = slugs.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            slugs.len(),
            unique.len(),
            "duplicate --print slug: {slugs:?}"
        );
    }

    #[test]
    fn every_slug_round_trips_back_to_its_drawer() {
        for drawer in Drawer::DEFAULT {
            assert_eq!(drawer_from_slug(slug(drawer)), Some(drawer));
        }
    }

    #[test]
    fn the_window_shelf_answers_to_its_enum_name_as_well_as_its_display_name() {
        assert_eq!(drawer_from_slug("windows"), Some(Drawer::WindowShelf));
        assert_eq!(drawer_from_slug("window-shelf"), Some(Drawer::WindowShelf));
        assert_eq!(drawer_from_slug("shelf"), Some(Drawer::WindowShelf));
    }

    #[test]
    fn slugs_are_case_insensitive_and_tolerate_surrounding_space() {
        assert_eq!(drawer_from_slug("  Drives "), Some(Drawer::Drives));
        assert_eq!(drawer_from_slug("RECENT"), Some(Drawer::Recent));
    }

    #[test]
    fn an_unknown_slug_is_rejected() {
        assert_eq!(drawer_from_slug("wallpaper"), None);
    }

    #[test]
    fn print_targets_covers_every_drawer_plus_the_two_extras() {
        let targets = print_targets();
        assert_eq!(targets.len(), Drawer::DEFAULT.len() + 2);
        assert!(targets.contains(&"dock".to_owned()));
        assert!(targets.contains(&"pressure".to_owned()));
    }

    #[tokio::test]
    async fn an_unknown_print_target_names_the_valid_ones() {
        let error = print_target("wallpaper", 10).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("wallpaper"));
        assert!(message.contains("drives"), "{message}");
        assert!(message.contains("pressure"), "{message}");
    }

    #[tokio::test]
    async fn the_two_bus_free_drawers_render_without_any_session_bus() {
        // Network has no service to reach, and the Window Shelf's producer does not exist yet;
        // both must still render, since this test runs with no session bus at all.
        let network = print_target("network", 10).await.unwrap();
        assert!(network.contains("not wired to a service yet"));

        let shelf = print_target("windows", 10).await.unwrap();
        assert!(shelf.contains("nothing shelved"));
        assert!(shelf.contains("lege_shell_v1"));
    }

    #[tokio::test]
    async fn the_pressure_glossary_needs_no_bus_and_covers_every_level() {
        let text = print_target("pressure", 10).await.unwrap();
        for level in ["normal", "elevated", "severe", "critical"] {
            assert!(text.contains(level), "{level} missing from the glossary");
        }
    }

    #[tokio::test]
    async fn a_service_backed_drawer_degrades_to_one_readable_line_with_no_bus() {
        // These tests run with no session bus. Every one of these drawers must therefore produce
        // a sentence rather than panicking, hanging, or printing a zbus debug dump.
        for target in ["recent", "drives", "operations", "trash", "projects"] {
            let text = print_target(target, 5).await.unwrap();
            assert!(
                text.contains("unavailable"),
                "{target} did not degrade readably: {text}"
            );
            assert_eq!(
                text.lines().count(),
                1,
                "{target} printed more than one line"
            );
        }
    }
}
