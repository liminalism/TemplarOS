# Index continuation — 2026-10-08

## Latest closure observations

The latest native private-bus/disposable-state run passes **1,803 tests**, zero
failures and 11 ignored across eleven selected packages, including display tests.
Required core/filed/client all-target Clippy, whole-workspace all-target compilation,
the complete interface recipe and **full workspace formatting** pass. Formatting
corrected 206 files (193 LegeOS, 13 sibling Pixelkit); 610 originals and a separate
patch are retained. Two display calculations were extracted into helpers for lint.
All 675 captured source inputs match. See the [new cohort](../measurements/index-formatting-2026-10-09-wp17.md)
and [closure report](../measurements/index-closure-2026-10-08-wp17.md).

The earlier acceptance audit passes **384 core/filed tests**, zero failures and
10 ignored, with stronger generation/parity properties and exact permission
and nested-root snapshot-decoder regressions. WP1–WP4 now have evidence for
every check and are completed. These tests overlap the earlier cohort.
Its index/wallpaper all-target Clippy command passed. See the
[acceptance audit](../measurements/index-qualification-audit-2026-10-08.md).

The complete **two-million-entry filesystem workload passes** in an owned
4 GiB KVM guest: 2,000,001 rows, complete initial scan in **37.07 s**, and
array-backed name-search **p95 13–25 ms**. Path queries retain their slower SQL
timings. The [scale report](../measurements/index-scale-2026-10-08-wp17.md)
discloses the ext4 fixture, watch allowance and measurement limits. The exact
guest and private disk were removed after export.

A separate quiet private-daemon sample records zero scheduler/context-switch
deltas for all 29 persistent threads over 55 seconds, and zero observer/IO
deltas over 60 seconds. One temporary worker exited without observed final
counters; these samples do not establish behavior under pressure.

The [coherent package set](../measurements/index-packages-2026-10-08-wp17.md)
is prepared: **23 packages** at `0.1.0~dev1791505403+gebc54d2.dirty`, with
actual control/hash checks and packaged compositor/helper self-checks passing.
All 625 archived first-party source hashes match the native working tree.
The three precise owned debug crash journeys pass again after formatting and the display helper extraction.
No installation or live-service convergence was performed.

Silvaview now guards known-offline file-manager targets and preserves native
clipboard bytes through file URIs. Its separate continuation passes **48
feature-on / 32 feature-off tests** and the refreshed private worker journey.
See [the new cohort](../measurements/index-silvaview-2026-10-08-offline-actions.md);
the earlier actual nested GUI evidence remains tied to its original executable.

WP9's software relocation gap is closed: a strongly identified volume observed
at one other mount supplies a native-byte location hint. Settings confirms the
new registration identity before removing the old registration. The 13 injected
volume cases and 75 Settings tests pass. Watcher/mount idle deltas are zero;
the pressure monitor recorded one wakeup whose cause was not sampled. Real mounts
remain owner-run. See [relocation](../measurements/index-volumes-2026-10-08-relocation.md).

Precise owned scan/snapshot and fake-checkpoint delta crash journeys pass again
on the refreshed daemon. The [crash report](../measurements/index-crashes-2026-10-08-wp17.md)
retains both binary/source cohorts. The WP8 oracle, installed developer-day
acceptance and physical HDMI gate remain open.

The owner deferred the legacy WP14 search comparison and WP16 fork adoption,
selecting a future own integrated Index1 GUI with a large FSearch-style window.
The FSearch fork remains a qualified prototype. Later sections retain historical
cohort results; use this section for current closure status.

The canonical parent contains all 18 WP entries. WP1–WP8 are completed; the
six retained before-change benchmark reports are committed as `ebc54d2`.
WP0's missing historical permission failure, WP9's real-volume journey and
WP10's retired nested search-view check remain explicit. WP17 has two of three
checks satisfied; the installed developer day remains open. The latest ledger
validation and its dirty/unmatched-scope warnings are retained in the closure
report.

This continuation resumes the interrupted WP7/WP8 edits in
[the implementation plan](implementation-plan.md). It preserves the preceding uncommitted
WP0–WP6 work. Later sections record the subsequent work packages. The broader campaign,
installed journeys and hardware qualification remain open. The observed source is the working tree based
on `12295f351ab14e649f900573190060cd88a7251e`. The later `ebc54d2` commit
contains only the six baseline reports. The latest 1,803-test run records
675 source hashes, all unchanged during that run. Earlier 61-hash observations
remain tied to their original cohort. These are native Linux results,
not installed-build claims.

## Implemented

- Interactive search/page lanes, bulk snapshot exports, large change reads, and maintenance
  array builds/cache retries. Admission bounds, caller fairness, latest-tag cancellation,
  dropped-future cleanup and bus-name departure cleanup prevent abandoned work/cursors.
- A bounded reader pool with cancellation-aware acquisition, SQLite interruption/progress
  checks and RAII cleanup; matching uses the configured thread cap and reusable scratch.
- Seekable streaming snapshots with a 256 KiB buffer, rolling CRC32C, service/request quotas
  and deadline cancellation. The header is patched only after successful serialization.
  All memfd creation, writing, sealing and read-only reopening run on the bulk lane.
- Borrowed entry/path iterators and immutable mmap consumption. The client checks all four
  seals, exact descriptor length and manifest identity before decoding into its final row vector.
- Summary-first shell requests and depth-three subtree materialization for descendant views.
  Rendering now retains cached directory bytes without fake leaf rows or double counting.
  Ordinary Detailed/Inventory directory aggregates are not currently cached: missing totals
  remain visibly partial, and their production is a proposed follow-up.
- The previous snapshot writer remains a test-only byte oracle until WP17. Format numbers and
  header layout remain unchanged. Interface gates pass; the previously implemented six
  Host.DisplayRecovery1 methods were also added to the frozen baseline.

## Native verification

The capped native command passed **1,186 tests**, with zero failures, across core, client,
filed, treemap, shell and compositor (24 target summaries). This includes 24 writer parity cases
(three fixtures × four masks × two details), rolling CRC chunk boundaries, byte/entry quotas,
SQL/snapshot cancellation, reader limits, lane replacement/fairness/cleanup, real kernel seals,
read-only descriptor checks, production snapshot decoding and rendered-tree cached totals.

Eight distinct private bus clients completed 160 typing queries while 78 concurrent Browse1
samples had a worst latency of **12.167 ms**, below the 100 ms bound. The setup now waits for
owned service readiness before acquiring its observation. The previous departure fixture's
20-minute hang was a retained proxy clone blocking zbus graceful shutdown; dropping the proxy
and bounding shutdown fixes it.

The model-only core check passed. The normal client dependency graph contains no rusqlite.
The headless compositor check returned `ok`. Interface, proxy and frozen-signature gates passed.
Two existing deprecated `wide::blend` warnings originate in sibling `bpg-rs/still265`.

Full native, private load and watchdog logs are retained via `akr scratch keep` in
`.agent/scratch/index-hdmi-native-regressions.log`, `index-eight-client-browse.log` and
`hdmi-watchdog-pressure-tests.log`.

## Measurements

The current release binaries run only on owned private buses with disposable fixture databases.
No project build or daemon was started on the person's live service bus.

| Measurement | Result | Retained report |
|---|---|---|
| Snapshot writer, 100k / 1M synthetic rows | Peak RSS deltas 0.8 / 0 MiB; difference below the 8 MiB WP8 bound | [Memory](../measurements/index-memory-2026-10-08-after-wp7-wp8.md) |
| Mapped production client, 1M rows | 184.7 MiB sealed payload + 90.4 MiB decoded storage; measured RSS delta 274.0 MiB | [Memory JSON](../measurements/index-memory-2026-10-08-after-wp7-wp8.json) |
| 120 s churn, 100k quiet + 20k build fixture | Browse p95 0.679 ms, max 1.013 ms, zero 100 ms timeouts; peak sampled threads 28 | [Churn](../measurements/index-churn-2026-10-08-after-wp7-wp8.md) |
| 30 s settled idle, 5k fixture | Watcher scheduler/context-switch delta 0; read bytes and write calls/bytes 0; owned children reaped | [Idle JSON](../measurements/index-idle-2026-10-08-after-wp7-wp8.json) |

RSS deltas are not allocation-profiler heap measurements: the synthetic scales reuse an allocator,
and retained pages can reduce a later delta. The 100k client delta can similarly reuse prior
allocation capacity. Mapped pages count toward client RSS. The after decoder includes
COVERAGE/AGGREGATE fields; masks and temporary native-path lengths differ from the before run.
The daemon memory workload uses two concurrent search tasks on one caller connection; the
separate eight-client runtime establishes distinct-caller behavior. No concurrent project build
was run during the release measurements; the native desktop remains an uncontrolled background.
At this earlier measurement point, the 2M filesystem fixture was unqualified
following Btrfs ENOSPC. The later owned ext4 guest qualifies the workload as
reported above. No live home or index was scanned for this continuation.

**The search latency target remains unmet.** Churn search p95 is 989 / 806 / 1813 ms for quiet /
build / all scopes, compared with the before run's 25 / 13 / 16 ms; arrays hit rate dropped from
94% to 78–79%. SQL fallback and global generation invalidation remain in the current design.
The measurements support continuing WP11 root-local shards and then evaluating the WP12 overlay;
this slice does not claim an acceleration improvement or a successful release latency gate.

## Knowledge and next work

The scoped continuation is tracked by `legeos.work.index-wp7-wp8-20261008/1`. Evidence records
retain observed commands and limits. Proposals track shared immutable snapshots and cached
summary directory aggregates. The original core and broader campaign records remain open.

Continue the plan with volume identity/presence (WP9), typed errors and native paging (WP10),
root-local shards (WP11), the measured overlay decision (WP12), diagnostics/WAL (WP13), external
consumers (WP14/WP15), provider switching (WP16), and owner-run installed qualification (WP17).
Do not remove the oracle until its specified closure gate.

## HDMI recovery

At the existing enabled watchdog abort deadline, critical memory pressure now selects a failure
exit without a core; ordinary pressure retains diagnostic aborts. Ten focused tests and an owned
subprocess establish the decision and exit status 70 without a core flag. This addresses the
observed core-collection delay, without proving display-manager recovery or fixing a kernel wait.
See [the HDMI handoff](../architecture/hdmi-validation-handoff.md). Physical multi-GPU/TV,
power and suspend qualification remain open; installed services and GPU ownership were untouched.

## WP9 and WP10 continuation

WP9 now stores mount-aware identity in migration 0008. Native mountinfo escapes remain byte
preserving; UUID plus mount-root fences strong identity, while mount IDs detect bind boundaries.
Absent, different and unobservable volumes retain cached rows offline. Scan, reconcile, watcher
arming and delta paths check presence. Weak returns perform full metadata reconciliation.
`AcceptVolume(xs)->()` checks the displayed replacement fingerprint again in the writer
transaction, forgets old rows in one published generation and queues repair on maintenance.
Settings offers explicit replacement confirmation and registration at a new entered location.

The native fake-provider volume suite passes all nine cases; filed passes 184 tests separately.
The release private-daemon idle sample (5,000 entries, 30 seconds) found both watcher and mount
threads and recorded zero scheduler/context-switch/I/O deltas. Both owned processes were reaped.
The interface/proxy/signature gates pass, including 305 frozen members across 44 interfaces.
See [the idle record](../measurements/index-idle-2026-10-08-after-wp9.json).

The owner-run [mount fixture](../../scripts/index-volume-fixture.sh) and its filed companion
build and dry-run cleanly. Its real loopback sequence has **not** been run by the agent. WP9
remains open for that acceptance and any optional UDisks enrichment selected for the
distribution. The later relocation continuation below closes the software discovery/display gap.

WP10 has named typed client errors, stable view tags, one 50 ms Busy retry, retained-row counts,
two concurrent page fetches, 100-row pages and placeholders for unloaded rows. Cached heights
cover the retained range; ordinary frames do not clone all paths. Old query/cursor responses
are rejected. Changed retained rows trigger a typed refresh with scroll/native selection
preserved; a second invalidation within one second waits for typing or scrolling.

The affected native regression run passed **953 tests**, zero failed, nine ignored, across 22
summaries. It includes all 20 pages of 2,000 private-daemon matches through ten renames and a
complete fresh-search ordering comparison. The headless PixelKit test rendered the final match
`paging-1999.txt` and activated `/native/paging-1999.txt`; its viewed screenshot is retained in
`.agent/scratch/index-wp10-last-match.png`. This does not qualify an actual nested display or
installed session. Those WP10 gates remain open.

The goal remains the complete plan through WP17. WP11 shards/compact layout and the measured
WP12 overlay gate are the next core implementation steps; WP13–WP17 and HDMI hardware
qualification remain required. The live desktop was untouched.

## Ledger validation

`akr build` regenerated the seven knowledge views and lock; `akr validate` passed parse, type,
link and resolve stages with no errors. It still reports uncommitted changes on 22 watched paths
and ten existing unmatched scope warnings (old `templar-shell-v1.xml` paths and `monitor-dev/**`).
Its review queue contains 40 stale and 23 at-risk records. These warnings were not repaired by
rewriting unrelated sealed records. The changed-file rustfmt check and `git diff --check` pass.

After the WP9/WP10 evidence revisions, validation again passed all four stages without errors.
The warning set now includes 32 dirty watched paths, the ten existing unmatched scopes, and
the new mount-fixture path that is still untracked at HEAD. The review queue remains 40 stale
and 23 at-risk records. The model-only index build and final changed-file formatting checks pass.

## WP11 continuation

Search accelerators are now immutable per-root shards. Their generation and the registered
root topology are checked in the query's SQLite view. A dirty build root leaves an unchanged
root's shard usable, including queries whose `within` scope intersects only that root. Mixed
validity uses SQL for the complete query. Native path splice cuts preserve ordering when a
root row and its descendants straddle adjacent root names; bounded primary-key lookups supply
raw name sort keys for the multi-root merge. Nested ancestor matches retain SQL semantics.

Folded names and temporary raw names use byte arenas. Final columns are boxed at exact length;
entry and directory IDs use base-relative `u32` offsets when their range permits, with a wide
fallback. A shared reservation counts old, new and reader-pinned retired shards against the
configured cap. Refused builds stay on SQL and expose a reason. Maintenance build claims are
owned by the queued job and released if admission fails or the job is dropped.

The optional cgroup memory-pressure monitor uses blocking kernel notifications and an owned
shutdown event. It releases unused shards in last-query order while active queries retain their
pinned view, then pauses new builds for ten seconds. Unsupported notification access leaves
filed operational and is logged. The trigger API follows the
[Linux PSI documentation](https://docs.kernel.org/accounting/psi.html).

The native release layout fixture contained 100,001 rows. Capacity fell from 8,415,760 to
5,735,062 bytes (31.85%); used bytes per row fell from 64.95 to 57.35. Build time was 65.31 ms
versus 75.00 ms and peak RSS rise was 7,352 KiB versus 11,864 KiB. All final column capacities
equal their used length. The four p95 query changes were −5.77%, −7.37%, +2.58% and −11.87%.
See [the layout report](../measurements/index-layout-2026-10-08-after-wp11.md) and its
[baseline](../measurements/index-layout-2026-10-08-before.md). These measurements share the
native desktop with its uncontrolled background work.

An initial layout run exceeded the latency limit for two broad needles. Removing an unnecessary
single-root positions-to-IDs allocation and copy produced the reported rerun; both runs are
retained in `.agent/scratch/index-wp11/`. The final binary and source hashes are recorded in its
`cohort.json`; the pre-WP11 daemon is retained for the paired churn run.

The original churn fixture edits the named `quiet` root ten times per second. The added
`--edit-build` mode leaves that root unchanged and records the actual edit scope; the legacy
mode remains available. Before/after runs use the same harness with an explicit daemon binary.
The paired 120-second runs produced the following initial-page search measurements:

| Scope | Before p95 ms / arrays | WP11 p95 ms / arrays |
|---|---|---|
| Quiet, 100k unchanged entries | 115.29 / 81.74% | 3.55 / 99.22% |
| Build, 20k base plus 5k bursts | 67.07 / 81.29% | 1641.50 / 22.48% |
| All roots | 1302.22 / 81.51% | 3279.27 / 0% |

See the [before](../measurements/index-churn-2026-10-08-before-wp11-edit-build.md) and
[WP11](../measurements/index-churn-2026-10-08-after-wp11-edit-build.md) reports. The scopes are
queried sequentially, so slower build/all queries reduce the quiet sample count (449 before,
129 after). Browse p95 was 0.700/0.781 ms with zero timeouts. Both owned runs performed at least
62 bursts and 1,230 edits. The Markdown fetch column has been corrected to say next-page fetch;
`search.p95_ms` is the initial-page timing used by the plan's gates.

WP11's unchanged-root acceptance passes. Churning-root performance remains unqualified and
both WP12 triggers are met: p95 above 150 ms and arrays hits below 50%. The ledger completes
`index-wp11-shards-20261008/2` and starts `index-wp12-overlay-20261008/1`; the overall goal
through WP17 remains active.

The final native core/filed regression run passed **367 tests**, zero failures, nine ignored,
across 16 target summaries. Ten parity cases compare complete ordering, total/per-kind counts
and retained rows, including dirty-root isolation, nested/adjacent native paths and reused IDs.
The [30-second idle sample](../measurements/index-idle-2026-10-08-after-wp11.json) found watcher,
mount and PSI threads with zero scheduler/context-switch deltas and zero I/O; both owned
children were reaped. This establishes notification availability and idle behavior in the owned
scope, without inducing real memory pressure or qualifying installed hardware recovery.

## WP12 continuation

The WP11 measurements justify a bounded overlay. Each published root view now shares an
immutable base image and owns an immutable overlay at a committed root generation/change
position. Updated base identities are removed before counting and retaining candidates; live
replacement rows carry their entry ID and written generation, parent, native path/name, folded
name, kind, coverage/hidden flags, size and time. Removed and live versions of one base identity
count once toward the affected-row bound `max(10,000, base_rows / 20)`.

Maintenance reads the retained change position, root generation and changed rows in one SQLite
view. The existing written/deleted-generation indexes expand directory markers to their moved,
removed and overwritten descendants. A rescan, missing history, changed directory kind or
exceeded bound marks the cache dirty and uses SQL until a base rebuild. A per-root queued
claim coalesces commits and releases on drop. Small overlays keep their base; compaction above
25% of the bound waits for the root's rebuild interval.

The merge preserves SQL rank, native-name and native-path ordering and per-kind counts. It
reads bounded base sort keys in the same validating view. Path sort with any in-scope live
replacement deliberately uses SQL; an unknown base `within` directory also keeps the declared
SQL fallback. Active older views remain immutable. The shared cap now reserves base-ID
scratch, copied row storage and native fields before allocation, including retired reader-pinned
overlays. `Roots.diagnostics.arrays.roots` exposes overlay rows, bytes and change positions;
read-only disk diagnostics carry no process-local accelerators.

The refined native core/filed regression run passed **372 tests**, zero failures, nine ignored,
across 16 target summaries. This includes 48 recorded random mutation sequences, deterministic
native rename/delete/recreate cases, every sort direction and metadata/kind/hidden filters, a
10,001-file directory fallback, older-view/retired-byte accounting and dropped-job cleanup. The
model-only core build passes. The release build has only the existing sibling `still265`
deprecation warnings.

The [first release prototype](../measurements/index-churn-2026-10-08-wp12-prototype-1.md)
observed build-root search p95 **37.63 ms** with **93.44%** arrays/overlay hits, versus WP11's
1641.50 ms / 22.48%. Quiet-root p95 was 3.26 ms / 99.92%; all-roots p95 was 32.79 ms / 94.15%.
Sampled live overlay bytes peaked at 900,900. These are capacity estimates rather than an
allocation-profiler measurement. Browse p95 was 0.782 ms with zero timeouts. Fallback tails
remain: build p99 1085.86 ms and all-roots p99 2542.89 ms. The prototype cohort and logs are
retained separately from the subsequent temporary-reservation refinement.

The [final refined-source run](../measurements/index-churn-2026-10-08-after-wp12.md) sampled
985 queries per scope over 120 seconds:

| Scope | Search p95 ms | Search p99 ms | Arrays / overlay hits |
|---|---|---|---|
| Quiet, 100k unchanged entries | 3.28 | 3.61 | 99.90% |
| Build, 20k base plus 5k bursts | 49.43 | 1209.34 | 90.15% |
| All roots | 72.41 | 2671.11 | 90.86% |

The run performed 63 bursts (315,000 created files) and 1,260 build-root edits. Sampled live
overlay capacity peaked at **908,224 bytes**; final retained accelerator reservations were
7,995,225 bytes. Browse p95 was 0.735 ms with zero timeouts and at most 30 sampled daemon
threads. The final source/binary hashes are retained in `.agent/scratch/index-wp12/final-cohort.json`.
The prototype and final reports expose their different sample counts and p99 tails; the new
harness periodically samples accelerator diagnostics as well as the strategy histogram.

WP12's p95 target passes and `index-wp12-overlay-20261008/2` is completed. Its declared SQL
fallbacks still produce long tails; this is not a p99 latency qualification. The overall goal
continues through WP13–WP17 and the owner-run mount, nested-display and HDMI qualifications.

## WP13 continuation

`Roots.diagnostics.runtime` now carries store-free typed lane, shard/overlay, snapshot, cursor,
reader and WAL telemetry. Per-root array hit/miss counters include declared fallback reasons;
shard validity is checked against the same root generation/topology view. Watch coverage now
belongs to each root instead of copying a global count onto every root. Unknown bounded refusal
coverage remains explicit. Snapshot accounting covers server-held descriptors and states that
scope; it does not invent visibility into client-held mappings.

Commit-triggered maintenance performs PASSIVE after scans and each 64 mutation batches, with
reader-age guarded, nonwaiting TRUNCATE above 64 MiB. The daemon's bounded maintenance lane is
asynchronous. Standalone core commits drain their owned checkpoint thread before returning, so
drop/reopen fixtures retain deterministic ownership. No maintenance or metric timer was added.

Doctor and CLI status print every runtime/storage field and health thresholds; disk diagnostics
remain the read-only fallback. Settings Files shows freshness, watch coverage, root relationships,
volume confidence, accelerator status and explicit cache retry/volume actions. The actual private
daemon, native CLI and Settings offline-volume fixture passed their observations. See the
[diagnostics report](../measurements/index-diagnostics-2026-10-08-wp13.md) and
[idle sample](../measurements/index-idle-2026-10-08-after-wp13.json).

Core/filed/Settings passed 452 tests, client 32 and store-free library 13. CLI passed 228 when its
existing Bash journal failure was excluded; the full CLI run still fails that one test and closure
must address it. Signature checks passed all 305 members. WP14–WP17 and the previously identified
real-volume, nested/installed and HDMI hardware checks remain open.

## WP14 — FSearch package and private GTK

The GPL FSearch pin `05ed650ab101ee3d61795fa5eeeb5c32e1e366c3` now has a complete
shared-index-only quilt overlay and an actual DEB built through
`scripts/build-fork-deb.sh` from a clean pinned checkout. Native Meson translation
tests and the owned 5,001-file/non-UTF8 GTK/backend integration pass. The packaged
window visibly renders 101 `f12` results at generation 5 and 5,003 indexed entries;
native paging/selection/sort/rename, bounded AND, typed faults and single-instance
activation pass, with zero watch FDs, no private database and no fixture enumeration.

An actual screenshot exposed a global-count bug missed by the model probe:
Index1 deliberately returns zero for an empty Search. The facade now uses Roots
combined counts and updates application counts on generation changes. The GUI
results-panel check is part of the integration script.

Build provenance, hashes, allowlist, screenshot and limitations are in
[the WP14 measurement](../measurements/index-fsearch-2026-10-08-wp14.md).
The actual nested shell/FSearch same-query generation comparison remains open;
WP14 is active. WP16 provider defaults and WP17 installed/physical HDMI gates
have not been accepted.

## WP15 — Silvaview source and actual nested binary

Silvaview now implements optional Linux service/auto/scan modes in its owning
sibling repository, committed at `413ea0422b37aaf849d501c60ff2d1350d67aab6`.
Existing user Cargo profiles and the screenshot remain uncommitted. The normal
service dependency graph is store-free. Native tests pass 44 with `templaros`
and 30 with the default feature set.

The owned private fixture matches direct scan at 41,251 logical bytes, preserves
native filename bytes, and advances generation 4 → 5 by exactly 37 bytes. Absent
and uncovered fallback, typed Unavailable without traversal, and explicit partial
summaries pass. The consumer has no watch FDs or directory-enumeration calls.

The actual software nested GUI passes summary → Expand folder → generation
refresh: 120 known summary bytes, then 937 materialized bytes, then 974 bytes at
generation 5. Source, freshness and indexed logical size are visible. The native
launcher now supports a lane-owned runtime directory and process marker for safe
cleanup. See [WP15 evidence](../measurements/index-silvaview-2026-10-08-wp15.md).
This qualifies a binary; installed and physical HDMI journeys remain open.

## WP17 — owned process crashes

The actual native filed was killed under an owned debugger immediately before
scan COMMIT and midway through a 5,002-entry snapshot. SQLite retains the complete
old rows/head/change sequence; the partial unsealed frame returns no descriptor.
Prior sealed descriptors survive both daemon deaths byte-identically. Restart
refuses old cursors with SnapshotRequired and publishes new sealed snapshots.

The production delta apply was also killed before COMMIT in the existing
fake-checkpoint filed test process. Its prior rows and checkpoint pointer remain
unchanged, the pending fence survives, and a new process using production
DeltaCatchUp refuses replay before contacting checkpoints. This is not an actual
Btrfs mount/send qualification. See [the crash report](../measurements/index-crashes-2026-10-08-wp17.md).

WP15 is completed. WP14's original shell-notice acceptance references the former
native Files route; clarification is pending and WP16 remains gated. WP17's
installed build-set/developer-day and physical HDMI checks remain open. The
writer oracle is retained. Architecture and the older integrated-indexing status
table now distinguish current implementations from those remaining gates.

The current core/filed/client/treemap/shell/Settings run passes 1,074 tests, zero
failures and 11 ignored. Interfaces, frozen signatures, proxies and packaging pass;
the full interface recipe fails its ownership-provenance gate. Assets/actions/
Wayland/logging pass separately. AKR validation has zero errors and 30 explicit
unmatched-scope warnings. CLI's prior Bash journal failure remains open.

## Owner amendment: integrated search direction

On 2026-10-08 the owner deferred WP14's original shell/FSearch generation
comparison and selected a future own integrated Index1 search GUI with a large
FSearch-style window. WP14/3 retains its two passing prototype checks and the
explicitly deferred manual comparison. WP16's fork shipping/default adoption is
deferred. The actual FSearch fork/package evidence remains available; it does not
establish the final search provider. The independent index, Silvaview, crash and
closure work continues. No retired native Files route was revived.

## WP17 CLI and ownership follow-through

The CLI journal regression now uses a clean owned Bash with `--noprofile --norc`
and explicitly sources its fixture rc; `--rcfile` had loaded installed system
hooks before the source under test. Production hooks were unchanged. All CLI236,
hostd109 and xtask64 tests pass (409 total), and the separate Bash3history/Kitty
integration passes. Current source hashes are in
[the closure report](../measurements/index-closure-2026-10-08-wp17.md).

Current native hostd factory/layout/engine/boot-entry fixtures support refreshed
observation `hostd-stages-candidates-outside-the-declared-deployments-subvolume/5`.
The old actual guest boot/rollback runs remain historical and were not rerun here.
The unchanged ownership decision's supporting pin follows that revision. The
full `just check-interfaces` recipe now passes, including ownership; no gate was
relaxed. Installed/physical qualification and the coherent package set remain open.
