//! `lege-shell` — everything decorative, restartable without terminating applications.
//!
//! The Home Surface and the dock are a **model over the services** (`lege_shell::drawers`,
//! `lege_shell::dock`) with two renderers over it: a text one (`--print <drawer>`,
//! `--list-drawers`) and a graphical one (`--surface`, `lege_shell::surface`). Every drawer is a
//! live view of an `org.legeos.*` interface rather than state the shell keeps for itself (planning
//! doc 01 §1), the dock groups by canonical `AppId` through `lege_window::shell` so it needs no
//! local `WM_CLASS` heuristics, and the pressure overlay's wording is a pure function of one
//! `Resources1` signal.
//!
//! The text renderer is not a fallback the graphical one obsoletes: `--print`/`--list-drawers`
//! stay, unconditionally, as `docs/architecture/accessibility.md`'s "everything the shell shows is
//! available through `lege` too" for the shell's own surfaces, and as what this crate's tests and
//! `tests/integration/run.sh` exercise with no compositor, no session bus and no display at all.
//!
//! A drawer whose service is not running prints one clear line saying so. The shell is the last
//! thing that should panic when a daemon is missing.

use anyhow::Result;
use clap::Parser;
use lege_client::proxies::SettingsProxy;
use lege_window::ShellToplevel;

use lege_shell::{data, dock, drawers, home_surface::Drawer, pressure, settings};

/// LegeOS shell.
#[derive(Debug, Parser)]
#[command(name = "lege-shell", version, about)]
struct Cli {
    /// List the Home Surface drawers and exit.
    #[arg(long)]
    list_drawers: bool,

    /// Render one drawer as text and exit. Also accepts `dock`, `pressure` and `field` (the
    /// Home Field: one line per tile).
    #[arg(long, value_name = "DRAWER")]
    print: Option<String>,

    /// How many entries to show in the drawers that are lists.
    #[arg(long, default_value_t = 20)]
    limit: u32,

    /// Open the graphical Home Surface and dock. Needs a Wayland compositor; for the private
    /// `lege_shell_v1` protocol (live Window Shelf and dock contents, layer/exclusive-zone
    /// placement) `WAYLAND_DISPLAY` must name the compositor's trusted-shell socket
    /// (`compositor/lege-comp/src/shell_socket.rs`'s `<main socket>-shell`), not the ordinary one
    /// — without that, the surface still opens and still shows every D-Bus-backed drawer, just
    /// with an empty dock and Window Shelf, the same graceful degradation every other drawer
    /// already has for its own service being absent.
    #[arg(long)]
    surface: bool,
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

    if cli.surface {
        return lege_shell::surface::run();
    }

    tracing::info!(
        "lege-shell: pass --surface for the graphical Home Surface and dock, or --print \
         <drawer>/--list-drawers for the text form"
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
    targets.push("field".to_owned());
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
    if target.eq_ignore_ascii_case("field") {
        return Ok(render_field().await);
    }

    let Some(drawer) = drawer_from_slug(target) else {
        anyhow::bail!(
            "unknown drawer {target:?}; expected one of {}",
            print_targets().join(", ")
        );
    };
    Ok(render_drawer(drawer, limit).await)
}

async fn render_drawer(drawer: Drawer, limit: u32) -> String {
    // The Window Shelf needs no bus at all: its producer is the private `lege_shell_v1`
    // protocol, which this text-only binary has no connection to (see `render_window_shelf`).
    // Every other drawer, including Network, is `data::render_drawer`'s job — shared verbatim
    // with the graphical surface's own background refresh (`surface::data`).
    if drawer == Drawer::WindowShelf {
        return render_window_shelf();
    }
    data::render_drawer(drawer, limit).await
}

/// This text-only command opens no Wayland connection at all
/// (`docs/architecture/accessibility.md`'s headless path), so it has no toplevel list to show —
/// say that rather than printing an empty shelf, which would wrongly read as "nothing is
/// minimised". The graphical surface (`lege-shell --surface`) renders this same drawer against a
/// live `lege_shell_v1` connection instead (`surface::home_screen`).
fn render_window_shelf() -> String {
    let none: [ShellToplevel; 0] = [];
    format!(
        "{}\n(this text command has no lege_shell_v1 connection; run `lege-shell --surface` for \
         a live window list)",
        drawers::window_shelf::render(&none)
    )
}

/// The dock, grouped by whatever `shell.dock.grouping` says. Same caveat as the Window Shelf:
/// no Wayland connection here means no toplevel list, so this renders the empty dock and says why.
async fn render_dock() -> String {
    let policy = match data::session().await {
        Ok(connection) => match SettingsProxy::new(&connection).await {
            Ok(proxy) => settings::dock_grouping(&proxy).await,
            Err(_) => lege_window::DockGrouping::ByApplication,
        },
        Err(_) => lege_window::DockGrouping::ByApplication,
    };
    let none: [ShellToplevel; 0] = [];
    format!(
        "{}\ngrouping: {policy:?}\n(this text command has no lege_shell_v1 connection; run \
         `lege-shell --surface` for a live dock)",
        dock::render(&none, policy)
    )
}

/// The Home Field as text: the same document the graphical surface draws, merged from the
/// installed applications (no windows — this command has no `lege_shell_v1` connection, see
/// `render_window_shelf`) on a default 16×8 grid, one line per tile.
async fn render_field() -> String {
    let apps = data::fetch_apps().await;
    let (cols, rows) =
        lege_shell::field_model::grid_for(1920, 1080, lege_shell::field_model::TILE_SIZE);
    let mut doc = lege_field::FieldDocument::new("primary", cols, rows);
    lege_shell::field_model::merge(
        &mut doc,
        &lege_shell::field_model::LiveState {
            apps: &apps,
            toplevels: &[],
            projects: &data::fetch_projects().await,
        },
    );
    format!(
        "{}(this text command has no lege_shell_v1 connection; windows appear only in \
         `lege-shell --surface`)",
        lege_field::text::render(&doc)
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
    fn print_targets_covers_every_drawer_plus_the_three_extras() {
        let targets = print_targets();
        assert_eq!(targets.len(), Drawer::DEFAULT.len() + 3);
        assert!(targets.contains(&"dock".to_owned()));
        assert!(targets.contains(&"pressure".to_owned()));
        assert!(targets.contains(&"field".to_owned()));
    }

    #[tokio::test]
    async fn print_field_renders_the_drawer_tiles_without_a_bus() {
        let text = print_target("field", 10).await.unwrap();
        assert!(text.starts_with("field primary  16x9"), "{text}");
        for drawer in lege_shell::field_model::DRAWER_TILES {
            assert!(
                text.contains(drawer.name()),
                "{} missing from {text}",
                drawer.name()
            );
        }
        assert!(text.contains("lege_shell_v1"));
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
