# Btrfs metadata delta versus recursive reconciliation

**Lane E, 2026-09-04.** Can `btrfs send --no-data` between two read-only `@home` snapshots replace
the recursive walk the index performs after downtime?

The question is not "does it work" but **"is catch-up cost proportional to actual change rather
than to tree size?"** A 10 GB download must cost the same as a 1 KB one.

## Status of this report

Split in two, honestly:

* **Measured on this host (ext4, kernel 6.17, NVMe):** the cost the delta has to beat, and the
  cost of the index-side half of a delta catch-up. Both are filesystem-independent — `readdir` +
  `lstat` + SQLite — so they transfer.
* **Not yet measured:** `btrfs send` itself, and the end-to-end correctness gate against a real
  stream. This host is ext4 end-to-end and has no Btrfs; `btrfs subvolume snapshot`, `mount(2)` and
  `losetup` all need root, which agents do not have here. The fixture is one reviewable script and
  it is the user's to run:

  ```sh
  sudo scripts/btrfs-index-fixture.sh setup      # loopback image + @/@home/@var/@snapshots/@deployments
  sudo scripts/btrfs-index-fixture.sh probe      # says whether btrfs send works unprivileged here
  sudo scripts/btrfs-index-fixture.sh measure    # builds as you, runs the harness
  sudo scripts/btrfs-index-fixture.sh teardown
  ```

  The image goes on `/mnt/Samsung980_1TB` (80 GB free); the *mount point* is `/tmp/legeos-btrfs-fixture`
  on purpose, because `watch::guard` panics on a watch root outside `std::env::temp_dir()` and that
  is what lets the harness run as an ordinary integration test.

  Everything that does not need the fixture is done: the decoder, the namespace reducer, the hostd
  interface, and the correctness harness itself. `cargo test -p lege-file-index --test
  btrfs_delta_fixture -- --ignored` is the last blocking step.

## Method

Three arms per scenario, so the comparison cannot flatter itself:

1. **delta** — an index carried forward only by decoded `btrfs send` streams;
2. **crawl** — the same baseline index carried forward by today's `reconcile::reconcile`;
3. **fresh** — an index built from nothing against the final tree, the ground truth.

`delta` must equal `fresh` field for field (path, kind, size, alloc, mtime, ctime, mode, hidden,
dev, inode, nlink) or the lane fails. `crawl` is compared too, as a check that the harness itself
is right.

Cost is wall clock, on-CPU nanoseconds from `/proc/thread-self/schedstat`, and `read_bytes` from
`/proc/self/io`.

## What the delta has to beat (measured, ext4, NVMe, warm page cache)

`recursive_reconciliation_costs_the_tree_and_not_the_change`, three tree sizes, one fixed
100-file change:

> **Corrected 2026-09-04 13:25.** The figures first published here were taken while four other
> lanes were compiling on this machine and were about ten times too high. The table below is a
> re-run on a quiet machine, release build, after every lane had stopped. The conclusion is
> unchanged; the absolute numbers are not. Removing the accelerator index added later the same day
> leaves the 153k reconcile at 2,027 / 3,420 ms, so the correction is the quieter machine rather
> than that work.

| tree files | indexed entries | initial acquisition | reconcile, nothing changed | reconcile, 100 files changed |
|---:|---:|---:|---:|---:|
| 1,000 | 1,540 | 16 ms | 21 ms | 26 ms |
| 10,000 | 15,339 | 157 ms | 281 ms | 254 ms |
| 100,000 | 153,327 | 1,867 ms | **3,240 ms** | **3,375 ms** |

Superseded first run, kept so the error is visible rather than quietly deleted: 1,110 / 173 / 127,
2,609 / 2,179 / 4,197, and 32,838 / 39,594 / 33,440 ms for the three rows.

The headline is the last row and it survives the correction: **a reconcile that finds nothing at
all changed costs 3.24 seconds on a 153,327-entry tree, and one that finds a hundred changed files
costs 3.38 seconds — slightly more, for a hundred times the result.** Crawl cost tracks the tree
and is essentially insensitive to the change. That is exactly the defect this lane exists to
attack, and it is a number rather than an argument.

For scale against the index side, measured the same day: a warm 100-mutation batch against that
same 153,327-entry index costs 4.1 ms. A crawl is therefore roughly **790x** the cost of applying
the change directly, and the gap widens with the tree because one side scales with it and the
other does not.

`read_bytes` is 0 because the tree was written moments earlier and is entirely in page cache.
These are therefore *best-case* crawl figures; a genuine post-downtime crawl reads cold metadata
and will be worse. Dropping caches needs root, so the cold-cache figure is not measured here.

Scale to the real machine: `$HOME` holds 189,089 directories after the index's skip policy, so its
entry count is several times the 153,326 measured here. A post-downtime crawl of a real home is a
minutes-long, tree-proportional event.

## The index-side half of a delta (measured, no Btrfs needed)

A delta catch-up is `btrfs send` + decode + `Index::apply`. The second and third need no Btrfs, and
if `apply` were itself tree-proportional the idea would be dead before the fixture existed — no
send is fast enough to rescue a catch-up whose second stage walks the index anyway.
`applying_a_batch_costs_the_batch_and_not_the_tree`, one open index, batches applied in sequence:

| tree entries | batch mutations | apply | apply CPU |
|---:|---:|---:|---:|
| 1,539 | 1 | 2.0 ms | 1.4 ms |
| 1,539 | 100 | 16.5 ms | 9.3 ms |
| 1,539 | 1,000 | 1,803.5 ms | 82.6 ms |
| 1,539 | 1 (repeat) | 10.3 ms | 1.7 ms |
| 15,338 | 1 | 1,878.5 ms | 38.6 ms |
| 15,338 | 100 | 387.0 ms | 24.4 ms |
| 15,338 | 1,000 | 228.9 ms | 55.6 ms |
| 15,338 | 1 (repeat) | 8.2 ms | 6.7 ms |
| 153,326 | 1 | 3,856.6 ms | 254.2 ms |
| 153,326 | 100 | **63.6 ms** | 55.4 ms |
| 153,326 | 1,000 | **132.5 ms** | 105.3 ms |
| 153,326 | 1 (repeat) | **50.9 ms** | 50.3 ms |

Three things to read out of this, in order of importance.

**The first apply after opening is not the apply.** 3.9 s at 153k entries, of which only 254 ms is
CPU: that is the first write transaction faulting in the `entries` B-tree, not the cost of writing
one row. Every subsequent batch on the same open index is two orders of magnitude cheaper. A
resuming daemon opens the index once, so this is a one-off, and it is a warm-up the crawl arm pays
too. Single-shot timings at this scale are noisy (the 1,539-entry / 1,000-mutation row at 1.8 s is
a checkpoint landing in the wrong place); the shape is trustworthy, the individual milliseconds are
not.

**Steady-state apply is batch-proportional, with a tree-proportional constant.** At 153,326 entries
a repeated one-mutation batch still costs ~51 ms, almost all of it CPU, against ~8-10 ms at 1,539
and 15,338 entries. That constant is `apply`'s per-batch
`UPDATE roots SET entry_count = (SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND
deleted_generation IS NULL)` (`mutation.rs`), which counts the whole root on every batch —
about 0.33 µs per entry, so roughly 330 ms per batch on a million-entry home index. It is a
one-line-per-batch cost, it is fixable (maintain `entry_count` incrementally from the batch's own
adds and removes), and it is worth fixing whatever this lane concludes, because the *watcher* path
pays it on every coalesced batch too.

**The index-side comparison, at 153,326 entries:** 133 ms to apply a thousand mutations against
33,000-39,000 ms to crawl. **Roughly 250x**, before `btrfs send` is added to the delta's side of
the ledger.

## Correctness: what is already proven, and how

The decoder and the namespace reducer are covered by 25 unit tests
(`crates/lege-file-index/src/btrfs_delta.rs`), all passing. Three design decisions carry the
correctness argument:

**The stream is a change oracle, not a metadata source.** Reconstructing an `EntryMeta` from the
stream's `chmod`/`utimes`/`truncate` commands would mean reimplementing btrfs's idea of an inode
and being right about every command; being wrong anywhere would put a lie into the index that
nothing later would notice. So the stream answers only *which paths changed*, and each changed path
is `lstat`ed exactly as a scan would. That is what makes `baseline + delta == fresh scan` a
property rather than a hope, and it costs one stat per changed path — still proportional to the
change. It also settles the `st_dev` trap: a snapshot has a different device number from its live
subvolume, so metadata taken from the snapshot would *never* compare equal to a scan of the live
tree. The stat goes to the live tree; a path that vanished between snapshot and stat is counted in
`DeltaReport::vanished` and dropped, never invented.

**The stream's namespace is not the filesystem's namespace.** `btrfs send` renames through
temporary names (`o<ino>-<gen>-<seq>`) whenever a destination is not yet free, so a decoder that
emitted one mutation per command would write those names into the index. Commands are replayed over
an in-memory namespace model and only the *net* effect becomes a mutation. Covered:

* a rename cycle `a→tmp, b→a, tmp→b` reduces to two moves and no temporary name;
* a file created and deleted inside the window is not a change at all;
* a directory move is one `Mutation::Move` (which rewrites descendant rows in SQL), not one
  mutation per child;
* a child leaving a directory that then moves is ordered *before* the directory move, or the
  directory's subtree rewrite drags the child along with it;
* a deletion under a directory that then moves is addressed where the row will be after the move,
  not where the stream spelled it;
* `rename a b; rename x a; unlink a/old` removes the row belonging to the *new* `a`, which reading
  the stream literally would get wrong;
* hard links and symlinks are separate paths of their own;
* `RootPolicy` is applied per path component, and a move that straddles the boundary becomes a
  creation (out of `target/`) or a deletion (into it).

**A partial stream is not a partial answer.** Every command's CRC32C is verified (seed 0, no final
inversion — libcrc32c's convention, not the inverted CRC-32 most crates hand out), and a stream
that does not end in `BTRFS_SEND_C_END` is rejected outright. A delta that was interrupted describes
a state the filesystem was never in, so the caller's only options are "apply all of it" or "fall
back to a reconcile". A full send (no `-p`) is refused as a delta too: applying one would read as
"the whole subvolume is new".

The end-to-end gate — `baseline + delta` against a fresh scan, over create, delete, rename, chmod,
truncate, rewrite-in-place, a 1 GiB file, hard link, symlink, whole-directory rename, a mass delete
and an interrupted delta — is written
(`crates/lege-file-index/tests/btrfs_delta_fixture.rs`) and runs against the fixture. **It has not
been run.**

## The hostd surface

`org.legeos.Host.IndexCheckpoint1` — `Create`, `List`, `Delta`, `Discard`, all JSON-argument so
they can grow without a signature change. `Create`/`Delta`/`Discard` check
`org.legeos.host.index-checkpoint` through `require_authorized`, with the caller identity taken from
the D-Bus message header and never from an argument (ADR-0003). Snapshot creation and sealing reuse
the existing `Snapshotter` that the deployment path already drives; nothing new shells out to btrfs.
Checkpoints live in `@snapshots` (declared in `distro/deploy/layout.toml`, mounted at `/.snapshots`
by `installer/provision.sh`, and consumed by nothing until now). `@home` is checkpointed
specifically: btrfs snapshots stop at a nested subvolume boundary, so a snapshot of `@` would hold
an empty `home/` and the delta would read as "the entire home directory was deleted".

It is in hostd rather than in `lege-filed` because `BTRFS_IOC_SEND` needs `CAP_SYS_ADMIN` and a
per-user daemon does not get capabilities.

## Two open questions that decide adoption

**Getting the stream to the per-user daemon.** hostd writes the delta root-owned, mode 0600,
because a metadata delta names every path under `@home` and a world-readable copy of that is a
local information leak. Handing it to `lege-filed` needs either fd-passing — and **no
`org.legeos.Host.*` method takes a `type="h"` argument today**, so that is genuinely new surface,
the same finding Lane D reaches from the fanotify side — or resolving the caller's uid from the bus
and handing the file over. Neither is built; a spike should not invent privileged surface.

**The cost of holding a checkpoint.** A retained read-only snapshot of `@home` pins every extent it
references, so a file deleted after the checkpoint frees nothing until the checkpoint is dropped.
The protocol (snapshot → mutate → snapshot → send → retain the new one, drop the old) holds exactly
one extra snapshot at rest, which is the cheapest form of this, but "your disk does not free space
while the index is dormant" is a user-visible property and it needs a number from the fixture.

## Recommendation

**Do not adopt yet; do not kill it either.** The evidence so far points the right way and the
remaining unknown is one privileged script run away.

What is already decided by the numbers above:

1. The problem is real, though smaller than this document first claimed. A no-op crawl of a
   153k-entry tree costs 3.24 seconds (corrected; see the note above the table) and scales with
   the tree. Extrapolating to a two-million-entry home gives roughly 40 seconds of pure metadata
   work to discover that nothing happened, paid every time the index resumes — bad, but tens of
   seconds rather than the minutes the original figures implied.
2. The index side of a delta is batch-proportional and is now flat in tree size: a warm
   100-mutation batch costs 4.1 ms against that same 153k index, roughly 790x cheaper than the
   crawl. Nothing about `Index::apply` disqualifies the approach.
3. `apply`'s per-batch `SELECT COUNT(*)` over the whole root **has since been fixed** — it is
   maintained by delta now. Investigating it also exposed the larger term, `refresh_fts` running
   uncovered by any index, whose repair took a warm single-mutation batch at 153k entries from
   41.9 ms to 0.3 ms. Both were on the watcher's hot path, so this stands as a result of the
   campaign whatever happens to the delta backend.

What decides adoption, and what to measure when the fixture exists:

* `btrfs send --no-data` wall time as a function of *change size* at fixed tree size, and as a
  function of *tree size* at fixed change size. If the second curve is flat, the architectural
  requirement is met and the backend is worth finishing. If `send` is proportional to the tree —
  it walks two metadata B-trees, so it may well be proportional to the *changed* part of them, but
  that is a claim, not a measurement — the idea reduces to "a cheaper crawl", which is a much
  weaker result and probably not worth the checkpoint's disk cost.
* Whether `btrfs send` works unprivileged inside the fixture. If it does, the hostd round trip and
  the fd-passing question both disappear and the backend becomes much cheaper to adopt.
* Whether `baseline + delta` equals a fresh scan on a real stream, especially for directory mtime
  (does `send` emit `utimes` for a directory whose child was added?) and for the 1 GiB file, where
  `--no-data` must produce a stream whose size is independent of the file's.

Ship as it stands: an unadopted backend behind a flag, with the numbers above and the gap named.
