//! Local archive commands and native dialogs. No desktop-service or bus connection is needed.

use anyhow::{Context, Result, anyhow, ensure};
use std::{
    ffi::OsString,
    io::Write as _,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use templar_archive::{Cancelled, ExtractFailure, ExtractMode, PasswordRequired, Progress};
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    time::{MissedTickBehavior, interval},
};

/// The CLI's destination choice, before resolving an optional native folder request.
#[derive(Clone, Copy, Debug)]
pub enum Destination {
    Here,
    Smart,
    Directory,
}

pub async fn compress(paths: Vec<PathBuf>, output: Option<PathBuf>, gui: bool) -> Result<()> {
    let mut completed = Vec::new();
    let result = async {
        let mut feedback = Feedback::start(gui, "Compressing files into ZIP")?;
        let result = feedback
            .execute(move |cancel, progress| {
                templar_archive::compress(&paths, output.as_deref(), cancel, progress)
            })
            .await;
        let result = match result {
            Ok(path) => {
                completed.push(path);
                Ok(())
            }
            Err(error) if is_cancelled(&error) => Ok(()),
            Err(error) => Err(error),
        };
        merge_results(result, feedback.finish().await)
    }
    .await;
    finish_command(result, &completed, gui).await
}

pub async fn extract(
    paths: Vec<PathBuf>,
    destination: Destination,
    directory: Option<PathBuf>,
    gui: bool,
) -> Result<()> {
    let mut completed = Vec::new();
    let result = async {
        ensure!(!paths.is_empty(), "Select at least one archive to extract");
        ensure!(
            directory.is_none() || matches!(destination, Destination::Directory),
            "--directory can only be used with --mode directory"
        );
        let mode = match destination {
            Destination::Here => ExtractMode::Here,
            Destination::Smart => ExtractMode::Smart,
            Destination::Directory => {
                let directory = match directory {
                    Some(path) => path,
                    None => match choose_directory(&paths[0]).await? {
                        Some(path) => path,
                        None => return Ok(()),
                    },
                };
                ExtractMode::Directory(directory)
            }
        };
        let mode = Arc::new(mode);
        let mut feedback = Feedback::start(gui, "Extracting archives")?;
        let result = async {
            for archive in paths {
                if feedback.cancelled() {
                    break;
                }
                let mut password = None;
                loop {
                    let path = archive.clone();
                    let mode = Arc::clone(&mode);
                    let passphrase = password.take();
                    let result = feedback
                        .execute(move |cancel, progress| {
                            templar_archive::extract(
                                &[path],
                                &mode,
                                passphrase.as_deref(),
                                cancel,
                                progress,
                            )
                        })
                        .await;
                    match result {
                        Ok(outputs) => {
                            completed.extend(outputs);
                            break;
                        }
                        Err(error) => {
                            record_partial_outputs(&error, &mut completed);
                            if is_cancelled(&error) {
                                return Ok(());
                            }
                            if needs_password(&error) && gui && !feedback.cancelled() {
                                password = feedback.request_password(&archive).await?;
                                if password.is_none() {
                                    return Ok(());
                                }
                                continue;
                            }
                            if needs_password(&error) && !gui {
                                return Err(error.context(
                                    "Use --gui to enter this archive's password securely",
                                ));
                            }
                            return Err(error);
                        }
                    }
                }
            }
            Ok(())
        }
        .await;
        merge_results(result, feedback.finish().await)
    }
    .await;
    finish_command(result, &completed, gui).await
}

fn is_cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(<dyn std::error::Error>::is::<Cancelled>)
}

fn needs_password(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(<dyn std::error::Error>::is::<PasswordRequired>)
}

fn record_partial_outputs(error: &anyhow::Error, completed: &mut Vec<PathBuf>) {
    if let Some(failure) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ExtractFailure>())
    {
        completed.extend(failure.completed.iter().cloned());
    }
}

fn merge_results(operation: Result<()>, secondary: Result<()>) -> Result<()> {
    match (operation, secondary) {
        (Err(error), Err(secondary)) => {
            Err(error.context(format!("An additional error occurred: {secondary:#}")))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

async fn finish_command(result: Result<()>, completed: &[PathBuf], gui: bool) -> Result<()> {
    let printed = (|| -> Result<()> {
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        for path in completed {
            writeln!(stdout, "{}", path.display()).context("Printing archive output paths")?;
        }
        Ok(())
    })();
    let result = merge_results(result, printed);
    if let Err(error) = result {
        if gui {
            let mut message = format!("{error:#}");
            if !completed.is_empty() {
                message.push_str("\n\nAlready completed (these outputs were kept):");
                for path in completed {
                    message.push('\n');
                    message.push_str(&path.to_string_lossy());
                }
            }
            if let Err(dialog) = show_error(&message).await {
                return Err(error.context(format!("Could not show the error dialog: {dialog:#}")));
            }
        }
        return Err(error);
    }
    Ok(())
}

fn zenity() -> Command {
    let mut command = Command::new("zenity");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

fn dialog_failure(purpose: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        anyhow!("{purpose} exited with {}", output.status)
    } else {
        anyhow!("{purpose} exited with {}: {stderr}", output.status)
    }
}

fn dialog_response(output: Output, purpose: &str) -> Result<Option<Vec<u8>>> {
    if output.status.success() {
        Ok(Some(output.stdout))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(dialog_failure(purpose, &output))
    }
}

// Zenity terminates one response with LF; trimming whitespace would corrupt valid paths.
fn dialog_line(mut bytes: Vec<u8>, purpose: &str) -> Result<Vec<u8>> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    ensure!(
        !bytes.contains(&b'\n') && !bytes.contains(&b'\r'),
        "{purpose} contains a newline; Zenity cannot represent this unambiguously"
    );
    ensure!(
        !bytes.contains(&0),
        "{purpose} contains an invalid NUL byte"
    );
    Ok(bytes)
}

async fn choose_directory(first_archive: &Path) -> Result<Option<PathBuf>> {
    let parent = first_archive
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = if parent.is_absolute() {
        parent.to_path_buf()
    } else {
        std::env::current_dir()
            .context("Finding the folder chooser's starting directory")?
            .join(parent)
    };
    ensure!(
        !parent.as_os_str().as_bytes().contains(&b'\n')
            && !parent.as_os_str().as_bytes().contains(&b'\r'),
        "The starting directory contains a newline, which Zenity cannot represent unambiguously; use --directory PATH"
    );
    let mut initial = parent.into_os_string();
    initial.push("/");
    let output = zenity()
        .args([
            "--file-selection",
            "--directory",
            "--title",
            "Extract archives to directory",
        ])
        .arg("--filename")
        .arg(initial)
        .output()
        .await
        .context("Opening the native folder chooser (zenity)")?;
    let Some(bytes) = dialog_response(output, "The folder chooser")? else {
        return Ok(None);
    };
    let bytes = dialog_line(bytes, "The chosen directory")?;
    ensure!(
        !bytes.is_empty(),
        "The folder chooser returned an empty directory"
    );
    let directory = PathBuf::from(OsString::from_vec(bytes));
    let metadata = tokio::fs::metadata(&directory)
        .await
        .with_context(|| format!("Opening chosen directory {}", directory.display()))?;
    ensure!(
        metadata.is_dir(),
        "{} is not a directory",
        directory.display()
    );
    Ok(Some(directory))
}

async fn show_error(message: &str) -> Result<()> {
    let output = zenity()
        .args([
            "--error",
            "--no-markup",
            "--title",
            "Archive operation failed",
            "--text",
        ])
        .arg(message)
        .output()
        .await
        .context("Opening the archive error dialog (zenity)")?;
    if !output.status.success() {
        return Err(dialog_failure("The error dialog", &output));
    }
    Ok(())
}

struct Feedback {
    dialog: Option<Child>,
    input: Option<ChildStdin>,
    cancel: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    rendered_bytes: u64,
    entry: Option<PathBuf>,
    dirty: bool,
    error: Option<anyhow::Error>,
}

impl Feedback {
    fn start(gui: bool, title: &str) -> Result<Self> {
        let (dialog, input) = if gui {
            let mut child = zenity()
                .args(["--progress", "--pulsate", "--auto-close", "--title"])
                .arg(title)
                .args(["--text", "Preparing archive operation…"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .context("Opening archive progress (zenity)")?;
            let input = child.stdin.take();
            (Some(child), input)
        } else {
            (None, None)
        };
        Ok(Self {
            dialog,
            input,
            cancel: Arc::new(AtomicBool::new(false)),
            bytes: Arc::new(AtomicU64::new(0)),
            rendered_bytes: 0,
            entry: None,
            dirty: false,
            error: None,
        })
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn fail(&mut self, error: anyhow::Error) {
        self.cancel.store(true, Ordering::Relaxed);
        drop(self.input.take());
        drop(self.dialog.take());
        self.error.get_or_insert(error);
    }

    async fn execute<T, F>(&mut self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&AtomicBool, &mut dyn FnMut(Progress)) -> Result<T> + Send + 'static,
    {
        let cancel = Arc::clone(&self.cancel);
        if self.dialog.is_none() {
            return tokio::task::spawn_blocking(move || operation(&cancel, &mut |_| {}))
                .await
                .context("The archive worker stopped unexpectedly")?;
        }
        let bytes = Arc::clone(&self.bytes);
        let gui = self.dialog.is_some();
        let (send, mut receive) = mpsc::channel(32);
        let mut work = tokio::task::spawn_blocking(move || {
            let mut progress = |progress| {
                if gui {
                    match progress {
                        Progress::Entry(path) => {
                            let _ = send.try_send(path);
                        }
                        Progress::Bytes(count) => {
                            bytes.fetch_add(count, Ordering::Relaxed);
                        }
                    }
                }
            };
            operation(&cancel, &mut progress)
        });
        let mut tick = interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut receiving = gui;
        loop {
            tokio::select! {
                result = &mut work => {
                    self.poll().await;
                    return result.context("The archive worker stopped unexpectedly")?;
                }
                entry = receive.recv(), if receiving => {
                    if let Some(entry) = entry {
                        self.entry = Some(entry);
                        self.dirty = true;
                    } else {
                        receiving = false;
                    }
                }
                _ = tick.tick(), if self.dialog.is_some() => {
                    self.poll().await;
                    self.update().await;
                }
            }
        }
    }

    async fn poll(&mut self) {
        let Some(dialog) = self.dialog.as_mut() else {
            return;
        };
        match dialog.try_wait() {
            Ok(None) => {}
            Ok(Some(_)) => {
                let Some(dialog) = self.dialog.take() else {
                    return;
                };
                drop(self.input.take());
                match dialog.wait_with_output().await {
                    Ok(output) if output.status.code() == Some(1) => {
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                    Ok(output) => self.fail(dialog_failure("The progress dialog", &output)),
                    Err(error) => self.fail(error.into()),
                }
            }
            Err(error) => self.fail(error.into()),
        }
    }

    async fn update(&mut self) {
        let bytes = self.bytes.load(Ordering::Relaxed);
        if !self.dirty && bytes == self.rendered_bytes {
            return;
        }
        self.dirty = false;
        self.rendered_bytes = bytes;
        let entry = self
            .entry
            .as_deref()
            .map_or_else(|| "Processing archive".to_owned(), progress_path);
        let message = format!("# {entry} — {} processed\n", crate::humanize::bytes(bytes));
        if let Some(input) = self.input.as_mut()
            && let Err(error) = input.write_all(message.as_bytes()).await
        {
            self.poll().await;
            if self.dialog.is_some() {
                self.fail(error.into());
            }
        }
    }

    async fn request_password(&mut self, archive: &Path) -> Result<Option<String>> {
        self.poll().await;
        if self.cancelled() {
            return Ok(None);
        }
        let mut command = zenity();
        command
            .arg("--password")
            .arg("--title")
            .arg(format!("Password for {}", display_path(archive)));
        // The secret is only returned on the dialog's private stdout pipe, never in argv.
        let response = command.output();
        tokio::pin!(response);
        let mut tick = interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                output = &mut response => {
                    let output = output.context("Opening the archive password dialog (zenity)")?;
                    let Some(bytes) = dialog_response(output, "The password dialog")? else {
                        self.cancel.store(true, Ordering::Relaxed);
                        return Ok(None);
                    };
                    let password = String::from_utf8(dialog_line(bytes, "The password")?)
                        .context("The password dialog returned non-UTF-8 text")?;
                    return Ok(Some(password));
                }
                _ = tick.tick() => {
                    self.poll().await;
                    self.update().await;
                    if self.cancelled() {
                        return Ok(None);
                    }
                }
            }
        }
    }

    async fn finish(&mut self) -> Result<()> {
        self.poll().await;
        if let Some(dialog) = self.dialog.take() {
            let sent = match self.input.take() {
                Some(mut input) => input.write_all(b"100\n").await,
                None => Ok(()),
            };
            match dialog.wait_with_output().await {
                Ok(output) if output.status.success() => {
                    if let Err(error) = sent {
                        self.fail(error.into());
                    }
                }
                Ok(output) if output.status.code() == Some(1) => {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(output) => self.fail(dialog_failure("The progress dialog", &output)),
                Err(error) => self.fail(error.into()),
            }
        }
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|character| match character {
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

// Progress stdin is always Pango markup after GLib's backslash unescaping, even
// though message dialogs support --no-markup. Escape both layers for filenames.
fn progress_path(path: &Path) -> String {
    let mut text = String::with_capacity(path.as_os_str().len());
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => text.push_str("\\\\"),
            '&' => text.push_str("&amp;"),
            '<' => text.push_str("&lt;"),
            '>' => text.push_str("&gt;"),
            character if character.is_control() => text.push(' '),
            character => text.push(character),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_paths_preserve_spaces_and_remove_only_the_record_terminator() -> Result<()> {
        assert_eq!(
            dialog_line(b" /tmp/a space/ \n".to_vec(), "Directory")?,
            b" /tmp/a space/ ".to_vec()
        );
        Ok(())
    }

    #[test]
    fn ambiguous_newline_paths_are_rejected() {
        assert!(dialog_line(b"/tmp/a\nb\n".to_vec(), "Directory").is_err());
        assert!(dialog_line(b"/tmp/a\n\n".to_vec(), "Directory").is_err());
        assert!(dialog_line(b"/tmp/a\rb\n".to_vec(), "Directory").is_err());
    }

    #[test]
    fn progress_filenames_escape_markup_and_glib_backslashes() {
        assert_eq!(
            progress_path(Path::new(r"a<&>\n.zip")),
            r"a&lt;&amp;&gt;\\n.zip"
        );
        assert_eq!(progress_path(Path::new("a\nb.zip")), "a b.zip");
    }
}
