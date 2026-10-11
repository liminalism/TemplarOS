//! `templar-shell` — the restartable Home surface and its headless views.
//!
//! The Home Surface is a **model over the services** (`templar_shell::drawers`) with two renderers
//! over it: a text one (`--print <drawer>`,
//! `--list-drawers`) and a graphical one (`--surface`, `templar_shell::surface`). Every drawer is a
//! live view of an `org.templaros.*` interface rather than state the shell keeps for itself (planning
//! doc 01 §1), and the pressure overlay's wording is a pure function of one `Resources1` signal.
//!
//! The text renderer is not a fallback the graphical one obsoletes: `--print`/`--list-drawers`
//! stay, unconditionally, as `docs/architecture/accessibility.md`'s "everything the shell shows is
//! available through `templar` too" for the shell's own surfaces, and as what this crate's tests and
//! `tests/integration/run.sh` exercise with no compositor, no session bus and no display at all.
//!
//! A drawer whose service is not running prints one clear line saying so. The shell is the last
//! thing that should panic when a daemon is missing.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use templar_window::ShellToplevel;

use templar_shell::{data, drawers, home_surface::Drawer, pressure};

/// TemplarOS shell.
#[derive(Debug, Parser)]
#[command(name = "templar-shell", version, about)]
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

    /// Draw the bottom bar to a PNG at scale 1 and scale 2 and exit — the same paint the session
    /// runs, in a surface the size the host asks for. Writes PATH and PATH with `@2x` before the
    /// extension.
    #[arg(long, value_name = "PATH")]
    render_bar: Option<PathBuf>,

    /// Draw Apps and Files launcher fixtures at desktop, narrow and high DPI sizes.
    #[arg(long, value_name = "PREFIX")]
    render_launcher: Option<PathBuf>,

    /// Draw both gesture rails, half and fully revealed, to PNGs with the mask as alpha and exit:
    /// `PATH-apps-half.png`, `PATH-apps-full.png`, `PATH-system-half.png`,
    /// `PATH-system-full.png`, on a 1920-pixel output at scale 1.
    #[arg(long, value_name = "PATH")]
    render_rails: Option<PathBuf>,

    /// Exercise and render the native clock/calendar popup without a display or session bus.
    /// Writes PREFIX-{corner,narrow,fractional,hidpi,rail-right,rail-left}-{open,next,today,closed}.png.
    #[arg(long, value_name = "PREFIX")]
    render_calendar: Option<PathBuf>,
    /// Exercise and render active Home work at 1x, 2x and narrow size, including wallpaper alpha.
    /// Writes PREFIX-{wide,hidpi,narrow}-{open,parked,closed,wallpaper-revealed,
    /// wallpaper-occluded,wallpaper-disabled}.png; checks covered pixels do not leak Home previews.
    #[arg(long, value_name = "PREFIX")]
    render_home_work: Option<PathBuf>,

    /// Render compositor-ordered switcher fixtures, including refreshed captures and overflow.
    #[arg(long, value_name = "PREFIX")]
    render_switcher: Option<PathBuf>,

    /// Draw every Place theme over a fixed set of synthetic places (a code project, a photo
    /// folder, a manuscript, a sparse new place) at the Wide and Dominant sizes, as
    /// `DIR/<theme>-<place>-<size>.png` plus a contact sheet `DIR/gallery.png`, and exit.
    /// Headless and bus-free, on the light palette.
    #[arg(long, value_name = "DIR")]
    garden_gallery: Option<PathBuf>,

    /// Size for `--render`, as WIDTHxHEIGHT in logical pixels.
    #[arg(long, value_name = "WxH", default_value = "1600x1000")]
    size: String,

    /// Capture the whole screen to a PNG and exit.
    ///
    /// The only way to see this desktop from outside it: the compositor's capture globals are
    /// visible on the trusted-shell socket alone, so `grim`, `xdotool` and every other screenshot
    /// tool cannot reach them. Opens its own short-lived connection and does not disturb the
    /// running shell.
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,

    /// Open the graphical Home Surface. Needs a Wayland compositor; for the private
    /// `templar_shell_v2` protocol (live Window Shelf, Home reveal and layer placement)
    /// `WAYLAND_DISPLAY` must name the compositor's trusted-shell socket
    /// (`compositor/templar-comp/src/shell_socket.rs`'s `templaros-shell-<main socket>`), not the ordinary one
    /// — without that, the surface still opens and still shows every D-Bus-backed drawer, just
    /// with an empty Window Shelf and no live window actions, the same graceful degradation every
    /// other drawer already has for its own service being absent.
    #[arg(long)]
    surface: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();

    if cli.list_drawers {
        for drawer in Drawer::DEFAULT {
            println!("{:<12} {}", drawer.name(), drawer.description());
        }
        return Ok(());
    }

    if let Some(dir) = &cli.garden_gallery {
        for written in templar_shell::surface::offscreen::render_garden_gallery(dir)? {
            println!("{}", written.display());
        }
        return Ok(());
    }

    if let Some(path) = &cli.render_bar {
        templar_shell::surface::design::set_palette(
            templar_shell::settings::load_appearance().await,
        );
        let at_2x = path.with_extension("");
        let at_2x = PathBuf::from(format!(
            "{}@2x.{}",
            at_2x.display(),
            path.extension().unwrap_or_default().to_string_lossy()
        ));
        templar_shell::surface::offscreen::render_bar(path, 1.0)?;
        templar_shell::surface::offscreen::render_bar(&at_2x, 2.0)?;
        println!("{}\n{}", path.display(), at_2x.display());
        return Ok(());
    }

    if let Some(path) = &cli.render_rails {
        templar_shell::surface::design::set_palette(
            templar_shell::settings::load_appearance().await,
        );
        for written in templar_shell::surface::offscreen::render_rails(path)? {
            println!("{}", written.display());
        }
        return Ok(());
    }
    if let Some(path) = &cli.render_launcher {
        for written in templar_shell::surface::offscreen::render_launcher(path)? {
            println!("{}", written.display());
        }
        return Ok(());
    }
    if let Some(path) = &cli.render_calendar {
        for written in templar_shell::surface::offscreen::render_calendar(path)? {
            println!("{}", written.display());
        }
        println!("calendar smoke passed: click open, month navigation, Today, Escape, geometry");
        return Ok(());
    }
    if let Some(path) = &cli.render_home_work {
        for written in templar_shell::surface::offscreen::render_home_work(path)? {
            println!("{}", written.display());
        }
        println!(
            "Home work smoke passed: contained project windows and one Loose tray, single-click \
             activation, parking, overflow, capture gating, project templates and tabs"
        );
        return Ok(());
    }

    if let Some(path) = &cli.render_switcher {
        templar_shell::surface::offscreen::render_switcher(path)?;
        println!("{}", path.display());
        return Ok(());
    }

    if let Some(path) = &cli.render {
        let (width, height) = parse_size(&cli.size)?;
        templar_shell::surface::design::set_palette(
            templar_shell::settings::load_appearance().await,
        );
        templar_shell::surface::offscreen::render_field(path, width, height).await?;
        println!("{}", path.display());
        return Ok(());
    }
    if let Some(path) = &cli.screenshot {
        templar_shell::screenshot::capture(path)?;
        println!("{}", path.display());
        return Ok(());
    }
    if let Some(target) = &cli.print {
        let size = parse_size(&cli.size)?;
        println!("{}", print_target(target, cli.limit, size).await?);
        return Ok(());
    }

    if cli.surface {
        // Say which build is running, for `templar dev status`. Diagnostic only: a stamp that cannot
        // be written must never keep the shell from starting.
        if let Err(error) = templar_build::write_stamp("templar-shell") {
            tracing::warn!(%error, "could not write the build stamp");
        }
        tracing::info!(
            wayland_display = ?std::env::var_os("WAYLAND_DISPLAY"),
            "starting graphical shell; connecting to Wayland"
        );
        // The palette is chosen here, before any surface exists, because every painter reads it
        // and a frame drawn in one palette and finished in another is worse than either.
        templar_shell::surface::design::set_palette(
            templar_shell::settings::load_appearance().await,
        );
        return templar_shell::surface::run();
    }

    tracing::info!(
        "templar-shell: pass --surface for the graphical Home Surface, or --print \
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
    // The Window Shelf needs no bus at all: its producer is the private `templar_shell_v2`
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
/// minimised". The graphical surface (`templar-shell --surface`) renders this same drawer against a
/// live `templar_shell_v2` connection instead (`surface::home_screen`).
fn render_window_shelf() -> String {
    let none: [ShellToplevel; 0] = [];
    format!(
        "{}\n(this text command has no templar_shell_v2 connection; run `templar-shell --surface` for \
         a live window list)",
        drawers::window_shelf::render(&none)
    )
}

/// The Home Field as text: the same document the graphical surface draws, merged from the
/// installed applications (no windows — this command has no `templar_shell_v2` connection, see
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
    let (cols, rows) = templar_shell::field_model::grid_for(
        width as i32,
        height as i32,
        templar_shell::field_model::TILE_SIZE,
    );
    let persisted = data::fetch_field("primary", cols, rows).await;
    let stored = persisted.is_some();
    let mut doc =
        persisted.unwrap_or_else(|| templar_field::FieldDocument::new("primary", cols, rows));
    templar_shell::field_model::merge(
        &mut doc,
        &templar_shell::field_model::LiveState {
            toplevels: &[],
            projects: &data::fetch_projects().await,
        },
    );
    let source = if stored {
        "the saved layout"
    } else {
        "a fresh field: no layout is saved, or the session service is not answering"
    };
    format!(
        "{}({source}; this text command has no templar_shell_v2 connection, so windows appear only \
         in `templar-shell --surface`)",
        templar_field::text::render(&doc)
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
}
