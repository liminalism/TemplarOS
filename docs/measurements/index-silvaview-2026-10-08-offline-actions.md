# Silvaview offline actions — 2026-10-08

The owning sibling `../Silvaview-rs` commit
`dbdeb29599e6026ae421c7ea6db29265eb50733c` follows the historical WP15
qualification. Exact sources, executables and results are in the
[JSON](index-silvaview-2026-10-08-offline-actions.json).

Accepted service snapshots now retain native offline-root boundaries alongside
their cached tree and freshness. File-manager targets are refused under those
roots, or while the source is loading/failed. Ready available targets retain
their Unix path bytes. Component-aware prefix checks avoid treating `/fix` as
the parent of `/fixture`.

Copying an absolute non-UTF-8 Unix path produces a percent-encoded file URI;
ordinary UTF-8 paths remain plain text. The copied indicator uses the same
native representation. An owned GIO conversion round-tripped `FF`, space,
percent and hash bytes exactly. Relative non-UTF-8 paths are refused rather
than copied lossily.

## Native observations

- Feature-on `cargo test --features templaros --all-targets`: **48 passed**,
  zero failures/ignored (22 library, 26 GUI binary, zero probe tests).
- Feature-off `cargo test --no-default-features --all-targets`: **32 passed**,
  zero failures/ignored (14 library, 18 GUI binary).
- The feature-on GUI and probe build succeeded. These commands used the native
  target directory and capped build scope.
- The refreshed production worker/private filed run retained direct/service
  parity at **41,251 bytes**, preserved the non-UTF-8 path, and refreshed from
  generation 4 to 5 at **41,288 bytes** after a 37-byte create.
- The service worker had **zero watch descriptors and zero directory
  enumeration calls**. Its directory summary remained explicitly partial at
  1,200 bytes. Absent/uncovered fallback and compatible-service failure behavior
  passed again. Owned processes were reaped.

The sibling's existing Cargo profile changes and untracked screenshot were
preserved; only the three source files in this fix were committed.

## Limits

The action guard uses the accepted snapshot's known offline boundaries. It does
not recheck volume identity before every click; a change before notification
and refresh can still race an action. The fixtures exercise pure action and
clipboard helpers, without launching a live file manager or changing the live
clipboard.

The current private worker was rerun. Actual nested GUI evidence remains the
[historical WP15 cohort](index-silvaview-2026-10-08-wp15.md) at `413ea04`,
not evidence for this newly built executable. No installation, seed adoption,
real NTFS unplug/return or physical HDMI qualification is claimed.
