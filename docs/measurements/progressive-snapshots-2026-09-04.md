# Progressive snapshots and `Opaque` aggregates — measurements (2026-09-04)

Lane F of the indexing campaign (`docs/planning/shared-incremental-file-index.md`), after Lane B's
coverage tiers (`031c319`). Two things measured here: the cost of caching a recursive total for an
`Opaque` directory, and the size/time difference between a `Summary` and a `Materialized`
`OpenSnapshot`. Every number below is a live measurement on this machine, produced by
`cargo test -p lege-file-index --release -- --ignored --nocapture progressive_snapshot_benchmark`
(`crates/lege-file-index/src/lib.rs`) unless stated otherwise.

## Fixture

A build-directory-shaped tree: 5,000 ordinary source files under `src/` (`Detailed`, 400 bytes
each) alongside a `target/debug/deps/` subtree holding 20,000 real files on disk (3,000 bytes
each) that classifies as `Opaque` under `RootPolicy::default_home()`. The `Opaque` subtree
contributes exactly **one row** to `entries` regardless of scale — the 20,000 files never become
rows at all, which is the whole point of the tier — so this fixture isolates what `Summary` mode
additionally removes: the 5,000 `Detailed` leaf rows a materialized snapshot still carries.

## Summary vs materialized snapshot

| Detail | Entries in payload | Payload bytes | Format | Build time |
|---|---:|---:|---:|---:|
| `Materialized` (default) | 5,003 | 723,252 | 1 | 5.625 ms |
| `Summary` | 3 | 580 | 2 (`FORMAT_EXTENDED`) | 0.314 ms |

**Ratio: 1,247x smaller, 17.9x faster to build.** The `Summary` payload holds exactly three rows —
`home`, `home/src`, `home/target` — because `Summary` mode selects directory rows only
(`kind = Directory`) and an `Opaque` directory's own row is the one place a raw-walked total lives;
nothing underneath it was ever a row to select. `home/target`'s row carries the cached aggregate:
`recursive_bytes = 20_000 * 3_000 = 60,000,000`, `readable = true`, `stale = false`.

A second, smaller fixture (500 + 500 files, `crates/lege-file-index/src/lib.rs`'s
`a_summary_snapshot_is_dramatically_smaller_than_a_materialized_one` test, which runs under
`just check` on every build rather than only on request) measured **123.7x** at that scale —
consistent with the ratio growing with the size of what `Summary` mode omits rather than being an
artefact of one fixture.

`Materialized`'s own format and byte count are unchanged from before this lane
(`snapshot::FORMAT == 1`, the same 8-field default mask) — verified directly by the pre-existing
`a_snapshot_frame_refuses_to_be_read_after_a_single_flipped_bit`, unmodified, which still asserts
`manifest.format == snapshot::FORMAT`; `a_non_utf8_path_round_trips`, also unmodified, still round
-trips a default request's `entry_paths()` byte for byte. A caller that does not ask for
`Summary`, `AGGREGATE` or `COVERAGE` gets exactly the same frame this format has always produced.

## `Opaque` aggregate correctness

`an_opaque_directory_reports_a_recursive_aggregate_excluding_nested_ignored_names`
(`crates/lege-file-index/src/lib.rs`) is the end-to-end proof, run on every `just check`:

- An `Opaque` directory with a nested `Trash/` holding 9,999 bytes and a non-trashed 1,234 bytes
  reports `recursive_bytes = 1234` — the trashed bytes are excluded at any depth, not just at the
  top of the walk.
- A live watcher mutation directly inside the `Opaque` directory (a new file created under it)
  marks the cached aggregate `stale = true` **without recomputing it** — the cached
  `recursive_bytes` is unchanged immediately after the event. Confirmed by inspecting the row
  before and after `Index::apply`.
- The next ordinary `reconcile_root` pass that actually visits the directory refreshes the
  aggregate (`recursive_bytes` now includes the new file) and clears `stale`. Nothing recomputes
  outside of a real pass — `aggregate::sum_subtree`'s raw filesystem walk is paid for exactly once
  per pass that visits the directory, never per query and never per event.

`aggregate::tests::the_sum_excludes_a_trash_name_nested_inside_the_opaque_subtree` is the isolated
version of the same exclusion rule, and
`aggregate::tests::an_unreadable_subdirectory_marks_the_totals_unreliable_but_still_sums_the_rest`
covers the partial-permission case (`readable = false`, the readable half still counted).

## Subtree rebase

`a_subtree_snapshot_request_rebases_to_just_that_subtree` proves `SnapshotRequest::subtree`
narrows the entries section to exactly the requested subtree (its own row plus everything under
it) while leaving the roots section naming every selected root for context — the mechanism
`crates/lege-client/src/tree_source.rs`'s `load_snapshot_tree` uses for a shell file tray, and the
one a drive-wide visualiser would combine with `Summary` for a subtree's first frame.

## Format rejection

`a_snapshot_reader_rejects_a_format_it_does_not_understand` corrupts just the format field of a
valid frame to `99` and confirms `SnapshotReader::new` refuses it with "unknown snapshot format"
rather than attempting to parse a byte shape it does not recognise. `FORMAT_EXTENDED` (2) is
stamped automatically whenever the effective field mask reaches outside the original eight fields
(`FieldMask::ALL`, 255) — `Summary` mode forces `COVERAGE` and `AGGREGATE` on, so it always
produces `FORMAT_EXTENDED`; an explicit request for those fields in `Materialized` mode would too.
An old build that only knows `FORMAT == 1` refuses a `FORMAT_EXTENDED` frame outright instead of
misreading its extra 36 bytes per entry as some other entry's fields.

## Treemap adapter vs direct scan

`lege-client`'s `tree_source::tests::the_snapshot_adapter_matches_a_direct_scan_for_a_fixture_where_both_are_valid`
builds an `Index` in-process (no `lege-filed`, no D-Bus), produces a `Materialized` subtree
snapshot rebased to a small fixture directory, and confirms the `RawEntry` rows the adapter
produces (path, size, `is_dir`) are identical, once sorted, to what `lege_treemap::scan` reports
for the same directory on disk. `shell/lege-shell/src/surface/app.rs`'s `spawn_treemap` now tries
`lege_client::tree_source::load_snapshot_tree` first (an async D-Bus round trip) and falls back to
`lege_treemap::scan` (`DirectScanSource`, off the event loop via `spawn_blocking`) only when no
service answers or the call fails — the disk walk `lege-shell` paid on every file tray redraw is
now the explicit fallback, not the only path.
