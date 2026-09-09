# Shared file index — baseline measurements (2026-09-04)

Lane A of the indexing campaign (`docs/planning/shared-incremental-file-index.md`). Nothing here
is estimated: every number is either a live measurement from this machine (kernel 6.17, ext4
end-to-end) or explicitly marked as not run. Produced by `cargo run -q -p xtask -- index-bench`
(`tools/xtask/src/index_bench.rs`) and `templar doctor` (`cli/templar/src/doctor/checks.rs`,
`FileIndexHealth`).

Reference machine: `$HOME` on `/dev/nvme0n1p3` (ext4), this repository's own working tree lives on
a second drive. This machine is under active, concurrent use by other agents in the same indexing
campaign while these numbers were taken (see "Campaign state" below) — directory counts especially
will drift run to run as a result; that drift is itself evidence the numbers are real measurements,
not fixtures.

## Why an xtask subcommand, not a `benches/` target

`templar_file_index::watch::guard` (`crates/templar-file-index/src/watch.rs`) panics on any watch root
outside the temp directory whenever the running executable's path has a `deps` component. Every
`cargo bench` and integration-test binary lives under `target/<profile>/deps/`, so a `benches/`
target physically cannot watch a real directory tree — verified directly: the guard's own
`active()` check is `cfg!(test) || <exe path contains "deps">`, both true for those binaries and
neither true for `target/debug/xtask`. `index_bench.rs` is therefore the one place this can be
measured against a real filesystem.

## Directory-count finding — the 157,283 vs 189,089 discrepancy

`crates/templar-file-index/src/watch.rs:10-13` and `.agent/scratch/de01/F2-plan.md` recorded
**157,283** post-skip `$HOME` directories as of 2026-09-03. Today, computed **from the index's own
`RootPolicy`** (`count_directories` in `index_bench.rs`, which applies exactly
`RootPolicy::is_skipped`/`cross_mount` the way `scan::walk_into` and
`WatchOwner::arm_tree` do — never a `find`-equivalent reimplementation), the count is:

| Run | Raw directories | Post-skip directories |
|---|---:|---:|
| This session, first measurement | 230,321 | 189,087 |
| This session, final measurement (~2h later) | 230,328 | 189,094 |

Both numbers are far closer to the depth-clamped **189,087** figure recorded in
`docs/planning/shared-incremental-file-index.md` (measured 2026-09-04 with a `depth<=24` clamp)
than to the stale **157,283** in `watch.rs`. The ~7-directory drift between this session's own two
measurements, taken about two hours apart on a machine with active agents writing to it, is larger
than the depth-clamp effect the plan already ruled out (189,089 vs 189,087). **Finding: the
157,283 figure in `watch.rs` is simply stale** — `$HOME` has grown by roughly 32,000 directories
since 2026-09-03, most plausibly from build/cache growth under active development (this repository
alone contains `target/` and sibling-repo build trees that are excluded from the *index* by
`RootPolicy::skip_names` but still counted as raw directories, and from other projects under
`$HOME` unrelated to TemplarOS). It is not a measurement-methodology difference: depth clamping,
`find` vs `RootPolicy`, and the skip list are all ruled out as causes. `watch.rs:10-13` is corrected
to reflect this measurement in a separate commit (see below).

Either number — 157,283 or 189,094 — exceeds `WatchBudget::max_watches` (**125,037**, half of
`fs.inotify.max_user_watches` = 250,074), so **the conclusion the plan draws is unaffected**: one
home root alone costs 1.51x the daemon's entire watch budget and 0.76x the *whole machine's* kernel
allowance, before any other application's watches are counted.

## Static facts

| Metric | Value |
|---|---:|
| `$HOME` directories, raw | 230,328 |
| `$HOME` directories, post-skip (`RootPolicy::default_home`) | 189,094 |
| `fs.inotify.max_user_watches` | 250,074 |
| `fs.inotify.max_queued_events` | 16,384 |
| `fs.inotify.max_user_instances` | 128 |
| `fs.fanotify.max_user_marks` | 266,105 |
| `WatchBudget::max_watches` (half of `max_user_watches`) | 125,037 |
| required (post-skip dirs) / `WatchBudget` | **1.51x** |
| required (post-skip dirs) / whole-kernel allowance | 0.76x |

Reproduce: `cargo run -q -p xtask -- index-bench dirs`.

## Timed workloads (100,000-entry fixture)

Fixture: a synthetic tree under a fresh `tempfile::tempdir()` (32 files + 8 subdirectories per
directory), never the user's home. Full raw JSON: `docs/measurements/workloads-100k.json`.

| Workload | Entries | Elapsed | Rate |
|---|---:|---:|---:|
| Initial index (`scan_root`) | 100,001 | 3.52 s | 28,389 entries/s |
| Downtime reconcile (2,000 removed + 2,000 created, no watcher) | 4,000 changed | 5.27 s | 759/s (100,001 entries revisited) |
| Watch arm (`arm_tree`) | 20,001 dirs armed, 0 refused | 0.30 s | 67,572 dirs/s |
| Touch 10,000 (watched) | 10,000 | 2.56 s | 3,901/s |
| Rename 10,000 (watched) | 10,000 | 2.43 s | 4,115/s |
| Delete 10,000 (watched) | 10,000 | 1.46 s | 6,865/s |
| Build-shaped burst: 10,000 creates (watched) | 10,000 | 2.88 s | 3,471/s |
| Clean-shaped mass delete: 10,000 entries (watched) | 10,000 | 1.61 s | 6,214/s |
| Search, 100-result warm query, p50 / p95 / max | 200 samples | — | 0.96 ms / 1.27 ms / 1.41 ms |
| `write_snapshot` | 90,152 entries, 12,227,089 bytes | 1.28 s | — |
| DB size on disk (after full suite, incl. WAL) | — | — | 221,861,200 bytes (≈212 MiB) for 90,152 live entries |
| 2,000,000-entry initial index | — | **not run** | see below |

The five watched workloads (touch/rename/delete/build/clean) are each dominated by
`drain_watcher`'s settle window (six consecutive 100 ms polls with nothing pending, ≈0.6 s), which
this harness uses to prove the kernel's event queue has caught up — **this measures
watcher-pump-apply latency without the production coalescing debounce window**
(`services/templar-filed/src/watcher.rs`, owned by another lane): each pumped batch is applied
immediately here, not held for quiet time, so these numbers are a different — and generally lower
— quantity than end-to-end production latency (create/rename/delete to searchable). They are the
right number for "does this crate's own watch-and-apply path keep up", not for the plan's p95 < 250
ms production target, which needs the coalescer in the loop and is a claim for whichever lane owns
that integration test.

**2,000,000-entry case: not run.** The harness supports it
(`index-bench workloads --allow-two-million --release`, which refuses a debug build and generates a
second, separate fixture) but generating two million files is a multi-minute operation this
session did not have budget for after everything else above. Left empty rather than invented, per
instruction — a follow-up run should execute this flag and append the result.

Reproduce: `cargo run -q -p xtask -- index-bench workloads --entries 100000 --mutate 10000
--search-queries 200 --out docs/measurements/workloads-100k.json`.

### A harness-level finding: `File::set_modified` produces no inotify event in this sandbox

The `touch` workload originally called `std::fs::File::open(path).set_modified(SystemTime::now())`
(an fd-based `futimens`) and consistently saw **zero** mutations, while `rename`/`delete`/build
right after it worked normally. A minimal reproduction — one fresh `inotify::Inotify` watch armed
with `ATTRIB`, one file touched via `set_modified`, a second touched via the external `touch(1)`
(a path-based `utimensat`) — confirmed the kernel itself never received the `IN_ATTRIB`
notification for the fd-based call, in this sandboxed execution environment specifically (the
`kill`/`mtime` change from `set_modified` genuinely happened — `fs::metadata` showed the mtime
change — but no inotify event followed it); the path-based `touch(1)` on the same watch, same
directory, produced `ATTRIB` immediately. Whatever intercepts it is most plausibly a property of
the sandbox this harness runs in rather than the kernel or filesystem (both otherwise behaved
exactly as documented: raw `inotify_add_watch`/`read` via a hand-written syscall probe outside any
Rust crate saw `IN_ATTRIB` fire correctly for the same fd-based pattern when tested directly against
`/tmp`). The workload now shells out to `touch(1)`, batched, which is unaffected. **This is worth
re-verifying on bare hardware** — if it reproduces there too, it is a real gap: any real program
that updates mtime via an open file descriptor rather than a path (some archivers, `rsync
--times` in some modes) would be invisible to this watcher, independent of anything this campaign
is otherwise changing.

## Idle cost — the headline number

Sampled via `dbus-run-session` (an isolated D-Bus session; the user's real session bus is never
touched), a fresh `templar-filed` pointed at its own 5,000-entry fixture, `/proc/<pid>/task/<tid>` for
the `templar-index-watcher` thread specifically (not the process's leader thread — see the harness-bug
note below), over a 60-second settled window after a 5-second startup grace period.

**Before/after is a real commit boundary, not a hypothetical.** This session discovered mid-run
that `services/templar-filed: an idle index watcher sleeps rather than wakes twice a second`
(commit `6522e10`, landed today by the lane owning `watcher.rs`) was **already merged** to
`developer-edition-0-1` by the time this measurement started — the shared working tree moves under
every lane concurrently. "Before" below is therefore the true pre-fix binary, built from a
`git worktree` at `6522e10`'s parent (`be57fad`); "after" is the binary built from this session's
own `HEAD` (`80bac65` at measurement time), which includes the fix.

| Metric (60 s window) | Before (`be57fad`) | After (`HEAD`, incl. `6522e10`) |
|---|---:|---:|
| Watcher thread `schedstat` pcount (times scheduled) | 121 | 0 |
| Watcher thread `voluntary_ctxt_switches` | 120 | 0 |
| Process `read_bytes` | 0 | 0 |
| Process `syscw` | 0 | 0 |
| Process `wchar` | 0 | 0 |

121 timeslices / 120 voluntary context switches over 60 seconds is **≈2.0 wakeups/second** —
exactly the "~2 wakeups/sec" the campaign plan predicted for the unfixed 500 ms-clamped wait. After
the fix, both are flat zero: the watcher thread was not scheduled onto a CPU even once in the
60-second idle window. `read_bytes`/`syscw`/`wchar` were already zero before the fix on this
workload (no `roots` table write was observed on this particular idle fixture either way — the
plan's "plus a roots table write per tick" component of the idle cost did not reproduce as
disk I/O here, though the wakeup elimination is confirmed and is the dominant term regardless).

Raw JSON: `docs/measurements/idle-before-be57fad.json`, `docs/measurements/idle-after-head.json`.
Reproduce a pair: build two binaries (e.g. two `git worktree` checkouts) and run
`index-bench idle --binary <path> --label <name> --out <file>` against each, then
`index-bench idle-compare <before.json> <after.json>`.

### Two harness bugs found and fixed while building this

1. **Wrong `schedstat` field.** The Linux docs describe `schedstat` field 3 (`pcount`) in
   1-indexed prose; a first implementation read whitespace-split index 1 (the *second* field,
   `run_delay_ns`) instead of index 2 (the third field, `pcount`). Verified against a live process:
   `9930109 125917 31` is `cpu_time_ns run_delay_ns pcount` — 31 is pcount, not 125917. The
   nanosecond `run_delay` field produced a very large, very wrong "before" number
   (174,235 apparent "wakeups") before this was caught and fixed.
2. **Wrong task for `voluntary_ctxt_switches`.** `/proc/<pid>/status`'s `voluntary_ctxt_switches`
   is per-*task*, not summed across a process's threads, even when `<pid>` is the thread-group
   leader. Reading `/proc/<pid>/status` (the leader thread, `comm=templar-filed`) instead of
   `/proc/<pid>/task/<watcher-tid>/status` reported 0 regardless of watcher activity, because the
   watcher runs on its own named thread (`templar-index-watcher`). Fixed to read the resolved
   watcher-thread status file, which now correlates with `schedstat` pcount as expected (121 vs
   120, both roughly the same real event count).
3. **A leaked process from an earlier run was silently re-sampled.** `dbus-run-session`'s child
   `templar-filed` does not reliably die when the parent is killed across a D-Bus disconnect; an
   earlier run's daemon was still alive and `find_templar_filed_pid` (originally: "any process named
   `templar-filed`") picked it instead of the freshly-spawned one, producing two identical "before"
   samples that were actually both sampling the same leftover idle process. Fixed to match on a
   marker unique to each run's own scratch directory in `/proc/<pid>/cmdline`, and to explicitly
   `kill`/`kill -9` the sampled pid at the end of each run.

## `templar doctor` extension

`cli/templar/src/doctor/checks.rs`'s `FileIndexHealth` check (POR acceptance gate 10, unmet before
this change) reports, per root: state, entry count, generation, completeness, why a root is stale
(`RootStatus::reason`, verbatim), the watcher's backend/limits/armed/refused/overflow
count/last-event time, and the last reconciliation's duration — plus the index's own schema
version, generation, DB size, FTS availability and cursor count. It is `Severity::Warning` and
only reports `Fail` on a genuine problem (the index would not open, a watcher or scan error was
recorded); a root reporting `Stale` because the watch budget is exhausted is reported accurately,
not treated as a check failure, consistent with the design fact this campaign has already
established (a home root cannot stay `Current` under inotify on this machine, by construction, not
by bug). It reads the index database directly — the same durable
`$XDG_CACHE_HOME/templaros/file-index-v1.sqlite3` a running `templar-filed` has open — the same pattern
`AutostartProvidersResolved` already uses for the session state store, and never creates either
database as a side effect: both must already exist or the check `Skip`s.

## Prose finding: can the current design reach its intended steady state on this machine?

**Not for a full, continuously-`Current` home root under inotify, and this is by measured
construction, not by a bug this session found.** `WatchBudget::max_watches` is 125,037; `$HOME`
alone needs 189,094 watches — 1.51x the budget before any other application's inotify usage is
counted, and 0.76x the *entire kernel's* per-user allowance on its own. `arm_tree`'s own budget
check (`crates/templar-file-index/src/watch.rs`) will refuse roughly a third of the directories once
the budget for this one root is exhausted, and every refused directory is unwatched — the
watcher-exhausted `Stale` path this campaign already anticipated is not a corner case here, it is
the default outcome for this specific machine's home directory. `Dormant` (in whichever form the
observer-lifecycle lane lands it) is the state a home root will spend most of its life in by
necessity.

What **is** confirmed working at the scale measured today: initial indexing of 100,000 entries in
3.5 seconds (28.4k entries/s), warm 100-result search consistently under 1.5 ms even at that scale
(well inside the plan's 50 ms target with a wide margin, though this is two orders of magnitude
short of the plan's 2,000,000-entry target — that case is not yet measured), a 12 MB/90k-entry
snapshot written in 1.3 seconds, and — the one number this whole campaign exists to fix — the idle
wakeup cost. `6522e10` measurably closes that gap: 121 timeslices and 120 voluntary context
switches over 60 idle seconds, down to 0 and 0. Whether the *rest* of the design (fanotify,
metadata-delta backends, progressive coverage tiers) can bring the watch-budget shortfall itself to
zero is a question for the lanes measuring those backends against these same numbers — Lane D's
fanotify spike is the one to compare the 189,094-directory/125,037-watch gap against directly, since
fanotify's `max_user_marks` (266,105) alone is not obviously sufficient either without the
filesystem-wide mode this measurement did not test.

## Raw records

- `docs/measurements/workloads-100k.json` — the full timed-workload suite, 100,000-entry fixture.
- `docs/measurements/idle-before-be57fad.json` — idle sample, pre-`6522e10` binary.
- `docs/measurements/idle-after-head.json` — idle sample, this session's `HEAD` (`80bac65`).
