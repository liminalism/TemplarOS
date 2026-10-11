# Changelog

## Unreleased — unified launcher and window transitions, 2026-10-11

- Add one focused PixelKit launcher above applications. Super+Space, the bottom
  rail and Homegarden open the same popup without revealing Home or rearranging
  windows. Tab switches Apps/Files while retaining the query; click or Enter
  activates immediately, Ctrl+Enter requests another instance, and Escape or
  clicking outside dismisses.
- Move app search, favorites and recent applications out of Home's permanent
  search strip. Ordinary activation creates no Home tile; shortcuts require an
  explicit context action. Existing Home project, window-card and tile gestures
  retain their roles.
- Search Files.Index1 directly with bounded pages, cancellation and generation
  fencing. Preserve native paths and open files through Associations1 and
  Launcher1; Search Here opens the same popup with an explicit directory scope.
- Persist application favorites, named terminal directory/argument profiles,
  display aliases and reversible hidden results through Settings1. Saved apps
  share one rail chip with their running task; distinct terminal profiles remain
  separate. Explicit Home profile shortcuts retain their copied launch target.
- Organize Settings into ten navigation groups. Add favorite/profile editing and
  hidden-result restoration; move algorithm and rail tuning into Advanced, and
  remove obsolete search-provider controls and unenforced ordinary choices.
- Render Essence park/restore transitions in the compositor's GLES scene, with
  reusable reversible plans and bounded GPU snapshots. Add Essence/Simple/Off
  and particle size (default 6 logical px, range 1–24); retain simple motion on
  shader/capability failure and the existing reduced-motion override. Material
  and bar shader experiments remain in the sandbox.
- Retire FSearch provider routing, fallback rows and distro fork packaging in
  favor of the first-party launcher. Keep Files.Index1 and its compatibility
  clients. Earlier FSearch plans and measurements remain historical evidence.

See [launcher behavior and configuration](docs/launcher.md). Headless interaction,
private-session and GPU fixtures qualify specific source paths; they do not
replace installed-session feel or integrated-GPU performance observation.

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
