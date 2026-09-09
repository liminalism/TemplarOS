# One file index for TemplarOS, FSearch, and Silvaview-rs

**Status:** implementation proposal  
**Date:** 2026-09-03  
**Scope:** `templar-filed`, TemplarOS search surfaces, an FSearch client fork, Silvaview-rs, and future
per-user filename/metadata consumers

## 1. Decision in one page

TemplarOS should have exactly one per-user process that walks indexed roots, owns filesystem watches,
persists derived file metadata, and answers queries. That process should remain `templar-filed`. Its
current `Files.Search1` implementation is already the right ownership boundary, but the current
implementation is only a bounded snapshot prototype: it deletes and rebuilds a root, stores at most
50,000 entries, performs no monitoring, and skips the default-root scan on subsequent startups once
it finds a persisted root. It therefore avoids repeated startup work at the cost of becoming stale.

The next implementation should extract the index core within the TemplarOS workspace, put rebuildable
index data in a dedicated SQLite database, add one serialized writer plus WAL readers, and make
`templar-filed` maintain the database continuously. Linux filesystem events make ordinary operation
incremental. A bounded reconciliation pass repairs watcher overflow, changes made while the daemon
was stopped, unsupported filesystems, and removable volumes returning after absence. Applications
must use a versioned D-Bus/client contract; they must not open the database file.

This changes the interim decision in
`docs/planning/window-groups-and-application-integration.md` section 4.2, lines 208–211. Two indexes
are no longer accepted even as the intended steady state. `Files.Search1` remains for compatibility,
but delegates to the new index core. FSearch becomes the full-window expert search UI over the same
index, while the Files object's partial-screen search remains the integrated UI over that service.

Do **not** share FSearch's `fsearch.db`. Upstream FSearch 0.3 has useful scanning, monitoring,
coalescing, and query code, but it is a GTK application with an in-process index rather than a
daemon or public library. Its database is a private, host-endian, versioned binary snapshot; even a
reader takes a non-blocking exclusive file lock, and saving replaces the entire file. The installed
development host has FSearch 0.2.3, which predates upstream 0.3 filesystem monitoring.
Forking FSearch is still worthwhile, but as a **remote-index UI fork**: remove its root scanner and
private database ownership, preserve its mature query/results UX, and adapt it to TemplarOS's index
API. Starting by daemonizing the FSearch engine would import GTK/GLib threading, a private ABI, and a
large C/GPL maintenance surface into the system service without solving durable multi-client
storage.

Silvaview-rs should likewise stop treating its display-oriented `FileTree` as the scanner's only
output. On TemplarOS it should request a consistent bulk snapshot from `templar-filed`, convert canonical
index entries through its existing tree builder, and retain its current scanner only as an explicit
fallback and as a possible future ingestion backend on platforms without the service. Rebuilding a
treemap projection in memory is acceptable; rereading every inode from storage on every Silvaview
launch is not.

The resulting ownership is:

```text
filesystems and mounted volumes
             |
             | initial scan + watch events + reconciliation
             v
  templar-filed / templar-file-index core
  (only DB writer and only watch owner)
             |
             +-- Files.Search1 compatibility API --> Home / templar-files
             +-- Files.Index1 query API ----------> FSearch fork / CLI / future clients
             +-- consistent snapshot FD ---------> Silvaview-rs treemap projection
             `-- generation/change signals ------> all live consumers
```

## 2. What exists today

### 2.1 TemplarOS already persists an index, but does not maintain it

`services/templar-filed/src/search.rs` defines an `Entry` with path, directory flag, logical size,
modification time, and optional MIME type. `Index::index_root` walks synchronously, deletes all rows
for the root, and inserts a replacement snapshot in one transaction. The walk does not follow
symlinks, skips source-control/cache/build directories and trash, and is bounded to depth 24 and
50,000 entries. `Index::search` uses a leading-wildcard SQLite `LIKE` over folded name and path,
then ranks exact, prefix, substring, and path-only matches in Rust.

The tables live in the general session database through the additive `0003-files` migration:
`search_roots`, `search_entries`, and `saved_queries`. `Files.Search1` exposes root registration,
whole-root reindexing, search, and saved queries over D-Bus. The shell parses returned JSON into the
same actionable rows used for directory browsing.

The startup policy in `services/templar-filed/src/main.rs` is important. If the default home root has
never been indexed, the daemon claims its bus name and scans home on a blocking worker. If the root
is already present, startup logs that it is reusing the index and performs no scan. This is fast,
and the database survives application restarts, but there is no watcher, event log, TTL, or periodic
reconciliation. The unit test even establishes that a newly created file stays absent until an
explicit reindex. A successful old scan can consequently be served indefinitely.

Other constraints are already correct and should be kept:

- `templar-filed` is a per-user D-Bus service, so one user has one policy and one index owner.
- Its systemd unit already classifies it as heavy I/O (`IOWeight=300`) and keeps it available under
  memory pressure.
- Search is names, paths, and metadata, not file contents. This keeps indexing useful without
  silently reading document contents or expanding the privacy and battery cost.
- File operations already flow through `templar-filed`; successful rename, move, trash, restore, copy,
  and delete operations can update the index immediately instead of waiting for a watcher.

### 2.2 Silvaview-rs has a fast scanner, not an index

Silvaview starts a scan after its window/GPU setup and starts another when the user chooses a drive.
It holds no durable cache. The background scanner returns a `FileTree` through a temporary
process-global handoff and the data disappears on exit.

There are two ingestion paths. On a Windows NTFS drive root, the fast path parses the MFT directly;
otherwise a parallel `jwalk` traversal collects paths, logical sizes, and directory flags. The MFT
implementation has valuable correctness work for fragmented extents, parent sequence validation,
and unresolved nodes, but it is a snapshot reader, not a USN Journal consumer. The portable path
has no watcher and records neither modification time nor stable filesystem identity.

The reusable seam is between scanning and tree construction:

- `src/scanner/mod.rs` chooses the scanner and currently returns `FileTree`.
- `src/scanner/types.rs::RawFileEntry` is close to an ingestion record but too small for durable
  identity or incremental change tracking.
- `src/tree/mod.rs::build_tree` turns records into the arena consumed by layout and rendering.
- `src/tree/arena.rs` and `src/tree/aggregate.rs` are presentation structures. They store arena
  links, aggregate sizes, and size-sorted children, not stable IDs, full metadata, or index
  generations.

The `FileTree` must remain a client-side projection. Making it the interchange format would discard
the identity and timestamps needed to update an entry and would couple every search consumer to a
treemap's sort and aggregation policy.

### 2.3 FSearch has the desired UX and useful algorithms, but no shared service

The upstream repository was inspected at commit
[`05ed650ab`](https://github.com/cboxdoerfer/fsearch/tree/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3).
FSearch is a C/GTK3 `GtkApplication`. Its static `libfsearch` target mixes scanning, database,
query, preferences, results, and UI sources and links GTK/GIO, PCRE2, and ICU. Internal headers expose
database/query functions, but they are not installed as a public ABI and their data types retain
GLib/GTK ownership assumptions.

Upstream 0.3 can monitor an included root. It prefers fanotify, falls back to inotify, queues and
coalesces events, processes batches once per second, and polls for an absent root to reappear. These
are good reference behaviours. The upstream documentation also states the limits: a mark/watch per
directory, kernel watch limits, unsupported network filesystems, and an automatic full rescan of a
monitored root when FSearch launches to repair changes made while it was absent. See
[Database update](https://github.com/cboxdoerfer/fsearch/wiki/Database-update).

Its on-disk format is not a database contract. In
[`fsearch_database_file.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_database_file.c)
it is identified by `FSDB`, currently major version 6, and stores configuration, entries, parent
indexes, optional metadata, and precomputed sorted arrays. Opening uses non-blocking exclusive
`flock`; saving writes `fsearch.db.tmp`, unlinks the prior database, and renames the temporary file.
Monitor changes update the live in-memory index, while durable save remains a whole-snapshot action.

The CLI is also not a query API. `fsearch --search <pattern>` populates/opens a GUI window;
`--update-database` refreshes the database. It does not emit structured results. Its D-Bus use finds
an existing GTK application instance and activates the update action, not a public search service.
The upstream README explicitly recommends other tools to people seeking a CLI and describes console
and alternative frontends as a future ambition. FSearch's
[search syntax](https://github.com/cboxdoerfer/fsearch/wiki/Search-syntax) is nevertheless a useful
compatibility target after basic shared indexing works.

## 3. Target architecture

### 3.1 Keep `templar-filed` as the one owner

Creating `templar-indexd` immediately would add a second service lifecycle, bus dependency, packaging
unit, resource policy, and failure mode while `templar-filed` would still proxy most searches. Keep the
daemon boundary and extract implementation structure inside it:

1. Add a pure `crates/templar-file-index` crate for schema types, query planning, entry mutation,
   reconciliation logic, and SQLite access.
2. Keep root policy, saved queries, and user choices in the authoritative TemplarOS state store.
3. Put derived entries, search acceleration data, root scan checkpoints, and the bounded change log
   in `$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3`. Add `CacheDirectory=templaros` to the systemd
   unit. A deleted cache is rebuilt; a deleted state database is a loss of user choices.
4. Give the derived database one writer task/thread and separate read connections in WAL mode.
   The current `templar_state::Store` intentionally serializes every domain through one mutex, which
   is excellent for authoritative desktop transactions but wrong for a million-row scan competing
   with file operations and interactive searches.
5. Host scanning, monitoring, mount tracking, and the new D-Bus interface in `templar-filed`. Do not
   allow clients to know or open the SQLite path.

If a later non-TemplarOS product needs the daemon, the pure crate can acquire a small Unix-socket or
named-pipe host without changing the TemplarOS contract. That extraction should follow a second real
consumer, not precede the TemplarOS and Silvaview integrations.

### 3.2 Canonical model and schema

The schema needs to represent directory entries, stable filesystem objects, and freshness. A
minimum v1 model is:

- **Root:** numeric ID; configured path as native bytes plus display text; volume identity; mount
  boundary policy; include/exclude policy hash; online/freshness state; last complete scan
  generation; last reconciliation time; last watcher error.
- **Entry:** numeric index ID; root ID; parent entry ID; native name bytes and display name; kind
  (regular, directory, symlink, other); logical and allocated bytes; mtime/ctime nanoseconds; mode
  and hidden flags; stable object key where available; last-seen scan generation.
- **Object key:** on Unix, device plus inode with explicit reuse limitations; on NTFS, volume serial
  plus MFT record number and sequence number. Paths remain the fallback identity where a backend
  cannot supply a stable key.
- **Change:** monotonically increasing sequence; committed generation; add/remove/move/metadata
  operation; entry/object IDs and enough old location data for a client to update a projection.
- **Metadata:** schema version, current committed generation, scanner/backend versions, and index
  policy version.

Paths must not be converted to lossy UTF-8 at the storage boundary. The current implementation uses
`to_string_lossy()` and D-Bus string arguments; this is acceptable only for the compatibility API.
Index1 should carry native Unix bytes (D-Bus `ay`, or base64 in explicitly versioned JSON) plus a
separate display string. FSearch and GTK can render the display value; file operations use native
bytes. A Windows host will need an equivalent lossless UTF-16 representation in its native IPC.

Model hard links explicitly enough that clients can choose whether aggregation counts directory
links or unique allocation. A path/parent row is not the same thing as a filesystem object. Store a
`file_key` and link count even if v1 treemaps retain today's path-based logical-size behaviour.
Symlinks are indexed as entries and never followed. Crossing a mount boundary is a per-root option,
defaulting off for `/` and on only for explicitly selected mounted-volume roots.

For substring performance, do not rely on `LIKE '%term%'`; the ordinary B-tree name index cannot
accelerate it. Prototype SQLite FTS5's trigram tokenizer over display name and path, retaining the
normal table as authority. Exact/prefix terms and metadata predicates should have normal indexes.
Regex and wildcards need a bounded candidate phase before PCRE2/regex evaluation. Keep the query
planner behind a trait so a measured need can replace FTS with sorted/memory-mapped arrays without
changing IPC.

### 3.3 Freshness and incremental maintenance

Linux does not offer a universal persistent per-user change journal. Inotify/fanotify can cover the
daemon's lifetime but cannot prove what happened while it was stopped, and network/removable
filesystems may not support monitoring. “Incremental” must therefore be an explicit state machine,
not a promise that a watcher makes the database eternally correct.

For every online root:

1. Load and serve the last committed snapshot immediately, with its freshness state and generation.
2. Arm a watch on the root before scanning, install descendant watches as traversal discovers
   directories, and queue events during the scan. Record the scan epoch and any event loss; a
   recursive inotify backend cannot watch descendants before it has discovered them.
3. If the root/policy/backend identity is new, perform an initial scan into a new generation. Do not
   delete the visible old generation first.
4. Commit scan changes atomically, then apply queued events in order. Only then mark the root
   `current`.
5. Coalesce ordinary events into small transactions (target 250–1000 ms) and advance the generation
   once per committed batch.
6. Apply successful `templar-filed` operations directly after filesystem commit. Deduplicate the later
   watcher event by stable key/path and observed metadata.
7. On queue overflow (including explicit `IN_Q_OVERFLOW`), ambiguous rename ordering, watcher
   failure, root disappearance, policy change, or detected inconsistency, mark the affected subtree
   or root `stale`/`offline` and schedule reconciliation. Do not interpret later events as complete
   history and never silently claim freshness.
8. Reconcile after daemon downtime and periodically at low priority. A backend may safely skip work
   only when it can prove a subtree unchanged. Otherwise it must enumerate that subtree, but this
   work is background repair shared by all clients rather than a startup scan in every application.
9. Garbage-collect unseen rows only when a complete scan generation commits. A partial or
   permission-limited scan must not turn unreadable data into deletions.

Expose at least `empty`, `building`, `current`, `catching-up`, `stale`, `offline`, and
`permission-limited`. Every query/snapshot response carries its generation and root states. UIs can
show “results may be out of date” without blocking. The last known snapshot of an offline removable
drive remains searchable, with hits visibly marked offline.

### 3.4 Client contract

Keep `org.templaros.Files.Search1` unchanged initially. Its methods delegate to the new core; its JSON
continues to be lossy/compatibility data for current clients. Add a versioned
`org.templaros.Files.Index1` interface with four workloads:

- `Roots` returns IDs, lossless locations, volume identity, policy, freshness, counts, and
  generation.
- `Search` accepts query text, root IDs, kind/size/time filters, sort, field mask, and page size; it
  returns a cursor plus typed hits, a stable row count where available, and the generation used.
  `FetchPage` provides random page access within that immutable result generation so a virtualized
  FSearch table can scroll, sort, and preserve selection. Cursors expire and never drift to a newer
  generation in place.
- `OpenSnapshot` returns a sealed read-only Unix FD and manifest for a consistent, streamed snapshot
  of selected roots/fields. This avoids millions of JSON objects and D-Bus message-size limits for
  Silvaview. The framing should be endian/version specified, checksummed, and consumable without
  knowledge of SQLite.
- `ChangesSince` returns bounded change batches after a sequence, while `GenerationAdvanced` and
  `RootStateChanged` signals tell live clients when to ask. If the requested sequence has aged out,
  the service says `SnapshotRequired` rather than inventing continuity.

Add the same surface to `templar-client`, and add `templar files index status`, `templar files search`, and
`templar files index reconcile` commands. Search should support human output, JSON, and NUL-delimited
paths so scripts and future tools become ordinary clients instead of new index owners. Sandboxed
applications should not receive the raw full index automatically; a future portal must filter roots
against granted filesystem access.

## 4. Consumer integrations

### 4.1 Existing TemplarOS Files surfaces

Move the current `search::Index` behind the new core while retaining `SearchApi`. The Home Files
object and `templar-files` should not need a flag-day change. Once Index1 is stable, move them to typed
hits, generation/freshness display, paging, and cancellation. Remove `IndexRoot` as an unrestricted
client-triggered synchronous walk; root configuration should be policy, and reconciliation should
be an asynchronous request with status/progress.

The integrated search panel keeps its current product role: scoped, fast filename/metadata search
inside the Files object. The Home field remains an application launcher. `Super+F` opens Files
search over the user's default indexed roots.

### 4.2 FSearch fork

Create a TemplarOS-maintained fork only after Index1 search and paging pass performance gates. Keep the
fork visibly based on FSearch and preserve upstream licensing per ADR-0013. GPL-2.0-or-later and the
AGPL/GPL combination need release review and accurate source/package notices; do not silently copy C
files into an AGPL-only crate.

The fork's purpose is UI reuse, not database reuse:

- Preserve the GTK result table, keyboard behaviour, supported filters/bookmarks, launch/open
  actions, and FSearch search-language UX. FSearch also has predicates such as child count, content
  type, depth, parent, extension, regex, wildcard, size, and date; each saved filter/bookmark must be
  translated losslessly or shown as unsupported, never silently degraded to plain text.
- Replace include/exclude root preferences with a view of Index1 roots and a link/action to TemplarOS
  Files indexing settings.
- Introduce a remote provider/model layer, then migrate or emulate the existing database-work,
  lazy-row, paging, selection, signal, and sort contracts. FSearch windows and result views retain
  `FsearchDatabase` and fetch row information through it, so this is a substantial model adaptation,
  not a one-function `FsearchDatabaseIndexStore` replacement. Choose explicitly between a
  compatibility `FsearchDatabase` facade backed by Index1 pages and a window/result-view rewrite.
- After the replacement row model works, remove or disable scanning, fanotify/inotify ownership,
  `fsearch.db`, update timers, and `--update-database` in the production TemplarOS build. Retain a
  small test-only local fixture provider if it makes upstream UI regressions easier to isolate.
- Keep `--search <query>` for the existing `ProviderRole::FileSearch` launch path. Add activation of
  an existing window without creating an index-owning second process.
- Translate the FSearch grammar into typed Index1 filters. Start with name/path, quoted terms,
  negation, file/folder, size, and modified-time predicates. Gate regex/wildcards on measured
  candidate limits.

Use a new desktop/application identity such as `org.templaros.FSearch` unless upstream accepts a
generic remote-provider mode. Switch the default `FileSearch` provider only when the fork reaches
feature parity for the workflows TemplarOS advertises. Until then, upstream FSearch may remain an
optional external application, but it cannot satisfy the one-index acceptance criteria.

### 4.3 Silvaview-rs

Add a tree-provider abstraction before changing UI code:

```rust
trait TreeSource {
    fn load_tree(&self, root: &Path) -> Result<TreeLoad>;
}
```

The TemplarOS implementation consumes `OpenSnapshot`, converts canonical rows into build records, and
feeds `tree::build_tree`. A treemap snapshot is necessarily complete for the requested subtree: it
identifies the selected logical root, includes every descendant and its parent relationship or
lossless full path, includes directories, and carries an explicit complete-subtree marker. Ordinary
paging or a default search field mask must never masquerade as a treemap snapshot.

The direct implementation delegates to today's scanner and accepts its `FileTree` result. This
separate tree-oriented adapter is required because the jwalk backend can produce `RawFileEntry`
rows, but the MFT backend currently constructs `FileTree` directly. It also preserves the MFT
builder's parent sequence validation and `_Unresolved MFT Entries` bucket. A future shared Windows
host should instead refactor MFT ingestion to emit canonical rows carrying record and sequence
numbers, with explicit semantics for unresolved records.

Select the provider with `--index=auto|service|scan`: `auto` uses the service when it covers the
requested path and falls back to a direct scan only when no compatible service exists. “Covers”
includes a subtree snapshot rebased at a selected subdirectory; it is not limited to exact
configured-root equality. `service` fails clearly rather than scanning unexpectedly. If a compatible
service exists but does not cover the chosen drive, Silvaview offers a user-mediated request to add
that root and lets the service perform the one initial scan; it does not silently create a parallel
direct index.

The UI displays source, root, generation, freshness, and scan or snapshot progress. Direct mode
reports a synthetic immutable snapshot generation and explicitly does not support `changes_since`
or live updates. Service and direct totals are comparable only under a declared shared policy for
symlinks, mount boundaries, excluded roots, permissions, hard links, and unresolved filesystem
records; tests must state that policy rather than assuming today's scanners behave identically.

First integration may rebuild the whole in-memory `FileTree` when a generation advances. That is
CPU/memory projection work, not drive enumeration, and it isolates risk. A later optimization can
apply change batches to a mutable aggregation tree if profiles show that full projection is too
slow. Preserve the existing direct NTFS MFT backend for Windows. A future shared Windows index host
can refactor that backend to emit canonical entries and add USN Journal ingestion; it is not needed
to prove the TemplarOS shared index.

## 5. Delivery plan

### Phase 0 — measurements and invariants

- Build deterministic fixture trees covering non-UTF-8 names, hard links, symlinks, mount
  boundaries, permission denial, rename storms, watcher overflow, and offline removable roots.
- Record present cold/warm `templar-filed`, FSearch, and Silvaview scan times, I/O bytes, peak RSS,
  database size, and search latency on at least 100k and 2m entries.
- Confirm whether bundled SQLite exposes FTS5 trigram and benchmark exact, prefix, substring, path,
  size/time-filtered, and regex-candidate searches.
- Freeze Index1 entry/freshness semantics before UI work.

Exit: a checked-in benchmark/fixture harness and measured choice of search accelerator.

### Phase 1 — durable index core without monitoring

- Add `templar-file-index`, its independently versioned migrations, generation commits, native path
  encoding, root status, streaming scan, query/paging, and snapshot framing.
- Open a separate cache database from `templar-filed`; copy configured roots/saved queries from state,
  discard old `search_entries` only after compatibility queries use the new core.
- Remove the 50,000-entry product cap. Replace it with configured storage/I/O budgets and explicit
  incomplete state; never return a truncated root as complete.
- Implement Search1 compatibility and Index1 in `templar-client` and interface XML.

Exit: warm daemon startup serves the prior committed snapshot without walking a root, and a failed
or cancelled rebuild leaves that snapshot readable.

### Phase 2 — continuous Linux maintenance

- Add fanotify/inotify backend selection, event normalization/coalescing, one watcher owner, and
  direct mutation hooks from successful file operations.
- Add mount/offline tracking, overflow handling, background reconciliation, progress, root state,
  bounded changes, and generation signals.
- Add deterministic backend tests that inject `IN_Q_OVERFLOW`, event loss during descendant-watch
  installation, and ambiguous cross-directory rename ordering; each must invalidate and reconcile
  rather than advancing a falsely current generation.
- Add service resource limits/diagnostics and `templar doctor` checks for watcher exhaustion, stale
  roots, DB health, and last reconciliation.

Exit: create/modify/rename/delete/move/trash/restore becomes searchable within two seconds; forced
overflow and daemon downtime converge through reconciliation without blocking client startup.

### Phase 3 — TemplarOS clients and general CLI

- Move Files search to typed Index1 paging/cancellation and surface stale/offline state.
- Add CLI status/search/reconcile with JSON and NUL output.
- Add Settings controls for roots, excludes, mount-boundary behaviour, monitoring, and rebuild.

Exit: no first-party TemplarOS search client walks roots or opens the cache database.

### Phase 4 — remote-index FSearch fork

- Establish the fork/packaging lane and provenance notices.
- Introduce its remote result-provider/model seam and make result virtualization, paging, selection,
  sorting and actions work before deleting/disabling private scanning and database code in the
  production TemplarOS build.
- Implement the agreed FSearch syntax subset and provider activation.

Exit: Files search and the FSearch window return the same indexed population/generation, starting
FSearch creates no filesystem watches and causes no recursive scan, and no `fsearch.db` is required.

### Phase 5 — Silvaview-rs adapter

- Add `TreeSource`, snapshot decoding, generation display, explicit source selection, and direct
  scanner fallback.
- Feed index rows through the existing tree builder; retain layout/render code unchanged.
- Add optional live refresh, initially by rebuilding the projection after a debounced generation
  advance.

Exit: on TemplarOS, a warm Silvaview launch performs zero recursive filesystem enumeration and renders
the same root totals/tree as a direct scan within the explicitly documented symlink, mount,
exclusion, permission, hard-link, and unresolved-record semantics.

### Phase 6 — hardening and upstreaming

- Fuzz snapshot and D-Bus decoding; test crash recovery around every generation commit.
- Run long rename/event storms, cache deletion/rebuild, rollback/readability, and removable/network
  volume scenarios.
- Offer upstream FSearch a generic remote-provider interface if it can remain desktop-neutral.
- Consider a standalone portable daemon host only after another platform has two real consumers.

## 6. Acceptance gates

The shared-index work is complete only when all of these are true:

1. Exactly one process per user owns watches and the derived database on TemplarOS.
2. Restarting `templar-filed`, FSearch, `templar-files`, Home, or Silvaview does not synchronously rescan a
   previously indexed root before showing useful results.
3. All responses identify their generation and freshness; overflow/offline/partial states cannot be
   mistaken for current complete data.
4. A crash during scan/event application preserves the prior committed generation, and a subsequent
   daemon converges without manual database deletion.
5. Two million indexed entries are not truncated. On reference hardware, warm name queries meet a
   measured p95 target (initial target 50 ms for 100 results), and ordinary filesystem events appear
   within two seconds without sustained scan-level I/O.
6. Non-UTF-8 paths survive index, snapshot, display, and open/copy actions without addressing the
   wrong file.
7. Current Search1 clients continue to work through the migration.
8. The TemplarOS FSearch build owns neither watchers nor `fsearch.db`, and `--search` still activates a
   useful full-window search.
9. Silvaview's service mode reads an index snapshot and never falls through to a scan silently; its
   explicit direct mode remains available and is tested.
10. `templar doctor` can explain index size, generation, root states, watcher backend/limits, last
    event, last reconciliation, and why a root is stale.

## 7. Risks and decisions deliberately deferred

- **No watcher closes the downtime gap on every Linux filesystem.** Background reconciliation and
  honest freshness are required even after monitoring lands. “Never enumerate again” is not a
  correctness goal.
- **SQLite search may miss FSearch-class latency at several million rows.** Measure FTS5 trigram
  first; preserve the API boundary so a memory-mapped/sorted query index can replace it.
- **A complete Silvaview snapshot can itself be large.** Stream through a sealed FD, field-mask it,
  and avoid JSON. Later add server-side subtree/aggregate snapshots if measurement warrants it.
- **The FSearch fork is GPL-derived.** Forked files keep upstream notices/license; release metadata
  and combined-work obligations need review. The service should be new AGPL Rust code informed by
  behaviour, not copied FSearch C.
- **Filesystem object identity is imperfect.** Inode reuse, hard links, bind mounts, and case rules
  need fixture-backed semantics. A stable index ID is scoped to an index database and generation;
  it is not promised as an eternal document ID.
- **Content search is out of scope.** The index may later carry opt-in extractors, but this plan does
  not read file contents or let FSearch integration smuggle content indexing into the service.
- **Cross-platform daemon hosting is deferred.** Silvaview keeps its direct provider outside TemplarOS.
  Windows sharing should use MFT plus USN Journal behind the same logical model when a second Windows
  consumer exists.

## 8. Source map for implementers

TemplarOS:

- `services/templar-filed/src/search.rs` — current schema-facing index, walker, ranking, and tests.
- `services/templar-filed/src/main.rs:148-194` — current background startup scan/reuse policy.
- `services/templar-filed/src/dbus.rs:730-790` — `Files.Search1` host.
- `interfaces/dbus/org.templaros.Files.Search1.xml` and
  `crates/templar-client/src/proxies/files.rs:148-166` — existing public contract/client.
- `crates/templar-state/src/migrations/files.rs:44-69` — current tables to migrate away from derived
  session state.
- `session/units/user/templar-filed.service` — lifetime and resource policy.
- `docs/architecture/files.md:111-116` — adopted names/path/metadata-only search scope.

Silvaview-rs:

- `src/app.rs:135-186` and `src/main.rs:92-95` — scan lifecycle and temporary handoff.
- `src/scanner/mod.rs` — MFT/walk selection.
- `src/scanner/types.rs` and `src/scanner/walk.rs` — current portable record/traversal.
- `src/scanner/mft.rs` — Windows snapshot backend and MFT identity data.
- `src/tree/mod.rs`, `src/tree/arena.rs`, and `src/tree/aggregate.rs` — projection boundary.

FSearch upstream:

- [`src/fsearch_database_index_store.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_database_index_store.c)
  — in-memory store, monitor/worker threads, one-second event batching.
- [`src/fsearch_database_index.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_database_index.c)
  — scan lifecycle and event coalescing.
- [`src/fsearch_database_scan.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_database_scan.c)
  — recursive traversal.
- [`src/fsearch_folder_monitor_fanotify.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_folder_monitor_fanotify.c)
  and
  [`src/fsearch_folder_monitor_inotify.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch_folder_monitor_inotify.c)
  — Linux event backends.
- [`src/fsearch.c`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/fsearch.c)
  — GTK application, GUI-oriented CLI, and internal D-Bus action forwarding.
- [`src/meson.build`](https://github.com/cboxdoerfer/fsearch/blob/05ed650ab101ee3d61795fa5eeeb5c32e1e366c3/src/meson.build)
  — engine/UI coupling and dependencies.
