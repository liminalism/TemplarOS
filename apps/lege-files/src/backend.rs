//! Asynchronous D-Bus backend. The window thread only sends requests and drains snapshots.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use lege_client::{
    files::{
        BrowserSources, FileInfo, OperationOptions, OperationRecord, OperationRequest,
        PreviewMetadata, TrashEntry, VolumeInfo,
    },
    proxies::{
        AssociationsProxy, FilesActionsProxy, FilesBrowseProxy, FilesOperationsProxy,
        FilesPreviewsProxy, FilesRecentsProxy, FilesTrashProxy, FilesVolumesProxy, LauncherProxy,
    },
};
use pixelkit_shell::Waker;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

use crate::model::{
    BrowseRequest, BrowseSnapshot, InspectRequest, PreviewRequest, PreviewSnapshot,
    PropertiesSnapshot, TrashIntent,
};

pub const APP_ID: &str = "org.legeos.Files";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenResult {
    pub app_id: String,
    pub mime: String,
    pub instance: String,
    pub reason: String,
}

#[derive(Debug)]
pub enum Update {
    Roots(std::result::Result<BrowserSources, String>),
    Browse {
        request: BrowseRequest,
        result: std::result::Result<BrowseSnapshot, String>,
    },
    Inspect {
        request: InspectRequest,
        result: std::result::Result<PropertiesSnapshot, String>,
    },
    Preview {
        request: PreviewRequest,
        result: std::result::Result<Option<PreviewSnapshot>, String>,
    },
    /// One grid thumbnail, decoded off-thread. Tagged with the generation it was asked under so
    /// the model can drop a decode whose directory is gone.
    Thumbnail {
        request: crate::model::ThumbnailRequest,
        bitmap: Option<pixelkit_raster::Bitmap>,
    },
    CopiedPath {
        path: PathBuf,
        result: std::result::Result<String, String>,
    },
    Opened {
        path: PathBuf,
        result: std::result::Result<OpenResult, String>,
    },
    OperationStarted {
        id: String,
        summary: String,
    },
    OperationProgress {
        record: OperationRecord,
        can_undo: bool,
    },
    OperationFailed {
        summary: String,
        error: String,
    },
    Cancelled(std::result::Result<(), String>),
    /// A `Files.Trash1.List` answer, or why there is none.
    Trash(std::result::Result<Vec<TrashEntry>, String>),
    /// `Files.Trash1.Changed`: the trash changed, whoever changed it. The window re-lists.
    TrashChanged,
}

pub struct Backend {
    runtime: tokio::runtime::Runtime,
    updates_tx: mpsc::Sender<Update>,
    updates_rx: mpsc::Receiver<Update>,
    waker: Arc<Mutex<Waker>>,
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Backend").finish_non_exhaustive()
    }
}

impl Backend {
    pub fn new() -> Result<Self> {
        let (updates_tx, updates_rx) = mpsc::channel();
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().context("creating Files async runtime")?,
            updates_tx,
            updates_rx,
            waker: Arc::new(Mutex::new(Waker::detached())),
        })
    }

    pub fn attach(&self, waker: Waker) {
        if let Ok(mut attached) = self.waker.lock() {
            *attached = waker;
        }
    }

    pub fn try_update(&self) -> Option<Update> {
        self.updates_rx.try_recv().ok()
    }

    pub fn roots(&self) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = fetch_roots().await.map_err(error_text);
            deliver(&tx, &waker, Update::Roots(result));
        });
    }

    pub fn browse(&self, request: BrowseRequest) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = fetch_browse(&request).await.map_err(error_text);
            deliver(&tx, &waker, Update::Browse { request, result });
        });
    }

    pub fn inspect(&self, request: InspectRequest) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = fetch_properties(&request.path).await.map_err(error_text);
            deliver(&tx, &waker, Update::Inspect { request, result });
        });
    }

    pub fn preview(&self, request: PreviewRequest) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = fetch_preview(&request.path).await.map_err(error_text);
            deliver(&tx, &waker, Update::Preview { request, result });
        });
    }

    pub fn thumbnail(&self, request: crate::model::ThumbnailRequest) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let bitmap = fetch_preview(&request.path)
                .await
                .ok()
                .flatten()
                .map(|snapshot| snapshot.bitmap);
            deliver(&tx, &waker, Update::Thumbnail { request, bitmap });
        });
    }

    pub fn copy_path(&self, path: PathBuf) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = copy_path_to_clipboard(&path).await.map_err(error_text);
            deliver(&tx, &waker, Update::CopiedPath { path, result });
        });
    }

    pub fn open(&self, path: PathBuf) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = open_through_services(&path).await.map_err(error_text);
            deliver(&tx, &waker, Update::Opened { path, result });
        });
    }

    pub fn start_operation(&self, request: OperationRequest, options: OperationOptions) {
        let summary = request.summary();
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = start_operation(&request, options).await;
            match result {
                Ok((connection, id)) => {
                    deliver(
                        &tx,
                        &waker,
                        Update::OperationStarted {
                            id: id.clone(),
                            summary: summary.clone(),
                        },
                    );
                    if let Err(error) = poll_operation(&connection, &id, &tx, &waker).await {
                        deliver(
                            &tx,
                            &waker,
                            Update::OperationFailed {
                                summary,
                                error: error_text(error),
                            },
                        );
                    }
                }
                Err(error) => deliver(
                    &tx,
                    &waker,
                    Update::OperationFailed {
                        summary,
                        error: error_text(error),
                    },
                ),
            }
        });
    }

    /// Ask `Files.Trash1` what is in every trash.
    pub fn trash_list(&self) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result = fetch_trash().await.map_err(error_text);
            deliver(&tx, &waker, Update::Trash(result));
        });
    }

    /// Perform one trash action, then follow the operation it started exactly as any other file
    /// operation is followed — progress, journal and all. `Trash1` is a wrapper over the same
    /// `Operations1.Start`, so there is nothing else to watch.
    pub fn trash_action(&self, intent: TrashIntent) {
        let summary = trash_summary(&intent);
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let started = async {
                let connection = session().await?;
                let proxy = FilesTrashProxy::new(&connection)
                    .await
                    .context("Files.Trash1")?;
                let id = match &intent {
                    TrashIntent::Restore(paths) => proxy.restore(path_strings(paths), APP_ID).await,
                    TrashIntent::DeletePermanently(paths) => {
                        proxy.delete_permanently(path_strings(paths), APP_ID).await
                    }
                    TrashIntent::Empty => proxy.empty(APP_ID).await,
                }
                .context("asking Files.Trash1")?;
                Ok::<_, anyhow::Error>((connection, id))
            }
            .await;
            match started {
                // An empty operation ID means there was nothing to do, which `Trash1.Empty`
                // reports rather than treating as a failure.
                Ok((_, id)) if id.is_empty() => {
                    deliver(&tx, &waker, Update::Trash(Ok(Vec::new())));
                }
                Ok((connection, id)) => {
                    deliver(
                        &tx,
                        &waker,
                        Update::OperationStarted {
                            id: id.clone(),
                            summary: summary.clone(),
                        },
                    );
                    if let Err(error) = poll_operation(&connection, &id, &tx, &waker).await {
                        deliver(
                            &tx,
                            &waker,
                            Update::OperationFailed {
                                summary,
                                error: error_text(error),
                            },
                        );
                    }
                }
                Err(error) => deliver(
                    &tx,
                    &waker,
                    Update::OperationFailed {
                        summary,
                        error: error_text(error),
                    },
                ),
            }
        });
    }

    /// Follow `Files.Trash1.Changed` for as long as the connection lasts.
    ///
    /// This is what keeps the trash view honest when something else did the changing — Home's
    /// EMPTY, `lege trash restore`, another window. Without it the listing would be a snapshot
    /// that quietly stops being true and still offers buttons over it.
    pub fn watch_trash(&self) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            use futures_util::StreamExt as _;
            let Ok(connection) = session().await else {
                return;
            };
            let Ok(proxy) = FilesTrashProxy::new(&connection).await else {
                return;
            };
            let Ok(mut changed) = proxy.receive_changed().await else {
                return;
            };
            while changed.next().await.is_some() {
                deliver(&tx, &waker, Update::TrashChanged);
            }
        });
    }

    pub fn undo(&self, id: String) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result: Result<(zbus::Connection, String)> = async {
                let connection = session().await?;
                let proxy = FilesOperationsProxy::new(&connection)
                    .await
                    .context("Files.Operations1")?;
                let undo_id = proxy.undo(&id).await.context("undoing operation")?;
                Ok((connection, undo_id))
            }
            .await;
            match result {
                Ok((connection, undo_id)) => {
                    deliver(
                        &tx,
                        &waker,
                        Update::OperationStarted {
                            id: undo_id.clone(),
                            summary: "Undo".into(),
                        },
                    );
                    if let Err(error) = poll_operation(&connection, &undo_id, &tx, &waker).await {
                        deliver(
                            &tx,
                            &waker,
                            Update::OperationFailed {
                                summary: "Undo".into(),
                                error: error_text(error),
                            },
                        );
                    }
                }
                Err(error) => deliver(
                    &tx,
                    &waker,
                    Update::OperationFailed {
                        summary: "Undo".into(),
                        error: error_text(error),
                    },
                ),
            }
        });
    }

    pub fn cancel(&self, id: String) {
        let (tx, waker) = (self.updates_tx.clone(), Arc::clone(&self.waker));
        self.runtime.spawn(async move {
            let result: Result<()> = async {
                let connection = session().await?;
                FilesOperationsProxy::new(&connection)
                    .await
                    .context("Files.Operations1")?
                    .cancel(&id)
                    .await
                    .context("cancelling operation")?;
                Ok(())
            }
            .await;
            deliver(&tx, &waker, Update::Cancelled(result.map_err(error_text)));
        });
    }
}

fn deliver(tx: &mpsc::Sender<Update>, waker: &Arc<Mutex<Waker>>, update: Update) {
    if tx.send(update).is_ok()
        && let Ok(waker) = waker.lock()
    {
        waker.wake();
    }
}

fn error_text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// One session-bus connection per runtime.
///
/// `zbus::Connection::session()` builds a new connection every call — socket, SASL handshake,
/// `Hello`, and a reader task — and browsing a directory here makes several. That handshake costs
/// about as much as the call it was made for, so half the cost of listing a folder was opening
/// connections to list it with.
///
/// Keyed by runtime because a connection owns a reader task on the runtime that built it, and one
/// that outlives its runtime does not report itself closed: it simply stops being answered. This
/// process holds one long-lived runtime for the browser and builds a second for `--print`, so the
/// key is what keeps the short-lived one from poisoning the cache for the other.
static SESSIONS: tokio::sync::Mutex<
    Option<std::collections::HashMap<tokio::runtime::Id, zbus::Connection>>,
> = tokio::sync::Mutex::const_new(None);

async fn session() -> Result<zbus::Connection> {
    let runtime = tokio::runtime::Handle::current().id();
    let mut held = SESSIONS.lock().await;
    let connections = held.get_or_insert_with(std::collections::HashMap::new);
    connections.retain(|_, connection| !connection.is_closed());
    if let Some(connection) = connections.get(&runtime) {
        return Ok(connection.clone());
    }
    let connection = zbus::Connection::session()
        .await
        .context("connecting to the LegeOS session bus")?;
    connections.insert(runtime, connection.clone());
    Ok(connection)
}

async fn fetch_roots() -> Result<BrowserSources> {
    let connection = session().await?;
    let json = FilesBrowseProxy::new(&connection)
        .await
        .context("Files.Browse1")?
        .roots()
        .await
        .context("loading places and drives")?;
    serde_json::from_str(&json).context("parsing Files.Browse1.Roots")
}

async fn fetch_browse(request: &BrowseRequest) -> Result<BrowseSnapshot> {
    let connection = session().await?;
    let browse = FilesBrowseProxy::new(&connection)
        .await
        .context("Files.Browse1")?;
    let volumes = FilesVolumesProxy::new(&connection)
        .await
        .context("Files.Volumes1")?;
    let path = request.path.to_string_lossy();
    let (listing_json, volume_json) = tokio::try_join!(
        browse.list_directory(&path, request.show_hidden, 500),
        volumes.info_for_path(&path),
    )
    .with_context(|| format!("loading {}", request.path.display()))?;
    Ok(BrowseSnapshot {
        listing: serde_json::from_str(&listing_json).context("parsing directory listing")?,
        volume: serde_json::from_str(&volume_json).context("parsing containing volume")?,
    })
}

async fn fetch_properties(path: &Path) -> Result<PropertiesSnapshot> {
    let connection = session().await?;
    let browse = FilesBrowseProxy::new(&connection)
        .await
        .context("Files.Browse1")?;
    let volumes = FilesVolumesProxy::new(&connection)
        .await
        .context("Files.Volumes1")?;
    let path_text = path.to_string_lossy();
    let (info_json, volume_json) = tokio::try_join!(
        browse.inspect(&path_text),
        volumes.info_for_path(&path_text),
    )
    .with_context(|| format!("inspecting {}", path.display()))?;
    Ok(PropertiesSnapshot {
        info: serde_json::from_str::<FileInfo>(&info_json).context("parsing file properties")?,
        volume: serde_json::from_str::<VolumeInfo>(&volume_json)
            .context("parsing properties volume")?,
    })
}

async fn fetch_preview(path: &Path) -> Result<Option<PreviewSnapshot>> {
    let connection = session().await?;
    let proxy = FilesPreviewsProxy::new(&connection)
        .await
        .context("Files.Previews1")?;
    let path_text = path.to_string_lossy();
    let (metadata_json, png) = proxy
        .render(&path_text, 512)
        .await
        .with_context(|| format!("rendering preview for {}", path.display()))?;
    if metadata_json.is_empty() && png.is_empty() {
        return Ok(None);
    }
    let metadata: PreviewMetadata =
        serde_json::from_str(&metadata_json).context("parsing preview metadata")?;
    let bitmap = pixelkit_raster::png::decode(&png)
        .map_err(|error| anyhow::anyhow!("decoding service preview PNG: {error}"))?;
    if (bitmap.width, bitmap.height) != (metadata.width, metadata.height) {
        bail!(
            "preview metadata says {}x{} but PNG is {}x{}",
            metadata.width,
            metadata.height,
            bitmap.width,
            bitmap.height
        );
    }
    Ok(Some(PreviewSnapshot { metadata, bitmap }))
}

#[async_trait]
pub trait PathRenderClient: Send + Sync {
    async fn render_as_shown(&self, path: &str) -> Result<String>;
}

#[async_trait]
impl PathRenderClient for FilesActionsProxy<'_> {
    async fn render_as_shown(&self, path: &str) -> Result<String> {
        self.render(path, "as-shown")
            .await
            .context("rendering path through Files.Actions1")
    }
}

#[async_trait]
pub trait ClipboardSink: Send + Sync {
    async fn set_text(&self, text: &str) -> Result<()>;
}

#[derive(Debug, Default)]
struct WaylandClipboard;

#[async_trait]
impl ClipboardSink for WaylandClipboard {
    async fn set_text(&self, text: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let mut child = tokio::process::Command::new("wl-copy")
            .arg("--type")
            .arg("text/plain;charset=utf-8")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("starting wl-copy")?;
        let mut stdin = child
            .stdin
            .take()
            .context("wl-copy did not expose standard input")?;
        stdin
            .write_all(text.as_bytes())
            .await
            .context("writing the path to wl-copy")?;
        stdin.shutdown().await.context("closing wl-copy input")?;
        drop(stdin);
        let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output())
            .await
            .context("wl-copy did not take ownership of the clipboard within 3 seconds")??;
        if !output.status.success() {
            bail!(
                "wl-copy failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

pub async fn copy_path(
    renderer: &impl PathRenderClient,
    clipboard: &impl ClipboardSink,
    path: &Path,
) -> Result<String> {
    if !path.is_absolute() {
        bail!("file path must be absolute: {}", path.display());
    }
    let path_text = path.to_string_lossy();
    let clipboard_text = renderer.render_as_shown(&path_text).await?;
    clipboard.set_text(&clipboard_text).await?;
    Ok(clipboard_text)
}

async fn copy_path_to_clipboard(path: &Path) -> Result<String> {
    let connection = session().await?;
    let renderer = FilesActionsProxy::new(&connection)
        .await
        .context("Files.Actions1")?;
    copy_path(&renderer, &WaylandClipboard, path).await
}

async fn start_operation(
    request: &OperationRequest,
    options: OperationOptions,
) -> Result<(zbus::Connection, String)> {
    let connection = session().await?;
    let proxy = FilesOperationsProxy::new(&connection)
        .await
        .context("Files.Operations1")?;
    let request_json = serde_json::to_string(request).context("serializing operation")?;
    let options_json = serde_json::to_string(&options).context("serializing operation options")?;
    let id = proxy
        .start(&request_json, &options_json, APP_ID)
        .await
        .with_context(|| request.summary())?;
    drop(proxy);
    Ok((connection, id))
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn trash_summary(intent: &TrashIntent) -> String {
    match intent {
        TrashIntent::Restore(paths) => format!("Restore {} item(s) from Trash", paths.len()),
        TrashIntent::DeletePermanently(paths) => {
            format!("Delete {} item(s) permanently", paths.len())
        }
        TrashIntent::Empty => "Empty the Trash".to_owned(),
    }
}

async fn fetch_trash() -> Result<Vec<TrashEntry>> {
    let connection = session().await?;
    let json = FilesTrashProxy::new(&connection)
        .await
        .context("Files.Trash1")?
        .list()
        .await
        .context("listing the trash")?;
    serde_json::from_str(&json).context("parsing the trash listing")
}

async fn poll_operation(
    connection: &zbus::Connection,
    id: &str,
    tx: &mpsc::Sender<Update>,
    waker: &Arc<Mutex<Waker>>,
) -> Result<()> {
    let proxy = FilesOperationsProxy::new(connection)
        .await
        .context("Files.Operations1")?;
    loop {
        let json = proxy.get(id).await.context("reading operation progress")?;
        let record: OperationRecord =
            serde_json::from_str(&json).context("parsing operation progress")?;
        let terminal = record.state.terminal();
        let can_undo = if terminal {
            proxy.can_undo(id).await.is_ok_and(|(allowed, _)| allowed)
        } else {
            false
        };
        deliver(tx, waker, Update::OperationProgress { record, can_undo });
        if terminal {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

#[async_trait]
pub trait AssociationClient: Send + Sync {
    async fn resolve(&self, path: &str) -> Result<(String, String, String)>;
}

#[async_trait]
impl AssociationClient for AssociationsProxy<'_> {
    async fn resolve(&self, path: &str) -> Result<(String, String, String)> {
        self.resolve_file(path, "")
            .await
            .context("resolving the file association")
    }
}

#[async_trait]
pub trait LaunchClient: Send + Sync {
    async fn launch_path(&self, app_id: &str, path: &str) -> Result<String>;
}

#[async_trait]
impl LaunchClient for LauncherProxy<'_> {
    async fn launch_path(&self, app_id: &str, path: &str) -> Result<String> {
        self.launch(app_id, vec![path.to_owned()], "")
            .await
            .context("launching the associated application")
    }
}

#[async_trait]
pub trait RecentClient: Send + Sync {
    async fn record_open(&self, path: &str, mime: &str) -> Result<()>;
}

#[async_trait]
impl RecentClient for FilesRecentsProxy<'_> {
    async fn record_open(&self, path: &str, mime: &str) -> Result<()> {
        self.record(path, APP_ID, "open", mime)
            .await
            .context("recording the opened file")
    }
}

/// The only permitted non-directory open path: association broker, identity-aware launcher,
/// then a best-effort recent-file record.
pub async fn brokered_open(
    associations: &impl AssociationClient,
    launcher: &impl LaunchClient,
    recents: &impl RecentClient,
    path: &Path,
) -> Result<OpenResult> {
    if !path.is_absolute() {
        bail!("file path must be absolute: {}", path.display());
    }
    let path = path.to_string_lossy();
    let (app_id, mime, reason) = associations.resolve(&path).await?;
    let instance = launcher.launch_path(&app_id, &path).await?;
    if let Err(error) = recents.record_open(&path, &mime).await {
        tracing::debug!(%error, "file opened but recent-file recording failed");
    }
    Ok(OpenResult {
        app_id,
        mime,
        instance,
        reason,
    })
}

async fn open_through_services(path: &Path) -> Result<OpenResult> {
    let connection = session().await?;
    let associations = AssociationsProxy::new(&connection)
        .await
        .context("Associations1")?;
    let launcher = LauncherProxy::new(&connection).await.context("Launcher1")?;
    let recents = FilesRecentsProxy::new(&connection)
        .await
        .context("Files.Recents1")?;
    brokered_open(&associations, &launcher, &recents, path).await
}

pub async fn print_snapshot(path: Option<&Path>, show_hidden: bool) -> Result<String> {
    let roots = fetch_roots().await?;
    let path = path
        .map(ToOwned::to_owned)
        .or_else(|| roots.home().map(ToOwned::to_owned))
        .or_else(|| roots.first_available().map(ToOwned::to_owned))
        .context("no available root")?;
    let request = BrowseRequest {
        id: 1,
        path,
        show_hidden,
    };
    let snapshot = fetch_browse(&request).await?;
    let mut lines = vec!["ROOTS".to_owned()];
    for root in roots.roots {
        let detail = root.volume.as_ref().map_or_else(
            || {
                if root.available {
                    "available".into()
                } else {
                    "not present".into()
                }
            },
            |volume| {
                format!(
                    "{} free of {} · {}{}",
                    crate::format::bytes(volume.free_bytes),
                    crate::format::bytes(volume.total_bytes),
                    volume.fs_type,
                    if volume.read_only {
                        " · read-only"
                    } else {
                        ""
                    }
                )
            },
        );
        lines.push(format!(
            "  {:<12} {:<24} {detail}",
            root.label,
            root.path.display()
        ));
    }
    lines.push(format!(
        "DIRECTORY {}",
        snapshot.listing.directory.display()
    ));
    for entry in snapshot.listing.entries {
        lines.push(format!(
            "  {:<6} {:>10}  {}",
            if entry.is_dir() { "folder" } else { "file" },
            if entry.is_dir() {
                "—".to_owned()
            } else {
                crate::format::bytes(entry.size_bytes)
            },
            entry.name
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeAssociation(Mutex<Vec<String>>);

    #[async_trait]
    impl AssociationClient for FakeAssociation {
        async fn resolve(&self, path: &str) -> Result<(String, String, String)> {
            self.0.lock().unwrap().push(format!("resolve:{path}"));
            Ok((
                "org.example.Reader".into(),
                "text/plain".into(),
                "detected".into(),
            ))
        }
    }

    #[derive(Default)]
    struct FakeLauncher(Mutex<Vec<String>>);

    #[async_trait]
    impl LaunchClient for FakeLauncher {
        async fn launch_path(&self, app_id: &str, path: &str) -> Result<String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("launch:{app_id}:{path}"));
            Ok("instance-1".into())
        }
    }

    #[derive(Default)]
    struct FakeRecents(Mutex<Vec<String>>);

    #[async_trait]
    impl RecentClient for FakeRecents {
        async fn record_open(&self, path: &str, mime: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("recent:{mime}:{path}"));
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakePathRenderer(Mutex<Vec<String>>);

    #[async_trait]
    impl PathRenderClient for FakePathRenderer {
        async fn render_as_shown(&self, path: &str) -> Result<String> {
            self.0.lock().unwrap().push(path.to_owned());
            Ok(path.to_owned())
        }
    }

    #[derive(Default)]
    struct FakeClipboard(Mutex<Vec<String>>);

    #[async_trait]
    impl ClipboardSink for FakeClipboard {
        async fn set_text(&self, text: &str) -> Result<()> {
            self.0.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn brokered_open_resolves_then_launches_and_records() {
        let (associations, launcher, recents) = (
            FakeAssociation::default(),
            FakeLauncher::default(),
            FakeRecents::default(),
        );
        let result = brokered_open(
            &associations,
            &launcher,
            &recents,
            Path::new("/home/u/note.txt"),
        )
        .await
        .unwrap();
        assert_eq!(result.app_id, "org.example.Reader");
        assert_eq!(result.instance, "instance-1");
        assert_eq!(associations.0.lock().unwrap().len(), 1);
        assert_eq!(launcher.0.lock().unwrap().len(), 1);
        assert_eq!(recents.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn brokered_open_never_accepts_a_relative_path() {
        let error = brokered_open(
            &FakeAssociation::default(),
            &FakeLauncher::default(),
            &FakeRecents::default(),
            Path::new("relative.txt"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("absolute"));
    }

    #[tokio::test]
    async fn copy_path_uses_the_service_form_and_real_clipboard_boundary() {
        let renderer = FakePathRenderer::default();
        let clipboard = FakeClipboard::default();
        let path = Path::new("/home/u/two words.txt");
        let copied = copy_path(&renderer, &clipboard, path).await.unwrap();
        assert_eq!(copied, "/home/u/two words.txt");
        assert_eq!(
            renderer.0.lock().unwrap().as_slice(),
            std::slice::from_ref(&copied)
        );
        assert_eq!(clipboard.0.lock().unwrap().as_slice(), [copied]);
    }

    #[tokio::test]
    async fn copy_path_refuses_relative_input_before_touching_the_clipboard() {
        let renderer = FakePathRenderer::default();
        let clipboard = FakeClipboard::default();
        assert!(
            copy_path(&renderer, &clipboard, Path::new("relative.txt"))
                .await
                .is_err()
        );
        assert!(renderer.0.lock().unwrap().is_empty());
        assert!(clipboard.0.lock().unwrap().is_empty());
    }
}
