//! `org.templaros.Files.Index1` — the versioned, lossless-path, generation-aware index query
//! surface. Additive to `Files.Search1` (`dbus.rs`), which keeps its exact six signatures and now
//! delegates to the same [`templar_file_index::Index`] core that this interface exposes directly.
//!
//! Contract: `.agent/scratch/de01/F1-contract.md` §9, mirrored in
//! `interfaces/dbus/org.templaros.Files.Index1.xml`. Every JSON payload here carries native path
//! bytes as base64 (`path_b64`/`bytes_b64`) beside a lossy `display` string — the wire never
//! collapses a path to text-only the way `Search1`'s compatibility JSON does.

use crate::{
    dbus::{FileService, failed, json},
    index_lanes::LaneKind,
    observer,
};
use serde_json::{Value, json as jsonval};
use std::os::fd::AsRawFd as _;
use templar_file_index::{
    self as index_core,
    query::base64,
    snapshot::{SnapshotManifest, SnapshotRequest},
};
use zbus::{fdo, object_server::SignalEmitter, zvariant::Fd};

/// An `Index1` error: the name [`index_core::IndexError::dbus_name`] gives it, so a correct client
/// can match `SnapshotRequired`, `Busy`, `Cancelled`, `TooLarge` and `Unavailable` without
/// parsing text.
#[derive(Debug)]
pub enum Index1Error {
    Fdo(fdo::Error),
    Named { name: &'static str, message: String },
}

impl Index1Error {
    pub fn snapshot_required(message: impl Into<String>) -> Self {
        Self::Named {
            name: "org.templaros.Files.Index1.Error.SnapshotRequired",
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Index1Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fdo(error) => write!(formatter, "{error}"),
            Self::Named { message, .. } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for Index1Error {}

impl zbus::DBusError for Index1Error {
    fn name(&self) -> zbus::names::ErrorName<'_> {
        match self {
            Self::Fdo(error) => zbus::DBusError::name(error),
            Self::Named { name, .. } => zbus::names::ErrorName::from_static_str_unchecked(name),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Fdo(error) => zbus::DBusError::description(error),
            Self::Named { message, .. } => Some(message.as_str()),
        }
    }

    fn create_reply(
        &self,
        call: &zbus::message::Header<'_>,
    ) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Fdo(error) => zbus::DBusError::create_reply(error, call),
            Self::Named { message, .. } => {
                zbus::message::Message::error(call, self.name())?.build(&message.as_str())
            }
        }
    }
}

impl From<index_core::IndexError> for Index1Error {
    fn from(error: index_core::IndexError) -> Self {
        let name = error.dbus_name();
        if name.starts_with("org.templaros.") {
            let message = match &error {
                index_core::IndexError::SnapshotRequired { reason } => (*reason).to_owned(),
                other => other.to_string(),
            };
            return Self::Named { name, message };
        }
        match error {
            index_core::IndexError::UnknownRoot(_)
            | index_core::IndexError::RootExists(_)
            | index_core::IndexError::IllegalTransition { .. } => {
                Self::Fdo(fdo::Error::InvalidArgs(error.to_string()))
            }
            other => Self::Fdo(failed(other)),
        }
    }
}

fn root_mutation_error(error: anyhow::Error) -> Index1Error {
    match error.downcast::<index_core::IndexError>() {
        Ok(error) => error.into(),
        Err(error) => Index1Error::Fdo(failed(error)),
    }
}

fn native_root(path_b64: &str) -> Result<index_core::NativePath> {
    let bytes = index_core::query::unbase64(path_b64)
        .filter(|bytes| {
            base64(bytes) == path_b64 && bytes.first() == Some(&b'/') && !bytes.contains(&0)
        })
        .ok_or_else(|| {
            Index1Error::Fdo(fdo::Error::InvalidArgs(
                "invalid absolute native root base64".to_owned(),
            ))
        })?;
    Ok(index_core::NativePath::from_bytes(bytes))
}

type Result<T> = std::result::Result<T, Index1Error>;

fn check_request_bytes(request: &str) -> Result<()> {
    if request.len() > 64 * 1024 {
        return Err(Index1Error::Fdo(fdo::Error::InvalidArgs(
            "index request exceeds 64 KiB".to_owned(),
        )));
    }
    Ok(())
}

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
        "fs_type": volume.fs_type, "fs_uuid": volume.fs_uuid, "fsid": volume.fsid,
        "mount_root": volume.mount_root.as_ref().map(wire_path), "mount_id": volume.mount_id,
        "confidence": volume.confidence,
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
        "offline_reason": status.offline_reason,
        "observed_fingerprint": match &status.offline_reason {
            Some(index_core::OfflineReason::DifferentVolume { observed }) => Some(index_core::volume::fingerprint(observed)),
            _ => None,
        },
        "offline_since_ms": status.offline_since_ms,
        "mounted_elsewhere": status.mounted_elsewhere.as_ref().map(wire_path),
        "nested_in": status.nested_in, "contains": status.contains,
        // The namespace axis and the metadata axis are separate answers. `current` has only ever
        // meant "the set of paths was exhaustive"; this says whether the sizes and times next to
        // them were re-read, which a `sort:size` consumer needs and a filename search does not.
        "metadata": status.metadata.as_str(),
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
        "retained": result.retained,
        "total_files": result.total_files, "total_folders": result.total_folders,
        "queue_ms": result.queue_ms,
        "strategy": result.strategy,
        "roots": result.roots.iter().map(wire_root_status).collect::<Vec<_>>(),
        "stale_metadata_roots": result.stale_metadata_roots.iter().map(|root| root.0)
            .collect::<Vec<_>>(),
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

/// `org.templaros.Files.Index1` — see the module doc and F1-contract.md §9.
#[derive(Debug)]
pub struct IndexApi(pub FileService);

#[zbus::interface(name = "org.templaros.Files.Index1")]
impl IndexApi {
    /// Explicit replacement-volume confirmation, fenced against both the displayed observation
    /// and the current mount. Queue the repair before returning the maintenance result.
    async fn accept_volume(
        &self,
        root_id: i64,
        observed_fingerprint: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<()> {
        if root_id <= 0
            || observed_fingerprint.len() != 64
            || !observed_fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(Index1Error::Fdo(fdo::Error::InvalidArgs(
                "expected a root id and the observed lowercase SHA256 fingerprint".into(),
            )));
        }
        let core = self.0.index.core()?;
        let control = observer::control()
            .ok_or_else(|| Index1Error::Fdo(failed("index watcher is not ready")))?;
        let expected = observed_fingerprint.to_owned();
        self.0
            .lanes
            .run(LaneKind::Maintenance, sender(&header), None, move |cx| {
                cx.check()?;
                core.accept_volume(index_core::RootId(root_id), &expected)?;
                control
                    .send_required(observer::Command::Reconcile(
                        index_core::RootId(root_id),
                        index_core::reconcile::Trigger::Requested,
                    ))
                    .map_err(|error| index_core::IndexError::Io(std::io::Error::other(error)))?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Every root's status plus the core's own diagnostics, so nothing renders results without
    /// knowing how fresh — or how healthy — the index behind them is. An unavailable cache answers
    /// with no roots and `diagnostics.availability` saying why, rather than with an error.
    async fn roots(&self, #[zbus(header)] header: zbus::message::Header<'_>) -> Result<String> {
        note_consumer(&header);
        let slot = self.0.index.slot();
        let core = match slot.get() {
            Ok(core) => core,
            Err(index_core::IndexError::Unavailable { .. }) => {
                let storage = slot.storage();
                let diagnostics = index_core::Diagnostics {
                    path: slot.path().display().to_string(),
                    db_bytes: storage.total(),
                    storage,
                    availability: slot.availability(),
                    ..index_core::Diagnostics::default()
                };
                return json(&jsonval!({
                    "roots": [],
                    "diagnostics": wire_diagnostics(&diagnostics)?,
                    "cache_identity": "",
                }))
                .map_err(Index1Error::Fdo);
            }
            Err(error) => return Err(error.into()),
        };
        let mut diagnostics = core.diagnostics()?;
        if let Some(runtime) = &mut diagnostics.runtime {
            runtime.lanes = self.0.lanes.metrics();
            runtime.snapshots = Some(self.0.lanes.snapshots.metrics());
        }
        json(&jsonval!({
            "roots": diagnostics.roots.iter().map(wire_root_status).collect::<Vec<_>>(),
            "diagnostics": wire_diagnostics(&diagnostics)?,
            "cache_identity": core.cache_identity(),
        }))
        .map_err(Index1Error::Fdo)
    }

    /// Observation tokens are scoped to the caller's unique bus name, not cursor activity.
    async fn observe(
        &self,
        observing: bool,
        observation: u64,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> Result<()> {
        let client = header.sender().ok_or_else(|| {
            Index1Error::Fdo(fdo::Error::InvalidArgs(
                "Observe requires a session-bus unique name".to_owned(),
            ))
        })?;
        let control = observer::control()
            .ok_or_else(|| Index1Error::Fdo(failed("index watcher is not ready")))?;
        let command = if observing {
            // Register before checking the bus owner: loss during the await revokes this flag.
            let alive = control.observation_liveness(client.as_str());
            let bus = zbus::fdo::DBusProxy::new(connection)
                .await
                .map_err(|error| Index1Error::Fdo(failed(error)))?;
            if !bus
                .name_has_owner(client.clone().into())
                .await
                .map_err(Index1Error::Fdo)?
            {
                control.send(observer::Command::Release(client.as_str().to_owned()));
                return Err(Index1Error::Fdo(fdo::Error::Disconnected(
                    "observation caller disconnected".to_owned(),
                )));
            }
            observer::Command::Observe(client.as_str().to_owned(), observation, alive)
        } else {
            observer::Command::Unobserve(client.as_str().to_owned(), observation)
        };
        control
            .send_required(command)
            .map_err(|error| Index1Error::Fdo(failed(error)))
    }

    async fn root_policy(&self, path_b64: &str, expected_hash: &str) -> Result<String> {
        let path = native_root(path_b64)?;
        json(
            &self
                .0
                .index
                .root_policy_fenced(&path, expected_hash)
                .map_err(root_mutation_error)?,
        )
        .map_err(Index1Error::Fdo)
    }

    /// Persist policy first, then let the only watcher perform the asynchronous repair.
    async fn configure_root(
        &self,
        path_b64: &str,
        policy_json: &str,
        expected_hash: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<i64> {
        let control = observer::control()
            .ok_or_else(|| Index1Error::Fdo(failed("index watcher is not ready")))?;
        let path = native_root(path_b64)?;
        let policy: index_core::RootPolicy = serde_json::from_str(policy_json)
            .map_err(|error| Index1Error::Fdo(fdo::Error::InvalidArgs(error.to_string())))?;
        let status = self
            .0
            .index
            .configure_root(&path, policy, expected_hash)
            .map_err(root_mutation_error)?;
        note_consumer(&header);
        control
            .send_required(observer::Command::Reconcile(
                status.root,
                index_core::reconcile::Trigger::PolicyChanged,
            ))
            .map_err(|error| Index1Error::Fdo(failed(error)))?;
        Ok(status.root.0)
    }

    async fn remove_root(&self, path_b64: &str, expected_hash: &str) -> Result<()> {
        let control = observer::control()
            .ok_or_else(|| Index1Error::Fdo(failed("index watcher is not ready")))?;
        let path = native_root(path_b64)?;
        if let Some(root) = self
            .0
            .index
            .forget_root_native(&path, expected_hash)
            .map_err(root_mutation_error)?
        {
            control
                .send_required(observer::Command::ForgetRoot(root))
                .map_err(|error| Index1Error::Fdo(failed(error)))?;
        }
        Ok(())
    }

    /// One JSON argument (`Query`, `query.rs`) so filters/sort/field-mask can grow without a
    /// signature change. Runs on the interactive lane; a `client_tag` makes a newer search from
    /// the same caller cancel this one.
    async fn search(
        &self,
        query_json: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(String, u64, String)> {
        note_consumer(&header);
        check_request_bytes(query_json)?;
        let query: index_core::Query =
            serde_json::from_str(query_json).map_err(|error| Index1Error::Fdo(failed(error)))?;
        let core = self.0.index.core()?;
        let tag = query.client_tag.clone();
        let cleanup_core = core.clone();
        let owner = sender(&header);
        let cleanup_cx = index_core::CallContext::for_owner(owner.clone());
        let ran = self
            .0
            .lanes
            .run_with_cleanup(
                LaneKind::Interactive,
                owner,
                tag,
                move |cx| core.search(&query, cx),
                move |result| cleanup_core.close_cursor(&result.cursor, &cleanup_cx),
            )
            .await?;
        let mut result = ran.value;
        result.queue_ms = ran.queue_ms;
        let generation = result.generation.0;
        let cursor = result.cursor.clone();
        let page_json = json(&wire_search_result(&result)).map_err(Index1Error::Fdo)?;
        Ok((cursor, generation, page_json))
    }

    /// Random access inside a pinned result set. `SnapshotRequired` when the cursor aged out.
    async fn fetch_page(
        &self,
        cursor: &str,
        offset: u32,
        count: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String> {
        note_consumer(&header);
        let core = self.0.index.core()?;
        let cursor = cursor.to_owned();
        let page = self
            .0
            .lanes
            .run(LaneKind::Interactive, sender(&header), None, move |cx| {
                core.fetch_page(&cursor, offset, count, cx)
            })
            .await?
            .value;
        json(&wire_page(&page)).map_err(Index1Error::Fdo)
    }

    /// Best effort: releases a cursor early, if this caller opened it. Expiry and the caller's
    /// departure cover a client that never calls this.
    async fn close_cursor(&self, cursor: &str, #[zbus(header)] header: zbus::message::Header<'_>) {
        if let Some(core) = self.0.index.slot().current() {
            core.close_cursor(cursor, &caller(&header));
        }
    }

    /// A kernel-sealed `TEMPLARIDX1` bulk snapshot: a consistent, streamed picture of selected
    /// roots, consumable without SQLite. Publication fails if kernel sealing is unavailable.
    /// Created, written, sealed and reopened read-only entirely on the bulk lane.
    async fn open_snapshot(
        &self,
        request_json: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(Fd<'static>, String)> {
        note_consumer(&header);
        check_request_bytes(request_json)?;
        let request: SnapshotRequest =
            serde_json::from_str(request_json).map_err(|error| Index1Error::Fdo(failed(error)))?;
        let core = self.0.index.core()?;
        let meter = std::sync::Arc::clone(&self.0.lanes.snapshots);
        let (owned, manifest) = self
            .0
            .lanes
            .run(LaneKind::Bulk, sender(&header), None, move |cx| {
                publish_snapshot(&core, &request, cx, &meter)
            })
            .await?
            .value;
        let manifest_json = json(&manifest).map_err(Index1Error::Fdo)?;
        Ok((Fd::from(owned), manifest_json))
    }

    /// A bounded page of the change log after `seq`. `SnapshotRequired` below the retained floor:
    /// the service never invents continuity it cannot prove. A large page is bulk work.
    async fn changes_since(
        &self,
        seq: u64,
        limit: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String> {
        note_consumer(&header);
        let core = self.0.index.core()?;
        let lane = if limit > 1_000 {
            LaneKind::Bulk
        } else {
            LaneKind::Interactive
        };
        let batch = self
            .0
            .lanes
            .run(lane, sender(&header), None, move |cx| {
                core.changes_since(seq, limit, cx)
            })
            .await?
            .value;
        json(&wire_changes_batch(&batch)).map_err(Index1Error::Fdo)
    }

    /// Schedules a background repair of every listed root (empty means every registered root)
    /// and returns immediately; the caller watches `Roots` or the signals below for progress.
    /// This does not walk anything on the calling task, and it does not block the D-Bus reply on
    /// however long a full reconcile takes — the difference from the old, synchronous
    /// `Search1.IndexRoot`. On an unavailable cache it is the explicit retry: the reopen happens
    /// now, and a cache that opens is brought up and repaired exactly as at startup.
    async fn reconcile(&self, roots_json: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct Request {
            cache_identity: String,
            roots: Vec<index_core::RootId>,
        }
        let slot = self.0.index.slot();
        check_request_bytes(roots_json)?;
        if slot.current().is_none() {
            let slot = slot.clone();
            self.0
                .lanes
                .run(LaneKind::Maintenance, None, None, move |_| slot.retry())
                .await?;
            return Ok(uuid::Uuid::new_v4().to_string());
        }
        let request: Request = serde_json::from_str(roots_json).map_err(|_| {
            Index1Error::snapshot_required(
                "rebuild request needs observed cache identity and roots; refresh roots",
            )
        })?;
        let core = self.0.index.core()?;
        if request.cache_identity != core.cache_identity() {
            return Err(Index1Error::snapshot_required(
                "cache was replaced; refresh roots before requesting rebuild",
            ));
        }
        let targets = if request.roots.is_empty() {
            core.roots()?
                .into_iter()
                .map(|status| status.root)
                .collect()
        } else {
            request.roots
        };
        // Validate every root before queueing any of them: an unknown id is not an acknowledged
        // background job that silently skips a requested target.
        for root in &targets {
            match core.root_status(*root) {
                Ok(_) => {}
                Err(index_core::IndexError::UnknownRoot(_)) => {
                    return Err(Index1Error::snapshot_required(
                        "requested root was removed; refresh roots before rebuilding",
                    ));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let control = observer::control()
            .ok_or_else(|| Index1Error::Fdo(failed("index watcher is not ready")))?;
        for root in targets {
            control
                .send_required(observer::Command::Reconcile(
                    root,
                    index_core::reconcile::Trigger::Requested,
                ))
                .map_err(|error| Index1Error::Fdo(failed(error)))?;
        }
        Ok(uuid::Uuid::new_v4().to_string())
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

/// Register the caller as a consumer of the index.
///
/// `#[zbus(header)]` is stripped by the `interface` macro before the method signature is computed
/// (`zbus_macros::iface::add_method` filters header arguments out of both the dispatch tuple and
/// the introspection XML), so learning who is asking costs no wire change on a frozen interface —
/// verified against the vendored zbus 5.19.0, not assumed.
fn note_consumer(header: &zbus::message::Header<'_>) {
    observer::note_consumer(header.sender().map(zbus::names::UniqueName::as_str));
}

/// The context an `Index1` call runs under: owned by the caller's unique bus name, so its cursors
/// are its own and go when it leaves the bus.
fn caller(header: &zbus::message::Header<'_>) -> index_core::CallContext {
    index_core::CallContext::for_owner(sender(header))
}

/// The caller's unique bus name, the key lanes and cursors are owned by.
fn sender(header: &zbus::message::Header<'_>) -> Option<std::sync::Arc<str>> {
    header
        .sender()
        .map(|name| std::sync::Arc::from(name.as_str()))
}

/// Writes a snapshot into a fresh sealable memfd, seals it against every change, and hands back a
/// read-only descriptor to it. Runs on the bulk lane: the frame is streamed through a bounded
/// buffer and abandoned with `TooLarge`/`Cancelled` before sealing when it exceeds the service's
/// limits.
fn publish_snapshot(
    core: &index_core::Index,
    request: &SnapshotRequest,
    cx: &index_core::CallContext,
    meter: &crate::index_metrics::SnapshotMeter,
) -> std::result::Result<(std::os::fd::OwnedFd, SnapshotManifest), index_core::IndexError> {
    let result = (|| {
        let io = |error: std::io::Error| index_core::IndexError::Io(error);
        let descriptor = rustix::fs::memfd_create(
            "templar-index-snapshot",
            rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
        )
        .map_err(|error| io(error.into()))?;
        let mut file = std::fs::File::from(descriptor);
        meter.track(&file).map_err(io)?;
        let limits = core.config().snapshot_limits;
        let mut manifest = core.write_snapshot(&mut file, request, &limits, cx)?;
        rustix::fs::fcntl_add_seals(
            &file,
            rustix::fs::SealFlags::WRITE
                | rustix::fs::SealFlags::GROW
                | rustix::fs::SealFlags::SHRINK
                | rustix::fs::SealFlags::SEAL,
        )
        .map_err(|error| io(error.into()))?;
        manifest.sealed = true;
        let readonly =
            std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(io)?;
        Ok((readonly.into(), manifest))
    })();
    meter.record(&result);
    result
}

/// A [`index_core::ChangeSink`] that re-emits generation and root-state changes as `Index1`
/// signals. Constructed once the object is hosted, because it needs the live [`SignalEmitter`];
/// `main.rs` wires it into `Index::set_change_sink` immediately after the connection is built and
/// before the watcher starts, so no generation the watcher commits goes unannounced.
///
/// The runtime handle is not a convenience. [`index_core::ChangeSink`] is a *synchronous* trait
/// and its only caller is the watcher's plain `std::thread` loop, where a bare `tokio::spawn`
/// panics — and that panic would unwind out of the index's commit path and kill the watcher
/// thread, leaving a daemon that answers queries and has silently stopped watching. Capturing the
/// handle at wiring time, on the runtime thread, is what makes the call legal from anywhere.
#[derive(Debug, Clone)]
pub struct Index1Signals {
    emitter: SignalEmitter<'static>,
    runtime: tokio::runtime::Handle,
}

impl Index1Signals {
    #[must_use]
    pub fn new(emitter: SignalEmitter<'static>, runtime: tokio::runtime::Handle) -> Self {
        Self { emitter, runtime }
    }
}

impl index_core::ChangeSink for Index1Signals {
    fn generation_advanced(
        &self,
        generation: index_core::Generation,
        changes: &[index_core::Change],
    ) {
        let emitter = self.emitter.clone();
        let count = changes.len() as u32;
        self.runtime.spawn(async move {
            if let Err(error) = IndexApi::generation_advanced(&emitter, generation.0, count).await {
                tracing::warn!(%error, "could not emit GenerationAdvanced");
            }
        });
    }

    fn root_state_changed(&self, status: &index_core::RootStatus) {
        let emitter = self.emitter.clone();
        let root_id = status.root.0;
        let state = status.state.as_str();
        let reason = status.reason.clone().unwrap_or_default();
        self.runtime.spawn(async move {
            if let Err(error) =
                IndexApi::root_state_changed(&emitter, root_id, state, &reason).await
            {
                tracing::warn!(%error, "could not emit RootStateChanged");
            }
        });
    }

    fn metadata_repair_requested(&self, root: index_core::RootId) {
        let Some(control) = observer::control() else {
            tracing::warn!(
                root = root.0,
                "cannot repair metadata: index watcher is not ready"
            );
            return;
        };
        if let Err(error) = control.send_required(observer::Command::Reconcile(
            root,
            index_core::reconcile::Trigger::Requested,
        )) {
            tracing::warn!(%error, root = root.0, "could not queue index metadata repair");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use index_core::ChangeSink as _;

    /// A connection with nobody on the other end, which is all a signal emitter needs.
    ///
    /// `Channel::pair` is an in-process socket, so this costs no bus, no filesystem and no
    /// authentication handshake.
    async fn hosted_emitter() -> (zbus::Connection, SignalEmitter<'static>) {
        let (ours, theirs) = zbus::connection::socket::Channel::pair();
        // The far end is kept alive for the length of the test: a peer that has hung up turns
        // every emission into an error, which is not the thing being tested here.
        std::mem::forget(theirs);
        let connection =
            zbus::connection::Builder::authenticated_socket(ours, zbus::Guid::generate())
                .expect("a p2p builder")
                .p2p()
                .build()
                .await
                .expect("an in-process connection");
        let emitter = SignalEmitter::new(&connection, "/org/templaros/Files")
            .expect("a signal emitter")
            .into_owned();
        (connection, emitter)
    }

    /// The hazard that made wiring these signals up dangerous.
    ///
    /// [`index_core::ChangeSink`] is a synchronous trait and its only production caller is the
    /// watcher's plain `std::thread` loop. A bare `tokio::spawn` panics outside a runtime context,
    /// and that panic would unwind out of the index's commit path and take the watcher thread with
    /// it — leaving a daemon that still answers queries and has silently stopped watching, which
    /// is the worst failure this subsystem has. Capturing the runtime handle at wiring time is
    /// what makes the call legal from a thread the runtime has never heard of.
    ///
    /// Revert `Index1Signals` to `tokio::spawn` and this test fails.
    #[tokio::test]
    async fn the_change_sink_may_be_called_from_a_thread_the_runtime_never_heard_of() {
        let (_connection, emitter) = hosted_emitter().await;
        let signals = Index1Signals::new(emitter, tokio::runtime::Handle::current());

        let from_a_plain_thread = std::thread::spawn(move || {
            signals.generation_advanced(index_core::Generation(7), &[]);
            signals.root_state_changed(&index_core::RootStatus {
                root: index_core::RootId(1),
                path: index_core::NativePath::from_path(std::path::Path::new("/tmp/tree")),
                display: "/tmp/tree".to_owned(),
                volume: index_core::VolumeIdentity::default(),
                state: index_core::RootState::Stale,
                reason: Some("unwatched".to_owned()),
                offline_reason: None,
                offline_since_ms: None,
                mounted_elsewhere: None,
                metadata: index_core::MetadataFreshness::Unverified,
                generation: index_core::Generation(7),
                complete: false,
                entry_count: 0,
                policy_hash: String::new(),
                last_scan_ms: None,
                last_reconcile_started_ms: None,
                last_reconcile_finished_ms: None,
                last_error: None,
                watch: None,
                nested_in: None,
                contains: Vec::new(),
            });
        })
        .join();

        assert!(
            from_a_plain_thread.is_ok(),
            "the watcher thread must be able to announce a generation without panicking"
        );
    }
}
