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
| Trace | 2×1 | relevance ≥ 3 | 10 later sessions and 30 garden-hours idle, relevance < 1 |
| Tile | 2×2 | ≥ 8, 2 sessions | 6 sessions and 15 hours, < 4 |
| Expanded | 4×3 | ≥ 20, 3 sessions, 2 kinds/roles | 3 sessions and 6 hours, < 10 |
| Dominant | 6×4 | ≥ 45, 5 sessions, 3 kinds/roles | 2 sessions and 4 hours, < 22 |

The range is deliberately aggressive: a Trace is a *line* (a name and a state line, which is all
the Small rung draws), and a Dominant is twelve times its area with its form drawn at size. The
Folder class floor is 2×1 so a line is a shape a folder may have; nothing else about the class
moved, and `GROWTH_ROOM` follows the top stage so a first landing still leaves room to reach it.

One step per tending moment: a place recedes visibly, never vanishes. Pinned never recedes and
is never removed by recession, but may grow; **kept** is pinned and frozen at its size; hidden
never grows, and its tile leaves Home at once. A folder the user placed or resized by hand is
kept without asking: the tender only prunes what it grew.

## Forms

Shape is the *mode of use*, and the ledger settles it. Each context carries a `form` — `plain`,
`branch`, `shelf` or `folio` — derived at a tending moment from the activity tree it has already
earned: directories worked *through* are branch evidence (weighted by depth, and half again when
a Terminal or CodeEditor role has been seen there), file leaves are shelf or folio evidence
according to their extension. A form must clear a floor of 2 and lead the next form by half
again, and it must have led for **two sessions** before the drawing changes — a place that
changed shape every time a photo was opened under it would be noise, not identity.

Extension, not MIME: `Observe(kind, path, app_id, role)` is a shipped signature and there is no
media or reader `ProviderRole` to ask either, so a file's name is the whole of the evidence.
`created-or-renamed` and `file-saved` are still unemitted, and the tree is three deep, so a
library organised more deeply than that under-reports. Good enough to tell a photo directory
from a manuscript one; anything finer waits for a signal that exists.

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

Same object as any folder tile. The name is the tile's own chrome; the body is one state line
and then the **form** the place has earned (`shell/lege-shell/src/surface/place_forms.rs`) —
a branch (trunk at the left, the used descendants fanning off it, each band and stroke by share,
twigs where the band has room), a shelf (the things it is looked at for, standing on a rule at
heights by share), or nothing at all for a plain place, which falls back to the ordinary folder
listing once it is large enough to browse. **Small** is the state line alone; the form starts at
**Wide**; **Full** is the folder window.

There is no roles row: `place.open-terminal`, `place.open-files` and `place.open-editor` are
rows of the Place context menu, which reaches the same providers by the same route. Drawn into
the body they made every place look like a collage of the applications used to manipulate it.

## Blooms

An earned Place says what arrived in it. At each tending moment the daemon reads one bounded
page of the shared index's change log (`Files.Index1.ChangesSince`) from a cursor kept in the
ledger — **never a subscription**, because a bloom is only ever drawn and drawing only happens
at a tending moment. Each arriving path is *looked up* against the contexts that already exist
(the deepest one above it at `Tile` or better, not hidden) and nothing is ever inserted: content
may alter what an earned Place shows, and may never create, resize, move or promote one.

`Bloom { kind, since, items, total }` sits on the context and shows as `2 new ·` in front of the
state line. It ends where it started, on the brokered open the ledger already hears: `file-opened`
on a bloomed item drops it, the last one clears the bloom, and a bloom nobody answers for five
later sessions is dropped as stale. Below the log's retained floor there is no continuity to
prove, so the cursor moves to the head and that moment shows nothing.

The same read carries a rename: a move whose source is a context root brings the place to its
new name with its score, buckets and material seed intact.

## What is deliberately not here yet

The folio drawing (the form is derived but nothing draws it yet), material identity drawn from
the seed, sibling grouping, a Wide-vs-Tall geometry axis, and portal sources. There is deliberately
no media consumption signal — no started/progress/completed and no `Resume` bloom: *arrived and
never opened* is the only claim this system can make honestly (`docs/garden/…§5.8`).
