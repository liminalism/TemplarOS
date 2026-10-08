use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "templar-files",
    about = "TemplarOS service-backed file manager"
)]
struct Cli {
    /// Folder to open. Defaults to the service-provided Home place.
    path: Option<PathBuf>,

    /// Print the service-backed roots and directory listing without opening a window.
    #[arg(long)]
    print: bool,

    /// Include dotfiles in the initial listing.
    #[arg(long)]
    show_hidden: bool,
}

fn main() -> Result<()> {
    use std::io::IsTerminal as _;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "templar_files=info".into()),
        )
        .compact()
        // No ANSI under the journal: journald stores coloured MESSAGE fields as byte arrays.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
    let cli = Cli::parse();
    if cli.print {
        templar_files::run_print(cli.path.as_deref(), cli.show_hidden)
    } else {
        templar_files::run_graphical(cli.path, cli.show_hidden)
    }
}
