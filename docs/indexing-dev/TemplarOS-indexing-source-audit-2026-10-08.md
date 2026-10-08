# TemplarOS indexing and distro: source audit

Review date: **October 8, 2026**. Source snapshot: **October 7, 2026**.

## Provenance and validation boundary

- Input: `LegeOS-agent-pack-2026-10-07_062827.7z`
- SHA-256: `480ce670954799ba103cb0e049c6ffc58eb46d2082c63c2a0d39c26d0c4702d5`
- Extracted root: `LegeOS/`; all file/line references below are relative to that root.
- Source inspection covered the shared index core, service host, watcher/reconcile surfaces, snapshot/tree consumers, shell search, provider integration and the relevant planning/architecture records.
- **No Rust build, Rust test suite, desktop boot, hardware test, index benchmark or live filesystem mutation was performed.** No `cargo`/`rustc` was available in this environment; the archive also references absent sibling source trees including PixelKit and bpg-rs.
- The only executable verification in this review is the isolated Python interval-algorithm reproduction in `templaros_scope_regression.py`. It does not execute TemplarOS.
- No repository, user Library, installed system, application settings or live user data was changed. This audit and its companion plan are recommendations, not applied patches or accepted ledger records.
- Prior October 1/5 assessments and October 6 observations are treated as dated records, not automatically as the October 7 executable behavior.

## Findings index

| ID | Finding |
|---|---|
| E01 | Existing index work: preserve it, then verify it |
| E02 | The fast arrays require exact global-generation equality |
| E03 | Overlapping scope intervals are sorted but not normalized |
| E04 | Sealed snapshots still materialize complete payloads and client copies |
| E05 | Synchronous queries/snapshots execute in async handlers; reader retention is not admission control |
| E06 | Volume-return primitives are not wired into a complete production lifecycle |
| E07 | Every open/migration error triggers derived-cache replacement |
| E08 | Cursor expiry is opportunistic and the native shell only consumes its first page |
| E09 | External consumer completion remains an explicit open lane |
| E10 | Diagnostics undercount cache storage and budgets need whole-service accounting |
| E11 | Recent distro evidence is substantially newer than the older daily-driver critique |
| E12 | Original first-release goals and source-closure limits |

<a id="e01"></a>
## E01 — Existing index work: preserve it, then verify it

Source handoff, not a fresh test run. The October 5 handoff inside the October 7 pack supersedes the older status table for these changes. Read executable source for specific defects below.

### `docs/planning/integrated-indexing.md:25–85`

```text
25: ## Source handoff — 2026-10-05
26: 
27: The observations and ledger-state table below describe the 2026-10-01 assessment, not a new
28: runtime verification. Reopened index heads require provenance/evidence revalidation; they do
29: not by themselves establish a regression. The following source changes are implemented but
30: have not been built or exercised by the delegated index worker:
31: 
32: - Doctor opens the existing derived database **read-only**, without creating/migrating the
33:   index or opening authoritative session state. Schema 3 through the current supported schema
34:   are inspected without migration; unsupported schemas fail untouched. The bounded live read
35:   found schema 5, generation 13,510, an incomplete watcher-exhausted home root, refused watches,
36:   previous overflows and an ENOENT watcher error for a disappearing rustc output directory.
37:   A vanished child is now distinct from watch refusal/exhaustion; genuine exhaustion, overflow,
38:   budgets and stale root truth remain visible rather than being downgraded.
39: - Indexed immediate children use `(root_id,parent_id,path_bytes)` and streaming repair
40:   consumption, not a descendant-range decode. Migration 6 repairs legacy links; take-on-trust
41:   reconciliation and moves preserve parent identity. Default repair entry/byte budgets are
42:   unlimited unless explicitly configured, and an explicit budget remains an incomplete pass.
43:   Atomic delta acquisition cannot commit just a prefix above the watcher chunk limit.
44: - Production full-root reconciliation acquires **pre/post** Btrfs checkpoints. A bounded,
45:   recognized incremental stream must prove no overlapping change during that pass before the
46:   post-checkpoint can become a baseline. Raw commands are checked too, so a transient
47:   create/delete is not hidden by an empty net plan. Incompatible or busy acquisition keeps the
48:   ordinary watcher/reconcile path. Baseline identity includes disposable-cache UUID (migration
49:   7), root, generation, policy and subvolume. Durable pending fences prevent replay after a
50:   crash between commit and pointer publication; recursive watch coverage precedes Current.
51: - Store-free client models, native typed search sessions and snapshot consumption retain
52:   generation, total/truncation, namespace and metadata truth. `Index1.Observe(bool,u64)` pins an
53:   independent caller-owned token without renewal polling; disconnect releases all tokens.
54:   Closing an old cursor cannot detach a replacement or a reacquired observation after restart.
55:   Acquire/search cancellation drops an eventual owned result after the reply, and cleanup uses
56:   the acquired runtime even when Drop runs on the GUI thread. Unique-name loss revokes pending
57:   acquisition flags before commands are drained; no disconnected-name tombstone history or
58:   renewal timer is needed. The loss subscription is installed before the public name is claimed;
59:   subscription failure/end is an honest service failure instead of immortal lifetime pins.
60:   Newer row facts cannot be mislabeled with an older pinned page generation: they return
61:   `SnapshotRequired`; unchanged paging and deletion-as-absent retain their contracts.
62:   Snapshot wire flag constants are outside the store-only writer module; this fixes the missing
63:   aggregate-flag names reported by the parent's first store-free compile attempt. `OpenSnapshot`
64:   streams into a memfd and applies kernel WRITE/GROW/SHRINK/SEAL seals, then reopens an O_RDONLY
65:   descriptor before returning `sealed:true`; kernel creation/sealing failure is an actual
66:   publication failure, not an unsealed fallback. Private runtime coverage checks access mode,
67:   kernel seals, write, truncation and growth.
68: - Root policies use the existing `RootPolicy` through `RootPolicy`, `ConfigureRoot` and
69:   `RemoveRoot`. Native path bytes and policies live in authoritative `file_index_roots`, not
70:   only the disposable cache. Empty choices and removed HOME survive cache replacement/restart.
71:   Disabled monitoring preserves searchable rows but reports unwatched; it does not claim Current.
72:   Native path plus observed policy hash fences reads, updates and removals atomically in the
73:   authoritative store; `"absent"` is explicitly add-only. Cache UUID fences rebuild requests,
74:   and root IDs increase monotonically within a cache so removal/addition cannot recycle an
75:   old queued target. CLI `index policy/configure/remove` mirrors these Settings controls.
76: - `Actions1.RenderNative` and the client `render_native_path` helper preserve raw bytes through
77:   percent-encoded URIs and ANSI-C shell quoting. Plain UTF-8 text forms fail explicitly when
78:   unrepresentable. Treemap fallback is bounded and reports real depth/entry/read/missing-path
79:   limits; available-but-stale or broken indexed results do not silently trigger another scan.
80: - Bookmark export is explicit and additive with backup, owned-line evidence, native-entry
81:   preservation and atomic exchange conflict recovery. External file-manager mutations enter
82:   the watcher, never a fabricated filed undo journal.
83:   Ownership is recorded only after successful GTK publication; interrupted exports can leave
84:   an unmanaged line but cannot falsely adopt a native application's line. Editing/removing an
85:   owned native label drops its tracking without rewriting GTK when no additions are needed.
```

### `docs/planning/integrated-indexing.md:87–111`

```text
87: ### Owner verification and remaining gates
88: 
89: Run once after source integration, not against the live desktop:
90: 
91: ```sh
92: cargo test -p templar-file-index -p templar-filed -p templar-client -p templar-treemap -- --nocapture
93: ```
94: 
95: `private_runtime` owns a private D-Bus socket, HOME/XDG stores and exact child PIDs. Its nonzero
96: exercise covers 258 warm paged/native hits, metadata update, rename, move, external trash without
97: invented undo, native URI/shell byte round trips, bookmark backup/preservation, bounded restart
98: visibility, GUI-thread cursor cleanup, observation reacquisition, kernel-sealed O_RDONLY snapshots
99: and persisted non-UTF8 root policy after cache deletion. Root-policy tests exercise stale
100: read/update/remove rejection and exactly one concurrent durable CAS winner.
101: It requires `dbus-daemon` and Bash; both session and optional system-bus calls are isolated.
102: `snapshot_consumer` consumes real writer frames, cached opaque totals, native paths, scope and
103: freshness. Focused core/service tests cover large atomic acquisition, parent identity, failed
104: publication/crash fences, transient/late acquisition changes, disposable cache identity and
105: last-check-to-rename bookmark conflicts. No success counts are claimed before the owner runs them.
106: 
107: Physical/mount evidence remains separate: real `@home` checkpoint privilege/lineage and reboot
108: recovery, the complete mounted home/project watch budget, overflow/convergence under real load,
109: 2M broad-query latency/memory and installed first-party journeys. FSearch fork/packaging and
110: external Silvaview service mode still require their owned source/provenance lane; no substitute
111: private index or claimed external-consumer success is introduced here.
```

<a id="e02"></a>
## E02 — The fast arrays require exact global-generation equality

Source-confirmed eligibility and rebuild behavior. The conclusion that sustained churn can substantially reduce accelerator availability follows from that control flow; no current workload latency was measured. Historical timings are quoted as the project document's measurements, not reproduced results.

### `crates/templar-file-index/src/arrays.rs:105–143`

```text
105: /// The shortest gap between two background builds. A tree that changes every second (a build
106: /// directory, a download) would otherwise keep one core rebuilding arrays that are stale again
107: /// before anyone searches them; SQL answers in between.
108: const MIN_REBUILD_INTERVAL: Duration = Duration::from_secs(5);
109: 
110: impl ArraysSlot {
111:     /// The arrays, if they describe exactly `generation` and `roots`.
112:     pub(crate) fn current_for(
113:         &self,
114:         generation: Generation,
115:         roots: &[RootId],
116:     ) -> Option<Arc<SearchArrays>> {
117:         let guard = self.current.read();
118:         let arrays = guard.as_ref()?;
119:         let mut held: Vec<RootId> = arrays.roots.iter().map(|span| span.root).collect();
120:         let mut wanted = roots.to_vec();
121:         held.sort_unstable();
122:         wanted.sort_unstable();
123:         (arrays.generation == generation && held == wanted).then(|| Arc::clone(arrays))
124:     }
125: 
126:     /// Claims the right to build. False when a build is already running, or when the last one
127:     /// started less than `max(MIN_REBUILD_INTERVAL, 2 × its duration)` ago.
128:     pub(crate) fn begin_build(&self) -> bool {
129:         let mut last = self.last_build.lock();
130:         if let Some((started, took)) = *last
131:             && started.elapsed() < MIN_REBUILD_INTERVAL.max(took * 2)
132:         {
133:             return false;
134:         }
135:         if self
136:             .building
137:             .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
138:             .is_err()
139:         {
140:             return false;
141:         }
142:         *last = Some((Instant::now(), Duration::ZERO));
143:         true
```

### `crates/templar-file-index/src/query.rs:685–728`

```text
685:     /// The arrays' answer, or `None` when they cannot give one: not built for this generation and
686:     /// these roots (a background build is requested, and SQL answers this time), a needle that
687:     /// names a path, or a `within` directory they do not hold.
688:     fn search_arrays(
689:         &self,
690:         connection: &rusqlite::Connection,
691:         query: &Query,
692:         needle: &str,
693:         generation: Generation,
694:         root_ids: &[RootId],
695:         keep: usize,
696:         page_size: usize,
697:     ) -> Result<Option<RankedMatches>> {
698:         let Some(arrays) = self.arrays_slot().current_for(generation, root_ids) else {
699:             self.request_arrays();
700:             return Ok(None);
701:         };
702:         let within_ids = resolve_within(connection, &query.within)?;
703:         let Some(answer) = arrays.search(query, needle, &within_ids, keep) else {
704:             return Ok(None);
705:         };
706:         let first = answer.ids.len().min(page_size);
707:         let (hits, _) = materialize(connection, &answer.ids[..first], &answer.ranks[..first])?;
708:         Ok(Some((answer.ids, answer.ranks, answer.total, hits)))
709:     }
710: 
711:     /// Starts one background build of the arrays, unless one is already running.
712:     fn request_arrays(&self) {
713:         if !self.arrays_slot().begin_build() {
714:             return;
715:         }
716:         let index = self.clone();
717:         let spawned = std::thread::Builder::new()
718:             .name("templar-index-arrays".to_owned())
719:             .spawn(move || {
720:                 let started = Instant::now();
721:                 match index.with_read(SearchArrays::build) {
722:                     Ok(arrays) => {
723:                         tracing::info!(
724:                             rows = arrays.len(),
725:                             elapsed_ms = started.elapsed().as_millis() as u64,
726:                             "built the search arrays"
727:                         );
728:                         index.arrays_slot().finish_build(Some(arrays));
```

### `docs/planning/integrated-indexing.md:428–463`

```text
428: 
429: `cargo run -q -p xtask --release -- index-bench workloads --allow-two-million`. The synthetic
430: fixture names its directories `d<n>` and its files `f<n>.txt`. Times are p50/p95, first page of
431: 100 results.
432: 
433: | Needle (matches at 2M) | SQL stage 1 | Arrays |
434: | --- | ---: | ---: |
435: | `d` (1,999,968, nearly all path-only) | 3,257 / 3,264 ms | 16.8 / 17.8 ms |
436: | `f1` (550,000) | 1,169 / 1,171 ms | 14.9 / 18.0 ms |
437: | `txt` (1,600,000) | 1,728 / 1,737 ms | 16.8 / 18.2 ms |
438: | `f12.txt` (50,000) | 274 / 276 ms | 10.7 / 11.9 ms |
439: | `d3/d4` (118,673, path needle) | 688 / 689 ms | stays on SQL: 692 / 710 ms |
440: 
441: Building the arrays takes 1.6 s at 2,000,001 rows and 60–83 ms at 90–99k. On the real home index
442: copy (99,447 rows), SQL vs arrays:
443: 
444: | Needle | SQL | Arrays |
445: | --- | ---: | ---: |
446: | `e` | 234 ms | 2.4 ms |
447: | `cargo` | 28 ms | 1.7 ms |
448: | `main.rs` | 0.7 ms | 1.4 ms |
449: 
450: Selective needles cost about 1 ms more from the arrays, because of the fixed scan.
451: 
452: Known costs:
453: 
454: - **The SQL fallback got slower for one- and two-character needles.** `e` takes 234 ms where the
455:   old capped search took 126 ms, because it now counts and orders every match and expands
456:   directories. It only serves while the arrays are stale.
457: - **Memory.** The arrays add about 60 bytes per row plus the names: roughly 6 MB for this home,
458:   and on the order of 130 MB at 2M rows [INFERENCE from the column sizes].
459: - **Path needles.** A needle with `/` stays on SQL (690 ms at 2M). Matching it from the arrays
460:   would need path strings or a component-wise matcher; that is not built.
461: - **Database size.** At 2M rows the database measured 3.2 GB on disk, including WAL. That was not
462:   investigated here.
463: 
```

<a id="e03"></a>
## E03 — Overlapping scope intervals are sorted but not normalized

Source-confirmed algorithm defect. A nested interval can hide the tail of its containing interval in the binary-search membership check. The accompanying Python fixture reproduces this algorithm, not the compiled Rust crate. Overlapping registered roots require a separate ownership/order contract; the tree decoder already recognizes that possibility.

### `crates/templar-file-index/src/arrays.rs:175–188`

```text
175:         // Disjoint subtrees are contiguous in byte order, so roots in path order, each in its
176:         // own path order, is the global path order the SQL search sorts by.
177:         root_list.sort_by(|left, right| left.1.cmp(&right.1));
178: 
179:         let mut arrays = Self::empty(generation);
180:         let mut raw_names: Vec<Vec<u8>> = Vec::new();
181:         let mut statement = transaction.prepare(
182:             "SELECT entry_id, name_bytes, name_fold, path_bytes, kind, size_bytes, mtime_ns,
183:                     hidden, coverage
184:                FROM entries
185:               WHERE root_id = ?1 AND deleted_generation IS NULL
186:               ORDER BY path_bytes",
187:         )?;
188:         for (root, root_path) in &root_list {
```

### `crates/templar-file-index/src/arrays.rs:443–469`

```text
443:     fn scope(&self, query: &Query, within_ids: &[Option<EntryId>]) -> Option<Vec<(u32, u32)>> {
444:         let mut ranges: Vec<(u32, u32)> = self
445:             .roots
446:             .iter()
447:             .filter(|span| query.roots.is_empty() || query.roots.contains(&span.root))
448:             .map(|span| (span.start, span.end))
449:             .collect();
450:         if query.within.is_empty() {
451:             return Some(ranges);
452:         }
453:         let mut within = Vec::with_capacity(query.within.len());
454:         for (directory, id) in query.within.iter().zip(within_ids) {
455:             within.push(self.subtree_of(directory, *id)?);
456:         }
457:         ranges = ranges
458:             .iter()
459:             .flat_map(|(start, end)| {
460:                 within.iter().filter_map(move |(low, high)| {
461:                     let low = (*low).max(*start);
462:                     let high = (*high).min(*end);
463:                     (low < high).then_some((low, high))
464:                 })
465:             })
466:             .collect();
467:         ranges.sort_unstable();
468:         Some(ranges)
469:     }
```

### `crates/templar-file-index/src/arrays.rs:592–597`

```text
592:     match scope.binary_search_by(|(start, _)| start.cmp(&position)) {
593:         Ok(_) => true,
594:         Err(0) => false,
595:         Err(index) => position < scope[index - 1].1,
596:     }
597: }
```

### `crates/templar-file-index/src/lib.rs:429–469`

```text
429:     pub fn add_root(&self, path: &NativePath, policy: RootPolicy) -> Result<RootStatus> {
430:         let policy_json = serde_json::to_string(&policy)?;
431:         let root_id = self.with_write(|connection| {
432:             let existing: Option<i64> = connection
433:                 .query_row(
434:                     "SELECT root_id FROM roots WHERE path_bytes = ?1",
435:                     [path.as_bytes()],
436:                     |row| row.get(0),
437:                 )
438:                 .map(Some)
439:                 .or_else(|error| match error {
440:                     rusqlite::Error::QueryReturnedNoRows => Ok(None),
441:                     other => Err(other),
442:                 })?;
443:             if let Some(root_id) = existing {
444:                 return Ok(RootId(root_id));
445:             }
446:             let transaction = connection.transaction()?;
447:             let next: String = transaction.query_row(
448:                 "SELECT value FROM index_meta WHERE key='next_root_id'", [], |row| row.get(0),
449:             )?;
450:             let root_id: i64 = next.parse().map_err(|_| IndexError::Corrupt {
451:                 detail: "invalid next root identity".to_owned(),
452:             })?;
453:             let following = root_id.checked_add(1).filter(|_| root_id > 0).ok_or_else(|| IndexError::Unsupported {
454:                 detail: "root identity space exhausted".to_owned(),
455:             })?;
456:             transaction.execute("UPDATE index_meta SET value=?1 WHERE key='next_root_id'", [following.to_string()])?;
457:             transaction.execute(
458:                 "INSERT INTO roots (root_id, path_bytes, display, cross_mount, policy_json, policy_hash,
459:                                     state, committed_generation, complete, entry_count)
460:                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'empty', 0, 0, 0)",
461:                 rusqlite::params![root_id, path.as_bytes(), path.display(), i64::from(policy.cross_mount), policy_json, policy.hash()],
462:             )?;
463:             transaction.commit()?;
464:             Ok(RootId(root_id))
465:         })?;
466:         self.root_status(root_id)
467:     }
468: 
469:     /// Forgets a root and every row derived from it.
```

### `crates/templar-client/src/tree_source.rs:130–140`

```text
130:     // Overlapping registered roots may carry the same path. Its most specific root owns it;
131:     // keys become RawEntry paths without another owned-path copy.
132:     let mut entries = std::collections::HashMap::<std::path::PathBuf, (usize, u64, bool)>::new();
133:     for entry in reader.entries().map_err(TreeSourceError::Snapshot)? {
134:         let path = entry.path.ok_or_else(|| TreeSourceError::Snapshot("missing native path".to_owned()))?;
135:         let Some((_, root_path)) = roots.iter().find(|(id, _)| *id == entry.root) else {
136:             return Err(TreeSourceError::Snapshot("entry names an unknown root".to_owned()));
137:         };
138:         if !path.starts_with(&wanted) || !path.starts_with(root_path) || path.as_bytes().contains(&0) {
139:             return Err(TreeSourceError::Snapshot("entry escapes the requested native scope".to_owned()));
140:         }
```

<a id="e04"></a>
## E04 — Sealed snapshots still materialize complete payloads and client copies

Source-confirmed allocation path. Sealing is implemented; bounded-memory streaming is not. Byte/count quotas and client/producer admission require explicit implementation, not just a read-only descriptor. No peak RSS measurement was performed.

### `crates/templar-file-index/src/snapshot.rs:175–200`

```text
175:         // replay from a cursor that did not describe the rows it had. `BEGIN DEFERRED` on a
176:         // read-only connection takes its snapshot at the first read and holds it under WAL until
177:         // the transaction ends, which is exactly the view every section below reads from.
178:         let (generation, snapshot_seq, selected, payload, entry_count) =
179:             self.with_read(|connection| {
180:                 let view = connection.unchecked_transaction()?;
181:                 let generation = crate::read_generation(&view)?;
182:                 // Read before the rows, not after: a change committed while the frame is being
183:                 // written must land in the stream a consumer replays, never be skipped as
184:                 // "already in the frame". An over-count is a change replayed; an under-count is a
185:                 // change lost, and every mutation is idempotent.
186:                 let snapshot_seq = crate::mutation::read_latest_change_seq(&view)?;
187:                 let selected: Vec<_> = crate::read_roots(&view)?
188:                     .into_iter()
189:                     .filter(|status| {
190:                         request.roots.is_empty() || request.roots.contains(&status.root)
191:                     })
192:                     .filter(|status| request.include_offline || status.state != RootState::Offline)
193:                     .collect();
194: 
195:                 let mut payload = Vec::new();
196:                 write_roots_section(&mut payload, &selected);
197:                 let entry_count =
198:                     write_entries_section(&view, &mut payload, &selected, request, mask)?;
199:                 Ok((generation, snapshot_seq, selected, payload, entry_count))
200:             })?;
```

### `crates/templar-file-index/src/snapshot.rs:218–244`

```text
218:         let payload_crc = crc32c(&payload);
219:         let mut header = Vec::with_capacity(HEADER_LEN as usize);
220:         header.extend_from_slice(MAGIC);
221:         header.extend_from_slice(&format.to_le_bytes());
222:         header.extend_from_slice(&FLAG_LITTLE_ENDIAN.to_le_bytes());
223:         header.extend_from_slice(&HEADER_LEN.to_le_bytes());
224:         header.extend_from_slice(&mask.0.to_le_bytes());
225:         header.extend_from_slice(&generation.0.to_le_bytes());
226:         header.extend_from_slice(&(selected.len() as u32).to_le_bytes());
227:         header.extend_from_slice(&0u32.to_le_bytes());
228:         header.extend_from_slice(&entry_count.to_le_bytes());
229:         header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
230:         header.extend_from_slice(&payload_crc.to_le_bytes());
231:         let header_crc = crc32c(&header[..60]);
232:         header.extend_from_slice(&header_crc.to_le_bytes());
233:         debug_assert_eq!(header.len(), HEADER_LEN as usize);
234: 
235:         sink.write_all(&header)?;
236:         sink.write_all(&payload)?;
237:         sink.flush()?;
238: 
239:         Ok(SnapshotManifest {
240:             format,
241:             generation,
242:             field_mask: mask.0,
243:             entry_count,
244:             payload_len: payload.len() as u64,
```

### `crates/templar-file-index/src/snapshot.rs:275–298`

```text
275: fn write_entries_section(
276:     connection: &Connection,
277:     payload: &mut Vec<u8>,
278:     selected: &[RootStatus],
279:     request: &SnapshotRequest,
280:     mask: FieldMask,
281: ) -> Result<u64> {
282:     let want_aggregate = mask.contains(FieldMask::AGGREGATE);
283:     let mut entry_count = 0u64;
284:     for status in selected {
285:         let (sql, params) = entries_query(status.root.0, request, want_aggregate);
286:         let mut statement = connection.prepare(&sql)?;
287:         let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
288:         while let Some(sql_row) = rows.next()? {
289:             let row = entry_row_from_row(sql_row)?;
290:             write_entry_fields(payload, mask, &row);
291:             if want_aggregate {
292:                 write_aggregate_field(payload, sql_row)?;
293:             }
294:             entry_count += 1;
295:         }
296:     }
297:     Ok(entry_count)
298: }
```

### `crates/templar-file-index/src/snapshot.rs:618–644`

```text
618:     pub fn entries(&self) -> std::result::Result<Vec<SnapshotEntry>, String> {
619:         let mut cursor = self.entries_offset()?;
620:         let count = usize::try_from(self.entry_count).map_err(|_| "impossible entry count")?;
621:         if count > self.payload.len().saturating_sub(cursor) / 16 {
622:             return Err("entry count exceeds its payload".into());
623:         }
624:         let mut entries = Vec::with_capacity(count);
625:         for _ in 0..self.entry_count {
626:             let entry_id = i64::from_le_bytes(self.bytes_at(cursor, 8)?.try_into().unwrap());
627:             cursor += 8;
628:             let root = RootId(i64::from_le_bytes(
629:                 self.bytes_at(cursor, 8)?.try_into().unwrap(),
630:             ));
631:             cursor += 8;
632:             let mut entry = SnapshotEntry {
633:                 entry_id,
634:                 root,
635:                 parent: None,
636:                 name: None,
637:                 path: None,
638:                 kind: None,
639:                 hidden: false,
640:                 size_bytes: None,
641:                 alloc_bytes: None,
642:                 mtime_ns: None,
643:                 ctime_ns: None,
644:                 mode: None,
```

### `crates/templar-client/src/tree_source.rs:80–98`

```text
80: const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
81: 
82: pub async fn load_snapshot_tree(connection: &zbus::Connection, root: &Path) -> Result<TreeLoad, TreeSourceError> {
83:     let proxy = FilesIndexProxy::new(connection).await.map_err(|error| call_error(&error))?;
84:     let request = SnapshotRequest {
85:         fields: TREE_FIELDS,
86:         detail: SnapshotDetail::Materialized,
87:         subtree: Some(NativePath::from_path(root)),
88:         include_coverage: vec![Coverage::Opaque, Coverage::Inventory, Coverage::Detailed],
89:         ..SnapshotRequest::default()
90:     };
91:     let request_json = serde_json::to_string(&request).map_err(|error| TreeSourceError::Call(error.to_string()))?;
92:     let (fd, manifest_json) = proxy.open_snapshot(&request_json).await.map_err(|error| call_error(&error))?;
93:     let manifest: SnapshotManifest = serde_json::from_str(&manifest_json)
94:         .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
95:     let bytes = read_fd(fd.into(), manifest.payload_len)
96:         .map_err(|error| TreeSourceError::Snapshot(error.to_string()))?;
97:     decode_snapshot_tree(&bytes, &manifest, root)
98: }
```

### `crates/templar-client/src/tree_source.rs:173–186`

```text
173: fn read_fd(fd: std::os::fd::OwnedFd, payload_len: u64) -> std::io::Result<Vec<u8>> {
174:     use std::io::{Read, Seek, SeekFrom};
175:     let expected = payload_len.checked_add(u64::from(HEADER_LEN)).filter(|bytes| *bytes <= MAX_SNAPSHOT_BYTES)
176:         .ok_or_else(|| std::io::Error::other("snapshot exceeds the 512 MiB client transport bound"))?;
177:     let mut file = std::fs::File::from(fd);
178:     if file.metadata()?.len() != expected { return Err(std::io::Error::other("snapshot descriptor length differs from manifest")); }
179:     file.seek(SeekFrom::Start(0))?;
180:     let mut bytes = Vec::new();
181:     bytes.try_reserve_exact(usize::try_from(expected).map_err(std::io::Error::other)?)
182:         .map_err(std::io::Error::other)?;
183:     file.take(expected + 1).read_to_end(&mut bytes)?;
184:     if bytes.len() as u64 != expected { return Err(std::io::Error::other("snapshot descriptor changed length")); }
185:     Ok(bytes)
186: }
```

<a id="e05"></a>
## E05 — Synchronous queries/snapshots execute in async handlers; reader retention is not admission control

Source-confirmed call paths. This is a responsiveness/concurrency risk, not a measured executor starvation incident. A bounded blocking lane and cooperative cancellation are proposals.

### `services/templar-filed/src/index_host.rs:298–353`

```text
298:     /// One JSON argument (`Query`, `query.rs`) so filters/sort/field-mask can grow without a
299:     /// signature change.
300:     async fn search(
301:         &self,
302:         query_json: &str,
303:         #[zbus(header)] header: zbus::message::Header<'_>,
304:     ) -> Result<(String, u64, String)> {
305:         note_consumer(&header);
306:         let query: index_core::Query =
307:             serde_json::from_str(query_json).map_err(|error| Index1Error::Fdo(failed(error)))?;
308:         let result = self.0.index.core().search(&query)?;
309:         let generation = result.generation.0;
310:         let cursor = result.cursor.clone();
311:         let page_json = json(&wire_search_result(&result)).map_err(Index1Error::Fdo)?;
312:         Ok((cursor, generation, page_json))
313:     }
314: 
315:     /// Random access inside a pinned result set. `SnapshotRequired` when the cursor aged out.
316:     async fn fetch_page(
317:         &self,
318:         cursor: &str,
319:         offset: u32,
320:         count: u32,
321:         #[zbus(header)] header: zbus::message::Header<'_>,
322:     ) -> Result<String> {
323:         note_consumer(&header);
324:         let page = self.0.index.core().fetch_page(cursor, offset, count)?;
325:         json(&wire_page(&page)).map_err(Index1Error::Fdo)
326:     }
327: 
328:     /// Best effort: releases a cursor early. Idle expiry covers a client that never calls this.
329:     async fn close_cursor(&self, cursor: &str) {
330:         self.0.index.core().close_cursor(cursor);
331:     }
332: 
333:     /// A kernel-sealed `TEMPLARIDX1` bulk snapshot: a consistent, streamed picture of selected
334:     /// roots, consumable without SQLite. Publication fails if kernel sealing is unavailable.
335:     async fn open_snapshot(&self, request_json: &str, #[zbus(header)] header: zbus::message::Header<'_>) -> Result<(Fd<'static>, String)> {
336:         note_consumer(&header);
337:         let request: SnapshotRequest =
338:             serde_json::from_str(request_json).map_err(|error| Index1Error::Fdo(failed(error)))?;
339:         let descriptor = rustix::fs::memfd_create("templar-index-snapshot",
340:             rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING)
341:             .map_err(|error| Index1Error::Fdo(failed(error)))?;
342:         let mut file = std::fs::File::from(descriptor);
343:         let mut manifest: SnapshotManifest = self.0.index.core().write_snapshot(&mut file, &request)?;
344:         rustix::fs::fcntl_add_seals(&file, rustix::fs::SealFlags::WRITE
345:             | rustix::fs::SealFlags::GROW | rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::SEAL)
346:             .map_err(|error| Index1Error::Fdo(failed(error)))?;
347:         manifest.sealed = true;
348:         let readonly = std::fs::File::open(format!("/proc/self/fd/{}", file.as_raw_fd()))
349:             .map_err(|error| Index1Error::Fdo(failed(error)))?;
350:         let owned: std::os::fd::OwnedFd = readonly.into();
351:         let manifest_json = json(&manifest).map_err(Index1Error::Fdo)?;
352:         Ok((Fd::from(owned), manifest_json))
353:     }
```

### `crates/templar-file-index/src/lib.rs:338–358`

```text
338:     pub(crate) fn with_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
339:         let connection = {
340:             let mut pool = self.0.readers.lock();
341:             pool.pop()
342:         };
343:         let connection = if let Some(connection) = connection {
344:             connection
345:         } else {
346:             let connection = Connection::open_with_flags(
347:                 &self.0.path,
348:                 OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
349:             )?;
350:             connection.busy_timeout(std::time::Duration::from_secs(5))?;
351:             connection
352:         };
353:         let outcome = body(&connection);
354:         let mut pool = self.0.readers.lock();
355:         if pool.len() < self.0.config.read_connections {
356:             pool.push(connection);
357:         }
358:         outcome
```

### `crates/templar-file-index/src/arrays.rs:495–546`

```text
495:     fn match_ranks(&self, needle: &[u8]) -> Vec<u8> {
496:         let count = self.len();
497:         let mut ranks = vec![NONE; count];
498:         let threads = std::thread::available_parallelism()
499:             .map_or(1, std::num::NonZero::get)
500:             .min(8);
501:         if count < PARALLEL_FROM || threads == 1 {
502:             self.match_names(needle, 0, &mut ranks);
503:         } else {
504:             let chunk = count.div_ceil(threads);
505:             std::thread::scope(|scope| {
506:                 for (index, slice) in ranks.chunks_mut(chunk).enumerate() {
507:                     scope.spawn(move || self.match_names(needle, index * chunk, slice));
508:                 }
509:             });
510:         }
511:         // Path-only: every row in the subtree range of a directory whose name matches. Ranges
512:         // are marked as +1/-1 at their ends and swept once, so nested matches cost nothing extra.
513:         let directory = EntryKind::Directory.as_i64() as u8;
514:         let mut delta: Vec<i32> = Vec::new();
515:         for (_, position) in &self.dirs_by_id {
516:             let position = *position as usize;
517:             if ranks[position] < PATH_ONLY && self.kind[position] == directory {
518:                 let (start, end) = (self.subtree_start[position], self.subtree_end[position]);
519:                 if start < end {
520:                     if delta.is_empty() {
521:                         delta = vec![0; count + 1];
522:                     }
523:                     delta[start as usize] += 1;
524:                     delta[end as usize] -= 1;
525:                 }
526:             }
527:         }
528:         if !delta.is_empty() {
529:             let mut depth = 0i32;
530:             for (rank, change) in ranks.iter_mut().zip(&delta) {
531:                 depth += change;
532:                 if depth > 0 && *rank == NONE {
533:                     *rank = PATH_ONLY;
534:                 }
535:             }
536:         }
537:         ranks
538:     }
539: 
540:     fn match_names(&self, needle: &[u8], offset: usize, ranks: &mut [u8]) {
541:         for (index, slot) in ranks.iter_mut().enumerate() {
542:             let name = self.name(offset + index);
543:             *slot = if name == needle {
544:                 0
545:             } else if name.starts_with(needle) {
546:                 1
```

<a id="e06"></a>
## E06 — Volume-return primitives are not wired into a complete production lifecycle

Whole-tree *.rs search found only the definitions of root_returned( and set_volume_identity(. This does not prove that no other reconciliation path can eventually rediscover files. It establishes that these advertised identity/return helpers have no call sites in the supplied source. The return helper compares device number and treats missing identity as matching.

### `crates/templar-file-index/src/model.rs:440–452`

```text
440: }
441: 
442: /// Enough identity to notice that a removable root went away and came back as the same volume.
443: #[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
444: pub struct VolumeIdentity {
445:     pub dev_id: Option<u64>,
446:     pub uuid: Option<String>,
447:     pub mount_point: Option<NativePath>,
448:     pub label: Option<String>,
449: }
450: 
451: /// The watcher's honest self-report, owned here so `templar files index status`, `Index1.Roots` and
452: /// a later `templar doctor` check all read one model instead of three.
```

### `crates/templar-file-index/src/lib.rs:565–581`

```text
565:     pub fn set_volume_identity(&self, root: RootId, volume: &VolumeIdentity) -> Result<()> {
566:         self.with_write(|connection| {
567:             connection.execute(
568:                 "UPDATE roots SET volume_dev_id = ?2, volume_uuid = ?3, volume_mount = ?4,
569:                                   volume_label = ?5
570:                   WHERE root_id = ?1",
571:                 rusqlite::params![
572:                     root.0,
573:                     volume.dev_id.map(|id| id as i64),
574:                     volume.uuid,
575:                     volume.mount_point.as_ref().map(NativePath::as_bytes),
576:                     volume.label,
577:                 ],
578:             )?;
579:             Ok(())
580:         })
581:     }
```

### `crates/templar-file-index/src/reconcile.rs:472–499`

```text
472: /// Bring a removable root back.
473: ///
474: /// A volume that returns is not automatically the volume that left: a different disk can appear at
475: /// the same mount point. When the identity differs, everything the index remembers about that path
476: /// belongs to another disk and the root is rebuilt from nothing rather than merged.
477: pub fn root_returned(index: &Index, root: RootId) -> Result<RootState, IndexError> {
478:     let status = index.root_status(root)?;
479:     let path = status.path.to_path();
480:     if !path.exists() {
481:         return Ok(status.state);
482:     }
483:     let now = volume_identity(&path);
484:     let same = match (status.volume.dev_id, now) {
485:         (Some(before), Some(after)) => before == after,
486:         // Nothing to compare with: treat it as the same volume and let the repair pass decide.
487:         _ => true,
488:     };
489:     let status = index.mark_online(root)?;
490:     if !same {
491:         index.mark_stale(root, &StaleReason::MountDisappeared)?;
492:     }
493:     Ok(status.state)
494: }
495: 
496: fn volume_identity(path: &Path) -> Option<u64> {
497:     use std::os::unix::fs::MetadataExt;
498:     path.metadata().ok().map(|metadata| metadata.dev())
499: }
```

<a id="e07"></a>
## E07 — Every open/migration error triggers derived-cache replacement

Source-confirmed recovery behavior. The index is derived data, not the user's documents. Busy, full, inaccessible and corrupted caches must not all be handled with the same destructive policy. Isolation and error classification should precede recovery.

### `crates/templar-file-index/src/lib.rs:238–254`

```text
238:     /// Opens (creating and migrating if needed) the index at `path`.
239:     ///
240:     /// A database this build cannot understand is **rebuilt**, not rejected: these rows are
241:     /// derived from the filesystem, so the worst a rebuild costs is a rescan. The old file is
242:     /// moved aside as `*.corrupt-<ms>` when that is possible, so a bug leaves evidence.
243:     pub fn open(path: &Path, config: IndexConfig) -> Result<Self> {
244:         if let Some(parent) = path.parent() {
245:             std::fs::create_dir_all(parent)?;
246:         }
247:         let writer = match Self::open_migrated(path) {
248:             Ok(connection) => connection,
249:             Err(error) => {
250:                 tracing::warn!(%error, path = %path.display(), "rebuilding the file index");
251:                 Self::move_aside(path);
252:                 Self::open_migrated(path)?
253:             }
254:         };
```

### `crates/templar-file-index/src/lib.rs:280–302`

```text
280:     fn open_migrated(path: &Path) -> Result<Connection> {
281:         let connection = Connection::open_with_flags(
282:             path,
283:             OpenFlags::SQLITE_OPEN_READ_WRITE
284:                 | OpenFlags::SQLITE_OPEN_CREATE
285:                 | OpenFlags::SQLITE_OPEN_NO_MUTEX,
286:         )?;
287:         connection.busy_timeout(std::time::Duration::from_secs(5))?;
288:         schema::migrate(&connection)?;
289:         Ok(connection)
290:     }
291: 
292:     fn move_aside(path: &Path) {
293:         let aside = path.with_extension(format!("corrupt-{}", model::now_ms()));
294:         if std::fs::rename(path, &aside).is_err() {
295:             let _ = std::fs::remove_file(path);
296:         }
297:         for suffix in ["-wal", "-shm"] {
298:             let mut sidecar = path.as_os_str().to_os_string();
299:             sidecar.push(suffix);
300:             let _ = std::fs::remove_file(PathBuf::from(sidecar));
301:         }
302:     }
```

<a id="e08"></a>
## E08 — Cursor expiry is opportunistic and the native shell only consumes its first page

Source-confirmed store expiry calls and shell first-page code. A whole-tree shell Rust search found no .fetch_page( or .page( calls. Core/client paging exists; the UI does not expose it through these calls. Cursor expiry occurs on insert/get, not oldest_generation; retention can therefore remain pinned after nominal TTL until further cursor activity.

### `crates/templar-file-index/src/query.rs:293–343`

```text
293: impl CursorStore {
294:     pub(crate) fn len(&self) -> usize {
295:         self.cursors.len()
296:     }
297: 
298:     pub(crate) fn oldest_generation(&self) -> Option<Generation> {
299:         self.cursors.values().map(|cursor| cursor.generation).min()
300:     }
301: 
302:     pub(crate) fn insert(&mut self, cursor: Cursor, max: usize, ttl: Duration) {
303:         self.expire(ttl);
304:         while self.cursors.len() >= max {
305:             let Some(victim) = self
306:                 .cursors
307:                 .values()
308:                 .min_by_key(|cursor| cursor.last_used)
309:                 .map(|cursor| cursor.id.clone())
310:             else {
311:                 break;
312:             };
313:             self.cursors.remove(&victim);
314:         }
315:         self.cursors.insert(cursor.id.clone(), cursor);
316:     }
317: 
318:     pub(crate) fn get(&mut self, id: &str, ttl: Duration) -> Result<&Cursor> {
319:         self.expire(ttl);
320:         match self.cursors.get_mut(id) {
321:             Some(cursor) => {
322:                 cursor.last_used = Instant::now();
323:                 Ok(&*cursor)
324:             }
325:             None => Err(IndexError::SnapshotRequired {
326:                 reason: "the cursor has expired or was never issued by this process",
327:             }),
328:         }
329:     }
330: 
331:     pub(crate) fn remove(&mut self, id: &str) {
332:         self.cursors.remove(id);
333:     }
334: 
335:     pub(crate) fn invalidate_root(&mut self, root: RootId) {
336:         self.cursors
337:             .retain(|_, cursor| !cursor.roots.contains(&root));
338:     }
339: 
340:     fn expire(&mut self, ttl: Duration) {
341:         self.cursors
342:             .retain(|_, cursor| cursor.last_used.elapsed() < ttl);
343:     }
```

### `crates/templar-file-index/src/generation.rs:390–414`

```text
390:             MetadataFreshness::Unverified
391:         };
392:         let generation = self.generation;
393:         let root = self.root;
394:         let retention = self.index.config().tombstone_retention_generations;
395:         let max_changes = self.index.config().max_changes;
396:         let oldest_cursor = self.index.cursors().lock().oldest_generation();
397:         let permission_denied = self.permission_denied;
398:         let limited = self.limited;
399:         let reason = self.options.reason.clone();
400: 
401:         let (entries, state) = self.index.with_write(|connection| {
402:             self.commit_transaction(
403:                 connection,
404:                 Committing {
405:                     complete,
406:                     metadata,
407:                     retention,
408:                     max_changes,
409:                     oldest_cursor,
410:                 },
411:             )
412:         })?;
413:         self.finished = true;
414: 
```

### `shell/templar-shell/src/surface/app.rs:5948–5980`

```text
5948: async fn run_search(roots: Vec<PathBuf>, text: &str) -> Result<IndexedSearchResults, String> {
5949:     use templar_file_index::{NativePath, Query};
5950:     let connection = crate::data::session().await.map_err(|error| format!("Search — {error}"))?;
5951:     let query = Query {
5952:         text: text.to_owned(),
5953:         within: roots.iter().map(|path| NativePath::from_path(path)).collect(),
5954:         page_size: 50,
5955:         require_metadata: true,
5956:         ..Query::default()
5957:     };
5958:     let session = templar_client::file_index::SearchSession::search(&connection, &query)
5959:         .await.map_err(|error| format!("Search — {error:#}"))?;
5960:     let result = session.result();
5961:     let entries = result.hits.iter().filter(|hit| hit.present).map(|hit| {
5962:         let row = &hit.row;
5963:         let path = row.path.native_path().map_err(|error| format!("Search — invalid native path: {error}"))?;
5964:         let name = path.file_name().map_or_else(|| path.display().to_string(),
5965:             |name| name.to_string_lossy().into_owned());
5966:         Ok(BrowseEntry {
5967:             path, name,
5968:             kind: if row.kind == 1 { EntryKind::Directory } else { EntryKind::File },
5969:             hidden: row.hidden,
5970:             is_symlink: row.kind == 2,
5971:             size_bytes: row.size_bytes,
5972:             mtime_ms: row.mtime_ns / 1_000_000,
5973:         })
5974:     }).collect::<Result<Vec<_>, String>>()?;
5975:     let total = result.total.map_or_else(|| "unknown".to_owned(), |total| total.to_string());
5976:     let notice = format!("{} of {total} matches · generation {}{}{}",
5977:         entries.len(), result.generation,
5978:         if result.current() { "" } else { " · names or metadata not current; see Settings Files for the reason" },
5979:         if result.truncated { " · retained search truncated; refine the query" } else { "" });
5980:     Ok(IndexedSearchResults { session, entries, notice })
```

<a id="e09"></a>
## E09 — External consumer completion remains an explicit open lane

These are current source-planning statements and provider defaults, not proof of an external fork. The supplied pack does not contain the FSearch or external Silvaview implementation. Do not change provider defaults or claim remote-index integration without those repositories, packaging and runtime evidence.

### `docs/planning/integrated-indexing.md:107–111`

```text
107: Physical/mount evidence remains separate: real `@home` checkpoint privilege/lineage and reboot
108: recovery, the complete mounted home/project watch budget, overflow/convergence under real load,
109: 2M broad-query latency/memory and installed first-party journeys. FSearch fork/packaging and
110: external Silvaview service mode still require their owned source/provenance lane; no substitute
111: private index or claimed external-consumer success is introduced here.
```

### `crates/templar-session-core/src/providers.rs:159–168`

```text
159: const TERMINAL_CATEGORY: &str = "TerminalEmulator";
160: 
161: /// The file-search application TemplarOS integrates, by executable name. Its desktop id is
162: /// `io.github.cboxdoerfer.FSearch.desktop` and its `Exec=` is plain `fsearch`; the executable is
163: /// the more stable of the two, since a repackaging can change the id and not the program.
164: const DEFAULT_FILE_SEARCH: &str = "fsearch";
165: 
166: /// The terminal TemplarOS ships with, by executable name. It wins by priority when it is installed,
167: /// exactly as a distribution default is supposed to: installing a second terminal does not
168: /// silently take the role, and choosing one in Settings still overrules both.
```

### `docs/architecture/files.md:252–279`

```text
252: ### What integrates, and what cannot
253: 
254: There is no plugin API in libfm-qt, so integration is exactly what a file manager offers a
255: distribution from outside:
256: 
257: - **Custom actions** (DES-EMA, `/usr/share/file-manager/actions/*.desktop`, shipped by
258:   `templaros-cli`): "Add to Home" runs `templar home place %f` over `HomeField1.PlaceFolder`, and
259:   "Create project here" runs `templar project create-here %f` over `Projects1.Create` with the id
260:   derived from the folder name. Archive actions run locally, as described below.
261: - **Bookmarks**: filed owns Home places only, not PCManFM-Qt's list. `Browse1.SetBookmark` never
262:   exports implicitly. `Browse1.ExportBookmarks` / `templar files bookmarks export` explicitly
263:   adds missing places to `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, preserving native labels and
264:   unmanaged bytes. A unique durable backup and `bookmarks.templaros-owned.json` record which
265:   lines Templar actually added; existing matching URIs are not adopted. Removing a Home bookmark
266:   never deletes a GTK entry. Publication is atomic; intervening native writes are detected,
267:   restored when possible and retained at the returned conflict recovery path. No import or
268:   destructive synchronization is performed.
269: - **Trash**: nothing to do. `templar-filed`'s `Trash1` already implements the freedesktop Trash
270:   specification on the same directories libfm-qt uses — `$XDG_DATA_HOME/Trash`, the administrator
271:   `$topdir/.Trash/$uid` when it passes the sticky/non-symlink gate, and `$topdir/.Trash-$uid`
272:   otherwise — so Home's Trash drawer and pcmanfm-qt's Trash are the same trash.
273: - **External operations**: PCManFM-Qt create/update/rename/move/trash changes enter the shared
274:   index through the native watcher. They do not become fabricated filed journal/undo records.
275: - **Recents**: not integrated. pcmanfm-qt writes `recently-used.xbel`; `Files.Recents1` keeps its
276:   own answer in the state store and does not read xbel. Deferred, not planned: the two answer
277:   different questions ("what did I open" against "where was I last working").
278: - **Search**: cannot integrate. pcmanfm-qt's Find is its own, and there is no route by which it
279:   could ask `Files.Search1`. Home's search and FSearch remain the indexed answer.
```

<a id="e10"></a>
## E10 — Diagnostics undercount cache storage and budgets need whole-service accounting

The main database file size is reported, not main+WAL+SHM. Read-connection configuration is an idle-retention limit as shown in E05. Cursor count/row limits do exist; they do not replace byte/concurrency/ownership limits.

### `crates/templar-file-index/src/lib.rs:90–134`

```text
90: };
91: 
92: /// How the index behaves, as opposed to what it holds.
93: #[derive(Clone, Debug)]
94: pub struct IndexConfig {
95:     /// Read connections kept open alongside the single writer.
96:     pub read_connections: usize,
97:     /// Rows kept in the bounded change log before the oldest are dropped.
98:     pub max_changes: u64,
99:     /// Live search cursors kept; the least recently used is evicted beyond this.
100:     pub max_cursors: usize,
101:     /// Rows a cursor may pin: the best this many matches of a search, in order. Every match is
102:     /// still counted, so this bounds memory, not correctness.
103:     pub max_cursor_rows: usize,
104:     /// How long an idle cursor survives.
105:     pub cursor_ttl: std::time::Duration,
106:     /// Generations a tombstone outlives before a commit may sweep it.
107:     pub tombstone_retention_generations: u64,
108:     /// Entries staged before a scan flushes them.
109:     pub scan_flush_rows: usize,
110:     /// Mutations applied in one transaction before a batch is split.
111:     pub max_batch: usize,
112:     /// Explicit daemon repair budgets. Unlimited unless the owner opts into a cap.
113:     pub scan_budget: ScanBudget,
114:     /// Answer searches from in-memory arrays of the live rows (`arrays.rs`) once a search has
115:     /// had them built for the current generation, instead of through SQL. Off by default so a
116:     /// library user, and every test that does not ask for it, gets the one deterministic SQL
117:     /// path; the file service turns it on.
118:     pub search_arrays: bool,
119: }
120: 
121: impl Default for IndexConfig {
122:     fn default() -> Self {
123:         Self {
124:             read_connections: 4,
125:             max_changes: 100_000,
126:             max_cursors: 32,
127:             max_cursor_rows: 100_000,
128:             cursor_ttl: std::time::Duration::from_secs(300),
129:             tombstone_retention_generations: 2,
130:             scan_flush_rows: 4_096,
131:             max_batch: 10_000,
132:             scan_budget: ScanBudget::unlimited(),
133:             search_arrays: false,
134:         }
```

### `crates/templar-file-index/src/lib.rs:748–777`

```text
748:     }
749: 
750:     pub fn diagnostics(&self) -> Result<Diagnostics> {
751:         let cursor_count = self.0.cursors.lock().len();
752:         let db_bytes = std::fs::metadata(&self.0.path).map_or(0, |meta| meta.len());
753:         self.with_read(|connection| {
754:             let view = connection.unchecked_transaction()?;
755:             let head = mutation::read_latest_change_seq(&view)?;
756:             Ok(Diagnostics {
757:                 path: self.0.path.display().to_string(),
758:                 schema_version: schema::schema_version(),
759:                 format_version: schema::FORMAT_VERSION,
760:                 scanner_version: schema::SCANNER_VERSION,
761:                 db_bytes,
762:                 generation: read_generation(&view)?,
763:                 planner: if self.0.fts {
764:                     "fts5-trigram".to_owned()
765:                 } else {
766:                     "like-scan".to_owned()
767:                 },
768:                 fts_available: self.0.fts,
769:                 change_seq_floor: schema::read_meta(&view, "min_retained_change_seq")?
770:                     .and_then(|value| value.parse().ok())
771:                     .unwrap_or(0),
772:                 change_seq_head: head,
773:                 cursor_count,
774:                 roots: read_roots(&view)?,
775:             })
776:         })
777:     }
```

<a id="e11"></a>
## E11 — Recent distro evidence is substantially newer than the older daily-driver critique

These are observations recorded in the uploaded source, not tests independently rerun in this review. They establish a narrower, genuine progress claim and explicitly exclude installed desktop, full health promotion, state-schema rollback, Secure Boot and physical hardware acceptance.

### `docs/planning/daily-driver-matrix.md:37–52`

```text
37: ## Bounded native portal observation — 2026-10-06
38: 
39: The native Ubuntu run used private HOME/XDG state, separately owned session/system buses, an
40: authenticated Xvfb display and only an owned portal descriptor/selector. No installed selector,
41: user unit, desktop session or default was changed. Chrome 154.0.8037.97 ran with an owned profile
42: and `--no-sandbox`; this is genuine native frontend/dialog/application interaction, not Flatpak
43: document access or physical/S19 acceptance.
44: 
45: - The installed frontend was `xdg-desktop-portal 1.21.1`. Private `XDG_DESKTOP_PORTAL_DIR` also
46:   relocated its selector lookup; read-only installed GLib schemas plus the memory GSettings backend
47:   were necessary with private data directories. No fabricated application ID was supplied.
48: - Native frontend OpenFile, SaveFile and SaveFiles returned selected URI results. An actual
49:   SaveFiles edit was ignored because its filenames belong to the request; the misleading editable
50:   field was removed, and post-fix keyboard selection plus still-editable SaveFile were exercised.
51: - Concurrent requests had independent windows. Close and caller departure removed only their
52:   request; duplicate handles were rejected. Backend death and unavailable display yielded response
```

### `docs/planning/daily-driver-matrix.md:128–137`

```text
128: observed coordinate mismatch; it does not retroactively convert the earlier browser fixture
129: into physical-button or hardware acceptance.
130: 
131: 
132: Limits: Xvfb/winit remain virtual, Chrome used an owned `--no-sandbox` profile, and no physical
133: hotplug, installed sandbox grant, physical fullscreen/recovery, exclusive modality, Orca, full
134: CJK/emoji font coverage or shipping-default claim is made. Standard parent tokens confer a
135: relationship only: they do not transfer application identity, protected layers or activation
136: authority. Independent fallback is not a fabricated parent or a fabricated activation grant.
137: 
```

### `docs/planning/daily-driver-matrix.md:175–197`

```text
175: ## Bounded canonical Settings and source-package observations — 2026-10-06
176: 
177: Actual native Settings and its CLI use one 17-page inventory with ordinary devices,
178: appearance and Files before advanced pages; internal view discriminants do not determine
179: navigation order. Existing aliases and exact search labels resolve the same typed destination,
180: including explicit `volume`/`audio` precedence over System's volume search label.
181: An owned X11 run exposed wheel-down failing to reveal advanced navigation rows: the shared
182: PixelKit list now follows the existing scrollable wheel convention. Actual post-fix wheel
183: input revealed Resources, History and Hardware. The Files GUI and `--print --page index`
184: both showed the actual watched root at generation 5 with seven entries. Missing private
185: audio/input/display owners remained explicitly unavailable, not fabricated applied values.
186: An external CLI write changed the owned Homegarden limit from 64 to 32; the already-open
187: native GUI showed 32 and the private setting was restored to 64 afterward. The compact
188: screenshots and observations are retained in `.agent/scratch/native-ui-proof-20261006/`.
189: 
190: The unexpected new LiminalView image default now has a real selected Debian package and
191: content-addressed local source closure: five owning source roots, 334 locked dependencies,
192: vendored registry inputs and embedded fonts. Rootless native build from that archive with an
193: empty Cargo home produced the real executable and generated desktop. The extracted package
194: actually rendered the owned PNG in a private native window. Its supported-format command
195: advertised 22 format entries/26 MIME types; this is not a claim that every decoder was exercised.
196: No release Git origin or pin was invented for the non-Git local viewer; the complete archive
197: is the local reproduction input. No installed desktop, application, unit or image was changed.
```

### `docs/planning/daily-driver-matrix.md:199–236`

```text
199: ## Real disposable native deployment observations — 2026-10-06
200: 
201: An unprivileged QEMU TCG/OVMF guest used an actual GPT disk, Btrfs subvolumes,
202: FAT ESP, Ubuntu kernel, systemd, dpkg, APT and a disposable signed archive. No host
203: mount, host-root privilege, network, live user bus or installed desktop was changed.
204: The real hostd factory staged a package-2.0 candidate from package-1.0 known-good,
205: then actual firmware selected the candidate entry. Boot-entry selection uses the
206: stable `templaros-{id}.conf` ID; systemd-boot's `+tries` suffix belongs to the on-disk
207: entry filename, not its selectable identity.
208: 
209: Fresh native hostd in the guest's runtime initramfs overlay resumed persisted
210: `AwaitingReboot` as `Trial` only when the actual kernel deployment ID matched.
211: Selecting an entry alone no longer records a trial boot. The candidate executable
212: and dpkg database both reported 2.0; system and dpkg writes failed with the expected
213: EROFS. Native `Deployments1.Rollback` selected known-good, and a subsequent actual
214: firmware boot restored executable and dpkg 1.0. Original Home/profile hashes,
215: candidate-created documents and shared runtime-state data survived; root stayed
216: read-only. Sealed roots, package databases, journal and EFI selection were not
217: rewritten by the runtime overlay.
218: 
219: A separate guest transaction was interrupted at its fsynced `Staging` journal by
220: killing only the owned guest hostd fixture service. After poweroff, unchanged
221: firmware selection booted known-good; the actual journal still had no candidate
222: and remained `Staging`. Executable/dpkg 1.0, read-only root, Home/profile hashes,
223: the prior candidate-created document and shared-state data were preserved. No
224: incomplete candidate was selected, and no journal, EFI default or health marker
225: was manufactured to obtain this outcome.
226: 
227: That guest also exposed early bus-name publication: the first host API could time
228: out before interfaces existed. Hostd now claims its name after recovery and all
229: registrations, following filed/sessiond. A fresh actual native daemon on a private
230: bus answered its first deployment API after name ownership within a one-second
231: deadline; exact children and disposable state were removed afterward.
232: 
233: These observations cover the minimal real deployment/rollback path, not a fresh
234: installed TemplarOS desktop, full promotion-health chain, shared-state schema
235: migration, Secure Boot, PAM/fingerprint or physical GPU/display daily-driver
236: acceptance. Those remain unsigned release gates.
```

### `docs/architecture/ownership-tiers.md:28–71`

```text
28: ## Tiers
29: 
30: | Tier | Authority | Persistence and rollback |
31: | --- | --- | --- |
32: | System | Installed packages and the privileged host deployment/account writers | Root-paired OS, `/usr`, `/etc`, dpkg/APT databases and APT cache; boot selects that root's own relative Btrfs identity. |
33: | State | The named service/application or reviewed external runtime owner | Narrow directory binds backed by `@var`, host journal/registry, device configuration, caches and application profiles. Reader contracts and explicit migration govern compatibility. |
34: | Work | The user, with application operations explicitly authorized by the user | `/home`, optional `/data` and user documents/projects/media. Package payloads never install here and OS rollback never restores a Work snapshot. |
35: 
36: Most-specific declarations classify nested State below a user's home. A filed
37: exception for user-authorized Work operations is tier-scoped: `%h/**` is not a grant
38: to overwrite another service's State. Shared SQLite/container/bookmark surfaces and
39: legacy overlay deployment exceptions carry explicit pinned decision references and
40: reasons; no blanket wildcard changes ownership.
41: 
42: ## Native deployment boundary
43: 
44: `@var` mounts at `/var/lib/templaros/state`, **not `/var`**. Only the declared
45: directory domains are bound from it. In particular `/var/lib/dpkg`, `/var/lib/apt`
46: and `/var/cache/apt` stay in the selected System root; atomic machine/account file
47: writers are not broken by per-file binds. The native snapshotter verifies that the
48: package databases/cache share the source root's mount identity and that the
49: mounted candidate namespace matches the declared relative subvolume namespace.
50: Legacy blanket-`/var` roots require reviewed offline migration; automatic resume
51: or candidate staging must refuse rather than guess which package database belongs
52: to the root.
53: 
54: GPU mode is persistent State at `/var/lib/templaros/hardware/gpu-mode`; offline
55: and candidate preparation preserve only the old GPU mode when the new target
56: is absent. The driver version lock remains canonical deployment-paired System
57: at `/etc/templaros/hardware/version-lock.toml`; it is not an @var bind or a lock
58: copied from another root's package set. Explicit authenticated lock repair is a
59: declared live-System exception on RW only; RO repair must refuse until a real
60: candidate repair operation exists, never suppress mismatches.
61: Before candidate ESP publication/sealing, hostd derives that root's lock from its
62: selected matching kernel/initramfs and readable paired dpkg metadata. Retained
63: other-ABI module packages are not mistaken for the selected ABI's baseline.
64: 
65: 
66: Native `templar dev deploy/install` queues a pinned hostd candidate. Its return is
67: not installation or promotion. APT download/configuration/installation runs inside
68: the writable candidate's transient `RootDirectory`, with private API/device/PID
69: mounts, no live service-manager bus, read-only public archive inputs (never the
70: archive signer), bounded lifetime and restricted kernel/device authority. Ubuntu
71: maintainer scripts still run; a failing script fails the candidate, not a fake
```

<a id="e12"></a>
## E12 — Original first-release goals and source-closure limits

The founding intent is an integrated desktop on Ubuntu, not replacement plumbing. README status is stale relative to the newer native observations and needs a ledger-derived refresh. Cargo points at absent sibling source directories in this review environment.

### `README.md:1–30`

```text
1: # TemplarOS
2: 
3: **A tightly integrated desktop operating system on an Ubuntu 26.04 LTS core** — with its own compositor policy,
4: shell, file layer, application identity system, resource governor, driver/update transaction system and
5: installation image. Start from scratch in *policy and user interaction*, not in plumbing that already works.
6: 
7: Status: **M1 — the services are real.** The session daemon persists every domain in one state store and
8: launches applications through systemd `StartTransientUnit`; the governor serves its interface, applies
9: protection and contains through a staged ladder that defaults to observing; the file service runs
10: operations as resumable journalled transactions; the compositor core enforces typed layers, one scene and
11: recovery that works without the shell; the host daemon promotes or rolls back a kernel and its driver as a
12: pair. The clients (CLI, portal, shell, Settings) are landing; the compositor and any graphical surface are
13: M2, so **you cannot log into this yet**. See `docs/generated/ROADMAP.md` for what is accepted and on
14: what evidence, `docs/planning/` for the founding analysis, and `docs/adr/` for every decision.
15: 
16: ## The five things the first release must prove
17: 
18: 1. A desktopless **Home Surface** is better than wallpaper and desktop icons.
19: 2. Windows **cannot become lost** during monitor or geometry changes.
20: 3. The machine **remains interactive** under extreme application memory pressure.
21: 4. File opening is **predictable and centrally controlled**.
22: 5. Kernel and NVIDIA updates **succeed as a pair or roll back as a pair**.
23: 
24: ## What is different
25: 
26: * **One application contract.** `org.templaros.AppIdentity1`: a canonical `AppId`, one cgroup per launch
27:   (`app-templaros-<AppId>@<InstanceId>.service`), identity verified through the cgroup — never through the
28:   client's self-declared `app_id`. Dock grouping, associations, resource accounting, notifications and
29:   session restore all key on it.
30: * **One policy daemon** (`templar-sessiond`) instead of a dozen applets and helpers; **one small protected
```

### `docs/planning/01-initial-shape.md:1–15`

```text
1: # My assessment
2: 
3: There is substantial room to build a better Linux desktop operating system around the problems you identified. The useful project, however, is **not a new kernel, libc, init system, package format, or application ecosystem**. Those would consume most of the work while contributing little to your actual goals.
4: 
5: The stronger design is:
6: 
7: > **A tightly integrated desktop OS built on an Ubuntu LTS core, with its own compositor policy, shell, file layer, application identity system, resource governor, driver/update transaction system, and installation image.**
8: 
9: That would legitimately become a new distribution once it had its own repository, installation image, updater, defaults, release policy, and hardware-validation process. It can begin as a replacement desktop session installed on Lubuntu.
10: 
11: The governing principle should be:
12: 
13: > **Start from scratch in policy and user interaction, not in plumbing that already works.**
14: 
15: ---
```

### `Cargo.toml:83–107`

```text
83: 
84: # external
85: anyhow = "1"
86: async-trait = "0.1"
87: bpg-decode = { package = "still265", path = "../bpg-rs/crates/still265", features = ["rayon"] }
88: clap = { version = "4", features = ["derive"] }
89: futures-util = { version = "0.3", default-features = false, features = ["std"] }
90: image = { version = "0.25.10", default-features = false, features = ["bmp", "dds", "exr", "ff", "gif", "hdr", "ico", "jpeg", "png", "pnm", "qoi", "tga", "tiff", "webp"] }
91: libc = "0.2"
92: parking_lot = "0.12"
93: # Dev-tool only (`cargo xtask check-interface-signatures`): no service links it.
94: quick-xml = "0.41"
95: # First-party toolkit adapters. Raster/text/shell/ui are window-host independent; only
96: # pixelkit-windowing links winit/softbuffer. The compositor may use pure UI primitives without
97: # pulling a second event loop or host window backend into its render thread.
98: #
99: # Path dependencies because pixelkit has no git remote yet. Before this is packaged, they must
100: # become git dependencies pinned by revision (ADR-0009's vendored source package cannot reach a
101: # sibling directory outside the repository) — a one-line change per manifest, tracked as a
102: # release blocker rather than a design question.
103: pixelkit-raster = { path = "../pixelkit/crates/pixelkit-raster" }
104: pixelkit-text = { path = "../pixelkit/crates/pixelkit-text" }
105: pixelkit-ui = { path = "../pixelkit/crates/pixelkit-ui" }
106: pixelkit-shell = { path = "../pixelkit/crates/pixelkit-shell" }
107: pixelkit-windowing = { path = "../pixelkit/crates/pixelkit-windowing" }
```

## Isolated algorithm reproduction result

```json
{
  "validation": "Python algorithm reproduction, not compiled Rust or TemplarOS runtime",
  "source": "crates/templar-file-index/src/arrays.rs:443-469,592-597",
  "fixture": {
    "ranges": [
      [
        1,
        100
      ],
      [
        10,
        20
      ]
    ],
    "position": 30,
    "expected": true,
    "source_algorithm": false,
    "normalized_algorithm": true
  },
  "membership_checks": 6300,
  "mismatches_before_normalization": 252,
  "mismatches_after_normalization": 0
}
```

## Relevant upstream constraints

These support API design recommendations, not claims that TemplarOS passed tests.

- U1: Tokio `spawn_blocking` documentation — blocking work on executor threads, bounded CPU concurrency, non-abortability once started. https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
- U2: Linux `inotify(7)` — nonrecursive watches, new-subdirectory races, queue overflow and mount-over-directory blind spot. https://man7.org/linux/man-pages/man7/inotify.7.html
- U3: SQLite WAL — read transactions, checkpointing, reader interference and WAL growth. https://sqlite.org/wal.html
- U4: UDisks Block — filesystem identity metadata including potentially absent IdUUID. https://storaged.org/doc/udisks2-api/latest/gdbus-org.freedesktop.UDisks2.Block.html

## Interpretation rules for the implementation owner

1. Reproduce a defect in the native crate before accepting its fix. E03 supplies a tiny deterministic starting case.
2. Do not replace the fixed October 5 source work with a reimplementation of the older October 1 backlog.
3. Re-run existing private-runtime fixtures only in their isolated environments. Never point a test at the live session bus, HOME, Trash, mount or installed deployment.
4. Treat the external FSearch/Silvaview lanes as unverified until their actual source and package provenance are supplied.
5. Preserve native-path bytes, coverage honesty, namespace/metadata freshness separation, cache identity, generation/change-sequence lineage and owner boundaries throughout optimizations.
