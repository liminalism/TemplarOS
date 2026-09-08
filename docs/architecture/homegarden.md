# Homegarden (`HomeGarden1`)

Home deals with your stuff; the bar deals with the machine. Homegarden is how Home learns what
your stuff is: directories appear on it as **Places** because you opened them, launched a
terminal or editor into them, opened files under them, or came back to them; they grow when
activity concentrates there, hold their place while used, shrink and finally leave when you
spend later sessions elsewhere, and regrow where they were. The decision is
`@legeos.decision.homegarden-activity-grown-places`; the design conversation is `docs/garden/`.

Every visual variable has one job: **shape = mode, material = identity, colour = state,
size = relevance, position = memory.** Only size and presence are automatic. Position is the
user's.

## The pieces

| Piece | Where | What it is |
|---|---|---|
| The ledger | `crates/lege-session-core/src/homegarden.rs` | Pure. `HomeActivity` in, `HomeContext` records with score, buckets, roles, an activity tree and a stage; root attribution behind `RootProbe`; the ignore list; the Garden Clock. |
| The tender | `crates/lege-field/src/garden.rs` | Pure. `tend(document, contexts, corner)` → `Place` / `Resize` / `Remove`, each with a reason. Never `Move`. Property-tested in `tests/field.rs`. |
| The host | `services/lege-sessiond/src/apis/homegarden.rs` | `org.legeos.HomeGarden1`. Persists the ledger (one JSON row, `home_garden`), applies each tending as one `HomeField1` batch (undoable, in `History1`), subscribes to the session edges and the launch channel. |
| The view | `shell/lege-shell/src/garden.rs`, `surface/field_screen.rs::paint_place` | The shell tends at session start and on every Home reveal, draws a grown Folder tile by rung, and takes every change the daemon makes (`field_model::reconcile_remote`: adopt, resize, remove). It owns no garden state. |
| The verbs | `crates/lege-types/src/action.rs` (`place.*`, `garden.*`) | One declaration → context menu, cheat sheet, `lege garden`. |
| The levers | `shell.home-garden.*` in `crates/lege-session-core/src/settings.rs` | `enabled`, `half-life-hours`, `half-life-sessions`, `max-contexts`, `home-corner`, `ignore`. |

## Time is Garden time

A `GardenMoment` is `(session, active_ms)`: how many user sessions have been entered, and how
many milliseconds of *unlocked* session runtime have accumulated. The clock advances only at the
edges `Lifecycle1` observes — session start, unlock, lock, sleep, logout — through the
in-process `SessionEdge` broadcast. A machine that is off ages nothing. A place recedes because
you kept using the machine without returning to it, never because Tuesday became Friday.
Nothing ticks: the open interval is arithmetic at read time and is never persisted.

## Evidence, not events

Weight is bucketed per `(kind, role, session)` and capped per bucket, so forty saves in a session
are one act of work. Relevance is `score × recency × (1 + breadth)`, where recency halves over
`half-life-hours` of garden time *and* over `half-life-sessions` later sessions, and breadth is
the number of distinct `(kind, role)` pairs seen in the last six sessions. A place used across
several sessions with a terminal, an editor and a file manager outranks any burst.

| Kind | Weight | Source today |
|---|---|---|
| `placed-on-home` | 8 | `HomeField1.PlaceFolder` (also pins) |
| `project-entered` | 5 | successful `Projects1.Open` and `Projects1.SetCurrent` |
| `launched-here` | 3 | the launch channel: `--directory X`, `-d X`, or a bare directory argument; the role is the app's (Terminal, FileManager, CodeEditor) |
| `terminal-entered` | 3 | an interactive LegeOS Bash prompt whose working directory changed; no resident observer or timer |
| `directory-opened` | 2 | successful Home folder-window and Files-object navigation |
| `file-opened` | 1.5 | the shell's brokered `open_file` |
| `created-or-renamed` | 1.5 | reserved for `Files.Operations1` |
| `file-saved` | 0.8 | reserved for first-party saves |

Activity under hidden directories, `target/`, `build/`, `node_modules/`, caches and the
machine's own trees is dropped before attribution (`shell.home-garden.ignore`).

## Attribution

An event on a path accrues to its **context root**: the root of a declared project containing
it; else the nearest existing context above it (a formed context keeps attracting its
descendants); else the highest ancestor with a workspace marker (`.git`, `Cargo.toml`,
`package.json`, `pyproject.toml`, `Makefile`, `justfile`) below a mount point or XDG user
directory; else that mount point or user directory; else the directory itself. `$HOME` is never a
Place; the walk stops beneath it and is capped at eight ancestors. Only `stat`-class reads.

## Stages

| Stage | Tile | Promotion needs | Demotion needs (both) |
|---|---|---|---|
| Dormant | none | — | — |
| Trace | 2×2 | relevance ≥ 3 | 10 later sessions and 30 garden-hours idle, relevance < 1 |
| Tile | 3×2 | ≥ 8, 2 sessions | 6 sessions and 15 hours, < 4 |
| Expanded | 4×3 | ≥ 20, 3 sessions, 2 kinds/roles | 3 sessions and 6 hours, < 10 |

One step per tending moment: a place recedes visibly, never vanishes. Pinned never recedes and
is never removed by recession, but may grow; **kept** is pinned and frozen at its size; hidden
never grows, and its tile leaves Home at once. A folder the user placed or resized by hand is
kept without asking: the tender only prunes what it grew.

| Verb | Record | Tile |
|---|---|---|
| `garden.pin` | `pinned` | never removed by recession; may still grow |
| `garden.keep` | `pinned`, `user_span = current` | frozen in size and place |
| `garden.unpin` | neither | tended freely again |
| `garden.hide` | `hidden`, unpinned | removed from every Home now (undoable); the record stays |
| `garden.forget` | dropped | removed from every Home now (undoable); activity may regrow it |

## The tender's rules

1. Tending happens only at tending moments: session start, Home reveal, `garden.tend`.
2. It emits `Place`, `Resize`, `Remove`. Never `Move`.
3. A `Place` lands at the remembered landing if free, else the nearest free cell to it, else
   the nearest free cell to the home corner — the corner as a 4×3 would sit in it, so a Trace
   has room to become Expanded in place. Nothing is displaced; a full field gets nothing.
4. A `Resize` is accepted only when `reflow::find_resize` satisfies it without any tile the
   tender does not own giving up span (garden-owned, unpinned, not user-sized) **and without any
   neighbour's cell changing**; when the requested span would shift a neighbour, each axis alone
   is tried before nothing is emitted.
5. A tile the user moved keeps its new cell as its landing; a tile the user resized becomes
   user-sized and is never resized again; a tile the tender never placed is the user's outright
   (pinned and kept); a pinned place is never removed by recession.
6. Every op carries a reason (`grew legeos to 4x3: 14 opens, 3 launches, 4 sessions`); the
   batch is one `History1` event and one `HomeField1.Undo` step.

## The Place tile

Same object as any folder tile, drawn by rung: **Small** — name in the place's colour, one
state line; **Wide** — the roles row `>_ Terminal  ▤ Files  <> Edit`, resolved through
`Providers1` at press; **Large** — the branch body: the used structure beneath the root as a
trunk with the branches that earned it, heavier branches thicker, files dimmer; the ordinary
listing until any structure has been earned. **Full** is the folder window.

## What is deliberately not here yet

Growth forms and facets (Hierarchy/Media/Documents/Gallery), blooms (`2 new`, `unwatched`),
material identity, sibling grouping, a Wide-vs-Tall geometry axis, bloom consumption of the
index change log, and portal sources. Each waits on Phase 4: a day's use, judged by
`lege garden why` on every place.
