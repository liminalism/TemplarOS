//! `lege-shell` — everything decorative, restartable without terminating applications.
//! Talks to the compositor over the private `lege_shell_v1` protocol and to sessiond over D-Bus.
//! UI toolkit: libcosmic (git dependency added when the first surface lands).

use clap::Parser;

mod home_surface;

/// LegeOS shell.
#[derive(Debug, Parser)]
#[command(name = "lege-shell", version, about)]
struct Cli {
    /// List the Home Surface drawers and exit.
    #[arg(long)]
    list_drawers: bool,
}

fn main() {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();
    if cli.list_drawers {
        for drawer in home_surface::Drawer::DEFAULT {
            println!("{:<12} {}", drawer.name(), drawer.description());
        }
        return;
    }
    tracing::info!("lege-shell skeleton: no surfaces yet (see shell/lege-shell/README.md)");
}
