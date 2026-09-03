//! `org.legeos.Files.Index1` — the versioned, lossless-path, generation-aware index query
//! surface. Additive to `Files.Search1` (`dbus.rs`), which keeps its exact six signatures and now
//! delegates to the same [`lege_file_index::Index`] core that this interface exposes directly.
//!
//! Contract: `.agent/scratch/de01/F1-contract.md` §9, mirrored in
//! `interfaces/dbus/org.legeos.Files.Index1.xml`. Every JSON payload here carries native path
//! bytes as base64 (`path_b64`/`bytes_b64`) beside a lossy `display` string — the wire never
//! collapses a path to text-only the way `Search1`'s compatibility JSON does.

use crate::dbus::{FileService, failed, json};
use lege_file_index::{
    self as index_core,
    query::base64,
    snapshot::{SnapshotManifest, SnapshotRequest},
};
use serde_json::{Value, json as jsonval};
use std::io::{Seek, SeekFrom};
use zbus::{fdo, object_server::SignalEmitter, zvariant::Fd};

/// An `Index1` error: mirrors [`index_core::IndexError::dbus_name`] except for the one name the
/// crate cannot know on its own (`Failed`'s exact text does not matter to a correct client, only
/// its name does).
#[derive(Debug)]
pub enum Index1Error {
    Fdo(fdo::Error),
    SnapshotRequired(String),
}

impl std::fmt::Display for Index1Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fdo(error) => write!(formatter, "{error}"),
            Self::SnapshotRequired(reason) => write!(formatter, "snapshot required: {reason}"),
        }
    }
}

impl std::error::Error for Index1Error {}

impl zbus::DBusError for Index1Error {
    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            Self::Fdo(error) => zbus::DBusError::name(error),
            Self::SnapshotRequired(_) => zbus::names::ErrorName::from_static_str_unchecked(
                "org.legeos.Files.Index1.Error.SnapshotRequired",
            ),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Fdo(error) => zbus::DBusError::description(error),
            Self::SnapshotRequired(reason) => Some(reason.as_str()),
        }
    }

    fn create_reply(&self, call: &zbus::message::Header<'_>) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Fdo(error) => zbus::DBusError::create_reply(error, call),
            Self::SnapshotRequired(reason) => {
                zbus::message::Message::error(call, self.name())?.build(&reason.as_str())
            }
        }
    }
}

impl From<index_core::IndexError> for Index1Error {
    fn from(error: index_core::IndexError) -> Self {
        match error {
            index_core::IndexError::SnapshotRequired { reason } => {
                Self::SnapshotRequired(reason.to_owned())
            }
            index_core::IndexError::UnknownRoot(_)
            | index_core::IndexError::RootExists(_)
            | index_core::IndexError::IllegalTransition { .. } => {
                Self::Fdo(fdo::Error::InvalidArgs(error.to_string()))
            }
            other => Self::Fdo(failed(other)),
        }
    }
}

type Result<T> = std::result::Result<T, Index1Error>;

/// A native path as `Index1` carries it: the authoritative bytes, base64, beside a lossy display
/// string never fed back to the filesystem.
fn wire_path(path: &index_core::NativePath) -> Value {
    jsonval!({ "bytes_b64": base64(path.as_bytes()), "display": path.display() })
}

fn wire_volume(volume: &index_core::VolumeIdentity) -> Value {
    jsonval!({
        "dev_id": volume.dev_id,
        "uuid": volume.uuid,
        "mount_point": volume.mount_point.as_ref().map(wire_path),
        "label": volume.label,
    })
}

fn wire_root_status(status: &index_core::RootStatus) -> Value {
    jsonval!({
        "root": status.root.0,
        "path_b64": base64(status.path.as_bytes()),
        "display": status.display,
        "volume": wire_volume(&status.volume),
        "state": status.state.as_str(),
        "reason": status.reason,
        "generation": status.generation.0,
        "complete": status.complete,
        "entry_count": status.entry_count,
        "policy_hash": status.policy_hash,
        "last_scan_ms": status.last_scan_ms,
        "last_reconcile_started_ms": status.last_reconcile_started_ms,
        "last_reconcile_finished_ms": status.last_reconcile_finished_ms,
        "last_error": status.last_error,
        "watch": status.watch,
    })
}

fn wire_entry_row(row: &index_core::EntryRow) -> Value {
    jsonval!({
        "entry_id": row.entry_id.0,
        "root": row.root.0,
        "parent": row.parent.map(|id| id.0),
        "path": wire_path(&row.path),
        "name": wire_path(&row.name),
        "display": row.display,
        "kind": row.meta.kind.as_i64(),
        "hidden": row.meta.hidden,
        "size_bytes": row.meta.size_bytes,
        "alloc_bytes": row.meta.alloc_bytes,
        "mtime_ns": row.meta.mtime_ns,
        "ctime_ns": row.meta.ctime_ns,
        "mode": row.meta.mode,
        "nlink": row.meta.nlink,
        "object_key": row.meta.object_key.map(|key| jsonval!({ "dev": key.dev, "ino": key.ino })),
        "mime": row.meta.mime,
        "written_generation": row.written_generation.0,
        "last_seen_generation": row.last_seen_generation.0,
        "deleted_generation": row.deleted_generation.map(|generation| generation.0),
    })
}

fn wire_hit(hit: &index_core::Hit) -> Value {
    jsonval!({
        "row": wire_entry_row(&hit.row),
        "rank": hit.rank.as_str(),
        "present": hit.present,
    })
}

fn wire_search_result(result: &index_core::SearchResult) -> Value {
    jsonval!({
        "cursor": result.cursor,
        "generation": result.generation.0,
        "total": result.total,
        "truncated": result.truncated,
        "strategy": result.strategy,
        "roots": result.roots.iter().map(wire_root_status).collect::<Vec<_>>(),
        "hits": result.hits.iter().map(wire_hit).collect::<Vec<_>>(),
    })
}

fn wire_page(page: &index_core::Page) -> Value {
    jsonval!({
        "cursor": page.cursor,
        "generation": page.generation.0,
        "offset": page.offset,
        "hits": page.hits.iter().map(wire_hit).collect::<Vec<_>>(),
        "total": page.total,
        "dropped": page.dropped,
    })
}

fn wire_change(change: &index_core::Change) -> Value {
    jsonval!({
        "seq": change.seq,
        "generation": change.generation.0,
        "root": change.root.0,
        "op": change.op.as_str(),
        "entry_id": change.entry_id.map(|id| id.0),
        "path": wire_path(&change.path),
        "old_path": change.old_path.as_ref().map(wire_path),
        "kind": change.kind.as_i64(),
        "size_bytes": change.size_bytes,
        "mtime_ns": change.mtime_ns,
        "at_ms": change.at_ms,
    })
}

fn wire_changes_batch(batch: &index_core::ChangesBatch) -> Value {
    jsonval!({
        "generation": batch.generation.0,
        "next_seq": batch.next_seq,
        "more": batch.more,
        "changes": batch.changes.iter().map(wire_change).collect::<Vec<_>>(),
    })
}

fn wire_diagnostics(diagnostics: &index_core::Diagnostics) -> Result<Value> {
    serde_json::to_value(diagnostics).map_err(|error| Index1Error::Fdo(failed(error)))
}

/// `org.legeos.Files.Index1` — see the module doc and F1-contract.md §9.
#[derive(Debug)]
pub struct IndexApi(pub FileService);

#[zbus::interface(name = "org.legeos.Files.Index1")]
impl IndexApi {
    /// Every root's status plus the core's own diagnostics, so nothing renders results without
    /// knowing how fresh — or how healthy — the index behind them is.
    async fn roots(&self) -> Result<String> {
        let core = self.0.index.core();
        let roots = core.roots()?;
        let diagnostics = core.diagnostics()?;
        json(&jsonval!({
            "roots": roots.iter().map(wire_root_status).collect::<Vec<_>>(),
            "diagnostics": wire_diagnostics(&diagnostics)?,
        }))
        .map_err(Index1Error::Fdo)
    }

    /// One JSON argument (`Query`, `query.rs`) so filters/sort/field-mask can grow without a
    /// signature change.
    async fn search(&self, query_json: &str) -> Result<(String, u64, String)> {
        let query: index_core::Query =
            serde_json::from_str(query_json).map_err(|error| Index1Error::Fdo(failed(error)))?;
        let result = self.0.index.core().search(&query)?;
        let generation = result.generation.0;
        let cursor = result.cursor.clone();
        let page_json = json(&wire_search_result(&result)).map_err(Index1Error::Fdo)?;
        Ok((cursor, generation, page_json))
    }

    /// Random access inside a pinned result set. `SnapshotRequired` when the cursor aged out.
    async fn fetch_page(&self, cursor: &str, offset: u32, count: u32) -> Result<String> {
        let page = self.0.index.core().fetch_page(cursor, offset, count)?;
        json(&wire_page(&page)).map_err(Index1Error::Fdo)
    }

    /// Best effort: releases a cursor early. Idle expiry covers a client that never calls this.
    async fn close_cursor(&self, cursor: &str) {
        self.0.index.core().close_cursor(cursor);
    }

    /// A sealed `LEGEIDX1` bulk snapshot: a consistent, streamed picture of selected roots,
    /// consumable without SQLite (`snapshot.rs`). The fd is an unlinked temporary file in
    /// `$XDG_RUNTIME_DIR` — this build does not attempt `memfd_create` sealing, and the manifest
    /// says so honestly (`sealed: false`) rather than claiming a guarantee it did not take.
    async fn open_snapshot(&self, request_json: &str) -> Result<(Fd<'static>, String)> {
        let request: SnapshotRequest = serde_json::from_str(request_json)
            .map_err(|error| Index1Error::Fdo(failed(error)))?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let mut file = tempfile::tempfile_in(&runtime_dir)
            .map_err(|error| Index1Error::Fdo(failed(error)))?;
        let manifest: SnapshotManifest = self.0.index.core().write_snapshot(&mut file, &request)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| Index1Error::Fdo(failed(error)))?;
        let owned: std::os::fd::OwnedFd = file.into();
        let manifest_json = json(&manifest).map_err(Index1Error::Fdo)?;
        Ok((Fd::from(owned), manifest_json))
    }

    /// A bounded page of the change log after `seq`. `SnapshotRequired` below the retained floor:
    /// the service never invents continuity it cannot prove.
    async fn changes_since(&self, seq: u64, limit: u32) -> Result<String> {
        let batch = self.0.index.core().changes_since(seq, limit)?;
        json(&wire_changes_batch(&batch)).map_err(Index1Error::Fdo)
    }

    /// Schedules a background repair of every listed root (empty means every registered root)
    /// and returns immediately; the caller watches `Roots` or the signals below for progress.
    /// This does not walk anything on the calling task, and it does not block the D-Bus reply on
    /// however long a full reconcile takes — the difference from the old, synchronous
    /// `Search1.IndexRoot`.
    async fn reconcile(&self, roots_json: &str) -> Result<String> {
        let requested: Vec<index_core::RootId> = serde_json::from_str(roots_json)
            .map_err(|error| Index1Error::Fdo(failed(error)))?;
        let core = self.0.index.core().clone();
        let targets = if requested.is_empty() {
            core.roots()?.into_iter().map(|status| status.root).collect()
        } else {
            requested
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        tokio::task::spawn_blocking(move || {
            for root in targets {
                let policy = match core.root_policy(root) {
                    Ok(policy) => policy,
                    Err(error) => {
                        tracing::warn!(root = root.0, %error, "reconcile: could not read policy");
                        continue;
                    }
                };
                let options = index_core::ScanOptions {
                    policy,
                    ..index_core::ScanOptions::reconcile(index_core::RootPolicy::default_home())
                };
                if let Err(error) = core.reconcile_root(root, options) {
                    tracing::warn!(root = root.0, %error, "background reconcile failed");
                }
            }
        });
        Ok(request_id)
    }

    /// A generation committed: `changes_since`/a fresh `Search` now sees it.
    #[zbus(signal)]
    async fn generation_advanced(
        emitter: &SignalEmitter<'_>,
        generation: u64,
        change_count: u32,
    ) -> zbus::Result<()>;

    /// A root's freshness state changed.
    #[zbus(signal)]
    async fn root_state_changed(
        emitter: &SignalEmitter<'_>,
        root_id: i64,
        state: &str,
        reason: &str,
    ) -> zbus::Result<()>;
}

/// A [`index_core::ChangeSink`] that re-emits generation and root-state changes as `Index1`
/// signals. Constructed once the object is hosted (it needs the live [`SignalEmitter`]); nothing
/// wires this into `Index::set_change_sink` yet in this build — that is the maintenance lane's
/// watcher startup path (`main.rs`), which owns the moment a live connection exists to emit on.
#[derive(Debug, Clone)]
pub struct Index1Signals {
    emitter: SignalEmitter<'static>,
}

impl Index1Signals {
    #[must_use]
    pub fn new(emitter: SignalEmitter<'static>) -> Self {
        Self { emitter }
    }
}

impl index_core::ChangeSink for Index1Signals {
    fn generation_advanced(&self, generation: index_core::Generation, changes: &[index_core::Change]) {
        let emitter = self.emitter.clone();
        let count = changes.len() as u32;
        tokio::spawn(async move {
            if let Err(error) =
                IndexApi::generation_advanced(&emitter, generation.0, count).await
            {
                tracing::warn!(%error, "could not emit GenerationAdvanced");
            }
        });
    }

    fn root_state_changed(&self, status: &index_core::RootStatus) {
        let emitter = self.emitter.clone();
        let root_id = status.root.0;
        let state = status.state.as_str();
        let reason = status.reason.clone().unwrap_or_default();
        tokio::spawn(async move {
            if let Err(error) =
                IndexApi::root_state_changed(&emitter, root_id, state, &reason).await
            {
                tracing::warn!(%error, "could not emit RootStateChanged");
            }
        });
    }
}
