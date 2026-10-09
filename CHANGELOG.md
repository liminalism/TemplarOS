# Changelog

## Unreleased — shared file index continuation, 2026-10-08

- Bound file-index execution with interactive, bulk and maintenance lanes,
  caller-owned cursors, cancellation and a capped reader pool.
- Stream quota-limited sealed snapshots and decode borrowed mmap entries;
  preserve native paths, split freshness and explicit partial summary totals.
- Add volume identity/presence fencing, explicit different-volume acceptance,
  root-local search shards, bounded delta overlays and runtime/storage diagnostics.
- Show discovered locations of strongly identified offline volumes in Settings,
  with an explicit new registration and full rescan action.
- Build a qualified FSearch prototype over Index1, with bounded pages, native
  actions and visible unsupported modes. Defer provider adoption in favor of a
  future integrated search GUI with a large FSearch-style window.
- Add optional shared-index service/auto/scan sources to the owning Silvaview
  repository; qualify native tests, private parity and the actual software nested GUI.
- Guard known-offline Silvaview file-manager actions and copy non-UTF-8 Unix
  paths as lossless file URIs.
- Qualify owned scan/snapshot crashes and fake-checkpoint delta replay fencing.
- Qualify the complete two-million-entry filesystem workload in an owned guest,
  and strengthen native generation, parity, permission and nested-root tests.
- Pass required Index1 all-target Clippy, whole-workspace compilation and
  formatting, and 1,803 selected-package native tests. Preserve the formatting
  patch separately from two display helper extractions.
- At critical memory pressure, make the compositor watchdog exit without waiting
  for a core dump; HDMI/multi-GPU behavior still needs physical qualification.

Prepare and inspect a coherent 23-package native build-set. These are source,
artifact and owned fixture observations. Provider adoption is deferred; the
installed developer day, broader workspace CI and physical HDMI gate remain open.
See [the continuation](docs/indexing-dev/continuation-2026-10-08.md).
