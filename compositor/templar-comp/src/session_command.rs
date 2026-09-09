//! `--exec`: run one client under this compositor and exit when it exits.
//!
//! This is what a greeter needs, and it exists so that nothing has to guess the compositor's
//! socket name. The alternative — starting `templar-comp`, scraping its log output for the line that
//! announces the socket, and re-execing yourself with `WAYLAND_DISPLAY` set — makes a
//! human-readable log line into an API. It breaks the moment the message is reworded, and it
//! breaks silently and completely if `RUST_LOG` is set to anything that filters that line out,
//! because then the socket is never announced at all. For a greeter that means no graphical
//! login, caused by an environment variable.
//!
//! Here the compositor puts `WAYLAND_DISPLAY` directly into the child's environment, which needs
//! no parsing, no timeout and no re-exec, and cannot race: the socket is already listening before
//! the child is spawned. `Command::env` sets it for the child alone, so this stays safe code —
//! `std::env::set_var` is `unsafe` in Rust 2024 and would be wrong here anyway, since it would
//! mutate the compositor's own environment for every thread.
//!
//! The child inherits everything else, which is how `GREETD_SOCK` reaches a greeter started this
//! way without this module knowing greetd exists.

use crate::state::{Backend, TemplarCompState};
use smithay::reexports::calloop::{LoopHandle, ping::make_ping};
use std::sync::{
    Arc,
    atomic::{AtomicI32, Ordering},
};

/// A running `--exec` child and the exit code it eventually reports.
pub struct SessionCommand {
    child: Arc<std::sync::Mutex<std::process::Child>>,
    exit_code: Arc<AtomicI32>,
}

impl SessionCommand {
    /// Spawn `argv` with `WAYLAND_DISPLAY` set to `socket_name`, and stop the event loop when it
    /// exits.
    ///
    /// `argv` must be non-empty; the first element is the program.
    ///
    /// The loop is stopped through a calloop ping rather than by calling `LoopSignal::stop` from
    /// the waiting thread, and the difference matters. `EventLoop::run` **clears the stop flag on
    /// entry** (`calloop-0.13.0/src/loop_logic.rs`: `self.signals.stop.store(false, ...)`), so a
    /// child that exits before the loop starts has its stop erased and the compositor then blocks
    /// forever with nothing to wake it. That is not a rare interleaving: it is exactly what a
    /// greeter that dies immediately on startup does — a missing font, an unreachable
    /// `GREETD_SOCK`, a panic — and the result would be a black screen with no way to log in.
    ///
    /// A ping is a pipe. Firing it before the loop starts leaves the wakeup queued, so the loop
    /// receives it on its first dispatch and stops from inside, after `run` has done its
    /// clearing. Early exit and late exit take the same path.
    pub fn spawn<B: Backend + 'static>(
        argv: &[String],
        socket_name: &str,
        loop_handle: &LoopHandle<'static, TemplarCompState<B>>,
    ) -> anyhow::Result<Self> {
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("--exec needs a command to run"))?;

        let child = std::process::Command::new(program)
            .args(arguments)
            .env("WAYLAND_DISPLAY", socket_name)
            .spawn()
            .map_err(|error| anyhow::anyhow!("spawning --exec command {program}: {error}"))?;
        tracing::info!(command = %program, socket = %socket_name, "started --exec client");

        let child = Arc::new(std::sync::Mutex::new(child));
        let exit_code = Arc::new(AtomicI32::new(0));

        let (ping, ping_source) = make_ping()
            .map_err(|error| anyhow::anyhow!("creating the --exec shutdown ping: {error}"))?;
        loop_handle
            .insert_source(ping_source, |(), (), state: &mut TemplarCompState<B>| {
                state.loop_signal.stop();
            })
            .map_err(|error| {
                anyhow::anyhow!("inserting the --exec shutdown ping source: {error}")
            })?;

        // Waiting blocks, so it happens off the render thread; `LoopSignal` is `Send` precisely so
        // another thread can end the loop.
        let waiter = Arc::clone(&child);
        let reported = Arc::clone(&exit_code);
        std::thread::spawn(move || {
            // The lock is held across `wait`, which is what makes `shut_down`'s `try_lock` fail
            // while the child is still running — see there for why that is the behaviour we want.
            let status = waiter.lock().map(|mut child| child.wait());
            match status {
                Ok(Ok(status)) => {
                    let code = status.code().unwrap_or(1);
                    tracing::info!(code, "--exec client exited; shutting the compositor down");
                    reported.store(code, Ordering::SeqCst);
                }
                Ok(Err(error)) => {
                    tracing::error!(%error, "waiting for the --exec client");
                    reported.store(1, Ordering::SeqCst);
                }
                Err(_) => reported.store(1, Ordering::SeqCst),
            }
            ping.ping();
        });

        Ok(Self { child, exit_code })
    }

    /// Kill the child if it is still running, and report the exit code to propagate.
    ///
    /// Called after the event loop ends, which happens either because the child exited (the
    /// ordinary case) or because the compositor is going down for its own reasons — a closed
    /// winit window, a fatal backend error. In that second case the child must not outlive the
    /// display server it was given, so it is killed here.
    ///
    /// `try_lock` distinguishes the two without a second flag: the waiter thread holds the lock
    /// for as long as the child is running, so acquiring it means the child has already been
    /// reaped and there is nothing to kill.
    pub fn shut_down(&self) -> i32 {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.wait();
        } else {
            // Still running: the compositor is going down first.
            if let Ok(mut child) = self.child.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.exit_code.load(Ordering::SeqCst)
    }
}
