# Homegarden (`HomeGarden1`)

Homegarden presents remembered projects and their current work. A project container owns its
live/parked window slots, sessions, recent files and application actions. Uncontained windows
share one **Loose** tray. The independent Desktop dock presents the real Desktop directory,
not machine statistics. Alt-Tab retains the compositor's existing ordering.

Projects grow from genuine activity and recede through Garden time. Their positions remain
stable; Keep protects retention, while an explicit resize owns geometry. Current authority is
`legeos.decision.homegarden-activity-grown-places/3`; the September documents in `docs/garden/`
are historical rationale, not permission to defer the October 7 owner PDF.

## Current work

The Homegarden and HDMI owner plan is tracked in AKR. The current secondary-output
qualification boundary and safe next steps are recorded in
[`hdmi-validation-handoff.md`](hdmi-validation-handoff.md).

The shell merges the compositor's event-driven window snapshot into transient session entries.
`work_layout.rs` allocates contained project slots and one bounded Loose area rather than
free-standing folder clusters. Verified instance/project context determines containment;
window titles do not establish identity or project authority.

- **Contained.** A visible project's Now strip holds its available window slots. Windows that
  cannot fit there remain reachable through Loose/overflow rather than disappearing.
- **Stable.** Activation, parking and restoring do not rearrange the persistent project field.
  A window minimizes toward its own current contained slot, or the explicit overflow target.
- **Single release.** A click activates the exact open window or restores the exact parked
  window. It does not wait for a double-click classification. Crossing the lift threshold
  starts a drag instead; Escape cancels it.
- **Explicit linking.** Dropping a window onto a project requests a broker-validated instance
  association. Presentation does not grant application rights or create compositor groups.
- **Bounded.** Overflow opens the compositor's all-window overview. Loose reserves its title
  before packing cards, including at narrow widths.

Window cards retain application icons, title, Park and letterboxed previews. Text-only Now
slots are one title row; a status label is drawn only when another full line fits. Disabling
previews leaves actionable text, not an empty project. Closing a window removes its transient
entry, not the project's history, files or retained sessions.

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

Window cards do not become persistent field objects. The field remains the owner of project
geometry, while the garden owns verified links and retained session data. Neither transient
window counts nor Desktop directory changes manufacture project growth.

`templar-shell --render-home-work PREFIX` exercises containment, Loose/overflow, activation,
parking, capture gating and project templates/tabs with deterministic fixture data. Its 27
normal/narrow/HiDPI and project-state images are renderer evidence, not genuine process/session
or hardware qualification.
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

Interactive shell hooks send `HomeGarden1.Observe(terminal-entered, cwd, ..., terminal)` on
directory transitions. The ordinary path resolves the D-Bus sender through the broker's current
registered cgroup identity; applications never supply an authoritative instance identifier.
A verified transition updates that instance's exact cwd in `working_directories`, independently
of project-root attribution. `WorkingDirectories` includes HOME/cache/target directories that
grow no Place; `ContextChanged(instance_id, path)` tells Home to refresh that display context.

Some real terminals, including Kitty, move their shell into a separate systemd scope.
HomeGarden alone can associate these descendants for display: it pins same-UID ancestry with
pidfds, rechecks PID/start time/parentage, stops at other managed applications, and requires the
D-Bus sender's `ProcessFD` for descendant observations. Missing credentials fail closed.
Traversal and evidence reads are bounded and occur at tending/observation, not through a
resident process poller. This path updates existing containment/cwd/session presentation;
it does not add strong activity, grow a Place, override explicit adoption, or extend canonical
AppIdentity, provider, permission or launch authority. An actual Kitty child-scope probe retained
its Home session while `ResolveWindow` returned unknown and provider `Declare` returned
`Access denied`.

Unverified observations cannot attach or move a window instance. Hooks cache a directory only
after observation succeeds, allowing a service-start race to retry at the next prompt.

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
stray `.git` in a projects folder cannot swallow the repositories beneath it. `/`, `$HOME` and
ancestors of `$HOME` are not Places. The walk is bounded; child Places are not duplicated in a
parent's default body. Path transport preserves native bytes rather than guessing from titles.

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

One step per tending moment: a place recedes visibly rather than vanishing. Pin/Keep protect
retention but do not freeze an automatically sized project. Explicit user resizing sets size
provenance and prevents automatic resizing. Hidden places leave Home without erasing their
retained records.

## Templates

Project presentation infers Coding, Video, Images and Documents from catalogue categories and
genuine context evidence. Neutral applications do not vote for a specialized template.
Hysteresis prevents transient evidence from repeatedly switching a project's body; the manual
Show As override takes precedence.

The default body is not a directory tree. Sessions, Files and Apps remain available as tabs;
Files lists the actual project directory on demand. Video, Images and Documents use real
recent-file/preview data. Missing data stays missing rather than becoming sample content.
Legacy activity-tree/form data is not the current default Home presentation.

| Verb | Record | Tile |
|---|---|---|
| `garden.pin` | `pinned` | never removed by recession; may still grow |
| `garden.keep` | retention protection | automatic geometry remains eligible for growth |
| `garden.unpin` | neither | tended freely again |
| `garden.hide` | `hidden`, unpinned | removed from every Home now (undoable); the record stays |
| `garden.forget` | dropped | removed from every Home now (undoable); activity may regrow it |

## The tender's rules

1. Tending happens only at tending moments: session start, Home reveal, `garden.tend`.
2. It emits `Place`, `Resize`, `Remove`. Never `Move`.
3. A `Place` uses its remembered landing when free, otherwise a stable path-seeded candidate
   with bounded free-space search. Nothing is displaced; a full field gets nothing.
4. A `Resize` is accepted only when `reflow::find_resize` satisfies it without any tile the
   tender does not own giving up span (garden-owned, unpinned, not user-sized) **and without any
   neighbour's cell changing**; when the requested span would shift a neighbour, each axis alone
   is tried before nothing is emitted.
5. A tile the user moved keeps its new cell as its landing; a tile the user resized becomes
   user-sized and is never resized again; a tile the tender never placed is the user's outright
   (pinned and kept); a pinned place is never removed by recession.
6. Every op carries a reason (`grew templaros to 4x3: 14 opens, 3 launches, 4 sessions`); the
   batch is one `History1` event and one `HomeField1.Undo` step.

## The project body and sessions

Small projects show their name and actionable state. Larger projects add contained Now slots
and template/session content, with Sessions, Files and Apps tabs. Active projects have the
strongest light surface, resting projects recede, and fading projects reduce to actionable
text. Color indicates attention such as failure or waiting, not an arbitrary application hue.

Session rows expose Focus/Restore for a live bound window and Resume for an ended session.
Resume is a brokered launch with the project's real working directory; it does not replay an
arbitrary recorded command. Plain shells and supported agent adapters have explicit resume
contracts. Foreground-process collection happens at tending/activation, not in a resident
process poller.

Supported Bash, fish and zsh hooks keep bounded private journals with sanitized command labels,
exit and pipeline status. Leading-space commands are omitted; full command text requires
explicit opt-in. Bash uses the command-read boundary rather than mistaking compound prompt
callbacks for commands, including with Kitty shell integration. Agent hooks consume declared
payloads rather than private agent stores. Editor folder hooks and process-directory/startup
evidence require current verified launch identity or the bounded display-only terminal
association above before attaching a window instance.

The Neovim folder hook is packaged separately. Code/VS Code workspace integration is
deferred indefinitely by the owner as of October 7, 2026; its experimental extension
sources are retained but excluded from both Debian packaging paths. No Code runtime
acceptance is claimed.

Place context actions and the Apps tab reach the selected providers through the same broker.
Unknown identity remains advisory and cannot borrow another application's launch authority.

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

## Native Home host and Desktop dock

Home uses the shared PixelKit input, focus, semantic graph, native IME caret and retained clipboard
worker. Its custom-painted controls declare their actual bounds/actions rather than a parallel widget
tree. Files and group rename dialogs are input-first: Tab/Shift-Tab visit the field, accept and cancel,
Enter activates the focused control, and Escape cancels. Their modal semantic roots exclude Home's
background; replacing a layout or loading a new scope retires old reader targets.

Home's custom role styles are logical on construction and become physical once through
`Ui::text_style` before measurement and drawing; tracked spacing remains in ems. The fonts in
`Ui::theme` are already physical and are not scaled again. Native Tab traversal registers the
actual tiles before their input handlers, so a tile keeps its existing arrows/activation/move/resize
behavior while nested controls own their own keyboard input. Escape cannot re-focus the search
while its pending key is trying to leave it.

The Desktop dock follows the configured Desktop directory through filed's change feed and
retains its own ordered placement and geometry. It supports native file previews, opening,
cross-client file drag/drop and filed's journalled copy/move/collision/undo operations.
Its contents are independent of garden activity. The old machine-facts Files tile is not the
current Home surface.

An outgoing file drag retains its immutable URI payload on the Wayland source. Transport
dispatch starts its bounded writer directly, so a foreign application's synchronous read
does not wait for Home to finish painting. Source lifecycle events still use the bridge's
UI queue; pointer serial and origin-surface checks still authorize the drag.

The dock's ordered native listing remains separate from generic folder refreshes. Index-change
notifications refresh ordinary folders while the dock follows its own watcher; an in-flight
generic folder response cannot replace the dock's order or hide its watcher error. An exact
resolved Desktop index root needs no optional volume device metadata to cover its own directory.
Coverage inherited from an ancestor still requires matching device evidence or an explicit
cross-mount policy, as well as a monitoring policy that materializes the directory's children.

Native paths remain byte-preserving through URI/drop and broker transports. A failed or stale
file-service operation is reported rather than replaced with invented directory contents.
See [accessibility](accessibility.md) for shared reader, clipboard, IME and font boundaries.

Owned October 7 rendering exercised 27 Home cases, including all project templates and the
corrected compact text-only strip. The managed desktop separately demonstrated single-click
window activation and verified compositor identity delivery. Full session, dock and physical
output acceptance remains separately evidenced; synthetic render data is not substituted for it.

## What is deliberately not here yet

Sibling grouping, a Wide-vs-Tall geometry axis, and portal sources. There is deliberately
no media consumption signal — no started/progress/completed and no `Resume` bloom: *arrived and
never opened* is the only claim this system can make honestly (`docs/garden/…§5.8`).
