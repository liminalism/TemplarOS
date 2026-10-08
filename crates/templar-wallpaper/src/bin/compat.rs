//! Standalone compatibility command; native sessions use Wallpaper1 instead.
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::io::Read;
use std::path::PathBuf;
use templar_types::wallpaper::{SizingMode, TextureLimit, WallpaperConfig};
use templar_wallpaper::compat::{self, Request};
use templar_wallpaper::selection::Direction;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend { X11, LayerShell, Portal }
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Sizing { Fill, Fit, Stretch, Center, Actual, Tile }
impl From<Sizing> for SizingMode {
    fn from(value: Sizing) -> Self { match value {
        Sizing::Fill => Self::Fill, Sizing::Fit => Self::Fit, Sizing::Stretch => Self::Stretch,
        Sizing::Center => Self::Center, Sizing::Actual => Self::Actual, Sizing::Tile => Self::Tile,
    } }
}
#[derive(Debug, Parser)]
#[command(name = "templar-wallpaper", about = "Pure Rust compatibility wallpaper adapters", long_about = "Pure Rust X11, layer-shell and wallpaper-portal adapters. Native TemplarOS uses its existing session/compositor wallpaper owner instead.\n\nX11 and portal commands exit after one change; schedule them ONLY from a session-scoped external owner (for example, a user unit with PartOf=graphical-session.target, never an independent persistent user timer). XWayland root publication is rejected. Layer-shell must stay resident; its optional rotation deadline dies with the compositor connection. Layer-shell stdin accepts next, previous, pause, resume, refresh, inspect and stop (one line each).\n\nJSON configuration is the canonical templar_types::wallpaper::WallpaperConfig, including lossless Unix source filename byte arrays. Every result is JSON. Portal success is owner acceptance, not verified presentation; exact sizing and residency controls cannot be delegated.")]
struct Cli {
    #[arg(long, value_enum)]
    backend: Backend,
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    /// Probe the actual selected backend and advertise supported controls.
    Inspect,
    /// Select/apply a source. Layer-shell remains resident until disconnect/stop.
    Set(Apply),
    /// Apply the next source; --previous-source provides one-shot selection history.
    Next(Apply),
    /// Apply the previous source; --previous-source provides one-shot history.
    Previous(Apply),
}
#[derive(Debug, Args)]
struct Apply {
    /// Canonical WallpaperConfig JSON file (not a second settings schema).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Explicit local source, opened once and passed by FD to the decoder.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Previous successful source for one-shot sequence/shuffle navigation.
    #[arg(long)]
    previous_source: Option<PathBuf>,
    /// Layer-shell output name, or X11 screen-N. Portal has no output selector.
    #[arg(long)]
    output: Option<String>,
    #[arg(long, value_enum)]
    sizing: Option<Sizing>,
    /// Explicit smaller texture/canvas ceiling; no silent lower-resolution fallback.
    #[arg(long)]
    texture_max_edge: Option<u32>,
    /// 300..=3600; layer-shell only. One-shot backends require external scheduling.
    #[arg(long)]
    interval_seconds: Option<u64>,
    /// Decoder executable; defaults to the installed sibling or PATH name.
    #[arg(long)]
    decoder: Option<PathBuf>,
}
fn request(args: Apply, direction: Direction, backend: Backend) -> Result<Request> {
    let mut config = if let Some(path) = args.config {
        let mut bytes = Vec::new();
        std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?
            .take(131073).read_to_end(&mut bytes)?;
        if bytes.len() > 131072 { bail!("configuration exceeds 128 KiB"); }
        serde_json::from_slice::<WallpaperConfig>(&bytes).context("invalid canonical WallpaperConfig JSON")?
    } else { WallpaperConfig::default() };
    if let Some(sizing) = args.sizing { config.sizing = sizing.into(); }
    if let Some(pixels) = args.texture_max_edge { config.texture = TextureLimit::MaxEdge { pixels }; }
    if let Some(seconds) = args.interval_seconds { config.interval_seconds = Some(seconds); }
    if let Some(source) = &args.input {
        if config.sources.is_empty() { config.sources.push(compat::path_bytes(source)); }
    }
    if !matches!(backend, Backend::LayerShell) && config.interval_seconds.is_some() {
        bail!("one-shot X11/portal does not own rotation; disable interval_seconds and schedule externally within the graphical session");
    }
    if matches!(backend, Backend::Portal) && (args.sizing.is_some() || args.texture_max_edge.is_some()) {
        bail!("portal exposes no exact sizing or texture/residency ceiling; use the desktop owner's settings");
    }
    templar_wallpaper::config::validate(&config)?;
    let decoder = args.decoder.unwrap_or_else(|| {
        std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join("templar-wallpaper-decode")))
            .filter(|path| path.is_file()).unwrap_or_else(|| PathBuf::from("templar-wallpaper-decode"))
    });
    Ok(Request { config, input: args.input, previous: args.previous_source, direction, decoder, output: args.output })
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Inspect) {
        let inspected = match cli.backend {
            Backend::X11 => compat::x11::inspect()?,
            Backend::LayerShell => compat::layer_shell::inspect()?,
            Backend::Portal => compat::portal::inspect().await?,
        };
        return compat::print_json(&inspected);
    }
    let (args, direction) = match cli.command {
        Command::Set(args) | Command::Next(args) => (args, Direction::Next),
        Command::Previous(args) => (args, Direction::Previous),
        Command::Inspect => unreachable!("handled above"),
    };
    let request = request(args, direction, cli.backend)?;
    match cli.backend {
        Backend::X11 => compat::print_json(&compat::x11::apply(&request)?),
        Backend::LayerShell => compat::layer_shell::run(&request),
        Backend::Portal => compat::print_json(&compat::portal::apply(&request).await?),
    }
}
