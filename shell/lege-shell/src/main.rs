//! `lege-shell` — the restartable Home surface and its headless views.
//!
//! The Home Surface is a **model over the services** (`lege_shell::drawers`) with two renderers
//! over it: a text one (`--print <drawer>`,
//! `--list-drawers`) and a graphical one (`--surface`, `lege_shell::surface`). Every drawer is a
//! live view of an `org.legeos.*` interface rather than state the shell keeps for itself (planning
//! doc 01 §1), and the pressure overlay's wording is a pure function of one `Resources1` signal.
//!
//! The text renderer is not a fallback the graphical one obsoletes: `--print`/`--list-drawers`
//! stay, unconditionally, as `docs/architecture/accessibility.md`'s "everything the shell shows is
//! available through `lege` too" for the shell's own surfaces, and as what this crate's tests and
//! `tests/integration/run.sh` exercise with no compositor, no session bus and no display at all.
//!
//! A drawer whose service is not running prints one clear line saying so. The shell is the last
//! thing that should panic when a daemon is missing.

use anyhow::{Context, Result};
use clap::Parser;
use lege_window::ShellToplevel;
use std::path::PathBuf;

use lege_shell::{data, drawers, home_surface::Drawer, pressure};

/// LegeOS shell.
#[derive(Debug, Parser)]
#[command(name = "lege-shell", version, about)]
struct Cli {
    /// List the Home Surface drawers and exit.
    #[arg(long)]
    list_drawers: bool,

    /// Render one drawer as text and exit. Also accepts `pressure` and `field` (the
    /// Home Field: one line per tile).
    #[arg(long, value_name = "DRAWER")]
    print: Option<String>,

    /// How many entries to show in the drawers that are lists.
    #[arg(long, default_value_t = 20)]
    limit: u32,

    /// Draw the Home Field to a PNG and exit — no compositor, no display, nothing on screen.
    ///
    /// The visual counterpart to `--print field`: between them, no question about how Home looks
    /// requires ending someone's session to answer. Windows cannot appear (there is no compositor
    /// to have any), but every service-backed part is live if its service is answering.
    #[arg(long, value_name = "PATH")]
    render: Option<PathBuf>,

    /// Size for `--render`, as WIDTHxHEIGHT in logical pixels.
    #[arg(long, value_name = "WxH", default_value = "1600x1000")]
    size: String,

    /// Open the graphical Home Surface. Needs a Wayland compositor; for the private
    /// `lege_shell_v1` protocol (live Window Shelf, Home reveal and layer placement)
    /// `WAYLAND_DISPLAY` must name the compositor's trusted-shell socket
    /// (`compositor/lege-comp/src/shell_socket.rs`'s `<main socket>-shell`), not the ordinary one
    /// — without that, the surface still opens and still shows every D-Bus-backed drawer, just
    /// with an empty Window Shelf and no live window actions, the same graceful degradation every
    /// other drawer already has for its own service being absent.
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

    if let Some(path) = &cli.render {
        let (width, height) = parse_size(&cli.size)?;
        lege_shell::surface::offscreen::render_field(path, width, height).await?;
        println!("{}", path.display());
        return Ok(());
    }
    if let Some(target) = &cli.print {
        let size = parse_size(&cli.size)?;
        println!("{}", print_target(target, cli.limit, size).await?);
        return Ok(());
    }

    if cli.surface {
        tracing::info!(
            wayland_display = ?std::env::var_os("WAYLAND_DISPLAY"),
            "starting graphical shell; connecting to Wayland"
        );
        return lege_shell::surface::run();
    }

    tracing::info!(
        "lege-shell: pass --surface for the graphical Home Surface, or --print \
         <drawer>/--list-drawers for the text form"
    );
    Ok(())
}

/// `WIDTHxHEIGHT`, the one format `--size` accepts.
fn parse_size(text: &str) -> Result<(u32, u32)> {
    let (width, height) = text
        .split_once(['x', 'X'])
        .with_context(|| format!("--size wants WIDTHxHEIGHT, got {text:?}"))?;
    Ok((
        width
            .trim()
            .parse()
            .with_context(|| format!("width in {text:?}"))?,
        height
            .trim()
            .parse()
            .with_context(|| format!("height in {text:?}"))?,
    ))
}

/// Every value `--print` accepts, for the error message when one is misspelled.
fn print_targets() -> Vec<String> {
    let mut targets: Vec<String> = Drawer::DEFAULT
        .iter()
        .map(|drawer| slug(*drawer).to_owned())
        .collect();
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
async fn print_target(target: &str, limit: u32, size: (u32, u32)) -> Result<String> {
    if target.eq_ignore_ascii_case("pressure") {
        return Ok(render_pressure_glossary());
    }
    if target.eq_ignore_ascii_case("field") {
        return Ok(render_field(size).await);
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

/// The Home Field as text: the same document the graphical surface draws, merged from the
/// installed applications (no windows — this command has no `lege_shell_v1` connection, see
/// `render_window_shelf`) on a default 16×8 grid, one line per tile.
/// The Home Field as text, at the grid `--size` implies.
///
/// The *persisted* field, the same one `--render` draws and the session shows, not a fresh one:
/// this built `FieldDocument::new` and merged into it, so it always described the arrangement a
/// brand-new field would have and never the one on screen. That made it actively misleading as a
/// diagnostic — the two commands are meant to be the text and picture of one field, and they
/// silently described different ones whenever a layout had ever been saved.
async fn render_field(size: (u32, u32)) -> String {
    let (width, height) = size;
    let (cols, rows) = lege_shell::field_model::grid_for(
        width as i32,
        height as i32,
        lege_shell::field_model::TILE_SIZE,
    );
    let persisted = data::fetch_field("primary", cols, rows).await;
    let stored = persisted.is_some();
    let mut doc =
        persisted.unwrap_or_else(|| lege_field::FieldDocument::new("primary", cols, rows));
    lege_shell::field_model::merge(
        &mut doc,
        &lege_shell::field_model::LiveState {
            toplevels: &[],
            projects: &data::fetch_projects().await,
        },
    );
    lege_shell::field_model::merge_desktop_folder(
        &mut doc,
        lege_shell::drawers::standard_paths::desktop_dir().as_deref(),
    );
    let source = if stored {
        "the saved layout"
    } else {
        "a fresh field: no layout is saved, or the session service is not answering"
    };
    format!(
        "{}({source}; this text command has no lege_shell_v1 connection, so windows appear only \
         in `lege-shell --surface`)",
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
    fn print_targets_covers_every_drawer_plus_the_two_extras() {
        let targets = print_targets();
        assert_eq!(targets.len(), Drawer::DEFAULT.len() + 2);
        assert!(!targets.contains(&"dock".to_owned()));
        assert!(targets.contains(&"pressure".to_owned()));
        assert!(targets.contains(&"field".to_owned()));
    }

    /// The default field, in text, with no bus: one Files object where eight drawer tiles used
    /// to be, the system shelf beside it, and no drawer tile left anywhere.
    #[tokio::test]
    async fn print_field_renders_the_files_object_without_a_bus() {
        let text = print_target("field", 10, (1920, 1080)).await.unwrap();
        assert!(text.starts_with("field primary  18x10"), "{text}");
        assert!(text.contains("Files"), "{text}");
        for drawer in lege_shell::home_surface::Drawer::DEFAULT {
            assert!(
                !text.contains(&format!("drawer:{}", drawer.name().to_ascii_lowercase())),
                "{} is still a tile in {text}",
                drawer.name()
            );
        }
        assert!(text.contains("lege_shell_v1"));
    }

    #[tokio::test]
    async fn an_unknown_print_target_names_the_valid_ones() {
        let error = print_target("wallpaper", 10, (1920, 1080))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("wallpaper"));
        assert!(message.contains("drives"), "{message}");
        assert!(message.contains("pressure"), "{message}");
    }

    #[tokio::test]
    async fn the_two_bus_free_drawers_render_without_any_session_bus() {
        // Network has no service to reach, and the Window Shelf's producer does not exist yet;
        // both must still render, since this test runs with no session bus at all.
        let network = print_target("network", 10, (1920, 1080)).await.unwrap();
        assert!(network.contains("not wired to a service yet"));

        let shelf = print_target("windows", 10, (1920, 1080)).await.unwrap();
        assert!(shelf.contains("nothing shelved"));
        assert!(shelf.contains("lege_shell_v1"));
    }

    #[tokio::test]
    async fn the_pressure_glossary_needs_no_bus_and_covers_every_level() {
        let text = print_target("pressure", 10, (1920, 1080)).await.unwrap();
        for level in ["normal", "elevated", "severe", "critical"] {
            assert!(text.contains(level), "{level} missing from the glossary");
        }
    }

    #[tokio::test]
    async fn a_service_backed_drawer_degrades_to_one_readable_line_with_no_bus() {
        // These tests describe the no-service path, which can only be observed when no service is
        // answering. A developer running the session — installed, or nested via
        // `.agent/scratch/nested-session.sh` — has a live `org.legeos.Session` and gets real
        // content here, which is the service working rather than a regression. See
        // `crate::data`'s `a_session_daemon_is_answering` for the same guard.
        if lege_shell::data::a_session_daemon_is_answering().await {
            eprintln!(
                "skipped: org.legeos.Session is answering on this bus, so the no-service path \
                 cannot be observed here"
            );
            return;
        }
        // Every one of these drawers must produce a sentence rather than panicking, hanging, or
        // printing a zbus debug dump.
        for target in ["recent", "drives", "operations", "trash", "projects"] {
            let text = print_target(target, 5, (1920, 1080)).await.unwrap();
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
