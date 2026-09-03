The founding analysis: `01-initial-shape.md` (assessment and architecture) and `02-subsequent-shape.md`
(Rust scaffold). Registered as AKR **sources** (non-authoritative provenance); the authoritative form is
the ADRs and AKR records. `ADOPTION.md` maps one to the other.

Current focused assessments:

- [`hotfixability-current-state.md`](hotfixability-current-state.md) — how far the installed desktop,
  restartable services, compositor boundary, and boot deployments support developing LegeOS while it
  is in daily use.
- [`window-groups-and-application-integration.md`](window-groups-and-application-integration.md) — the
  fourth hands-on run's findings, the window-group design (explicit, compositor-owned, one tile on Home,
  `Alt+Tab` inside and `Ctrl+Tab` across) and the interim application integration choices: the Home
  field as a launcher, fsearch for search, Kitty as the terminal, and the Files object's context menu.
