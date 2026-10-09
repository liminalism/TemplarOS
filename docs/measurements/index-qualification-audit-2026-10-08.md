# Index qualification audit — 2026-10-08

## October 9 formatting and refreshed gates

Full workspace formatting now passes after correcting 206 files (193 LegeOS,
13 sibling Pixelkit). The formatter patch and 610 original Rust sources are
retained separately from the two display helper extractions. Required Index1
all-target Clippy and whole-workspace all-target compilation also pass.

The eleven-package native private run passes **1,803 tests, zero failures and
11 ignored** across 35 target summaries, including display tests. The full
interface recipe and all three precise owned debug crash journeys pass again;
the checkpoint provider remains fake. All 675 captured source inputs match.
Counts overlap previous runs and are not added.

The fresh [23-package set](index-packages-2026-10-08-wp17.md) is
`0.1.0~dev1791505403+gebc54d2.dirty`. Controls, hashes, seven runtime stamps and packaged headless
self-checks pass; all 625 archived first-party Rust/TOML/lock files match.
The retained workspace archive is not an isolated workspace rebuild. No
installation, real-volume, installed developer-day or physical HDMI claim.

See the [formatting cohort](index-formatting-2026-10-09-wp17.md) for exact scope,
commands, source and artifact hashes. WP17 retains two of three satisfied checks;
the owner-run day remains open, and the WP8 writer oracle remains. The owner
deferred WP14's legacy comparison and WP16's FSearch adoption for the future own
Index1 GUI. WP0's historical permission reproduction and WP10's retired nested
search-view acceptance still need explicit reconciliation.

The regenerated ledger validates: **1,906 revisions, 1,447 heads, zero errors and
32 warnings**. There are 151 dirty watched paths and 31 unmatched scopes. The
review queue retains 39 stale and 17 at-risk records. WP17 revision 9 is active
with two of three checks satisfied; the canonical parent still has 12 of 18 WPs
completed. The ledger evidence snapshot and final handoff snapshot are retained
separately.

## Earlier post-lint required-gate continuation

After the catalogue/client/filed lint refactors, the native ten-package private-bus
run passes **1,757 tests, zero failures and 11 ignored** across 33 target summaries.
All 623 recorded LegeOS Rust/TOML/lock hashes stayed unchanged during the run.
These counts overlap the earlier cohorts and are not added to them.

The plan's required core/filed/client **all-target Clippy passes with `-D warnings`**,
without new lint suppressions. Whole-workspace all-target compilation and the
complete interface recipe also pass. The formatter still reports differences in
**206 files**: 193 in LegeOS and 13 in sibling Pixelkit (208 path strings include
two aliases). All 193 LegeOS files also need formatting in tracked HEAD. That gate remains
open; this is not a full-workspace CI pass. Exact logs and hashes are retained in
the JSON's `post_lint_required_gates` observation.

The fresh [23-package set](index-packages-2026-10-08-wp17.md) at
`0.1.0~dev1791502518+gebc54d2.dirty` passes controls/hash/stamp and packaged self-checks;
all 625 archived first-party source hashes match. All three precise owned debug
crash journeys also pass again, with the fake checkpoint boundary explicit.
The earlier idle/crash/package observations below retain their original cohorts.
Installed developer-day, real-volume and physical HDMI gates remain open.
The WP8 writer oracle is retained.

The final regenerated ledger validates: **1,890 revisions, 1,432 heads, zero
errors and 32 warnings** (59 dirty watched paths and 31 unmatched scopes).
The review queue retains 39 stale and 17 at-risk records. WP17 revision 8
keeps the original acceptance statements and two of three checks satisfied;
the owner-run installed day is still open.

This extends the [WP17 closure cohort](index-closure-2026-10-08-wp17.md).
The [JSON](index-qualification-audit-2026-10-08.json) retains exact observations
and artifact hashes. Results use native Lubuntu builds and owned fixtures.

## Quiet service

The earlier packaged release daemon (`9f6323e4…`) indexed 5,001 rows on a
private bus. During the settled 60-second window, watcher, mount and PSI
observers recorded zero scheduling/context-switch deltas. Read bytes, write
calls and write bytes also stayed zero. Both owned child processes were reaped.

A separate 55-second sample inside that window recorded zero scheduling and
context-switch deltas for all 29 persistent threads, including the interactive,
bulk and maintenance workers. One temporary Tokio worker exited; its final
counters were not observed. This is not a claim of zero activity for every
thread that existed at any point. The first sampler attempt asserted a fixed
thread set and failed; the successful repeat explicitly accounts for arrivals
and exits.

The earlier 30-second relocation sample remains retained separately: its PSI
observer woke once, with no cause sampled. The quiet repeat does not erase that
observation or establish behavior under pressure.

## Scale attempts

The existing ignored synthetic two-million-row unit test was stopped at an
exact owned process boundary after its WAL reached 5,340,117,432 bytes and its
capped scope encountered memory throttling. It produced no assertion result.
The exact stopped fixture was removed. This attempt is inconclusive and cannot
qualify the physical filesystem workload.

The first headless scale guest completed its 100,000-entry initial scan, then
stopped during the watched prelude because the low-RAM guest's kernel allowance
refused watches. The workload reported incomplete reconciliation and returned
nonzero before reaching two million entries. Logs were exported, the exact guest
was reaped, and its private disk was removed. A TCG repeat completed the prelude
and was stopped at its exact owned workload process after KVM availability was
discovered. Its exported measurements remain a partial run. A 2 GiB KVM guest
then created all two million entries, but its Python monitor failed at a 9p
directory-buffer allocation before a scan result was observed. Its serial log
records an order-six allocation failure rather than an OOM kill. Both exact
guests and private disks were reaped. The 4 GiB KVM repeat, with a guest-memory
copy of the monitor's Python library, passes the complete workload: 2,000,001
rows, complete=true, 37.07-second initial scan and accelerated name p95s below
30 ms. Its exact guest and disk are also reaped. See the
[scale report](index-scale-2026-10-08-wp17.md) for SQL timings and fixture limits.

## Strengthened native acceptance checks

The earlier generation property generated 1,000 actions with skipped commits;
it did not establish 1,000 publications. The recorded parity seeds ran 64 cases
each, below the plan's 512. The tests now require exactly 1,000 commits per
generation case and 512 cases for each of the three recorded parity seeds.
The exact inaccessible (`chmod 000`) cache-parent regression is also added,
along with a production snapshot-decoder check over registered parent and
nested roots with non-UTF8 paths and no duplicate rows.

The native core/filed run now passes **384 tests, zero failures and 10 ignored**
on a private bus with disposable HOME/XDG state. All four generation cases
publish exactly 1,000 commits; parity passes 512 random cases and 512 for each
of the three recorded seeds. Cache files and both sidecars remain byte-identical
under the inaccessible parent. The decoded nested-root rows equal a fresh
treemap scan. The current all-target index/wallpaper Clippy command also passes.
Exact test-source and log hashes are retained in the JSON. This scoped run
overlaps the earlier 1,506-test cohort; their counts are not added.

Historical baseline evidence proves the generation and nested-scope failures
and an unsafe second cache owner. It does not contain the original plan's
before-fix `chmod 000` failure. That missing reproduction must remain explicit.

An ordinary-user native observation of the old recovery filesystem operations
returned `EACCES` for rename and every unlink, preserving the main/WAL/SHM
hashes. This does not execute the old Rust `Index::open`; it shows why this
permission mode prevents the proposed deletion reproduction.

## Ledger coverage

The canonical completion parent now contains all 18 WP acceptance entries.
WP1–WP8 carry their scoped evidence and are completed. The six retained
before-change churn/memory/layout reports are committed as `ebc54d2`, satisfying
WP6's publication requirement. Their measurements precede the implementation
work; this commit is documentation only. All 61 source hashes from that
native core/filed run matched at baseline publication. The subsequent lint
refactors and their fresh cohort are recorded above. Four legacy correctness heads
(bounded watch handoff, one-view snapshots, subtree truth and ordered recovery)
now use the final native cohort's exact passing fixtures. All six audited source
hashes match the retained final run. Their historical provenance remains intact.
This is a reconciliation of that run, not a new execution of those fixtures.

WP0's original before-fix permission reproduction remains open.
WP9 still needs the real-volume journey; WP10's original
nested search-view acceptance needs reconciliation with the retired route.
WP14's comparison and WP16's adoption remain owner-deferred for the future
integrated Index1 GUI. Installed developer-day and physical HDMI observations
remain open. The WP8 oracle remains retained.
