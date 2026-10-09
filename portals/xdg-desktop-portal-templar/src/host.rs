//! Async portal requests and compatible main-thread PixelKit Wayland/X11 hosts.
use crate::dialog;
use pixelkit_windowing::{PixelApp, Waker, WindowConfig};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UiError(pub String);
pub type UiResult<T> = Result<Option<T>, UiError>;

#[derive(Debug)]
pub struct Pending {
    pub model: dialog::Model,
    pub answer: oneshot::Sender<UiResult<dialog::Answer>>,
    pub abandoned: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
pub struct Client {
    sender: mpsc::Sender<Pending>,
    waker: Waker,
}

impl Client {
    pub fn new(sender: mpsc::Sender<Pending>, waker: Waker) -> Self {
        Self { sender, waker }
    }

    pub async fn request(&self, spec: dialog::Spec) -> UiResult<dialog::Answer> {
        if matches!(spec.parent(), crate::parent::Parent::X11(_))
            && std::env::var_os("WAYLAND_DISPLAY").is_some()
            && std::env::var_os("DISPLAY").is_some()
        {
            return request_x11(spec).await;
        }
        let model = spec.into_model();
        let (answer, received) = oneshot::channel();
        let abandoned = Arc::new(AtomicBool::new(false));
        // Dropping the awaiting D-Bus future (Close, caller loss or deadline)
        // must retire its window, even if it has not been opened yet.
        let _guard = Abandon {
            abandoned: abandoned.clone(),
            waker: self.waker.clone(),
        };
        self.sender
            .send(Pending {
                model,
                answer,
                abandoned,
            })
            .map_err(|_| UiError("graphical host stopped".into()))?;
        self.waker.wake();
        received
            .await
            .map_err(|_| UiError("graphical response channel closed".into()))?
    }
}

struct Abandon {
    abandoned: Arc<AtomicBool>,
    waker: Waker,
}
impl Drop for Abandon {
    fn drop(&mut self) {
        self.abandoned.store(true, Ordering::Release);
        self.waker.wake();
    }
}

fn window(pending: Pending) -> (WindowConfig, Box<dyn PixelApp>) {
    let mut config = WindowConfig::new(pending.model.title(), 720.0, 560.0).min_size(560.0, 440.0);
    config.activation_token = pending.model.activation_token().map(str::to_owned);
    (config, dialog::application(pending))
}

pub fn receive(receiver: &mpsc::Receiver<Pending>) -> Vec<(WindowConfig, Box<dyn PixelApp>)> {
    receiver
        .try_iter()
        .filter(|pending| !pending.abandoned.load(Ordering::Acquire) && !pending.answer.is_closed())
        .map(window)
        .collect()
}

/// Winit chooses one display protocol for a process. Use an owned process only
/// for X11 parents in a mixed session; never force Wayland callers onto X11.
/// kill_on_drop retires the helper on Close/caller loss/deadline; the kernel
/// parent-death signal also retires it if the backend crashes or is stopped.
async fn request_x11(spec: dialog::Spec) -> UiResult<dialog::Answer> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;
    let executable = std::env::current_exe().map_err(|error| UiError(error.to_string()))?;
    let bytes = serde_json::to_vec(&spec).map_err(|error| UiError(error.to_string()))?;
    let owner = libc::pid_t::try_from(std::process::id())
        .map_err(|_| UiError("invalid native picker owner PID".into()))?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--x11-dialog")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe syscalls/error construction run between
    // fork and exec. Recheck PPID to close the race before the death signal was armed.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != owner {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| UiError(format!("starting native X11 picker: {error}")))?;
    {
        let mut input = child
            .stdin
            .take()
            .ok_or_else(|| UiError("X11 picker input unavailable".into()))?;
        input
            .write_all(&bytes)
            .await
            .map_err(|error| UiError(format!("sending X11 picker request: {error}")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| UiError(format!("waiting for X11 picker: {error}")))?;
    if !output.status.success() {
        return Err(UiError(format!(
            "native X11 picker exited: {}",
            output.status
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| UiError(format!("reading native X11 picker answer: {error}")))?
}

/// Hidden helper mode. One event loop on this process's main thread; no portal
/// bus name or second backend. Stdout is exclusively the typed request outcome.
pub(crate) fn run_x11_dialog() -> anyhow::Result<()> {
    use std::io::Write as _;
    let spec: dialog::Spec = serde_json::from_reader(std::io::stdin().lock())?;
    anyhow::ensure!(
        matches!(spec.parent(), crate::parent::Parent::X11(_)),
        "X11 helper requires an X11 parent"
    );
    let (answer, received) = oneshot::channel();
    let pending = Pending {
        model: spec.into_model(),
        answer,
        abandoned: Arc::new(AtomicBool::new(false)),
    };
    let result = match pixelkit_windowing::run_windows(vec![window(pending)]) {
        Ok(()) => received
            .blocking_recv()
            .unwrap_or_else(|_| Err(UiError("X11 graphical response channel closed".into()))),
        Err(error) => Err(UiError(format!("native X11 graphical host: {error}"))),
    };
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &result)?;
    output.flush()?;
    Ok(())
}
