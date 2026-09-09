The founding analysis: `01-initial-shape.md` (assessment and architecture) and `02-subsequent-shape.md`
(Rust scaffold). Registered as AKR **sources** (non-authoritative provenance); the authoritative form is
the ADRs and AKR records. `ADOPTION.md` maps one to the other.

Current focused assessments:

- [`hotfixability-current-state.md`](hotfixability-current-state.md) — how far the installed desktop,
  restartable services, compositor boundary, and boot deployments support developing TemplarOS while it
  is in daily use.
- [`window-groups-and-application-integration.md`](window-groups-and-application-integration.md) — the
  fourth hands-on run's findings, the window-group design (explicit, compositor-owned, one tile on Home,
  `Alt+Tab` inside and `Ctrl+Tab` across) and the interim application integration choices: the Home
  field as a launcher, fsearch for search, Kitty as the terminal, and the Files object's context menu.
- [`shared-incremental-file-index.md`](shared-incremental-file-index.md) — replace the interim two-index
  search arrangement with one persistent, incrementally maintained `templar-filed` index consumed by
  TemplarOS search, a remote-index FSearch fork, Silvaview-rs, the CLI, and future clients.
- [`ubuntu-derived-full-distribution-roadmap.md`](ubuntu-derived-full-distribution-roadmap.md) — the
  Ubuntu components TemplarOS should reuse, the distribution infrastructure it must own, a latest-qualified
  Linux 7.2/7.3 kernel-channel plan, phased delivery gates, and a remaining-work estimate for a
  supportable amd64 OS.
