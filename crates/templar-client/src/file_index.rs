//! Store-free Index1 wire models and caller-owned search cursors. Native bytes name files;
//! display strings never do. A search retains generation, root freshness and truncation truth.
use crate::proxies::FilesIndexProxy;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use templar_file_index::{
    NativePath, Query, RootPolicy,
    query::{base64, unbase64},
};

pub use templar_file_index::RootPolicy as IndexRootPolicy;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexWirePath {
    pub bytes_b64: String,
    pub display: String,
}

impl IndexWirePath {
    pub fn native_bytes(&self) -> Result<Vec<u8>> {
        decode_native_bytes(&self.bytes_b64)
    }
    pub fn native_path(&self) -> Result<PathBuf> {
        Ok(NativePath::from_bytes(decode_path(&self.bytes_b64)?).into_path())
    }
}

fn decode_native_bytes(encoded: &str) -> Result<Vec<u8>> {
    let bytes = unbase64(encoded).context("invalid native path base64")?;
    ensure!(base64(&bytes) == encoded, "noncanonical native path base64");
    ensure!(!bytes.contains(&0), "native path contains NUL");
    Ok(bytes)
}

pub fn decode_path(encoded: &str) -> Result<Vec<u8>> {
    let bytes = decode_native_bytes(encoded)?;
    ensure!(bytes.first() == Some(&b'/'), "native path is not absolute");
    Ok(bytes)
}

pub fn encode_path(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    base64(path.as_os_str().as_bytes())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRoot {
    pub root: i64,
    pub path_b64: String,
    pub display: String,
    pub state: String,
    pub reason: Option<String>,
    pub metadata: String,
    pub generation: u64,
    pub complete: bool,
    pub entry_count: u64,
    pub policy_hash: String,
    pub last_scan_ms: Option<i64>,
    pub last_reconcile_started_ms: Option<i64>,
    pub last_reconcile_finished_ms: Option<i64>,
    pub last_error: Option<String>,
    pub watch: Option<templar_file_index::WatchStatus>,
    #[serde(default)]
    pub volume: IndexVolume,
    #[serde(default)]
    pub offline_reason: Option<templar_file_index::OfflineReason>,
    #[serde(default)]
    pub offline_since_ms: Option<i64>,
    #[serde(default)]
    pub mounted_elsewhere: Option<IndexWirePath>,
    #[serde(default)]
    pub observed_fingerprint: Option<String>,
    #[serde(default)]
    pub nested_in: Option<i64>,
    #[serde(default)]
    pub contains: Vec<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexVolume {
    pub dev_id: Option<u64>,
    pub uuid: Option<String>,
    pub label: Option<String>,
    pub fs_type: Option<String>,
    pub fs_uuid: Option<String>,
    pub fsid: Option<String>,
    pub mount_point: Option<IndexWirePath>,
    pub mount_root: Option<IndexWirePath>,
    pub mount_id: Option<u64>,
    pub confidence: templar_file_index::VolumeConfidence,
}

impl IndexRoot {
    pub fn native_path(&self) -> Result<PathBuf> {
        Ok(NativePath::from_bytes(decode_path(&self.path_b64)?).into_path())
    }
    #[must_use]
    pub fn current(&self) -> bool {
        self.state == "current" && self.complete
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDiagnostics {
    pub schema_version: u32,
    pub generation: u64,
    pub planner: String,
    pub fts_available: bool,
    pub change_seq_floor: u64,
    pub change_seq_head: u64,
    pub cursor_count: usize,
    /// Absent from an older service, which only ever answered with an open cache.
    #[serde(default)]
    pub availability: templar_file_index::Availability,
    #[serde(default)]
    pub storage: templar_file_index::StorageBytes,
    #[serde(default)]
    pub arrays: Option<templar_file_index::ArrayStats>,
    #[serde(default)]
    pub runtime: Option<templar_file_index::RuntimeDiagnostics>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRoots {
    pub roots: Vec<IndexRoot>,
    pub diagnostics: IndexDiagnostics,
    pub cache_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub entry_id: i64,
    pub root: i64,
    pub parent: Option<i64>,
    pub path: IndexWirePath,
    pub name: IndexWirePath,
    pub display: String,
    pub kind: i64,
    pub hidden: bool,
    pub size_bytes: u64,
    pub mtime_ns: i64,
    pub mime: Option<String>,
    pub written_generation: u64,
    pub deleted_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexHit {
    pub row: IndexEntry,
    pub rank: String,
    pub present: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSearch {
    pub cursor: String,
    pub generation: u64,
    pub total: Option<u64>,
    pub truncated: bool,
    #[serde(default)]
    pub retained: Option<u32>,
    #[serde(default)]
    pub total_files: Option<u64>,
    #[serde(default)]
    pub total_folders: Option<u64>,
    #[serde(default)]
    pub queue_ms: u32,
    pub strategy: String,
    pub roots: Vec<IndexRoot>,
    pub stale_metadata_roots: Vec<i64>,
    pub hits: Vec<IndexHit>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPage {
    pub cursor: String,
    pub generation: u64,
    pub offset: u32,
    pub hits: Vec<IndexHit>,
    pub total: Option<u64>,
    pub dropped: u32,
}

impl IndexSearch {
    #[must_use]
    pub fn current(&self) -> bool {
        !self.roots.is_empty()
            && self
                .roots
                .iter()
                .all(|root| root.current() && root.metadata == "verified")
            && self.stale_metadata_roots.is_empty()
    }
    pub fn validate_page(&self, page: &IndexPage, offset: u32, count: u32) -> Result<()> {
        ensure!(
            page.cursor == self.cursor && page.generation == self.generation,
            "index cursor changed generation or identity; search again"
        );
        let absent = page.hits.iter().filter(|hit| !hit.present).count();
        ensure!(
            page.offset == offset
                && page.total == self.total
                && page.dropped as usize >= absent
                && (page.hits.len() - absent).saturating_add(page.dropped as usize)
                    <= count.clamp(1, 1000) as usize,
            "inconsistent index page; search again"
        );
        Ok(())
    }
}

/// Process-unique observation token. Keep it stable for one visible view, not one request.
#[must_use]
pub fn observation_token() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A visible-page observation; disconnection also releases it server-side. Old-token release
/// cannot detach a newer page or one reacquired after producer restart.
#[derive(Debug)]
pub struct Observation {
    proxy: FilesIndexProxy<'static>,
    token: u64,
    closed: AtomicBool,
    runtime: tokio::runtime::Handle,
}

impl Observation {
    pub async fn acquire(connection: &zbus::Connection) -> Result<Self> {
        let proxy = FilesIndexProxy::new(connection).await?;
        let token = observation_token();
        // A cancelled caller detaches this task, whose eventual owned output is dropped only
        // after the acquire reply. Release cannot overtake an in-flight successful acquire.
        tokio::spawn(async move {
            let observation = Self {
                proxy,
                token,
                closed: AtomicBool::new(false),
                runtime: tokio::runtime::Handle::current(),
            };
            observation.proxy.observe(true, token).await?;
            Ok::<_, anyhow::Error>(observation)
        })
        .await
        .context("acquiring Index1 observation")?
    }

    pub async fn close(&self) -> Result<()> {
        if !self.closed.load(Ordering::Acquire) {
            self.proxy.observe(false, self.token).await?;
            self.closed.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) {
            let proxy = self.proxy.clone();
            let token = self.token;
            self.runtime.spawn(async move {
                let _ = proxy.observe(false, token).await;
            });
        }
    }
}

/// The caller keeps this value while results remain visible, fetches subsequent pages in order,
/// and explicitly closes it on replacement/dismissal. Drop is a cancellation safety net only.
#[derive(Debug)]
pub struct SearchSession {
    proxy: FilesIndexProxy<'static>,
    result: IndexSearch,
    closed: AtomicBool,
    observation: Observation,
    tag: String,
}

#[derive(Debug)]
pub enum IndexCallError {
    SnapshotRequired(String),
    Busy,
    Cancelled,
    TooLarge,
    Unavailable(String),
    Other(anyhow::Error),
}
impl std::fmt::Display for IndexCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SnapshotRequired(reason) => write!(f, "results need refreshing: {reason}"),
            Self::Busy => f.write_str("index is busy"),
            Self::Cancelled => f.write_str("search cancelled"),
            Self::TooLarge => f.write_str("index request exceeds its limit"),
            Self::Unavailable(reason) => {
                write!(f, "index unavailable: {reason}; Open Settings → Files")
            }
            Self::Other(error) => std::fmt::Display::fmt(error, f),
        }
    }
}
impl std::error::Error for IndexCallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
impl From<anyhow::Error> for IndexCallError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}
impl From<serde_json::Error> for IndexCallError {
    fn from(error: serde_json::Error) -> Self {
        Self::Other(error.into())
    }
}
impl From<zbus::Error> for IndexCallError {
    fn from(error: zbus::Error) -> Self {
        if let zbus::Error::MethodError(name, message, _) = &error {
            let message = message.clone().unwrap_or_default();
            match name.as_str() {
                "org.templaros.Files.Index1.Error.SnapshotRequired" => {
                    return Self::SnapshotRequired(message);
                }
                "org.templaros.Files.Index1.Error.Busy" => return Self::Busy,
                "org.templaros.Files.Index1.Error.Cancelled" => return Self::Cancelled,
                "org.templaros.Files.Index1.Error.TooLarge" => return Self::TooLarge,
                "org.templaros.Files.Index1.Error.Unavailable" => {
                    return Self::Unavailable(message);
                }
                _ => {}
            }
        }
        Self::Other(error.into())
    }
}

struct CancelAwait(std::sync::Arc<AtomicBool>);
impl Drop for CancelAwait {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl SearchSession {
    pub async fn search(
        connection: &zbus::Connection,
        query: &Query,
    ) -> std::result::Result<Self, IndexCallError> {
        let proxy = FilesIndexProxy::new(connection).await?;
        let observation = Observation::acquire(connection).await?;
        let mut query = query.clone();
        let tag = query
            .client_tag
            .clone()
            .unwrap_or_else(|| format!("search-{}", observation_token()));
        query.client_tag = Some(tag.clone());
        let query_json = serde_json::to_string(&query)?;
        let runtime = observation.runtime.clone();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let _await_cancel = CancelAwait(cancel.clone());
        // Let cancellation detach the in-flight request; its unclaimed owned result then closes
        // the cursor as well as the observation, instead of waiting for server idle expiry.
        runtime
            .spawn(async move {
                if cancel.load(Ordering::Acquire) {
                    return Err(IndexCallError::Cancelled);
                }
                let (cursor, generation, json) = match proxy.search(&query_json).await {
                    Err(error)
                        if matches!(IndexCallError::from(error.clone()), IndexCallError::Busy) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        if cancel.load(Ordering::Acquire) {
                            return Err(IndexCallError::Cancelled);
                        }
                        proxy.search(&query_json).await?
                    }
                    result => result?,
                };
                let result = match serde_json::from_str::<IndexSearch>(&json) {
                    Ok(result) if result.cursor == cursor && result.generation == generation => {
                        result
                    }
                    Ok(_) => {
                        let _ = proxy.close_cursor(&cursor).await;
                        return Err(IndexCallError::SnapshotRequired(
                            "Index1.Search returned inconsistent cursor/generation".into(),
                        ));
                    }
                    Err(error) => {
                        let _ = proxy.close_cursor(&cursor).await;
                        return Err(IndexCallError::Other(
                            anyhow::Error::new(error).context("decoding Index1.Search"),
                        ));
                    }
                };
                Ok(Self {
                    proxy,
                    result,
                    closed: AtomicBool::new(false),
                    observation,
                    tag,
                })
            })
            .await
            .context("acquiring Index1 search session")
            .map_err(IndexCallError::Other)?
    }

    #[must_use]
    pub fn result(&self) -> &IndexSearch {
        &self.result
    }

    #[must_use]
    pub fn retained(&self) -> u32 {
        self.result.retained.unwrap_or_else(|| {
            // Older servers did not expose a retained cap. Their initial page is the only
            // proven reachable range when truncated; never advertise invented rows.
            if self.result.truncated {
                self.result.hits.len() as u32
            } else {
                self.result
                    .total
                    .unwrap_or(self.result.hits.len() as u64)
                    .min(u64::from(u32::MAX)) as u32
            }
        })
    }
    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub async fn fetch_page(
        &self,
        offset: u32,
        count: u32,
    ) -> std::result::Result<IndexPage, IndexCallError> {
        if self.closed.load(Ordering::Acquire) || self.result.cursor.is_empty() {
            return Err(IndexCallError::SnapshotRequired(
                "index cursor is closed".into(),
            ));
        }
        let json = match self
            .proxy
            .fetch_page(&self.result.cursor, offset, count)
            .await
        {
            Err(error) if matches!(IndexCallError::from(error.clone()), IndexCallError::Busy) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                self.proxy
                    .fetch_page(&self.result.cursor, offset, count)
                    .await?
            }
            result => result?,
        };
        let page: IndexPage = serde_json::from_str(&json).context("decoding Index1.FetchPage")?;
        self.result
            .validate_page(&page, offset, count)
            .map_err(|error| IndexCallError::SnapshotRequired(error.to_string()))?;
        if page.dropped != 0 {
            return Err(IndexCallError::SnapshotRequired(
                "rows changed inside the retained cursor; search again".into(),
            ));
        }
        Ok(page)
    }

    pub async fn close(&self) -> Result<()> {
        let cursor_result =
            if !self.closed.load(Ordering::Acquire) && !self.result.cursor.is_empty() {
                self.proxy
                    .close_cursor(&self.result.cursor)
                    .await
                    .map_err(anyhow::Error::from)
            } else {
                Ok(())
            };
        if cursor_result.is_ok() {
            self.closed.store(true, Ordering::Release);
        }
        let observation_result = self.observation.close().await;
        cursor_result?;
        observation_result?;
        Ok(())
    }
}

impl Drop for SearchSession {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::Acquire) && !self.result.cursor.is_empty() {
            let proxy = self.proxy.clone();
            let cursor = self.result.cursor.clone();
            self.observation.runtime.spawn(async move {
                let _ = proxy.close_cursor(&cursor).await;
            });
        }
    }
}

pub async fn roots(proxy: &FilesIndexProxy<'_>) -> Result<IndexRoots> {
    Ok(serde_json::from_str(&proxy.roots().await?)?)
}

pub async fn root_policy(
    proxy: &FilesIndexProxy<'_>,
    path: &std::path::Path,
    expected_hash: &str,
) -> Result<RootPolicy> {
    Ok(serde_json::from_str(
        &proxy.root_policy(&encode_path(path), expected_hash).await?,
    )?)
}

pub async fn configure_root(
    proxy: &FilesIndexProxy<'_>,
    path: &std::path::Path,
    policy: &RootPolicy,
    expected_hash: &str,
) -> Result<i64> {
    Ok(proxy
        .configure_root(
            &encode_path(path),
            &serde_json::to_string(policy)?,
            expected_hash,
        )
        .await?)
}

pub async fn remove_root(
    proxy: &FilesIndexProxy<'_>,
    path: &std::path::Path,
    expected_hash: &str,
) -> Result<()> {
    Ok(proxy.remove_root(&encode_path(path), expected_hash).await?)
}

/// Copy a native indexed path without first substituting its lossy display string.
pub async fn render_native_path(
    connection: &zbus::Connection,
    path: &std::path::Path,
    form: &str,
) -> Result<String> {
    use std::os::unix::ffi::OsStrExt as _;
    let proxy = crate::proxies::FilesActionsProxy::new(connection).await?;
    Ok(proxy
        .render_native(&base64(path.as_os_str().as_bytes()), form)
        .await?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_index_error_name_maps_to_its_typed_variant() {
        use super::IndexCallError;
        for suffix in [
            "SnapshotRequired",
            "Busy",
            "Cancelled",
            "TooLarge",
            "Unavailable",
            "Unsupported",
        ] {
            let name = format!("org.templaros.Files.Index1.Error.{suffix}");
            let call = zbus::Message::method_call("/org/templaros/Files", "Search")
                .unwrap()
                .build(&"")
                .unwrap();
            let reply = zbus::Message::error(&call.header(), name.as_str())
                .unwrap()
                .build(&"observed reason")
                .unwrap();
            let error = zbus::Error::MethodError(
                name.try_into().unwrap(),
                Some("observed reason".into()),
                reply,
            );
            let mapped = IndexCallError::from(error);
            match suffix {
                "SnapshotRequired" => assert!(
                    matches!(mapped, IndexCallError::SnapshotRequired(ref reason) if reason == "observed reason")
                ),
                "Busy" => assert!(matches!(mapped, IndexCallError::Busy)),
                "Cancelled" => assert!(matches!(mapped, IndexCallError::Cancelled)),
                "TooLarge" => assert!(matches!(mapped, IndexCallError::TooLarge)),
                "Unavailable" => assert!(
                    matches!(mapped, IndexCallError::Unavailable(ref reason) if reason == "observed reason")
                ),
                _ => assert!(matches!(mapped, IndexCallError::Other(_))),
            }
        }
    }
    use super::*;

    #[test]
    fn lossless_native_paths_do_not_open_the_lossy_display_or_accept_malformed_base64() {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = b"/tmp/non-utf8-\xff ; $(command)";
        let path = IndexWirePath {
            bytes_b64: base64(bytes),
            display: "/tmp/not-the-file".to_owned(),
        };
        assert_eq!(path.native_path().unwrap().as_os_str().as_bytes(), bytes);
        assert!(decode_path("not!base64").is_err());
        assert!(decode_path("L3RtcA").is_err());
        assert!(decode_path(&base64(b"/tmp/\0bad")).is_err());
    }

    #[test]
    fn cursor_validation_rejects_restart_generation_identity_and_total_drift() {
        let search = IndexSearch {
            cursor: "old".to_owned(),
            generation: 4,
            total: Some(500),
            truncated: true,
            retained: Some(100),
            total_files: None,
            total_folders: None,
            queue_ms: 0,
            strategy: "arrays".to_owned(),
            roots: Vec::new(),
            stale_metadata_roots: Vec::new(),
            hits: Vec::new(),
        };
        let mut page = IndexPage {
            cursor: "old".to_owned(),
            generation: 4,
            offset: 100,
            hits: Vec::new(),
            total: Some(500),
            dropped: 0,
        };
        assert!(search.validate_page(&page, 100, 100).is_ok());
        page.generation = 5;
        assert!(search.validate_page(&page, 100, 100).is_err());
        page.generation = 4;
        page.cursor = "restarted".to_owned();
        assert!(search.validate_page(&page, 100, 100).is_err());
        page.cursor = "old".to_owned();
        page.total = Some(100);
        assert!(search.validate_page(&page, 100, 100).is_err());
        assert!(
            !search.current(),
            "an uncovered response must not claim current truth"
        );
    }

    #[test]
    fn search_truth_does_not_hide_unverified_metadata_when_repair_was_not_requested() {
        let root: IndexRoot = serde_json::from_value(serde_json::json!({
            "root": 1, "path_b64": base64(b"/home"), "display": "/home", "state": "current",
            "metadata": "unverified", "generation": 4, "complete": true, "entry_count": 1,
            "policy_hash": "p"
        }))
        .unwrap();
        let mut search = IndexSearch {
            cursor: "pin".to_owned(),
            generation: 4,
            total: Some(1),
            truncated: false,
            retained: Some(1),
            total_files: None,
            total_folders: None,
            queue_ms: 0,
            strategy: "sql".to_owned(),
            roots: vec![root],
            stale_metadata_roots: Vec::new(),
            hits: Vec::new(),
        };
        assert!(!search.current());
        search.roots[0].metadata = "verified".to_owned();
        assert!(search.current());
    }

    #[test]
    fn cursor_validation_counts_absent_hits_once_and_rejects_unreported_loss() {
        let search = IndexSearch {
            cursor: "pin".to_owned(),
            generation: 4,
            total: Some(1),
            truncated: false,
            retained: Some(1),
            total_files: None,
            total_folders: None,
            queue_ms: 0,
            strategy: "sql".to_owned(),
            roots: Vec::new(),
            stale_metadata_roots: Vec::new(),
            hits: Vec::new(),
        };
        let hit = IndexHit {
            rank: "exact".to_owned(),
            present: false,
            row: IndexEntry {
                entry_id: 1,
                root: 1,
                parent: None,
                path: IndexWirePath {
                    bytes_b64: base64(b"/home/removed"),
                    display: "/home/removed".to_owned(),
                },
                name: IndexWirePath {
                    bytes_b64: base64(b"removed"),
                    display: "removed".to_owned(),
                },
                display: "/home/removed".to_owned(),
                kind: 0,
                hidden: false,
                size_bytes: 0,
                mtime_ns: 0,
                mime: None,
                written_generation: 4,
                deleted_generation: Some(5),
            },
        };
        let mut page = IndexPage {
            cursor: "pin".to_owned(),
            generation: 4,
            offset: 0,
            hits: vec![hit],
            total: Some(1),
            dropped: 1,
        };
        assert!(search.validate_page(&page, 0, 1).is_ok());
        page.dropped = 0;
        assert!(search.validate_page(&page, 0, 1).is_err());
    }
}
