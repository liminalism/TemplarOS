# Btrfs metadata delta versus recursive reconciliation

**Lane E, 2026-09-04.** Can `btrfs send --no-data` between two read-only `@home` snapshots replace
the recursive walk the index performs after downtime?

The question is not "does it work" but **"is catch-up cost proportional to actual change rather
than to tree size?"** A 10 GB download must cost the same as a 1 KB one.

## Status of this report

**Complete.** Every number in it is measured. The user built the fixture with
`sudo scripts/btrfs-index-fixture.sh setup` and ran `measure` five times: four runs failed the
correctness gate, the fifth passed it clean across all 24 scenario/tree-size rows and the
edge-case round trip.

```sh
sudo scripts/btrfs-index-fixture.sh setup      # loopback image + @/@home/@var/@snapshots/@deployments
sudo scripts/btrfs-index-fixture.sh probe      # says whether btrfs send works unprivileged here
sudo scripts/btrfs-index-fixture.sh measure    # builds as you, runs the harness as root
sudo scripts/btrfs-index-fixture.sh teardown
```

The image goes on `/mnt/Samsung980_1TB`; the *mount point* is `/tmp/legeos-btrfs-fixture` on
purpose, because `watch::guard` panics on a watch root outside `std::env::temp_dir()` and that is
what lets the harness run as an ordinary integration test. Raw harness output, rewritten by every
run, is `docs/measurements/btrfs-delta-measured.md`.

**`btrfs send` requires `CAP_SYS_ADMIN` on this kernel (6.17).** Not inferred — attempted directly
against the fixture, as its owner, on subvolumes that user created:

```
$ btrfs send --no-data -p @snapshots/diag-a -f /dev/null @snapshots/diag-b
ERROR: can't perform the search: Operation not permitted
ERROR: send ioctl failed with -1: Operation not permitted
```

So the hostd round trip is not avoidable, and the stream-transport question at the end of this
report is a real one rather than a formality.

### What the gate found

Four failures, each a real bug. Three were in code the delta merely *exercised* rather than in the
delta itself, and two of those were live on the watcher's own path, where they had gone unnoticed
because the watcher's tests compared namespaces and not metadata.

| run | symptom | cause | also affected the watcher? |
|---|---|---|---|
| 1 | 933 rows differ; `vanished: 8` | stream paths are the *subvolume's*, joined onto the index root | no — delta only |
| 2 | stale metadata on a hard link; `UNIQUE constraint failed` | a write through one name left the other names stale; a move onto an occupied path collided | **yes, both** |
| 3 | a nested rename lost a whole subtree (34 rows) | deepest-first move ordering readdressed the parent out from under the child | no |
| 4 | one row: a directory's mtime (429 ms) | a redundant move was dropped, and its metadata refresh went with it | **yes** |


### First run: the gate failed, and it was right to

The fixture was built and `measure` was run as root. The correctness gate **failed**, and the
failure was a real bug rather than a harness artifact: every path in a send stream is spelled from
the root of the *snapshotted subvolume*, and the decoder was joining those paths onto the *index
root* instead. The fixture snapshots `@home` and indexes `@home/bench`, so `bench/inbox/new000.txt`
became `@home/bench/bench/inbox/new000.txt` — a path that exists nowhere. Every such path was
counted as `vanished` and dropped, so the delta applied almost nothing while reporting success.
`100 new files` differed from a fresh scan in 102 rows; the edge-case round trip differed in 933,
with `DeltaReport { planned_paths: 9, mutations: 1, removes: 1, vanished: 8, .. }`. Only the
`nothing at all` scenario passed, because an empty delta is the correct answer there.

This is a production bug, not a fixture shape: `@home` mounts at `/home`, so the index root
`/home/<user>` is always a subdirectory of the subvolume the stream speaks in, and a delta of
`@home` also names other users' paths that must never be applied to this root. `DeltaTarget` now
carries `subvolume_path` alongside `root_path`, resolves every stream path against the former, and
applies only what lands inside the latter — a component-bounded containment test, so `/home/dkx` is
not mistaken for something under `/home/dk`. Paths outside the root are counted in
`DeltaReport::outside_root` rather than folded into `vanished`: conflating "not ours" with "ours
and gone" is what let this look like it worked. Unit tests now cover the nested shape, the sibling
home, the `dkx`/`dk` boundary and moves across the root boundary, and the harness passes both roots.
That was run 1 of 5; three more failures followed, each a different bug.

### Runs two through four, and what they were worth

**Run 2 — a hard link and a collision.** A write through one name of a hard link left the index's
other names holding stale size and mtime, and a move onto an already-occupied path hit
`UNIQUE constraint failed: entries.root_id, entries.path_bytes`. Both are general index bugs: the
watcher reaches them through `IN_MODIFY` on a linked file and through `rename(2)` over an existing
destination, which is what every editor's atomic save does. `apply` now mirrors a write to an
inode's other names (gated on `nlink > 1`, so the common case costs one probe) and a move onto an
occupied path takes that row over.

**Run 3 — a nested rename.** `mv a/b c/d` inside a directory that itself moved lost the whole
subtree, 34 rows. The plan sorted moves deepest-first, which is right for a filesystem and wrong
for an index where a directory move rewrites its descendants in one SQL statement: the deep move
addressed a path its own ancestor was about to invalidate. Moves are now ordered shallowest-first
with each one readdressed under the ancestors already applied. The watcher does not reach this —
inotify delivers the two renames separately and each is applied against a settled tree.

**Run 4 — one directory's mtime.** `Replay::finish` classified each replayed node in a chain: a
node whose baseline path differs from its final path became a move, and a move that
`implied_by_ancestor` recognised as already performed by a directory move above it was dropped.
Dropping the *move* is right. Dropping the *metadata refresh* is not — a node is in the replay map
only because the stream named it, and `btrfs send` names a directory whose mtime moved. The
directory holding a rewritten file rode a rename above it and lost both.

**That one was general too, and it is user-visible.** Upgrading `tests/watch.rs` to the same
metadata comparison failed immediately, with exactly one differing row in each nested-rename test:
the directory the rename happened *in*. inotify names the child; nothing tells the index the
parent's own mtime moved. A folder in a file manager would show a modification time from before its
contents changed. `WatchOwner::pump` now re-reads a directory that gained or lost an entry — one
extra `lstat` per pump, so unpacking 10,000 files into a directory costs one stat, not 10,000.

Two further defects fell out of that upgrade, neither of which any namespace comparison could have
caught: `retarget_subtree` produced a trailing-separator path (`Path::join("")`), equal to its
sibling for every `Path` comparison but a different byte string the moment it becomes an index key;
and six hand-written test streams described sequences no kernel emits, passing by agreeing with
each other rather than with the filesystem. One unit test asserted the run-4 bug outright. There is
now a single metadata-comparison definition shared by the harness, the delta unit tests and the
watcher tests, so the asymmetry that hid all of this cannot reopen.

### Final run: clean

All five harness tests pass. The edge-case round trip — hard links, symlinks, a whole-directory
rename, a rename cycle, a move onto an occupied path — reports
`planned_paths: 10, mutations: 11, creates: 3, removes: 1, moves: 5, touches: 2`, which is what the
test encoder predicted before the fixture confirmed it. The interrupted-delta test refuses all 64
truncated prefixes of a 7,360-byte stream. Every scenario at every tree size agrees with a fresh
scan, field for field.


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
(`crates/lege-file-index/tests/btrfs_delta_fixture.rs`) and runs against the fixture. **It passes**
— on the fifth run, after four failures that are worth reading before trusting it (above).

## What `btrfs send` actually costs (measured on the fixture)

This is the number the lane existed to get, and it is the one that decides the architecture.

**Send is flat in tree size.** The same change, sent between snapshots of three trees two orders of
magnitude apart, costs the same and produces a byte-identical stream:

| change | 1,540-entry tree | 15,339-entry | 153,327-entry | stream |
|---|---:|---:|---:|---:|
| nothing at all | 1.6 ms | 1.7 ms | 1.9 ms | 113 B |
| a whole-directory rename (4 paths) | 2.4 ms | 2.4 ms | 2.8 ms | 481 B |
| an hour of desktop use (67 paths) | 4.1 ms | 5.5 ms | 5.9 ms | 13.6 KiB |
| 100 new files (102 paths) | 3.7 ms | 5.8 ms | 7.0 ms | 34.0 KiB |
| a build burst (7,007 paths) | 128.7 ms | 148.7 ms | 149.3 ms | 2.7 MiB |
| one 1 GiB file (2 paths) | 20.7 ms | 29.2 ms | 20.3 ms | 408.4 KiB |

That answers the open question this report was left on. `btrfs send --no-data` walks the *changed*
part of two metadata B-trees, not the whole of them: the residual drift across a hundredfold tree
is under 4 ms, and the stream does not grow by a byte. The delta is therefore proportional to the
change and not to the filesystem, which is the property no crawler can have.

**Send is proportional to the change.** At the 153,327-entry tree, against changed-path count:

| planned paths | 0 | 4 | 67 | 102 | 7,007 | 50,003 |
|---|---:|---:|---:|---:|---:|---:|
| send ms | 1.9 | 2.8 | 5.9 | 7.0 | 149.3 | 1,185.4 |

Roughly 24 µs per changed path at the top end, with a ~2 ms floor.

**`--no-data` does what it says for a large file**, and it is worth being precise about what it
scales with. A newly written 1 GiB file produces a 408.4 KiB stream in 8,200 commands — about one
`UPDATE_EXTENT` per 128 KiB extent, 0.04% of the data. So the stream is independent of file *size*
but proportional to *extent count*: a dormant window in which 100 GiB is written costs on the order
of 40 MiB of stream. Bounded and cheap against the alternative, but not free, and worth a cap
before this is enabled anywhere.

**End to end**, delta against today's reconcile, at the 153k tree:

| scenario | delta | crawl | speedup |
|---|---:|---:|---:|
| an hour of desktop use | 30.4 ms | 19,657.6 ms | **647x** |
| 100 new files | 37.8 ms | 19,198.3 ms | 508x |
| a whole-directory rename | 49.4 ms | 19,577.1 ms | 396x |
| truncate, chmod, rewrite in place | 46.8 ms | 16,865.9 ms | 361x |
| one 1 GiB file | 39.1 ms | 11,329.3 ms | 290x |
| nothing at all | 71.1 ms | 17,588.2 ms | 248x |
| a build-shaped burst (5,006 mutations) | 1,269.1 ms | 18,636.6 ms | 15x |
| a mass delete (50,003 rows) | 6,095.2 ms | 14,202.7 ms | 2.3x |

**Where the delta loses, and why it does not matter.** The build burst is *slower* than a crawl on
the 1,540-entry tree (0.4x) and barely ahead on the 15,339-entry one (1.3x); the crossover is a few
thousand entries. A real home is 100k+, and the burst's 2,001 `excluded` paths say the coverage
policy already discards most of a build directory before it reaches the index. The mass delete's
modest 2.3x is not the delta losing either: 4,910 of its 6,095 ms are `apply` writing 50,003
tombstones, which the crawl arm also pays. Both arms converge when the change *is* the tree.

**Read I/O, with its caveat stated.** The delta arm reads 0 bytes from the block layer in all 24
rows; the crawl arm reads 116-159 MiB at the 153k tree even with a warm page cache. The caveat is
that `read_bytes` comes from `/proc/self/io` of the harness process, and `btrfs send` is a child
process, so its own reads are not in that column. Its wall time bounds them: 5.9 ms on NVMe cannot
hide more than a few MiB. The direction of the result is not in doubt; the delta arm's zero is the
index side only.


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

## Getting the stream to the per-user daemon

hostd writes the delta root-owned, mode 0600, because a metadata delta names every path under
`@home` and a world-readable copy of that is a local information leak. `Delta` therefore returns a
*path*, and the per-user `lege-filed` cannot open it.

**A correction to what this report previously said, and to what I told the user.** I described this
as blocked by ADR-0003. It is not: ADR-0003 (`docs/adr/0003-hostd-scope.md`, 20 lines) settles what
hostd *owns* — updates, deployments, drivers, storage reserve, hardware profile — and says nothing
about file descriptors in either direction. There is no prohibition to amend, and the permission
the user granted to amend one is not needed. The real facts are narrower:

* no `org.legeos.Host.*` method takes or returns `type="h"` today, so this is new surface on the
  **system** bus and deserves a deliberate decision rather than an incremental one;
* the project already passes descriptors in exactly this shape — `Index1.OpenSnapshot`
  (`interfaces/dbus/org.legeos.Files.Index1.xml:50`) returns `type="h"` for a sealed snapshot on
  the session bus. The pattern is established; only the bus is new.

Three transports, in the order I would take them:

1. **Return the descriptor.** hostd opens the stream, unlinks it, and returns `type="h"` — the
   `OpenSnapshot` pattern, one file that exists only as an open descriptor and cannot be raced,
   leaked into a directory listing, or left behind on a crash. It needs one additive `DeltaOpen`
   member alongside `Delta` (a changed signature is frozen; a new member is not).
2. **Chown to the authenticated caller.** Cheapest, and wrong: it puts a readable inventory of
   `@home` in the filesystem for as long as the consumer takes to read it, and the failure mode of
   a consumer that dies mid-read is a file left on disk.
3. **Stream over the method call.** Simple and bounded for the 113-byte idle case, badly behaved
   for the 2.7 MiB burst and the 40 MiB write-heavy window.

Take (1). It is not an ADR change; it is a new member and one line in the AppArmor profile.

## The cost of holding a checkpoint — the one number still missing

A retained read-only snapshot of `@home` pins every extent it references, so a file deleted after
the checkpoint frees nothing until the checkpoint is dropped. The protocol holds exactly one extra
snapshot at rest (snapshot → mutate → snapshot → send → retain the new, drop the old), which is the
cheapest form of this. But "your disk does not free space while the index is dormant" is a
user-visible property, the fixture can measure it, and it is not measured. It is the last thing to
find out before this is enabled by default anywhere.

## Recommendation

**Adopt as an opt-in backend for Btrfs `@home`; do not make it the default until it has run on a
real LegeOS install and the checkpoint's disk cost has a number.**

What the measurements settle:

1. **The problem is real.** A no-op crawl of a 153,327-entry tree costs 3.24 s and scales with the
   tree; a real `$HOME` is several times that. Paid on every resume, to discover nothing happened.
2. **The delta has the property the crawler cannot.** `btrfs send --no-data` is flat in tree size
   (1.6 → 1.9 ms across a hundredfold range for the same change) and proportional to the change
   (~24 µs per changed path). Everything downstream of it — decode, `lstat` per changed path,
   `apply` — is already change-proportional.
3. **It is correct.** `baseline + delta == fresh direct scan`, field for field, across 24
   scenario/tree-size rows plus an edge-case round trip covering hard links, symlinks, rename
   cycles, moves onto occupied paths and whole-directory renames; a truncated stream is refused at
   all 64 prefixes rather than half-applied.
4. **The campaign paid for itself independently of the adoption decision.** Four gate failures
   found four bugs, three of them in shared code and two of them live on the watcher's path: stale
   metadata on a hard link's other names, a move onto an occupied path aborting a batch, and a
   directory whose mtime never refreshed when its contents changed — the last of which every file
   manager would have shown to a user. Those fixes ship whatever happens to this backend.

What holds the default back:

* Everything here is a **loopback image on an ext4 host**, exercised at up to 153k entries. It has
  never run on a real Btrfs root, a real `@home`, or a machine that was rebooted between snapshots.
* The **checkpoint's disk cost is unmeasured** (above).
* The **stream transport is undecided** (above) — one additive member, but a deliberate one.
* `send` is proportional to extent count, so a write-heavy dormant window is tens of MiB of stream.
  There should be a cap above which the backend gives up and asks for a reconcile, and there is
  none.

Concretely: keep `org.legeos.Host.IndexCheckpoint1` unadopted and documented as such, keep the
decoder and its 25 unit tests in `lege-file-index`, keep the harness runnable from one script, and
revisit when LegeOS installs on Btrfs on the reference machine — at which point the remaining
questions are a day's work, not a lane's.
