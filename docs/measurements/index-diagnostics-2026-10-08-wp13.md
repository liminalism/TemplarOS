# WP13 index diagnostics — 2026-10-08

Observed native working tree based on `12295f351ab14e649f900573190060cd88a7251e`.
The [cohort](index-diagnostics-2026-10-08-wp13.json) identifies the three actual debug binaries,
commands and retained logs. No installed service, live desktop bus, volume or GPU was changed.

## Observations

- Core/filed/Settings: 452 passed, zero failed, nine ignored. The store-free core library
  passed 13 tests; client passed 32. Native CLI passed 228 with one existing journal test
  explicitly excluded. The whole CLI run failed that test with an empty journal on Bash 5.3.9;
  this remains a closure issue and is not reported as a passing whole CLI suite.
- The real private filed indexed 200 fixture files and the isolated home root. Native
  `templar doctor --check file-index` and `templar files index status` printed storage,
  all lane fields, each shard/overlay and miss counters, snapshots, cursor ownership/rows/expiry/
  generation, readers, checkpoint result/time, per-root watches, metadata, nesting and volume
  identity. Array warming and a retained cursor were observed, with PASSIVE checkpoints completed.
- A real exported memfd had all four seals and a 30,832-byte frame. While the client still
  held that descriptor, server metrics reported zero open exports. The explicit metric scope is
  `server-held-descriptors`: client descriptors/mappings are not observable after SCM_RIGHTS
  transfer. Unit tests also measured a live server descriptor, duplicate handles, closure and
  success/cancellation/TooLarge counters without keeping telemetry descriptors alive.
- With filed stopped, doctor used the existing SQLite database without activating a service.
  Main, WAL and owner-lock bytes and mtimes were unchanged, and the directory gained no files.
  SQLite's SHM reader-coordination contents/mtime may change during a read-only WAL connection.
- Actual `templar-settings --print --page index` consumed a private typed fixture carrying an
  offline `different-volume` root and the observed per-root policies. It printed metadata,
  watch counts, nesting, strong identity, accelerator bytes/hit rate, cache sizes/availability,
  Retry cache and both explicit volume actions. This is UI qualification, not a real remount.
  Doctor on the same fixture reported unavailable cache and all three threshold warnings.
- Frozen interface signatures passed: 305 members across 44 interfaces. Both private bus
  processes, filed and the explicit fixture server were reaped.

## Maintenance and limits

Filed queues coalesced PASSIVE jobs on its maintenance lane after each scan commit and every
64 nonempty mutation batches. A WAL over 64 MiB permits nonwaiting TRUNCATE only when no tracked
reader is older than 30 seconds. Tests observed no checkpoint from interactive queries and no
TRUNCATE with an older reader; release of that reader allowed truncation. Automatic SQLite
checkpoints are disabled on the writer.

Standalone core users without a maintenance executor join their owned checkpoint thread before
returning from a commit. This preserves deterministic drop/reopen/copy behavior. Filed installs
the bounded executor and keeps asynchronous service admission. No checkpoint or metric timer was
added. Watch refusals are deduplicated by directory and attributed to their root; bounded refusal
tracking reports unknown required coverage and a refused lower bound if it overflows.

The [30-second, 5,000-entry settled idle sample](index-idle-2026-10-08-after-wp13.json)
recorded zero scheduler/context-switch deltas for watcher, mount monitor and PSI, zero read/write
I/O and owned-child cleanup. This does not qualify HDMI recovery or an installed developer day.
