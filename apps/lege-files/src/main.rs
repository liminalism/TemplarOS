use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "lege-files", about = "LegeOS service-backed file manager")]
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lege_files=info".into()),
        )
        .compact()
        .init();
    let cli = Cli::parse();
    if cli.print {
        lege_files::run_print(cli.path.as_deref(), cli.show_hidden)
    } else {
        lege_files::run_graphical(cli.path, cli.show_hidden)
    }
}
