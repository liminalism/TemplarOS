# Homegarden (`HomeGarden1`)

Homegarden presents current work as well as remembered Places. Every open or minimized window
has a same-instance card on Home's field, placed in free cells around the centre and gathered
by folder; the task switcher's popup remains the fast route and Alt-Tab keeps its existing
ordering. Directories remain **Places** because
you opened them, launched a terminal or editor into them, opened files under them, or came back
to them; they grow with activity and recede only through the existing garden policy. That policy
is `@templaros.decision.homegarden-activity-grown-places`; its design history is `docs/garden/`.

Every visual variable has one job: **shape = mode, material = identity, colour = state,
size = relevance, position = memory.** Only size and presence are automatic. Position is the
user's.

## Current work

The shell merges the compositor's event-driven window snapshot into transient session entries
and draws each as a card on the same grid as the persistent tiles, in cells none of them
occupies, reusing the existing preview painter and window actions. There is no dedicated row:
the arrangement (`shell/templar-shell/src/work_layout.rs`, pure and unit-tested) is

- **Folder-clustered.** A window's folder is its exact brokered terminal working directory,
  otherwise its explicit Place association (an instance the garden recorded under a Place),
  otherwise the folder its application most recently worked in according to `Files.Recents1` —
  a directory entry itself, otherwise the file's parent. Windows sharing a folder share one faint
  plate labelled once; unfiled windows have no plate. Window titles are never read for this.
  CWD and recency affect presentation only: Place context actions stay garden-only.
- **Centred.** Clusters are ordered by their most recently used window. A card goes to the free
  position nearest its cluster's existing cards, its remembered normalized folder centroid, or
  the field's centre for a new cluster, keeping a one-cell gap from other clusters where possible.
  Cards prefer a shared row baseline, without losing otherwise usable free cells.
- **Stable.** A card keeps its cells for as long as its window exists, its folder is unchanged
  and no persistent tile is put on top of it. Activation, recency, parking and restoring move
  nothing: a parked window's card stays exactly where it was, so a window minimizes to its
  own area. The arrangement is recomputed only when the windows, their folders, the persistent
  tiles' cells, the grid or the zoomed project change. Grid/project changes use persisted soft
  folder hints rather than imposing old absolute coordinates on a different grid.
- **Bounded.** Cards prefer 3×2 cells and fall back to 2×1 only when nothing larger fits.
  Windows that fit nowhere are counted on one "+N windows · Show all" card, which takes the
  cells of the least recent card placed in that pass and opens the compositor's window overview
  (`window.show-all`). With no free cells at all there is no overflow card.

Kinds are classified by the application catalogue's categories (`TerminalEmulator`,
`FileManager`, and `Graphics` with `Viewer`, `Photography` or `RasterGraphics` for images) and
the selected provider roles—not by guessing from window titles or app ids.

Every live window uses an application icon, activity stripe while running and dimming while parked,
including its preview. Names appear on hover or keyboard focus, without repeated kind/state words.
Preview wells letterbox rather than stretching client pixels. The bottom bar, both rails and the
shell-drawn Alt-Tab switcher use the same marks; bar groups stack up to three member icons.

An open card activates its exact window; a parked card restores and activates that same instance.
The window menu retains minimize, close and other existing window actions. Closing removes
only the transient session entry: Places, recent files/folders and pinned objects remain under
their existing owners. A terminal without a file association does not manufacture a recent file.
Presentation grouping never creates compositor window groups or changes MRU.

The compositor learns where each card is, so a window visibly minimizes into it. After every
Home frame the shell sends each window's card rectangle — or the "+N windows" card for a window
without one — as `templar_shell_toplevel_v1.set_minimize_target` (protocol version 12) in Home's
surface-local logical pixels, rounded outward. It sends only what changed since the last frame,
clears the target of a window whose card disappeared, forgets closed windows without a request,
and stops asking for the life of the connection when the compositor is older than the request.
Home is redrawn on every window-snapshot change whether or not it is revealed, so a window has a
target before its first park. The compositor uses the target only to animate parking into and
restoring out of the card; it never changes geometry, layout, stacking or input.
The existing `shell.home-field.reduced-motion` preference suppresses these transitions and rail
reveal as well as Home zoom/throw effects. Park/restore follows a smooth geometric size curve;
the client image keeps its own aspect inside the moving rectangle.

The persistent grid always has the whole field, so first-open/last-close transitions do not
move or resize the user's saved objects, and work cards never edit the field document. Work
cards do not participate in persistent-field drag, resize or shelf restoration.
The separate `$XDG_STATE_HOME/templar-shell/work-positions.json` (fallback
`~/.local/state/templar-shell/`) stores only normalized per-folder centroids. It contains no window
ids or field tiles, survives session/output/project changes and cannot reserve occupied cells.

`templar-shell --render-home-work /tmp/home-work` exercises the placement, activation,
restoration, parking and activation stability, the overflow card and closure with a
deterministic window snapshot and writes normal, narrow and HiDPI frames without a display or
session services.
`templar-shell --render-switcher /tmp/switcher` draws frozen compositor-order fixtures, selected
overflow rows, icons and refreshed preview frames at normal, fractional and HiDPI scales.
The compositor retains Alt-Tab ordering, keyboard handling and release/escape commit; the shell's
Presentation layer supplies pixels only, and the text HUD remains available without the shell.
Headless captures do not establish native park/restore feel or reproduce the intermittent black
bottom band. Connected empty shell frames now use a transparent coverage mask rather than the
opaque fallback; an actual mask/present failure still reports its error and uses that fallback.


## The pieces

| Piece | Where | What it is |
|---|---|---|
| The ledger | `crates/templar-session-core/src/homegarden.rs` | Pure. `HomeActivity` in, `HomeContext` records with score, buckets, roles, an activity tree and a stage; root attribution behind `RootProbe`; the ignore list; the Garden Clock. |
| The tender | `crates/templar-field/src/garden.rs` | Pure. `tend(document, contexts, corner)` → `Place` / `Resize` / `Remove`, each with a reason. Never `Move`. Property-tested in `tests/field.rs`. |
| The host | `services/templar-sessiond/src/apis/homegarden.rs` | `org.templaros.HomeGarden1`. Persists the ledger (one JSON row, `home_garden`) from a writer thread — observed acts in batches of 16 or every 5 minutes, everything else at once, a flush on SIGTERM — applies each tending as one `HomeField1` batch (undoable, in `History1`), subscribes to the session edges and the launch channel. |
| The view | `shell/templar-shell/src/garden.rs`, `surface/field_screen.rs::paint_place` | The shell tends at session start and on every Home reveal, draws a grown Folder tile by rung, and takes every change the daemon makes (`field_model::reconcile_remote`: adopt, resize, remove). It owns no garden state. |
| The verbs | `crates/templar-types/src/action.rs` (`place.*`, `garden.*`) | One declaration → context menu, cheat sheet, `templar garden`. |
| The levers | `shell.home-garden.*` in `crates/templar-session-core/src/settings.rs` | `enabled`, `half-life-hours`, `half-life-sessions`, `max-contexts`, `home-corner`, `theme`, `ignore`. |

## Time is Garden time

A `GardenMoment` is `(session, active_ms)`: how many user sessions have been entered, and how
many milliseconds of *used* session runtime have accumulated — unlocked, awake, and with the
screen on. The clock advances only at the edges `Lifecycle1` observes — session start, unlock,
lock, sleep, and idle (the compositor turned the screen off for want of input, reported through
`NotifyIdle`, and back on) — through the in-process `SessionEdge` broadcast. Lock/sleep and idle
are tracked separately, so a lit lock prompt counts nothing. A machine that is off, or on and
untouched, ages nothing. A place recedes because you kept using the machine without returning to
it, never because Tuesday became Friday. Nothing ticks: the open interval is arithmetic at read
time and is never persisted; sessiond folds it in at every write and on SIGTERM.

## Evidence, not events

Weight is bucketed per `(kind, role, session)` and capped per bucket, so forty saves in a session
are one act of work. `score` is evidence *as of the last act*: at every act it is first aged by
`decay` — halved over `half-life-hours` of garden time *and* over `half-life-sessions` later
sessions — and then the act's capped weight is added. Relevance is `score × decay(now − last) ×
(1 + breadth)`, where breadth is the number of distinct `(kind, role)` pairs seen in the last six
sessions. A place used across several sessions with a terminal, an editor and a file manager
outranks any burst, and a place busy a month ago that is touched once today comes back at what
the month left of it, not at its lifetime total.

| Kind | Weight | Source today |
|---|---|---|
| `placed-on-home` | 8 | `HomeField1.PlaceFolder` (also pins) |
| `project-entered` | 5 | successful `Projects1.Open` and `Projects1.SetCurrent` |
| `launched-here` | 3 | the launch channel: `--directory X`, `-d X`, or a bare directory argument; the role is the app's (Terminal, FileManager, CodeEditor) |
| `terminal-entered` | 3 | an interactive TemplarOS Bash prompt whose working directory changed; no resident observer or timer |
| `directory-opened` | 2 | successful Home folder-window and Files-object navigation |
| `file-opened` | 1.5 | the shell's brokered `open_file` |
| `created-or-renamed` | 1.5 | reserved for `Files.Operations1` |
| `file-saved` | 0.8 | reserved for first-party saves |

The Bash prompt hook sends `HomeGarden1.Observe(terminal-entered, cwd, ..., terminal)` on each
directory transition. Sessiond resolves the D-Bus sender's PID through the broker registry's
verified cgroup identity; the hook's CLI child shares the terminal's managed instance. It never
accepts an instance string supplied by an application. A verified transition moves only that
instance out of its previous context and stores its **exact cwd** in the ledger's optional
`working_directories` map (UUID string → absolute path), independent of context attribution.
`HomeGarden1.WorkingDirectories` exposes that map, including HOME/cache/target directories that
grow no Place. Activity weight still accrues only to the attributed project/place root.
`ContextChanged(instance_id, path)` tells the shell to refetch the cwd map and `Contexts`; Home
uses the exact directory for work clustering without altering the Place's root or persistent tiles.
Unverified callers can contribute activity but cannot attach or move a window instance. The
prompt hook caches a directory only after the observation succeeds, so a service-start race is
retried on the next prompt. No polling process or OSC parser is necessary.

Activity under hidden directories, `target/`, `build/`, `node_modules/`, caches and the
machine's own trees is dropped before attribution (`shell.home-garden.ignore`).

## Attribution

An event on a path accrues to its **context root**: the root of a declared project containing
it; else the nearest existing context above it (a formed context keeps attracting its
descendants); else the highest ancestor with a workspace marker (`.git`, `Cargo.toml`,
`package.json`, `pyproject.toml`, `Makefile`, `justfile`) below a mount point or XDG user
directory; else that mount point or user directory; else the directory itself. **The nearest real
repository wins over all of those above it** — a `.git` directory with a `HEAD`, a worktree's
`.git` file, `.hg`, `.jj`, `.svn` — including an existing context above the repository, so a
stray `.git` in a projects folder cannot swallow the repositories beneath it. `$HOME` is never a
Place; the walk stops beneath it and is capped at eight ancestors. Only `stat`-class reads.

## Stages

| Stage | Tile | Promotion needs | Dwell below it | Demotion needs (both) |
|---|---|---|---|---|
| Dormant | none | — | — | — |
| Trace | 2×1 | relevance ≥ 3 | none | 10 later sessions and 30 garden-hours idle, relevance < 1 |
| Tile | 2×2 | ≥ 8, 2 sessions | 1 garden-hour | 6 sessions and 15 hours, < 4 |
| Expanded | 4×3 | ≥ 20, 3 sessions, 2 kinds/roles | 2 garden-hours | 3 sessions and 6 hours, < 10 |
| Dominant | 6×4 | ≥ 45, 5 sessions, 3 kinds/roles | 4 garden-hours | 2 sessions and 4 hours, < 22 |

Sessions are counted among the last twelve. The dwell is how long the place must have held the
stage below (or a later session must have begun) before it may step up: every Home reveal is a
tending moment, and without it a place that qualified could climb a stage per reveal.

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
according to their extension. Tree nodes age like their place and gain at most 12 per session
each; the tree is five deep and 96 nodes, served over the bus aged to now. A form must clear a
floor of 2 and lead the next form by half
again, and it must have led for **two sessions** before the drawing changes — a place that
changed shape every time a photo was opened under it would be noise, not identity.

Extension, not MIME: `Observe(kind, path, app_id, role)` is a shipped signature and there is no
media or reader `ProviderRole` to ask either, so a file's name is the whole of the evidence.
`created-or-renamed` and `file-saved` are still unemitted. Good enough to tell a photo directory
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
6. Every op carries a reason (`grew templaros to 4x3: 14 opens, 3 launches, 4 sessions`); the
   batch is one `History1` event and one `HomeField1.Undo` step.

## The Place tile

Same object as any folder tile. The name is the tile's own chrome; the body is one state line
and then the **form** the place has earned, drawn in the style `shell.home-garden.theme` chooses
(`shell/templar-shell/src/surface/place_styles/`). A theme maps each form — and, optionally, a
plain place large enough to browse — to a structure laid out by `templar_treemap::garden` and a
material: `mineral` (the default: ink branches, a strip of books, a pile of sheets), `orchard`
(grown trees, clustered circles), `cushion` (shaded squarified treemaps), `stone` (dry-stone
Voronoi cells), `radial` (sunburst rings, icicle strata). The used-structure tree is cut to about
one node per 2500 logical px² of body, two to five levels deep, weakest leaves first; nodes keep
their position as weights drift because the layout orders siblings by path, never by weight.
Colours come from the palette's own ground (low chroma, tone by depth and weight); the place's
`material_seed` becomes the grain, never a hue; the accent marks only arrivals (a rim) and the
pointer. The body is rasterized once into a bitmap per tile, keyed by root, tree, size, scale and
theme, and blitted until the key changes; names are drawn live over it and open exactly as a
folder's rows do. A plain place the theme leaves plain falls back to the ordinary folder listing
once it is large enough to browse. **Small** is the state line alone; the form starts at
**Wide**; **Full** is the folder window. `templar-shell --garden-gallery DIR` draws every theme
over a fixed set of places, headless, for review.

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

Sibling grouping, a Wide-vs-Tall geometry axis, and portal sources. There is deliberately
no media consumption signal — no started/progress/completed and no `Resume` bloom: *arrived and
never opened* is the only claim this system can make honestly (`docs/garden/…§5.8`).
