//! Opt-in workloads inherit PTY and job-control state, but enter an owned sibling cgroup before exec.
use anyhow::{Context, Result, ensure};
use std::{ffi::OsString, io::{Read, Write}, os::unix::{fs::DirBuilderExt, process::{CommandExt, ExitStatusExt}},
    os::unix::net::{UnixListener, UnixStream}, path::{Path, PathBuf}, process::{Child, Command}, time::{Duration, Instant}};
use templar_types::{WorkloadClass, WorkloadId, WorkloadRecord};

struct GateDirectory(PathBuf);
impl Drop for GateDirectory { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
struct OwnedChild(Option<Child>);
impl Drop for OwnedChild {
    fn drop(&mut self) { if let Some(child) = &mut self.0 { let _ = child.kill(); let _ = child.wait(); } }
}

pub async fn run(class: WorkloadClass, command: Vec<OsString>, notify: bool) -> Result<i32> {
    let executable = command.first().context("provide a command after --")?;
    let summary: String = Path::new(executable).file_name().context("command has no executable name")?
        .as_encoded_bytes().iter().flat_map(|byte| byte.escape_ascii()).map(char::from).take(256).collect();
    let started = Instant::now();
    let root = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let directory = GateDirectory(root.join(format!("templar-workload-{}", WorkloadId::new())));
    std::fs::DirBuilder::new().mode(0o700).create(&directory.0).context("creating private execution gate")?;
    let socket = directory.0.join("gate");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    let mut child = OwnedChild(Some(Command::new(std::env::current_exe()?)
        .arg("workload-gate").arg(&socket).arg("--").args(&command).spawn().context("starting gated child")?));
    let child_pid = child.0.as_ref().expect("owned child").id();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut gate = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let credentials = rustix::net::sockopt::socket_peercred(&stream)?;
                ensure!(credentials.pid.as_raw_nonzero().get() as u32 == child_pid
                    && credentials.uid == rustix::process::getuid(), "gate peer is not the owned child");
                break stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = child.0.as_mut().expect("owned child").try_wait()? {
                    anyhow::bail!("gated child exited before admission: {status}");
                }
                ensure!(Instant::now() < deadline, "execution gate timed out");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let connection = zbus::Connection::session().await.context("connecting to workload authority")?;
    let proxy = templar_client::proxies::WorkloadsProxy::new(&connection).await?;
    let record: WorkloadRecord = serde_json::from_str(&proxy.register_child(child_pid, class.as_str(), &summary).await?)?;
    ensure!(record.class == class && templar_appd::identity::unified_cgroup_of(child_pid)? == record.control_group
        && !templar_appd::identity::unified_cgroup_of(std::process::id())?.starts_with(&record.control_group),
        "invalid workload admission readback");
    gate.write_all(b"G")?;
    drop(gate);
    let status = child.0.as_mut().expect("owned child").wait()?;
    child.0.take();
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1));
    if notify {
        notify_completion(&connection, &summary, started.elapsed(), code).await;
    }
    Ok(code)
}

/// The `templar run --notify` completion: the same policy route the shell hooks use, fed by
/// the owned child's exit rather than a hook measurement. The flag forces every-job
/// semantics and skips the focus check — the flag itself is the opt-in — while canceled
/// suppression and dedupe still apply, so a Ctrl-C stays silent and the shell hook firing
/// for the same `templar run` line does not double-notify.
///
/// Best-effort by contract: the child's exit code is what `run` returns, and a notification
/// failure warns on stderr rather than stealing it.
async fn notify_completion(
    connection: &zbus::Connection,
    summary: &str,
    elapsed: std::time::Duration,
    code: i32,
) {
    if let Err(error) = send_completion(connection, summary, elapsed, code).await {
        eprintln!("templar: completion notification failed: {error:#}");
    }
}

async fn send_completion(
    connection: &zbus::Connection,
    summary: &str,
    elapsed: std::time::Duration,
    code: i32,
) -> anyhow::Result<()> {
    let deps = crate::notify::BusDeps::connect(connection.clone());
    let mut policy = crate::notify::read_policy_on(&deps).await;
    policy.notify_every_job = true;
    policy.skip_when_focused = false;
    let focus = crate::notify::resolve_focus(crate::notify::FocusRequest::Auto).await;
    let request = crate::notify::CompletionRequest {
        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        status: code,
        label: templar_session_core::notifications::safe_label(summary),
        emulator: crate::notify::detect_emulator(),
    };
    crate::notify::terminal_completed_with_policy(&deps, &policy, &request, focus).await?;
    Ok(())
}

/// Hidden process entry point. No shell is involved; exec keeps the admitted PID, PTY and group.
pub fn gate(socket: &Path, command: &[OsString]) -> Result<()> {
    let executable = command.first().context("execution gate needs a command")?;
    let mut stream = UnixStream::connect(socket).context("connecting private execution gate")?;
    stream.set_read_timeout(Some(Duration::from_secs(45)))?;
    let mut admission = [0_u8; 1];
    stream.read_exact(&mut admission).context("parent ended before workload admission")?;
    ensure!(admission == *b"G", "execution gate refused admission");
    drop(stream);
    Err(Command::new(executable).args(&command[1..]).exec()).context("executing admitted command")
}
