# WP17 — precise owned crash observations

## October 9 post-format repeat

All three precise owned crash journeys pass on the fresh debug binaries after
workspace formatting and the display helper extraction. All 675 captured source
inputs match. The harness again observes a whole 5,002-row scan resnapshot,
snapshot death at row 2,500 without a returned partial FD, surviving sealed bytes
and refused old cursors. The checkpoint provider remains fake. Exact current
binary/source hashes are in the JSON's `post_format_crashes` observation and the
[formatting cohort](index-formatting-2026-10-09-wp17.md). Installed/physical checks
remain open. Earlier cohorts below retain their original binaries.

## Earlier October 9 post-lint repeat

All three precise owned crash journeys pass again against the newly executed
debug cohort: whole scan publication survives, a partial unsealed snapshot does
not escape, and checkpoint/fence recovery refuses replay. The scan resnapshot
returns 5,002 rows; snapshot death stops at entry 2,500. All 623 recorded source
hashes still match. Exact fresh binary/source hashes are in the JSON's
`post_lint_crashes` observation. The checkpoint provider remains fake; no real
Btrfs, installed session or physical HDMI journey is implied. Earlier results
below retain their original binaries.

Native Ubuntu 26.04, 2026-10-08; working tree over `12295f351ab14e649f900573190060cd88a7251e`.
The [JSON report](index-crashes-2026-10-08-wp17.json) retains source/binary hashes,
breakpoint locations, SQLite facts and returned errors. This is an owned-process
qualification, not an installed developer-day result.

`scripts/test-index-crashes.sh` passed all three journeys. It creates a private
bus without activation directories, HOME/XDG stores, 5,000-file tree and exact
child processes. GDB stops the unmodified daemon at source-derived boundaries;
the parent inspects its own fixture, then releases GDB to kill only its inferior.
No production fault switch, system service, live bus, desktop or mount is used.
The test was run inside the native 5/6 GiB build scope. Debugger kill semantics
are documented in the [GNU GDB manual](https://sourceware.org/gdb/download/onlinedocs/gdb.html/Kill-Process.html).

| Journey | Stopped boundary | Observed recovery |
| --- | --- | --- |
| Actual filed scan | `ScanSession::commit_transaction`, immediately before COMMIT, after rows/head/change log were written inside the transaction | Readers and reopened SQLite retain head 6 and the exact old row digest/change sequence; new file absent. Restart publishes a whole frame at generation 11 with 5,002 subtree entries. |
| Actual filed snapshot | Row 2,500 of 5,002; 392,700 payload bytes counted, 262,185 bytes emitted to the memfd | Header still 64 zero bytes; no seals. Kill returns NoReply and no descriptor. Durable head 15 and all rows/changes remain identical. Restart resnapshots at generation 19. |
| Production delta apply in filed test process | `Index::apply_transaction`, immediately before COMMIT | Head 4, old rows/change sequence and checkpoint pointer remain unchanged. Pending fence survives; a new process opens the cache and the production DeltaCatchUp refuses replay before contacting checkpoints. |

The unrelated Home root can commit before the target tree's paused scan; head 6
therefore differs from the original warm head. The assertion compares the target
root's old committed view and the entire visible SQLite view immediately before
and after the kill. It does not assume all roots stopped publishing earlier.
Every check includes `quick_check` and confirms row/root/change generations do
not exceed the published head.

Both daemon restarts refuse old client cursors with Index1 `SnapshotRequired`.
New snapshots have all four seals. The original sealed read-only descriptor
remains byte-identical across both daemon deaths. The partial memfd is inspected
only through the owned paused process and is never returned as a successful frame.

The delta fixture uses the existing fake checkpoint provider and synthetic
Btrfs stream, with temporary roots and the real SQLite mutation/fence code. It
does not run a Btrfs send ioctl or qualify physical mount monitoring. Its
subprocess reopen test is normally ignored and is invoked explicitly by this
owned harness; the fixture marker and path containment checks are mandatory.

Full debugger stacks/scripts, build output, native tests and report are retained
under `.agent/scratch/index-wp17`. The installed real-Home/project day, NTFS
unplug/return, owner logout/login, coherent build-set and physical HDMI acceptance
remain open. The WP8 oracle remains until those closure requirements are met.

## First native gate cohort

Core, filed, client, treemap, shell and Settings passed **1,074 tests**, zero
failures and 11 ignored over 26 target summaries. This excludes CLI and compositor;
the CLI Bash journal failure was unresolved at this first cohort.

`just check-interfaces` passed interface declarations, all 305 frozen signatures,
proxies and packaging, then failed at `check-ownership`: the three-ownership-tiers
decision's provenance needed reconciliation. Assets, actions, Wayland versions and
logging passed when run separately. The complete recipe is therefore not green.
`akr validate` passes with zero errors and 30 unmatched-scope warnings, including
new uncommitted paths and pre-existing retired paths. The warnings remain explicit;
they are not silently converted into current source authority.

The [later closure report](index-closure-2026-10-08-wp17.md) records the fixed CLI
fixture, passing current ownership/interface recipe and private 1,505-test cohort.
The results above retain their original source/binary cohort and limitations.

## Refreshed daemon after relocation and lint changes

The same owned harness was rerun successfully after the volume discovery and
index lint changes. Its [separate JSON](index-crashes-2026-10-08-after-relocation.json)
retains the new binary/source hashes and current breakpoint locations. Scan and
snapshot deaths again preserve the exact committed SQLite rows/change sequence;
old sealed frames survive and old cursors require resnapshot. The fake-checkpoint
delta death again preserves the pointer and pending fence, with replay refused by
a new process. Actual Btrfs mounts and installed/physical qualification remain open.

Logs: `.agent/scratch/index-wp17/current-crash-integration`. Run arguments:
`.agent/scratch/index-wp17/current-crash-run.sh`.
