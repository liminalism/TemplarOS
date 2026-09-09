//! `templar-greeter` — the login surface greetd runs (ADR-0010, ADR-0012;
//! `session/units/greetd/config.toml`).
//!
//! It connects to greetd over `GREETD_SOCK` and draws the login surface
//! (`templar_greeter::surface::run`). greetd performs the authentication; this binary links no PAM,
//! no shadow and nothing setuid.
//!
//! It expects a Wayland display to already exist, and on a bare VT the compositor provides one:
//! greetd runs `templar-comp --exec templar-greeter`, so the compositor comes up first, puts
//! `WAYLAND_DISPLAY` directly into this process's environment, and exits when this process does.
//! That last part is the mechanism, not a nicety — `greetd(5)`: "the session will start after the
//! greeter process terminates", and what must terminate is the whole `default_session.command`,
//! compositor included.
//!
//! Running it nested inside an existing session works with no change, since `WAYLAND_DISPLAY` is
//! then already set by that session.
//!
//! # What has not been verified here
//!
//! No greetd runs in this development environment and there is no seat to take over, so the
//! greetd handshake against a live PAM stack has not been exercised end to end. What was checked
//! directly: the whole `greetd-ipc(7)` client and login state machine against an in-process fake
//! greetd, session-file discovery against the real files on this machine, and — separately, in
//! `compositor/templar-comp` — that `--exec` sets `WAYLAND_DISPLAY`, inherits `GREETD_SOCK`, exits
//! when its client exits and propagates that client's exit code, including when the client exits
//! immediately. See `session/units/greetd/README.md` for what remains and how to recover.

use anyhow::Context;
use clap::Parser;

/// TemplarOS greeter: the client half of greetd's `default_session.command`.
#[derive(Debug, Parser)]
#[command(name = "templar-greeter", version, about)]
struct Cli;

fn main() -> anyhow::Result<()> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
    Cli::parse();

    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        anyhow::bail!(
            "no WAYLAND_DISPLAY or DISPLAY: templar-greeter draws into a compositor rather than \
             starting one. On a bare VT greetd should run `templar-comp --exec templar-greeter` (see \
             session/units/greetd/config.toml); nested in an existing session, the session \
             provides it."
        );
    }

    let exit_code = templar_greeter::surface::run().context("running the greeter surface")?;
    std::process::exit(exit_code);
}
