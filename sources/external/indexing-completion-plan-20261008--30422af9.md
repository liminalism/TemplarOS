# TemplarOS: complete the shared index and qualify the distro

**Decision and implementation handoff — October 8, 2026**  
Source snapshot: `LegeOS-agent-pack-2026-10-07_062827.7z`.

## Decision

Keep `templar-filed` and `templar-file-index` as the single Templar-owned derived metadata catalog. Keep SQLite as its durable store, inotify as the ordinary event source, and the existing optional Btrfs catch-up lane. Complete the contract and its consumers instead of changing databases, adding a second indexer, or expanding into semantic/full-document search.

The substantial improvement is not just faster scanning. It is **correct and fast answers while files change, bounded producer/client memory, reliable removable-volume recovery, and multiple consumers that agree about what is known**.

This is a source-grounded proposal, not an applied patch or accepted AKR update. Exact excerpts, provenance, limitations and the E-number references are in `TemplarOS-indexing-source-audit-2026-10-08.md`. No Rust build, Rust tests, hardware qualification or current index benchmark was run here. A small Python reproduction verifies the interval-algorithm defect only.

## Preserve the completed foundation

The October 7 source contains work that the older October 1 assessment still lists as missing. Preserve indexed immediate-child lookup and migration repair; persistent authoritative root policies and cache UUID; monotonic root IDs; independent observation lifetimes without renewal polling; store-free client models; native-path actions; split namespace/metadata freshness; generation/change-sequence lineage; correctly sealed read-only snapshots; cursor invalidation instead of relabeling newer facts; and production Btrfs pre/post-baseline acquisition with lineage/fences. The shell already uses the native Index1 client, and Settings already exposes index policy. [E01]

Do not mistake a test file or source handoff for fresh execution evidence. The existing private-runtime and snapshot-consumer tests are valuable starting fixtures, not grounds for skipping acceptance.

## Architectural boundaries

- **Filesystem:** source of file existence and actual access authority. The catalog is derived and can be incomplete or offline.
- **filed/index core:** owns Templar's durable catalog, watches, reconciliation, query planning, snapshot publication and changes. Keep heavy work out of session policy/render loops.
- **sessiond:** owns authoritative preferences, application identity, launch/default policy and work context. Do not put every device service behind a new privileged proxy.
- **consumers:** own presentation and disposable views, not independent full-tree metadata databases/watchers in service mode.
- **Homegarden:** meaningful work context, not a visualization of scanner traffic. Explicit projects, meaningful open/save activity and supported reopen/restore actions belong here.

“One shared index” does not forbid PCManFM from enumerating a directory the user is browsing. It forbids unnecessary parallel persistent indexes and recursive warm-start crawls by consumers that claim to use the shared service. File mutation completion and eventual catalog observation are different events; do not invent an undo journal for external mutations.

## Delivery order

These are proposed slices, not newly created ledger records. Attach them to the existing shared-index/integrated-consumer and daily-driver parents; update the authoritative ledger before regenerating derived status.

| Slice | Owner area | Dependency | Acceptance result |
|---|---|---|---|
| I0 — Establish the executable baseline | Build/test owner | Pinned source closure | Existing private index suites run from clean, isolated dependencies; current failures recorded honestly. |
| I1 — Correctness and cache safety | Index core | I0 fixture availability | Scope union parity; defined overlapping-root semantics; no destructive recovery on transient database errors. |
| I2 — Bounded execution and snapshots | filed + client | I1 invariants | Heavy work stays off async handlers; fixed admission limits; bounded encode/decode copies; cancellation and lifetime cleanup. |
| I3 — Fast under churn | Query/arrays | I1; I2 budget controls | Root-local invalidation/delta strategy preserves exact results under writes and passes measured latency/memory gates. |
| I4 — Root lifecycle and watch proof | Observer/reconcile | I1 identity contract | Real unmount/remount/replacement/overflow recovery, honest freshness, no phantom deletion on empty mountpoints. Can run parallel to I2/I3. |
| I5 — Native consumer completion | Shell/Settings/client | I2 and paging invariants | Reach all retained results, survive invalidation/restart, present partial coverage and native path actions correctly. |
| I6 — Intended external consumers | FSearch/Silvaview package owners | I4/I5 contract stable | Real remote-index builds and packages; no private full-tree catalog/watch set in service mode. |
| I7 — Installed distro qualification | Release/hardware owner | Completed index integration | One reference-machine daily-driver and failure/recovery record, tied to one build identity. |

### I0. Build and test the actual source closure

The supplied archive references absent sibling paths, including PixelKit and bpg-rs. This review environment also lacks Cargo/rustc. The implementation owner should pin the source closure and toolchain, then build in a clean container/VM without relying on a warmed developer workspace, unrecorded home-directory assets or missing sibling checkouts. [E12]

Start with the already documented index/core/client/treemap test selection, but use private HOME/XDG stores, private buses and owned child processes. Do not run destructive fixtures against the installed session or live Trash. Record build ID, dependency revisions, commands, exit codes and retained logs. A full installed-image test follows separately.

### I1. Fix scope correctness and derived-cache safety first

**Nested scopes:** `arrays.rs::scope` sorts intersected intervals but does not merge them. `in_scope` checks the interval with the latest starting position. With `[1,100)` and `[10,20)`, position 30 is incorrectly excluded. Normalize intervals into a disjoint union before membership/rank filtering. The attached Python fixture demonstrates the defect and a normalized alternative; turn it into a native crate regression. [E03]

Add SQL-versus-array property tests for duplicate, nested and disjoint `within` paths; multiple registered roots; all sorts/directions; tied names; path needles; hidden flags; coverage filters; native bytes; deletes and moves during cursor use. The arrays builder assumes disjoint root ranges, while root registration permits overlap and the tree decoder already uses most-specific ownership. Define one policy consistently. Prefer logical project scopes over redundant physical root registrations; migrate existing overlaps deliberately rather than silently rejecting or duplicating them.

**Cache recovery:** `Index::open` currently treats every open/migration error as a reason to move aside/recreate the cache. `move_aside` may delete the database if rename fails and removes WAL/SHM sidecars. This is derived-cache risk, not a claim of document deletion. [E07]

Classify proven corruption or incompatible disposable-cache schema separately from busy, permission, read-only, full-disk and transient I/O failures. Establish exclusive cache-owner recovery before touching files. On operational failures, preserve evidence and return a visible degraded/unavailable state. Quarantine safely or fail untouched; do not remove a live owner's sidecars. Keep authoritative root policy independent of the derived cache, as already implemented.

### I2. Bound work, memory and resource lifetimes

`Index1.Search`, `FetchPage` and `OpenSnapshot` call synchronous core operations directly from async handlers. The reader setting caps retained idle connections, not concurrent queries: an empty reader pool creates another connection. Array matching can additionally create multiple threads per query. [E05]

Introduce admission control before spawning: a small bounded execution lane, separate interactive/bulk/maintenance priorities, bounded pending queues, per-caller limits and request byte/count validation. As an initial configurable policy, allow a small number of interactive queries and only one bulk export and one compaction, then tune from traces. Do not combine many concurrent queries with eight new matching threads per query. Preserve file-operation responsiveness; do not indiscriminately throttle all of filed.

Use latest-request-wins cancellation for typing, cooperative checkpoints in scans/matching, and correctly scoped SQLite interruption/progress cancellation where supported. Dropping a future does not stop already-running blocking work. Always dispose of a cursor/snapshot produced after its requesting client canceled. Upstream Tokio explicitly recommends limiting CPU-bound blocking jobs and notes that started `spawn_blocking` work cannot simply be aborted. [U1]

**Snapshot rewrite:** the writer currently builds a complete payload Vec before writing it to the memfd. The tree client reads the whole descriptor into a second Vec, materializes decoded entries, then builds its tree/map. The 512 MiB client limit rejects large replies only after the service may have created them. Sealing and consistency are correct; memory use is not streaming. [E04]

Keep the wire format and single read-transaction boundary, but stream rows to a seekable output using a bounded buffer and rolling count/checksum. Reserve the header and patch it before sealing. Add service-side entry, byte, duration and concurrency quotas. Offer summary-first/subtree expansion, and a borrowed/streaming entry decoder or validated read-only mapping where suitable. Avoid collecting `entries()` into a complete second owned vector when an iterator suffices. Share equivalent immutable snapshots within a bounded retention policy rather than repeatedly regenerating them for clients. Mapping avoids copies; it does not make backing memory free.

Budget producer allocations, immutable snapshot backing, client views, arrays, old generations and SQLite/WAL readers together. Measure process and cgroup memory and account for shared mappings without double-counting. Under pressure, drop optional service-owned accelerators/previews/retained snapshots and ask participating clients to release optional views; do not assume the service can reclaim every descriptor a client retains.

**Cursor ownership:** current expiry happens on insert/get, not when maintenance reads the oldest retained generation. A dead client's cursor can therefore pin retention beyond its nominal TTL until further cursor activity. Assign caller ownership, release on disconnect, expire before retention calculations, and use a next-expiry deadline if needed. Preserve the no-renewal-poll observation design. [E08]

### I3. Keep the fast path useful while files change

Current arrays are usable only at exact global-generation equality. Any committed change makes them ineligible; rebuilds are separated by at least five seconds or twice the previous build duration. SQL serves in between. This makes the static warm benchmark an incomplete description of a development machine. [E02]

The dated project benchmark reports roughly 12–18 ms p95 for broad array queries at two million rows versus about 276–3,264 ms for the corresponding SQL cases. These are historical numbers, not this review's measurements. Reproduce them and add build/download/edit churn before deciding where effort belongs.

Prefer incremental acceleration over a backend replacement. First introduce root-local invalidation and explicit dirty shards. Then, where worthwhile, maintain an immutable base plus a bounded committed-change/tombstone overlay, merged under the same rank/order/scope/total rules. Directory moves must suppress/update affected descendants or make that shard use the correct SQL fallback until repaired. Bound overlay size, retained generations and compaction memory; compact by thresholds and pressure, not on every keystroke. Never label stale base arrays as current.

Keep SQLite as the correctness reference and selective-query path. An adaptive planner may choose SQL for very selective names and arrays for broad short queries; the old benchmark itself shows selective SQL can be faster. Optimize slash/path queries only after current broad-query latency under churn and memory amplification are addressed. GPU indexing, a new database and another watch backend are not prerequisites.

### I4. Complete removable-volume and watch recovery

The source defines `set_volume_identity` and `root_returned` but the repository-wide Rust search found no callers. The return helper compares `st_dev`, accepts missing identity as matching, and does not implement the full rebuild its comment describes. Other reconciliation can still rediscover files, but this is not a completed explicit mount/identity lifecycle. [E06]

Connect UDisks inventory/events plus mount-table reconciliation for mounts outside UDisks. Describe volume identity with supported filesystem identity, mount/subvolume/root-relative path and cache lineage, not the pathname alone. UUID may be absent; represent uncertainty rather than inventing stable identity. [U4]

On removal, preserve cached rows as offline. On return, distinguish the same volume at another mountpoint from another volume at the old path. Do not scan an empty underlying mount directory and publish the resulting absence as deletion of the detached volume's contents. Invalidate the relevant lineage, reattach watches, reconcile and only then claim the supported freshness state.

Inotify is not recursive, can overflow and does not report a filesystem mounted over a watched directory. Those upstream constraints require lifecycle repair; increasing watch limits is not a correctness strategy. [U2] Test the real watched-directory budget and tune coverage policies. Use Detailed for work needing individual results, Inventory or Opaque for noisy derived trees as appropriate, and Ignored for deliberate exclusion. Do not silently hide source files to meet a performance target.

Keep Btrfs catch-up optional until actual privilege, subvolume lineage, interruption and reboot evidence passes. Do not reopen already implemented baseline acquisition as though it were absent. Different remote/FUSE/native filesystem capabilities need explicit coverage/freshness semantics rather than a universal Current promise.

### I5. Finish the native consumer, not another mockup

The shell creates a native search session with a page size of 50 and displays the first result batch. The supplied shell source does not call the existing paging helper. Core paging is real, but users cannot reach the rest through that implementation. [E08]

Add virtualized paging/load-more, navigation state, preserved selection by native identity, and correct query-generation cancellation. Treat `SnapshotRequired` as a refresh/restart of that query rather than a silently empty page. Keep expected row actions: open, reveal, copy native URI/path where representable, and add to project/Home through the existing owners. Show total/truncation and distinguish partial, offline, stale metadata, disabled monitoring and ongoing repair.

Preserve the apps-only launcher. Scoped project/file search and the global file-search route can share Index1 without turning Home into a mixed apps/documents/settings omnibox. Settings already has index policy: improve actionable diagnosis and repair there rather than adding an unrelated monitoring app.

### I6. Complete FSearch and Silvaview against their actual source

The plan explicitly leaves the remote FSearch fork/packaging and external Silvaview service mode open. A provider named `fsearch` is not shared-index integration. Do not switch defaults before the intended build exists and passes its consumer tests. [E09]

For FSearch, supply a clearly identified remote-index mode backed by Index1 snapshots/queries/changes, with no independent full-tree watch set or database in that mode. Decide unsupported regex/filter behavior explicitly. Validate packaging, source provenance, launch/default routing, live changes, restart/reconnect and native filenames.

For Silvaview, retain explicit `service`, `auto` and standalone `scan` behavior. `service` must surface service errors; `auto` may fall back when the service is absent or incompatible, not simply because the available catalog is stale. Share the store-free decoder; request only needed fields/subtrees. Compare totals only for identical coverage and size definitions. An indexed logical-byte sum is not automatically physical/exclusive disk usage on sparse, hard-linked or copy-on-write data.

PCManFM remains the real file manager. Its own Find is not automatically converted to Index1 by bookmarks/provider integration. Keep that distinction visible and direct global indexed searches to the intended consumer rather than claiming all third-party search is integrated.

## Instrumentation and acceptance

Expand the existing doctor rather than introducing a second state database. Include main/WAL/SHM bytes, staged/live/deleted rows, query queue time, strategy and canceled work, array generation/bytes/hit rate/build peak, snapshot bytes/count/age, cursor owner/expiry/retained bytes, watch required/armed/refused counts, root identity, last verified change sequence, repair reason and coverage. Main-file `db_bytes` currently omits WAL. [E10]

Long read transactions can interfere with checkpoint progress and allow SQLite WAL growth. Diagnose reader age and checkpoint progress before blaming database size on one index. Use owner-scheduled checkpoint maintenance; do not force a truncating checkpoint from every interactive query. [U3]

| Gate | Required experiment | Passing evidence |
|---|---|---|
| Exact answers | Property/random fixtures; SQL vs arrays; overlapping roots/scopes | Same membership, rank, order, totals and native-path identity under the declared semantics. |
| Native paging | More than 258 hits; edit/rename/delete while paging | All retained pages reachable; honest invalidation; no missing tail or duplicate fake rows. |
| Churn latency | Real home plus 100k/2M fixtures; quiet and build/download churn | Query-to-visible-result p50/p95/p99, accelerator availability, queue delay and rebuild cost reported together. |
| Memory | Two search clients, bulk tree view and compaction under pressure | Peak service/client/backing memory and configured admission limits demonstrated; no unbounded payload working copy. |
| Freshness | Overflow, refused watches, new subtrees, missed interval | Never false-current; explicit dirty state followed by convergence against an isolated filesystem oracle. |
| Volumes | Same volume/new path; different volume/same path; inaccessible mount | Offline retention, correct identity decision and watch rearming; no accidental purge of detached contents. |
| Recovery | Kill at generation/baseline/snapshot publication boundaries | No half-published generation or unprovable delta replay; clients reconnect or resnapshot honestly. |
| Policy/privacy | Exclusions, disabled monitoring, non-UTF8 roots, cache replacement | Authoritative choices persist; “remove from index” does not delete files; hidden/private content policies explicit. |
| Consumer sharing | Native shell, packaged FSearch, Silvaview open a warm index | No extra recursive crawl or parallel private index in service mode; consistent scope/coverage truth. |
| Quiescence | Close consumers and stop churn | Observation cleanup, no renewal polling, no perpetual rebuild; maintenance follows explicit deadlines/policy. |

Candidate performance targets, to ratify on the reference laptop rather than claim today: warm first-page p95 below 50 ms; first-page p95 under representative churn below 150 ms; small local changes normally visible within a second while monitoring is active and healthy. Keep cold startup, dirty repair, unsupported mounts and stalled I/O separate. The correctness and memory gates are mandatory even if numerical targets are revised after measurement.

## Broader distro: finish the original five proofs

The source now records actual October 6 isolated native portal/PCManFM/Settings work and a disposable firmware/Btrfs/APT deployment/rollback journey. Those records explicitly exclude a fully installed desktop, complete promotion-health chain, shared-state schema compatibility, Secure Boot and physical GPU qualification. Do not repeat the older broken staging-path/portal descriptions as though no work happened. [E11]

### 1. One qualified reference-machine day

Use one pinned image on the intended laptop, not a broad hardware campaign. Demonstrate boot, firmware-backed Wi-Fi, authentication, browser upload/download through the supported sandbox path, CUDA development, video/audio, external-display audio, dock/unplug, lid behavior, lock/suspend/resume and return to work. Distinguish successful isolated native fixtures from installed package/environment proof. Keep the working LXQt/PCManFM routes until native replacements satisfy the same evidence.

### 2. One canonical settings route per ordinary domain

Keep Wi-Fi, Bluetooth, sound, displays, input and power owned by their existing authoritative services and exposed through consistent Templar routes. Shortcuts should deep-link to the same owner. For every visible control, show capabilities, desired state, applied state, saved preference and actionable failure. A UI switch is not completion without its effect and read-back. Avoid six new daemons or another settings shell.

### 3. App identity across the real launch paths

Test browser downloads/links, portal choices, file-manager Open With, terminal launches, Homegarden and supported sandboxed applications. Verify identity/grouping, defaults, resource attribution and return-to-work behavior. Do not quietly trust a client-supplied app ID to fill gaps. A compile launched in a terminal needs an explicit workload scope when it should have separate resource policy; identity alone cannot provide that.

### 4. Homegarden as useful Work

Retain stable folder/project organization and the current work-linked/parked-card design. Do not promote index mtime churn into personal activity. Represent offline project roots and renamed folders without losing user organization. Show Reopen where that is all an app supports and Restore session only with a real restoration contract. Keep Taskfield as a view over the same task model, not a rival desktop or a permanent telemetry dashboard.

### 5. Prove recovery beyond boot selection

The next deployment gate is a complete installed-image trial, health confirmation, promotion and failure rollback with matching root/package metadata/kernel/NVIDIA. Verify shared service-state schemas across rollback and preserve documents/profiles, including work created after the candidate boot. Keep System/State/Work storage ownership distinct from the user-facing Work/Applications/System grouping. The original plan does not require a custom kernel or changing distro base to complete these proofs.

Finally, make documentation reflect evidence. README still says M1 and “cannot log into this yet,” while newer records describe native sessions and deployment boots. Prefer one authoritative implementation/evidence ledger and generated summaries, not contradictory snapshots agents can mistake for current instructions. Pin build provenance and finish keyboard/focus/clipboard/Unicode/scaling/accessibility consistency across owned surfaces before expanding app count or decorative polish. [E12]

**Release principle:** the shared index is complete when normal users can rely on it while developing, changing disks and recovering from failure; the distro is complete when its existing ownership model makes that ordinary day predictable.

## Upstream references

- U1 — Tokio blocking execution and cancellation: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
- U2 — Linux inotify limitations: https://man7.org/linux/man-pages/man7/inotify.7.html
- U3 — SQLite WAL/checkpoint behavior: https://sqlite.org/wal.html
- U4 — UDisks filesystem identity metadata: https://storaged.org/doc/udisks2-api/latest/gdbus-org.freedesktop.UDisks2.Block.html
